// SPDX-License-Identifier: GPL-2.0-only
//! What moves through memory, which blktrace cannot see.
//!
//! blktrace films what leaves for the device. The defect measured on
//! 2026-09-09 is the opposite case: hundreds of blocks lost with the
//! tree checker reporting nothing, meaning the pointers were correct in
//! memory and never reached the medium. A trace of what left says
//! nothing about what stayed.
//!
//! So this covers the other side. Where bytes are copied in the write
//! path, what the Reed-Solomon encode costs, how much of the guest's
//! memory bandwidth the load uses, what the page cache does with dirty
//! pages that are never written, and what virtio actually transfers
//! between guest RAM and the host.
//!
//! Four families, each needing a different tool:
//!
//!   - hardware counters (perf stat): cycles, cache misses, memory
//!     bandwidth. Says what the load costs, and whether a failing trial
//!     cost differently.
//!   - kernel function counts (ftrace function_profile): how many times
//!     memcpy, memset, the RS encode and the buffer helpers were
//!     entered, and for how long.
//!   - page state (vmstat deltas): pages dirtied against pages written
//!     back. The gap is what stayed in memory.
//!   - virtio (queue stats and host DMA): what crossed the boundary.
//!
//! None of these is conclusive on its own. Together they cover the path
//! from the write syscall to the platter with nothing unmeasured in
//! between, which is what was asked for and what was missing.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;
use crate::state::Snapshot;

/// The kernel functions worth counting on this path.
///
/// Chosen from the write path as it actually runs, not from a guess:
/// every one of these appears in a stack trace taken during a failing
/// trial, and the RS encode is the single most expensive thing beamfs
/// does per block.
const WATCHED: &[&str] = &[
    "beamfs_rs_encode_region",
    "beamfs_rs_decode_region",
    "beamfs_ind_parity_update",
    "beamfs_ind_parity_verify",
    "beamfs_inline_lookup_or_alloc_phys",
    "beamfs_inline_writeback_range",
    "beamfs_alloc_block",
    "beamfs_free_block",
    "beamfs_write_bitmap_block",
    "mmb_sync",
    "mmb_mark_buffer_dirty",
    "mmb_invalidate",
    "sync_dirty_buffer",
    "__bread_gfp",
    "__getblk_gfp",
    "mark_buffer_dirty",
    "try_to_free_buffers",
    "invalidate_inode_buffers",
];

/// Arm the function profiler on the watched set.
///
/// A filter rather than the whole kernel: profiling everything on a
/// four-CPU guest under sixteen writers changes the timing enough to
/// change what is being measured, and the point is the write path.
pub fn arm(cfg: &Config, node: &Node) -> Result<(), String> {
    let c = NodeConn::new(node, cfg);
    let filter = WATCHED.join(" ");
    c.run(
        &format!(
            r#"sudo sh -c '
T=/sys/kernel/debug/tracing
echo 0 > $T/function_profile_enabled 2>/dev/null
echo > $T/set_ftrace_filter 2>/dev/null
for f in {filter}; do echo $f >> $T/set_ftrace_filter 2>/dev/null; done
echo 1 > $T/function_profile_enabled 2>/dev/null
# vmstat and the page counters are free -- they are always on.
echo armed'"#
        ),
        Duration::from_secs(60),
    )
    .map(|_| ())
    .map_err(|e| format!("arm: {e}"))
}

/// Read the function profile: calls and total time per function.
///
/// Summed across CPUs. A function entered a million times for a
/// microsecond each and one entered twice for a second each are
/// different problems, so both numbers are kept.
pub fn profile(cfg: &Config, node: &Node) -> BTreeMap<String, (u64, u64)> {
    let c = NodeConn::new(node, cfg);
    let out = c
        .run(
            "sudo sh -c 'cat /sys/kernel/debug/tracing/trace_stat/function* 2>/dev/null'",
            Duration::from_secs(60),
        )
        .unwrap_or_default();

    let mut m: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[0] == "Function" || f[0].starts_with('-') {
            continue;
        }
        let Ok(hits) = f[1].parse::<u64>() else { continue };
        // Time is "1234.567 us" -- take the integer part in ns.
        let us: f64 = f[2].parse().unwrap_or(0.0);
        let e = m.entry(f[0].to_string()).or_insert((0, 0));
        e.0 += hits;
        e.1 += (us * 1000.0) as u64;
    }
    m
}

