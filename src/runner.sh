#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# One shard of the suite, one test per invocation.
#
# Deployed to each node and run there. Writes a line per test to
# /tmp/xfs-results.txt as it goes, so a run that is interrupted keeps
# everything it had done and the next one resumes from it.
#
# $1 TEST_DEV  $2 SCRATCH_DEV  $3 shard index  $4 shard count
# $5 timeout seconds  $6 mkfs options  $7 resume (0/1)

TEST_DEV=$1; SCRATCH_DEV=$2; SHARD=$3; NSHARD=$4
LIMIT=${5:-300}; MKFS_OPTS=${6:--N 16384}; RESUME=${7:-1}
R=/tmp/xfs-results.txt

[ "$RESUME" = "1" ] || : > $R
touch $R

sudo mkdir -p /mnt/test /mnt/scratch
sudo tee /usr/xfstests/local.config >/dev/null <<CFG
export FSTYP=beamfs
export TEST_DEV=/dev/$TEST_DEV
export TEST_DIR=/mnt/test
export SCRATCH_DEV=/dev/$SCRATCH_DEV
export SCRATCH_MNT=/mnt/scratch
export MKFS_OPTIONS="$MKFS_OPTS"
export MOUNT_OPTIONS=""
CFG

# The harness dispatches on FSTYP through a long list of case statements
# and a filesystem it has not heard of falls through all of them. beamfs
# goes in the same arms as ext2 -- one mkfs, one fsck, no geometry
# options -- except mount options, since it implements neither acl nor
# user_xattr. Idempotent: applied only if not already there.
sudo python3 - <<'PY' 2>/dev/null
import re
from pathlib import Path
for p in ("/usr/xfstests/common/config", "/usr/xfstests/common/rc"):
    f = Path(p); s = f.read_text()
    if "beamfs|ext2" in s:
        continue
    s = re.sub(r"(?m)^(\s*)ext2\|", r"\1beamfs|ext2|", s)
    s = re.sub(r"(?m)^(\s*)ext2\)", r"\1beamfs|ext2)", s)
    f.write_text(s)
f = Path("/usr/xfstests/common/config"); s = f.read_text()
f.write_text(s.replace("beamfs|ext2|ext3|ext4|ext4dev)", "ext2|ext3|ext4|ext4dev)", 1))
PY

