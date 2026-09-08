#!/bin/sh
# beamfs-trace.sh -- capture harness for beamfs defect tracing.
#
# Every previous attempt lost its trace to the same handful of mechanics:
# an ssh session dying takes the drain with it, /tmp is a 982 MiB tmpfs
# that a 150 MiB run fills three times over, trace_printk with a
# double-escaped newline writes one 158 MB line, and a probe placed by
# line number lands in whichever function the file has drifted into.
#
# This runs entirely on the node, detached from any session, writes to
# the real disk, and reports what it captured before anyone tries to
# read it.
#
# Usage:  beamfs-trace.sh <test> [runs]
#   e.g.  beamfs-trace.sh 464 4
#
# Leaves, per run N, under /var/beamfs-trace/:
#   runN.txt   the trace, one event per line
#   lostN.txt  the blocks fsck calls used-but-unreferenced
#   sumN.txt   a summary: counts, and each lost block's history
set -u

TEST="${1:?usage: beamfs-trace.sh <test> [runs]}"
RUNS="${2:-1}"
T=/sys/kernel/debug/tracing
OUT=/var/beamfs-trace
TESTDEV=/dev/vdb
SCRATCHDEV=/dev/vdc
MKFS_OPTS="-N 16384"

die() { echo "harness: $*" >&2; exit 1; }

# --- preconditions, checked rather than assumed ---------------------
[ -d "$T" ] || die "no tracefs at $T"
[ -w "$T/tracing_on" ] || die "$T not writable (run as root)"
command -v fsck.beamfs >/dev/null || die "fsck.beamfs not found"
[ -d /usr/xfstests ] || die "no /usr/xfstests"

# The rootfs has room; /tmp is a tmpfs and does not.
mkdir -p "$OUT" || die "cannot create $OUT"
avail=$(df -k "$OUT" | awk 'NR==2 {print $4}')
[ "$avail" -gt 1048576 ] || die "$OUT has only ${avail}K free, need 1G"

cleanup_drain() {
	pkill -f "cat $T/trace_pipe" 2>/dev/null
	sleep 1
}

# A drain from a previous invocation still holds the pipe and would
# swallow this run's events.
cleanup_drain
trap cleanup_drain EXIT INT TERM

echo "harness: test=generic/$TEST runs=$RUNS out=$OUT"
echo "harness: $(( avail / 1024 )) MiB free on $OUT"

for r in $(seq 1 "$RUNS"); do
	echo "=== run $r/$RUNS ==="

	umount /mnt/test /mnt/scratch 2>/dev/null
	mkfs.beamfs $MKFS_OPTS "$SCRATCHDEV" >/dev/null 2>&1 || die "mkfs scratch"
	mkfs.beamfs $MKFS_OPTS "$TESTDEV"    >/dev/null 2>&1 || die "mkfs test"
	mkdir -p /mnt/test /mnt/scratch
	mount -t beamfs "$TESTDEV" /mnt/test || die "mount test"

	echo 0 > "$T/tracing_on"
	echo 16384 > "$T/buffer_size_kb"
	: > "$T/trace"

	# setsid so the drain outlives the shell that started it, and a
	# real file on disk rather than the tmpfs.
	setsid sh -c "cat $T/trace_pipe > $OUT/run$r.txt" </dev/null >/dev/null 2>&1 &
	sleep 1
	pgrep -f "cat $T/trace_pipe" >/dev/null || die "drain did not start"

	echo 1 > "$T/tracing_on"
	cd /usr/xfstests || die "cd xfstests"
	verdict=$(timeout -k 5 900 ./check "generic/$TEST" 2>&1 |
		  grep -E '^Ran:|^Passed all|^Failed' | tr '\n' ' ')
	echo 0 > "$T/tracing_on"
	sleep 3
	cleanup_drain

	chmod 644 "$OUT/run$r.txt" 2>/dev/null

	umount /mnt/scratch 2>/dev/null
	fsck.beamfs -v "$SCRATCHDEV" 2>&1 |
		grep -oE 'block [0-9]+ marked' | grep -oE '[0-9]+' > "$OUT/lost$r.txt"

	lines=$(grep -c '' "$OUT/run$r.txt" 2>/dev/null || echo 0)
	lost=$(grep -c '' "$OUT/lost$r.txt" 2>/dev/null || echo 0)
	bytes=$(stat -c %s "$OUT/run$r.txt" 2>/dev/null || echo 0)

	# One 158 MB line means trace_printk lost its newline. Say so here
	# rather than three commands later when the analysis comes back empty.
	if [ "$lines" -le 1 ] && [ "$bytes" -gt 1000000 ]; then
		echo "  WARNING: $bytes bytes in $lines line(s) -- trace_printk"
		echo "           newline is escaped wrong in the probes."
	fi

	{
		echo "run $r: $verdict"
		echo "events: $lines  bytes: $bytes  lost: $lost"
		echo
		echo "probes that fired:"
		for p in alloc free inst sindinst l1inst read_or_alloc read_lookup \
		         dind l1read trunc newibh newl1 wbdata; do
			c=$(grep -c "$p " "$OUT/run$r.txt" 2>/dev/null || echo 0)
			[ "$c" -gt 0 ] && printf "  %-14s %s\n" "$p" "$c"
		done
		echo
		echo "lost blocks, last event and full history of the first three:"
		head -3 "$OUT/lost$r.txt" | while read -r b; do
			echo "  ---- block $b ----"
			grep -E "(blk|new|val)=$b( |\$)" "$OUT/run$r.txt" |
				tail -8 | cut -c1-120 | sed 's/^/    /'
		done
		echo
		echo "last allocator of every lost block, by site:"
		while read -r b; do
			grep -E "alloc .*blk=$b " "$OUT/run$r.txt" | tail -1 |
				grep -oE 'from=[a-z_]+\+0x[0-9a-f]+'
		done < "$OUT/lost$r.txt" | sort | uniq -c | sort -rn | head -5 |
			sed 's/^/  /'
		echo
		echo "same, by inode:"
		while read -r b; do
			grep -E "alloc .*blk=$b " "$OUT/run$r.txt" | tail -1 |
				grep -oE 'ino=[0-9]+'
		done < "$OUT/lost$r.txt" | sort | uniq -c | sort -rn | head -5 |
			sed 's/^/  /'
	} > "$OUT/sum$r.txt" 2>&1

	echo "  $verdict"
	echo "  events=$lines lost=$lost  -> $OUT/sum$r.txt"

	# Keep the disk from filling across runs: the summary is what
	# matters, the raw trace only for the run being investigated.
	if [ "$r" -lt "$RUNS" ] && [ "$lost" -eq 0 ]; then
		rm -f "$OUT/run$r.txt"
		echo "  (clean run, raw trace dropped)"
	fi
done

echo
echo "=== summaries ==="
for f in "$OUT"/sum*.txt; do
	[ -f "$f" ] && { echo "--- $f ---"; head -4 "$f"; }
done
