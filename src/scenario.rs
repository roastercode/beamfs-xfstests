// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! One question, asked in ten seconds.
//!
//! generic/013 runs fsstress for minutes and answers "30 blocks
//! leaked": a number produced by thousands of operations, any of which
//! might be the one. A scenario writes a known amount, keeps a known
//! amount, and says whether the volume owns what it should.

use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// What the volume says about itself.
struct Counted {
    free: u64,
    leaked: u64,
}

/// Ask the checker, on an unmounted volume.
fn count(c: &NodeConn, dev: &str) -> Result<Counted, String> {
    let out = c
        .run(&format!("sudo umount /mnt/scratch 2>/dev/null; \
                       sudo fsck.beamfs -v /dev/{dev} 2>&1 | \
                       grep -E 'used-but-unreferenced|free:' | head -4"),
             Duration::from_secs(120))
        .map_err(|e| format!("fsck: {e:?}"))?;

    let leaked = out
        .split("referenced-but-free, ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    Ok(Counted { free: 0, leaked })
}

/// Write `blocks` blocks, then truncate to `keep`, and count.
///
/// The partial write is what iomap_end exists for: the mapping covers
/// the whole write and the write stops short, leaving the rest
/// allocated and named by nothing.
pub fn partial_write(cfg: &Config, node: &Node, blocks: u64, keep: u64)
    -> Result<(), String>
{
    let c = NodeConn::new(node, cfg);
    let dev = node.scratch_dev.trim_start_matches("/dev/");

    println!("  scenario: write {blocks} blocks, keep {keep}");

    c.run(&format!("sudo sh -c 'umount /mnt/scratch 2>/dev/null; \
                    mkfs.beamfs -N 16384 /dev/{dev} >/dev/null && \
                    mkdir -p /mnt/scratch && \
                    mount -t beamfs /dev/{dev} /mnt/scratch'"),
          Duration::from_secs(120))
        .map_err(|e| format!("cannot prepare: {e:?}"))?;

    // One file, one write, one truncate: nothing else touches the
    // volume, so every block it owns afterwards is accounted for.
    let bytes = blocks * 3824;      // INLINE payload per block
    let kept = keep * 3824;
    c.run(&format!("sudo sh -c 'dd if=/dev/urandom of=/mnt/scratch/f \
                    bs=3824 count={blocks} 2>/dev/null; \
                    truncate -s {kept} /mnt/scratch/f; sync'"),
          Duration::from_secs(120))
        .map_err(|e| format!("cannot write: {e:?}"))?;
    let _ = bytes;

    let after = count(&c, dev)?;

    println!("  {} block(s) marked used that nothing references",
             after.leaked);
    if after.leaked == 0 {
        println!("  nothing was left behind");
    } else {
        println!("  the {} blocks between {keep} and {blocks} are still taken",
                 after.leaked);
    }
    let _ = after.free;
    Ok(())
}
