// SPDX-License-Identifier: GPL-2.0-only
//! Reproduce the block leak under load and pull the evidence out.
//!
//! generic/464 loses blocks on roughly half its runs, and a run takes
//! three minutes. Chasing that through the suite means one answer per
//! twenty minutes. The load it applies -- sixteen processes truncating,
//! writing, appending and forcing writeback across two hundred files --
//! reproduces the leak by itself in two to nine loops, so this drives
//! that load directly and stops the moment fsck reports a lost block.
//!
//! Two things are the point of doing it here rather than in a shell
//! script on the node:
//!
//! The evidence lives on the workstation. Every trace taken during the
//! night of 2026-09-08 was written to the node's rootfs and lost when
//! the next image was built -- 107 captured leaks, gone. Traces are now
//! pulled back after each catch, into the same directory history.rs
//! already uses, which survives a rebuild and a power cut.
//!
//! And a campaign that is interrupted leaves nothing behind that breaks
//! the next one. The shell version signalled its writers through a file
//! in /tmp; a killed run left that file in place, and the next start
//! found its writers exiting immediately and looped for an hour writing
//! nothing. State that outlives the process is state that has to be
//! reset by hand, so there is none: the writers are killed by pkill and
//! the node is left mounted or not, either of which the next run fixes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::config::Node;
use crate::load;
use crate::node::NodeConn;

/// Where captures are kept, next to the run history.
///
/// Under $XDG_DATA_HOME or ~/.local/share, both of which are on the
/// workstation's disk. The node keeps nothing.
pub fn default_root() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("beamfs-xfstests/traces");
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".local/share/beamfs-xfstests/traces");
    }
    PathBuf::from("/var/tmp/beamfs-xfstests/traces")
}

pub struct Capture {
    pub seq: u32,
    pub lost: usize,
    pub loop_no: u32,
    pub events: usize,
    pub dir: PathBuf,
}

/// Drain the ring and bring it back, with the lost-block list.
fn pull(c: &NodeConn, dir: &Path, lost: &[u64]) -> Result<usize, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let trace = c
        .run("sudo cat /sys/kernel/debug/tracing/trace", Duration::from_secs(300))
        .map_err(|e| e.to_string())?;
    let events = trace.lines().filter(|l| !l.starts_with('#')).count();
    std::fs::write(dir.join("trace.txt"), &trace).map_err(|e| e.to_string())?;

    let list: String = lost.iter().map(|b| format!("{b}\n")).collect();
    std::fs::write(dir.join("lost.txt"), &list).map_err(|e| e.to_string())?;

    // The inode table, so the owning inodes can be read on disk later
    // without the node still being in that state.
    if let Ok(sb) = c.run(
        "sudo dd if=/dev/vdc bs=4096 count=1 2>/dev/null | od -An -tu8 -j16 -N8",
        Duration::from_secs(60),
    ) {
        let _ = std::fs::write(dir.join("inode_table_blk"), sb.trim());
    }
    Ok(events)
}

/// Delete captures older than thirty days.
///
/// Anything worth keeping past that is worth writing up: the point of a
/// trace is the conclusion drawn from it, and a conclusion belongs in
/// Documentation, not in two hundred megabytes of ring buffer. Set
/// XFSTESTS_TRACE_KEEP_DAYS to change it, or move a capture elsewhere
/// to exempt it.
pub fn prune(root: &Path, days: u64) -> (usize, u64) {
    let cutoff = SystemTime::now() - Duration::from_secs(days * 86_400);
    let (mut n, mut bytes) = (0, 0u64);
    let Ok(rd) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_dir() {
            continue;
        }
        let Ok(m) = md.modified() else { continue };
        if m >= cutoff {
            continue;
        }
        if let Ok(inner) = std::fs::read_dir(e.path()) {
            for f in inner.flatten() {
                if let Ok(fm) = f.metadata() {
                    bytes += fm.len();
                }
            }
        }
        if std::fs::remove_dir_all(e.path()).is_ok() {
            n += 1;
        }
    }
    (n, bytes)
}