i=0
for t in $(ls /usr/xfstests/tests/generic/[0-9]*.out 2>/dev/null \
           | sed 's|.*/||;s|\.out||' | sort -n); do
  i=$((i+1))
  # Modulo rather than contiguous ranges: the slow tests -- fsstress,
  # fsx, anything that fills the device -- are clustered by number, and
  # a contiguous split would land them all on one node.
  [ $(( i % NSHARD )) -ne "$SHARD" ] && continue
  grep -q "^generic/$t " $R 2>/dev/null && continue

  T0=$(date +%s)
  # A marker in the ring buffer, so the messages belonging to this test
  # can be told from the ones before it. Without it "dmesg | tail" after
  # a failure is a mix of this test and the twenty that came first.
  sudo sh -c "echo 'beamfs-xfstests: BEGIN generic/$t' > /dev/kmsg" 2>/dev/null
  sudo umount /mnt/test /mnt/scratch 2>/dev/null
  sudo mkfs.beamfs $MKFS_OPTS /dev/$TEST_DEV >/dev/null 2>&1
  if ! sudo mount -t beamfs /dev/$TEST_DEV /mnt/test 2>/dev/null; then
    echo "generic/$t MOUNTFAIL $(( $(date +%s) - T0 ))s" >> $R
    continue
  fi

  cd /usr/xfstests || exit 1
  OUT=$(sudo timeout -k 10 "$LIMIT" ./check "generic/$t" 2>&1)
  RC=$?
  EL=$(( $(date +%s) - T0 ))

  if [ $RC -eq 124 ]; then
    # Killed. Anything it left behind holds a lock on the filesystem and
    # would wedge the next test too.
    #
    # The stacks are taken here, before the kill, because a node this
    # far gone often stops answering the network within seconds and the
    # orchestrator's own attempt arrives too late.
    mkdir -p /tmp/xfs-failures
    {
      echo "=== blocked tasks at kill time ==="
      for p in $(ps -eo pid,state | awk '$2 ~ /D/ {print $1}' | head -5); do
        echo "--- pid $p $(ps -o comm= -p $p) $(ps -o etime= -p $p) ---"
        sudo cat /proc/$p/stack 2>/dev/null | head -12
      done
      echo ""
      echo "=== kernel messages ==="
      sudo dmesg | tail -40
      echo ""
      echo "=== mounts ==="
      mount | grep beamfs
    } > "/tmp/xfs-failures/generic-$t.log" 2>&1
    echo "generic/$t HANG ${EL}s" >> $R
    sudo pkill -9 -f "tests/generic" 2>/dev/null
    sudo pkill -9 -f "/usr/xfstests/check" 2>/dev/null
    # The workers the test spawned are named for themselves, not for the
    # test, so killing the test leaves them behind. 128 fsstress
    # processes accumulated this way on one node, each holding a folio
    # lock the next test then waited on: every test after the first hang
    # hung too, at 1200s each.
    sudo pkill -9 fsstress fsx dd aio-dio-regress 2>/dev/null
    sleep 3

    # A task in uninterruptible sleep does not die on SIGKILL, so if any
    # remain the node is not usable and no amount of killing will make
    # it so. Stop here: the orchestrator sees the shard end, restarts
    # the domain, and relaunches -- which resumes from this file.
    STUCK=$(ps -eo state | grep -c '^D')
    if [ "$STUCK" -gt 4 ]; then
      echo "STUCK $STUCK tasks in D after generic/$t" >> $R
      sudo umount -l /mnt/test /mnt/scratch 2>/dev/null
      echo "DONE" >> $R
      exit 0
    fi
  elif echo "$OUT" | grep -q "\[not run\]"; then
    WHY=$(echo "$OUT" | grep -oE '\[not run\].*' | head -1 | cut -c11-70)
    echo "generic/$t NOTRUN ${EL}s $WHY" >> $R
  elif echo "$OUT" | grep -q "^Passed all"; then
    # A test can pass while the kernel logs a BUG, a WARNING or an
    # uncorrectable block. The harness does not look, so the run reports
    # a green test over a filesystem that just corrupted something.
    INC=$(sudo dmesg | sed -n "/BEGIN generic\/$t\$/,\$p" \
          | grep -ciE "BUG:|WARNING:|Oops|call trace|uncorrectable|corrupt" 2>/dev/null)
    INC=${INC:-0}

    # And fsck after every test, not only after failures. beamfs has a
    # checker that found four real defects in a day; a test that passes
    # and leaves the volume inconsistent is exactly what it catches and
    # exactly what the harness misses.
    sudo umount /mnt/test 2>/dev/null
    FSCK=$(sudo fsck.beamfs /dev/$TEST_DEV 2>&1)
    FRC=$?
    sudo mount -t beamfs /dev/$TEST_DEV /mnt/test 2>/dev/null

    if [ "$INC" -gt 0 ] || [ $FRC -ne 0 ]; then
      # Recorded as a failure, because it is one: the test's own
      # criterion was met and the filesystem is still wrong.
      echo "generic/$t FAIL ${EL}s dirty-pass incidents=$INC fsck=$FRC" >> $R
      mkdir -p /tmp/xfs-failures
      {
        echo "=== test passed but left evidence ==="
        echo "kernel incidents: $INC   fsck rc: $FRC"
        echo ""
        echo "=== fsck output ==="
        echo "$FSCK"
        echo ""
        echo "=== kernel messages for this test ==="
        sudo dmesg | sed -n "/BEGIN generic\/$t\$/,\$p" | head -60
      } > "/tmp/xfs-failures/generic-$t.log" 2>&1
    else
      echo "generic/$t PASS ${EL}s" >> $R
    fi
  else
    echo "generic/$t FAIL ${EL}s" >> $R
    # The whole harness output, plus what the kernel said while the test
    # ran. A failure line in a results file names the test and nothing
    # else; the diff and the dmesg around it are what say why.
    mkdir -p /tmp/xfs-failures
    {
      echo "=== check output ==="
      echo "$OUT"
      echo ""
      echo "=== expected vs got ==="
      diff -u "/usr/xfstests/tests/generic/$t.out" \
              "/usr/xfstests/results/generic/$t.out.bad" 2>/dev/null | head -60
      echo ""
      echo "=== kernel messages ==="
      sudo dmesg | tail -30
    } > "/tmp/xfs-failures/generic-$t.log" 2>&1
  fi
done

sudo umount /mnt/test /mnt/scratch 2>/dev/null
echo "DONE" >> $R
