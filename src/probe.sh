#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Run one test under continuous instrumentation.
#
# The runner records an outcome. This records what the machine was doing
# while it reached that outcome, because a test that has stopped
# progressing looks identical to one that is merely slow, and telling
# them apart is the whole difficulty.
#
# Everything lands in /tmp/probe-<test>/ and is archived whole. No tail,
# no head, no filtering: the line that explains a hang is never the one
# that looked worth keeping.
#
# $1 test  $2 TEST_DEV  $3 SCRATCH_DEV  $4 timeout seconds

T=$1; TEST_DEV=$2; SCRATCH_DEV=$3; LIMIT=${4:-1200}
D=/tmp/probe-$(echo "$T" | tr / -)
rm -rf "$D"; mkdir -p "$D"

{
  echo "test:     $T"
  echo "kernel:   $(uname -r)"
  echo "started:  $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "uptime:   $(cut -d' ' -f1 /proc/uptime)s"
  echo ""
  lsblk -o NAME,SIZE,TYPE 2>/dev/null | grep -E "^vd|NAME"
  echo ""
  head -4 /proc/meminfo
  echo "cpus: $(grep -c processor /proc/cpuinfo)"
} > "$D/baseline.txt" 2>&1

sudo umount /mnt/test /mnt/scratch 2>/dev/null
sudo mkdir -p /mnt/test /mnt/scratch
sudo tee /usr/xfstests/local.config >/dev/null <<CFG
export FSTYP=beamfs
export TEST_DEV=/dev/$TEST_DEV
export TEST_DIR=/mnt/test
export SCRATCH_DEV=/dev/$SCRATCH_DEV
export SCRATCH_MNT=/mnt/scratch
export MKFS_OPTIONS="-N 16384"
export MOUNT_OPTIONS=""
CFG
cp /usr/xfstests/local.config "$D/local.config"

sudo mkfs.beamfs -N 16384 "/dev/$TEST_DEV" > "$D/mkfs.txt" 2>&1
sudo mount -t beamfs "/dev/$TEST_DEV" /mnt/test >> "$D/mkfs.txt" 2>&1

sudo sh -c "echo 'PROBE BEGIN $T' > /dev/kmsg" 2>/dev/null
MARK=$(sudo dmesg | wc -l)

cd /usr/xfstests || exit 1
T0=$(date +%s)
sudo timeout -k 10 "$LIMIT" ./check "$T" > "$D/check.out" 2>&1 &
CP=$!

N=0
while kill -0 $CP 2>/dev/null; do
  N=$((N + 1))
  {
    echo "=== sample $N at $(( $(date +%s) - T0 ))s ==="
    echo "--- diskstats ---"
    grep -E " ($TEST_DEV|$SCRATCH_DEV) " /proc/diskstats
    echo "--- space ---"
    df -h /mnt/test /mnt/scratch 2>&1 | tail -2
    echo "--- files ---"
    echo "test=$(ls /mnt/test 2>/dev/null | wc -l) scratch=$(ls /mnt/scratch 2>/dev/null | wc -l)"
    echo "--- load ---"
    cat /proc/loadavg
    echo "--- tasks D or R ---"
    ps -eo pid,state,wchan:24,etime,comm | awk 'NR==1 || $2 ~ /^D|^R/'
    echo "--- stacks ---"
    for p in $(ps -eo pid,state,comm | awk '$2 ~ /^D|^R/ && $3 !~ /^(ps|awk|sh|sleep)$/ {print $1}'); do
      echo "  pid $p $(ps -o comm= -p "$p" 2>/dev/null) $(ps -o etime= -p "$p" 2>/dev/null)"
      sudo cat "/proc/$p/stack" 2>/dev/null | sed 's/^/    /'
    done
    echo "--- test process ---"
    # Bracketed: the pgrep command line carries the pattern itself.
    for p in $(pgrep -f "[t]ests/$T" 2>/dev/null); do
      echo "  pid $p wchan=$(cat /proc/$p/wchan 2>/dev/null)"
      sed 's/^/    /' "/proc/$p/io" 2>/dev/null
    done
    echo "--- new kernel messages ---"
    sudo dmesg | tail -n +$((MARK + 1)) | tail -20
    echo ""
  } >> "$D/samples.txt" 2>&1
  sleep 10
done

wait $CP
RC=$?
EL=$(( $(date +%s) - T0 ))

{
  echo "rc:      $RC"
  echo "elapsed: ${EL}s"
  echo "samples: $N"
  case $RC in
    124) echo "verdict: TIMEOUT" ;;
    *) if grep -q '^Passed all' "$D/check.out"; then echo "verdict: PASS"
       elif grep -q '\[not run\]' "$D/check.out"; then echo "verdict: NOTRUN"
       else echo "verdict: FAIL"; fi ;;
  esac
} > "$D/verdict.txt" 2>&1

sudo dmesg > "$D/dmesg-full.txt" 2>&1
sudo dmesg | tail -n +$((MARK + 1)) > "$D/dmesg-test.txt" 2>&1

{
  for p in $(ps -eo pid,state,comm | awk '$2 ~ /^D|^R/ {print $1}'); do
    echo "pid $p $(ps -o comm= -p "$p" 2>/dev/null) state=$(ps -o state= -p "$p" 2>/dev/null)"
    sudo cat "/proc/$p/stack" 2>/dev/null | sed 's/^/  /'
  done
} > "$D/stacks-final.txt" 2>&1

sudo umount /mnt/test /mnt/scratch 2>/dev/null
{
  echo "=== fsck $TEST_DEV ==="
  sudo fsck.beamfs "/dev/$TEST_DEV" 2>&1
  echo "rc=$?"
  echo "=== fsck $SCRATCH_DEV ==="
  sudo fsck.beamfs "/dev/$SCRATCH_DEV" 2>&1
  echo "rc=$?"
} > "$D/fsck.txt" 2>&1

cp /usr/xfstests/results/generic/*.out.bad "$D/" 2>/dev/null
cp /usr/xfstests/results/generic/*.full "$D/" 2>/dev/null

sudo pkill -9 -f "tests/generic" 2>/dev/null
sudo umount -l /mnt/test /mnt/scratch 2>/dev/null

tar czf "$D.tar.gz" -C /tmp "$(basename "$D")" 2>/dev/null
echo "$D.tar.gz"