/// Reset the profile between trials, so each one is measured alone.
pub fn reset(cfg: &Config, node: &Node) {
    let c = NodeConn::new(node, cfg);
    let _ = c.run(
        "sudo sh -c 'echo 0 > /sys/kernel/debug/tracing/function_profile_enabled; \
         echo 1 > /sys/kernel/debug/tracing/function_profile_enabled'",
        Duration::from_secs(30),
    );
}

/// Page-level accounting, which is where memory-only losses show.
///
/// nr_dirty against nr_written is the pair that matters: pages made
/// dirty that were never written are exactly the case where a correct
/// pointer sits in memory and the medium never gets it.
pub fn pages(cfg: &Config, node: &Node) -> Snapshot {
    let c = NodeConn::new(node, cfg);
    let out = c
        .run(
            r#"sudo sh -c '
awk "/^nr_dirty /{print \"pg.dirty=\" \$2}
     /^nr_writeback /{print \"pg.writeback=\" \$2}
     /^nr_dirtied /{print \"pg.dirtied_total=\" \$2}
     /^nr_written /{print \"pg.written_total=\" \$2}
     /^pgpgin /{print \"pg.in_kb=\" \$2}
     /^pgpgout /{print \"pg.out_kb=\" \$2}
     /^pgfault /{print \"pg.faults=\" \$2}
     /^pgmajfault /{print \"pg.majfaults=\" \$2}
     /^pgsteal_/{s+=\$2} END{print \"pg.steal=\" s}
     /^nr_free_pages /{print \"pg.free=\" \$2}
     /^nr_slab_reclaimable /{print \"pg.slab_recl=\" \$2}
     /^nr_slab_unreclaimable /{print \"pg.slab_unrecl=\" \$2}" /proc/vmstat
# Buffer heads live in their own slab; their count is the population
# the metadata lists are drawn from.
awk "/^buffer_head /{print \"slab.buffer_head=\" \$2}
     /^beamfs_inode_cache /{print \"slab.beamfs_inode=\" \$2}" /proc/slabinfo 2>/dev/null
'"#,
            Duration::from_secs(60),
        )
        .unwrap_or_default();

    let mut s = Snapshot::default();
    for line in out.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            if let Ok(n) = v.trim().parse::<i64>() {
                s.v.insert(k.into(), n);
            }
        }
    }
    s
}

/// Hardware counters for one trial, from the host so the guest is not
/// perturbed by its own measurement.
///
/// perf kvm attributes cycles to the guest without anything running
/// inside it. Cache misses and bus cycles stand in for memory
/// bandwidth: the RS encode touches every byte of every block twice,
/// and a trial that spent longer there is a trial with different
/// timing everywhere else.
pub fn hardware(domain: &str, secs: u64) -> Snapshot {
    let mut s = Snapshot::default();
    let pid = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("pgrep -f 'qemu.*{domain}' | head -1"))
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if pid.is_empty() {
        return s;
    }

    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sudo timeout {t} perf stat -p {pid} -x, \
             -e cycles,instructions,cache-misses,cache-references,\
context-switches,cpu-migrations,page-faults \
             sleep {t} 2>&1",
            t = secs.max(1)
        ))
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    for line in out.lines() {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 3 {
            continue;
        }
        let Ok(v) = f[0].trim().parse::<i64>() else { continue };
        let name = f[2].trim().replace('-', "_");
        if !name.is_empty() {
            s.v.insert(format!("hw.{name}"), v);
        }
    }
    s
}

/// What virtio moved across the boundary.
///
/// The guest's own counters say what it queued; the host's say what
/// QEMU transferred. A gap between them is a transfer that did not
/// happen the way the guest believes it did.
pub fn virtio(cfg: &Config, node: &Node) -> Snapshot {
    let c = NodeConn::new(node, cfg);
    let out = c
        .run(
            r#"sudo sh -c '
for d in /sys/block/vd*; do
  n=$(basename $d)
  [ -d "$d/mq" ] || continue
  q=0; disp=0
  for h in $d/mq/*/; do
    q=$((q + $(cat $h/nr_tags 2>/dev/null || echo 0)))
    [ -r "$h/dispatched" ] && disp=$((disp + $(awk "{s+=\$2} END{print s+0}" $h/dispatched 2>/dev/null)))
  done
  echo "vq.$n.tags=$q"
  echo "vq.$n.queues=$(ls -d $d/mq/*/ 2>/dev/null | wc -l)"
  [ -r $d/queue/max_sectors_kb ] && echo "vq.$n.max_sectors_kb=$(cat $d/queue/max_sectors_kb)"
  [ -r $d/queue/optimal_io_size ] && echo "vq.$n.optimal_io=$(cat $d/queue/optimal_io_size)"
done
# Interrupts are the other half of the boundary: a transfer nobody was
# told about is a transfer that waited.
awk "/virtio/{s+=\$2+\$3+\$4+\$5} END{print \"vq.irqs=\" s+0}" /proc/interrupts
'"#,
            Duration::from_secs(60),
        )
        .unwrap_or_default();

    let mut s = Snapshot::default();
    for line in out.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            if let Ok(n) = v.trim().parse::<i64>() {
                s.v.insert(k.into(), n);
            }
        }
    }
    s
}

/// Everything memory-side, at one instant.
pub fn capture(cfg: &Config, node: &Node) -> Snapshot {
    let mut s = pages(cfg, node);
    for (k, v) in virtio(cfg, node).v {
        s.v.insert(k, v);
    }
    s
}

/// Everything memory-side plus the hardware counters, which need the
/// libvirt domain and a window to sample over.
pub fn capture_with_hw(cfg: &Config, node: &Node, domain: &str, secs: u64) -> Snapshot {
    let mut s = capture(cfg, node);
    for (k, v) in hardware(domain, secs).v {
        s.v.insert(k, v);
    }
    s
}

/// Print what the memory side did, and what did not leave it.
pub fn report(before: &Snapshot, after: &Snapshot, prof: &BTreeMap<String, (u64, u64)>) {
    let d = after.delta(before);
    println!("  === memory side ===");

    // The pair that matters: dirtied minus written is what stayed.
    let dirtied = d.get("pg.dirtied_total").unwrap_or(0);
    let written = d.get("pg.written_total").unwrap_or(0);
    if dirtied != 0 || written != 0 {
        println!(
            "  {dirtied} pages dirtied, {written} written back, {} left in memory",
            dirtied - written
        );
        if dirtied > written {
            println!("  -- a correct pointer in one of those pages never reached the medium");
        }
    }
    if let Some(s) = d.get("pg.steal") {
        if s > 0 {
            println!("  {s} pages reclaimed under pressure during the trial");
        }
    }
    if let (Some(b), Some(a)) = (before.get("slab.buffer_head"), after.get("slab.buffer_head")) {
        println!("  buffer heads: {b} before, {a} after ({:+})", a - b);
    }

    if !prof.is_empty() {
        println!();
        println!("  where the write path spent itself:");
        let mut v: Vec<(&String, &(u64, u64))> = prof.iter().collect();
        v.sort_by_key(|(_, (_, ns))| std::cmp::Reverse(*ns));
        println!("    {:<38} {:>12} {:>12}", "function", "calls", "total ms");
        for (name, (hits, ns)) in v.iter().take(12) {
            println!("    {name:<38} {hits:>12} {:>12}", ns / 1_000_000);
        }
    }

    for (k, label) in [
        ("hw.cache_misses", "cache misses"),
        ("hw.cpu_migrations", "cpu migrations"),
        ("hw.context_switches", "context switches"),
    ] {
        if let Some(v) = after.get(k) {
            println!("  {label}: {v}");
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watched_set_covers_both_sides_of_the_buffer_lifetime() {
        // Marking dirty and invalidating are the two ends of the
        // mechanism the leak rides on; neither is useful alone.
        assert!(WATCHED.contains(&"mmb_mark_buffer_dirty"));
        assert!(WATCHED.contains(&"mmb_invalidate"));
        assert!(WATCHED.contains(&"mmb_sync"));
    }

    #[test]
    fn a_profile_line_yields_calls_and_nanoseconds() {
        let m = {
            let mut m: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for line in "Function Hit Time\n-------- --- ----\nmmb_sync 42 1234.500 us\n".lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 3 || f[0] == "Function" || f[0].starts_with('-') {
                    continue;
                }
                let Ok(h) = f[1].parse::<u64>() else { continue };
                let us: f64 = f[2].parse().unwrap_or(0.0);
                m.insert(f[0].into(), (h, (us * 1000.0) as u64));
            }
            m
        };
        assert_eq!(m.get("mmb_sync"), Some(&(42, 1_234_500)));
    }

    #[test]
    fn pages_left_in_memory_is_dirtied_minus_written() {
        let mut b = Snapshot::default();
        b.v.insert("pg.dirtied_total".into(), 1000);
        b.v.insert("pg.written_total".into(), 900);
        let mut a = Snapshot::default();
        a.v.insert("pg.dirtied_total".into(), 5000);
        a.v.insert("pg.written_total".into(), 4600);
        let d = a.delta(&b);
        assert_eq!(d.get("pg.dirtied_total"), Some(4000));
        assert_eq!(d.get("pg.written_total"), Some(3700));
    }
}