/// Run the campaign until `hours` elapse or `max` leaks are caught.
pub fn campaign(cfg: &Config, node: &Node, hours: f64, max: u32) -> Result<Vec<Capture>, String> {
    let root = default_root();
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let days: u64 = std::env::var("XFSTESTS_TRACE_KEEP_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let (pruned, freed) = prune(&root, days);
    if pruned > 0 {
        println!(
            "  pruned {pruned} captures older than {days} days ({} MiB)",
            freed / (1024 * 1024)
        );
    }

    let c = NodeConn::new(node, cfg);
    let l = load::Load::from_env();
    println!("  load    : {}", l.describe());
    load::prepare(&c, "/mnt/scratch", 60000).map_err(|e| format!("prepare: {e}"))?;
    let _unused = c.run(
        "sudo sh -c 'pkill -9 xfs_io 2>/dev/null; mkdir -p /mnt/scratch /mnt/test; \
         echo 1 > /sys/kernel/debug/tracing/events/beamfs/enable; \
         for e in writeback_dirty_inode writeback_write_inode \
                  writeback_single_inode_start writeback_queue writeback_exec; do \
           [ -d /sys/kernel/debug/tracing/events/writeback/$e ] && \
             echo 1 > /sys/kernel/debug/tracing/events/writeback/$e/enable; \
         done; echo 60000 > /sys/kernel/debug/tracing/buffer_size_kb'",
        Duration::from_secs(120),
    )
    .map_err(|e| format!("setup: {e}"))?;

    let end = Instant::now() + Duration::from_secs_f64(hours * 3600.0);
    let mut caught: Vec<Capture> = Vec::new();
    let mut loops = 0u32;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    while Instant::now() < end && (caught.len() as u32) < max {
        loops += 1;
        // Progress as it happens. A loop is five seconds of load plus
        // mkfs, mount, unmount and fsck; a program silent for minutes is
        // indistinguishable from a hung one to whoever is watching.
        let t0 = Instant::now();
        // A spinner on its own line, redrawn every 200 ms while the loop
        // runs. A line printed once and then nothing for ten seconds
        // reads as a hung program, whatever the program is actually
        // doing; something moving reads as work in progress. The thread
        // ends when the loop returns.
        let spin = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let spin_stop = spin.clone();
        let n = loops;
        let h = std::thread::spawn(move || {
            let frames = ['|', '/', '-', '\\'];
            let mut i = 0usize;
            while spin_stop.load(std::sync::atomic::Ordering::Relaxed) {
                print!(
                    "\r  loop {n:<4} {} {:>3}s  mkfs, load, unmount, fsck   ",
                    frames[i % 4],
                    t0.elapsed().as_secs()
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
                i += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let lost = match load::run_loop(
            &c, &l, "/dev/vdc", "/mnt/scratch", "-N 16384",
            loops % 12 == 1, Duration::from_secs(300),
        ) {
            Ok(r) => {
                if r.dangling > 0 {
                    println!("\r  loop {loops:<4} {} referenced-but-free (opposite symptom)   ",
                             r.dangling);
                }
                r.lost
            }
            Err(e) => {
                spin.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = h.join();
                println!("\r  loop {loops:<4} failed after {}s: {e}                 ",
                         t0.elapsed().as_secs());
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        spin.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = h.join();
        if lost.is_empty() {
            println!("\r  loop {loops:<4} clean   {}s                              ",
                     t0.elapsed().as_secs());
            continue;
        }
        println!("\r  loop {loops:<4} {} BLOCKS LOST   {}s                         ",
                 lost.len(), t0.elapsed().as_secs());
        let seq = caught.len() as u32 + 1;
        let dir = root.join(format!("{stamp}-{seq:03}"));
        match pull(&c, &dir, &lost) {
            Ok(events) => {
                println!(
                    "  leak {seq}: {} blocks at loop {loops}, {events} events -> {}",
                    lost.len(),
                    dir.display()
                );
                caught.push(Capture {
                    seq,
                    lost: lost.len(),
                    loop_no: loops,
                    events,
                    dir,
                });
            }
            Err(e) => println!("  leak {seq}: {} blocks, pull failed: {e}", lost.len()),
        }
    }

    println!("  {loops} loops, {} leaks captured", caught.len());
    Ok(caught)
}
