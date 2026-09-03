#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# The node's own record of what it was doing.
#
# Runs beside the shard for the whole campaign and writes one line per
# sample to /tmp/xfs-watch.log, plus a stack dump whenever anything is
# blocked. It exists because every diagnosis this week began the same
# way: a node stopped, and nobody could say what it had been doing in
# the minutes before.
#
# Cheap on purpose -- reads from /proc and /sys, no perf, no tracing --
# so it can stay on for a seven-hour run without becoming the thing
# being measured.

L=/tmp/xfs-watch.log
: > "$L"
PREV_W=0
PREV_R=0
N=0

while true; do
  N=$((N + 1))
  T=$(date '+%H:%M:%S')

  # Block layer: issued vs merged says whether requests are batched or
  # dribbling out one at a time; in-flight and io_ms say whether the
  # device is the bottleneck.
  set -- $(grep -E " (vdb|vdh|vdc) " /proc/diskstats | awk '
    {r+=$4; rm+=$5; w+=$8; wm+=$9; infl+=$12; ioms+=$13}
    END {print r, rm, w, wm, infl, ioms}')
  R=${1:-0}; RM=${2:-0}; W=${3:-0}; WM=${4:-0}; INFL=${5:-0}; IOMS=${6:-0}
  DW=$((W - PREV_W)); DR=$((R - PREV_R)); PREV_W=$W; PREV_R=$R

  # Pressure: how much of the last ten seconds some task spent stalled.
  # One number, and it answers "is this machine saturated" without a
  # profiler.
  PIO=$(awk '/^some/{print $2}' /proc/pressure/io 2>/dev/null | cut -d= -f2)
  PCPU=$(awk '/^some/{print $2}' /proc/pressure/cpu 2>/dev/null | cut -d= -f2)
  PMEM=$(awk '/^some/{print $2}' /proc/pressure/memory 2>/dev/null | cut -d= -f2)

  D=$(ps -eo state | grep -c '^D')
  RR=$(ps -eo state | grep -c '^R')
  LOAD=$(cut -d' ' -f1 /proc/loadavg)
  DIRTY=$(awk '/^Dirty:/{print $2}' /proc/meminfo)
  WB=$(awk '/^Writeback:/{print $2}' /proc/meminfo)
  TEST=$(ps -eo args | grep -oE 'tests/generic/[0-9]+' | head -1 | grep -oE '[0-9]+$')

  printf '%s test=%s D=%s R=%s load=%s rd=+%s wr=+%s merged=%s infl=%s ioms=%s dirty=%sk wb=%sk psi_io=%s psi_cpu=%s psi_mem=%s\n' \
    "$T" "${TEST:--}" "$D" "$RR" "$LOAD" "$DR" "$DW" "$WM" "$INFL" "$IOMS" \
    "${DIRTY:-0}" "${WB:-0}" "${PIO:-0}" "${PCPU:-0}" "${PMEM:-0}" >> "$L"

  # Anything blocked, or a device with work in flight and no progress:
  # take the stacks while they still exist. timeout -k in the runner
  # kills the test and its children, and a capture taken after that
  # finds nothing.
  if [ "$D" -gt 0 ]; then
    {
      echo "--- $T blocked=$D ---"
      for p in $(ps -eo pid,state | awk '$2 ~ /^D/ {print $1}' | head -6); do
        echo "  pid $p $(ps -o comm= -p "$p" 2>/dev/null) $(ps -o etime= -p "$p" 2>/dev/null) wchan=$(cat /proc/$p/wchan 2>/dev/null)"
        sudo cat "/proc/$p/stack" 2>/dev/null | head -10 | sed 's/^/    /'
      done
    } >> "$L" 2>&1
  fi

  # Kernel messages that are not the routine mount chatter, once, as
  # they appear.
  sudo dmesg | grep -viE "bitmaps initialized|mounted v5|BEGIN generic|run fstests" \
    | tail -3 | while read -r line; do
      grep -qF "$line" "$L" 2>/dev/null || echo "  dmesg: $line" >> "$L"
    done

  sleep 10
done
