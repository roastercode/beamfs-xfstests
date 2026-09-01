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
    echo "generic/$t HANG ${EL}s" >> $R
    sudo pkill -9 -f "tests/generic" 2>/dev/null
    sudo pkill -9 -f "/usr/xfstests/check" 2>/dev/null
    sleep 2
  elif echo "$OUT" | grep -q "\[not run\]"; then
    WHY=$(echo "$OUT" | grep -oE '\[not run\].*' | head -1 | cut -c11-70)
    echo "generic/$t NOTRUN ${EL}s $WHY" >> $R
  elif echo "$OUT" | grep -q "^Passed all"; then
    echo "generic/$t PASS ${EL}s" >> $R
  else
    echo "generic/$t FAIL ${EL}s" >> $R
    mkdir -p /tmp/xfs-failures
    echo "$OUT" > "/tmp/xfs-failures/generic-$t.log"
  fi
done

sudo umount /mnt/test /mnt/scratch 2>/dev/null
echo "DONE" >> $R
