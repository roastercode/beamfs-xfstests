// SPDX-License-Identifier: GPL-2.0-only
//! Read a capture and say what happened to the blocks that went missing.
//!
//! A capture is half a gigabyte of trace and a list of block numbers.
//! Answering "why is this block unreferenced" from it means half a dozen
//! greps whose exact form has to be remembered, run once per block, and
//! then correlated by hand. That work is the same every time, so it
//! belongs here rather than in whoever is looking at it.
//!
//! The chain is always the same shape. A block is allocated to an inode;
//! a pointer to it is stored into a parent -- the inode itself for the
//! first twelve, an indirect block beyond that; the inode is dirtied,
//! and at some point written. fsck walks that chain from the medium. So
//! for each lost block the questions are: which inode owned it, which
//! parent holds its pointer, was the parent ever written, and was the
//! inode written after the pointer went in.
//!
//! An answer of "the inode is on disk, the parent is 0xcd" localises the
//! defect to the parent's writeback in one line, and that took three
//! days by hand.

use std::collections::HashMap;
use std::path::Path;

/// One `beamfs_*` line, parsed.
struct Event<'a> {
    kind: &'a str,
    ino: u64,
    fields: &'a str,
}

fn parse<'a>(line: &'a str) -> Option<Event<'a>> {
    let (_, rest) = line.split_once(": beamfs_")?;
    let (kind, fields) = rest.split_once(": ")?;
    let ino = field(fields, "ino=")?;
    Some(Event { kind, ino, fields })
}

/// The numeric value of `key` in a trace line's fields.
fn field(s: &str, key: &str) -> Option<u64> {
    let i = s.find(key)? + key.len();
    let t = &s[i..];
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    t[..end].parse().ok()
}

/// What the trace says about one inode.
#[derive(Default)]
struct Inode {
    dirtied: usize,
    written: usize,
    written_sync: usize,
    stores: usize,
    allocs: usize,
    frees: usize,
    /// Line number of the last write_inode, to order events against it.
    last_write_line: usize,
    /// i_indirect as of the last write_inode.
    indirect: u64,
}

/// What the trace says about one lost block.
struct Lost {
    block: u64,
    owner: Option<u64>,
    parent: Option<u64>,
    slot: Option<u64>,
    level: Option<u64>,
    /// Line where the pointer was stored, if it was.
    store_line: Option<usize>,
    /// Whether a free followed the last allocation.
    freed_after: bool,
}

pub struct Report {
    pub blocks: usize,
    pub events: usize,
    lost: Vec<Lost>,
    inodes: HashMap<u64, Inode>,
    /// Unmount issued WB_SYNC_ALL.
    sync_all_seen: bool,
    /// write_inode calls with sync=1.
    sync_writes: usize,
    /// Stores that overwrote a live pointer.
    overwrites: usize,
}

/// Read `dir` -- trace.txt and lost.txt -- and work out the chain for
/// each lost block.
pub fn analyse(dir: &Path) -> Result<Report, String> {
    let trace = std::fs::read_to_string(dir.join("trace.txt"))
        .map_err(|e| format!("{}: {e}", dir.join("trace.txt").display()))?;
    let lost_txt = std::fs::read_to_string(dir.join("lost.txt"))
        .map_err(|e| format!("{}: {e}", dir.join("lost.txt").display()))?;

    let wanted: Vec<u64> = lost_txt
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();

    let mut inodes: HashMap<u64, Inode> = HashMap::new();
    let mut owner: HashMap<u64, u64> = HashMap::new();
    let mut freed_after: HashMap<u64, bool> = HashMap::new();
    let mut store: HashMap<u64, (u64, u64, u64, usize)> = HashMap::new();
    let mut sync_all_seen = false;
    let mut sync_writes = 0usize;
    let mut overwrites = 0usize;
    let mut events = 0usize;

    for (n, line) in trace.lines().enumerate() {
        if line.contains("writeback_queue") && line.contains("sync_mode=1") {
            sync_all_seen = true;
        }
        let Some(e) = parse(line) else { continue };
        events += 1;
        let i = inodes.entry(e.ino).or_default();
        match e.kind {
            "block_alloc" => {
                i.allocs += 1;
                if let Some(b) = field(e.fields, "blk=") {
                    owner.insert(b, e.ino);
                    freed_after.insert(b, false);
                }
            }
            "block_free" => {
                i.frees += 1;
                if let Some(b) = field(e.fields, "blk=") {
                    freed_after.insert(b, true);
                }
            }
            "slot_store" => {
                i.stores += 1;
                // "ino=150 parent=33190 slot=5 0->26823 lvl=1": the old value is
                // the last token before the arrow, the new one the first after.
                // There is no new= field to read -- writing it as an arrow is what
                // makes an overwrite one line to read instead of two to correlate.
                let (old, new) = match e.fields.split_once("->") {
                    Some((before, after)) => (
                        before.trim_end().rsplit(' ').next()
                            .and_then(|t| t.parse::<u64>().ok()),
                        after.trim_start().split(' ').next()
                            .and_then(|t| t.parse::<u64>().ok()),
                    ),
                    None => (field(e.fields, "old="), field(e.fields, "new=")),
                };
                if old.unwrap_or(0) != 0 {
                    overwrites += 1;
                }
                if let (Some(b), Some(p), Some(s)) = (
                    new,
                    field(e.fields, "parent=").or_else(|| field(e.fields, "ibh=")),
                    field(e.fields, "slot="),
                ) {
                    let lvl = field(e.fields, "lvl=").unwrap_or(0);
                    store.insert(b, (p, s, lvl, n));
                }
            }
            "inode_dirty" => i.dirtied += 1,
            "write_inode" => {
                i.written += 1;
                i.last_write_line = n;
                if let Some(v) = field(e.fields, "i_indirect=") {
                    i.indirect = v;
                }
                if e.fields.contains("sync=1") {
                    i.written_sync += 1;
                    sync_writes += 1;
                }
            }
            _ => {}
        }
    }

    let lost = wanted
        .iter()
        .map(|&b| {
            let st = store.get(&b);
            Lost {
                block: b,
                owner: owner.get(&b).copied(),
                parent: st.map(|s| s.0),
                slot: st.map(|s| s.1),
                level: st.map(|s| s.2),
                store_line: st.map(|s| s.3),
                freed_after: freed_after.get(&b).copied().unwrap_or(false),
            }
        })
        .collect();

    Ok(Report { blocks: wanted.len(), events, lost, inodes, sync_all_seen, sync_writes, overwrites })
}

impl Report {
    /// Print what was found, in the order someone reading it needs.
    pub fn print(&self, dir: &Path) {
        println!("  === {} ===", dir.file_name().unwrap_or_default().to_string_lossy());
        println!("  {} blocks lost, {} beamfs events", self.blocks, self.events);
        println!();

        // The shape of the whole capture first: whether the pointers were
        // ever installed, whether anything overwrote a live one, and
        // whether unmount asked for a synchronous writeback. Each rules
        // out a class of cause before any individual block is looked at.
        let installed = self.lost.iter().filter(|l| l.store_line.is_some()).count();
        let freed = self.lost.iter().filter(|l| l.freed_after).count();
        println!("  pointer installed for {installed} of {} lost blocks", self.blocks);
        if freed > 0 {
            println!("  {freed} were freed after their last allocation -- not a leak, a race");
        }
        println!("  live-pointer overwrites in the whole trace: {}", self.overwrites);
        println!(
            "  unmount issued WB_SYNC_ALL: {}, write_inode with sync=1: {}",
            if self.sync_all_seen { "yes" } else { "no" },
            self.sync_writes
        );
        println!();

        // Then by owning inode, because leaks come in groups: one inode
        // losing thirty blocks is one defect, not thirty.
        let mut by_owner: HashMap<u64, Vec<&Lost>> = HashMap::new();
        for l in &self.lost {
            by_owner.entry(l.owner.unwrap_or(0)).or_default().push(l);
        }
        let mut owners: Vec<_> = by_owner.iter().collect();
        owners.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));

        for (ino, blocks) in owners.iter().take(6) {
            let d = self.inodes.get(ino);
            println!("  inode {ino}: {} lost block(s)", blocks.len());
            if let Some(i) = d {
                println!(
                    "    dirtied {} times, written {} ({} synchronous), {} tree stores",
                    i.dirtied, i.written, i.written_sync, i.stores
                );
                if i.dirtied > 20 && i.written <= 1 {
                    println!(
                        "    dirtied {} times and written {}: the inode on disk is older",
                        i.dirtied, i.written
                    );
                    println!("    than its tree, so pointers stored after that write are lost");
                }
                if i.indirect != 0 {
                    println!("    i_indirect at its last write: {}", i.indirect);
                }
            }
            // Parents matter more than blocks: several blocks under one
            // parent means the parent is what failed to reach the medium.
            let mut by_parent: HashMap<u64, usize> = HashMap::new();
            for b in blocks.iter() {
                if let Some(p) = b.parent {
                    *by_parent.entry(p).or_default() += 1;
                }
            }
            let mut ps: Vec<_> = by_parent.iter().collect();
            ps.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
            let mut nums: Vec<String> =
                blocks.iter().take(8).map(|b| b.block.to_string()).collect();
            if blocks.len() > 8 {
                nums.push(format!("... +{}", blocks.len() - 8));
            }
            println!("    blocks: {}", nums.join(" "));

            for (p, c) in ps.iter().take(4) {
                let lvl = blocks
                    .iter()
                    .find(|b| b.parent == Some(**p))
                    .and_then(|b| b.level)
                    .unwrap_or(0);
                let slots: Vec<String> = blocks
                    .iter()
                    .filter(|b| b.parent == Some(**p))
                    .take(6)
                    .filter_map(|b| b.slot.map(|s| s.to_string()))
                    .collect();
                println!(
                    "    {c} pointer(s) in block {p} (level {lvl}), slots {}",
                    slots.join(" ")
                );
                if d.map(|i| i.indirect) == Some(**p) {
                    println!("      -- named by i_indirect: read it on the device;");
                    println!("         0xcd means it was never written, which is the defect");
                }
            }
            // Whether the store came before or after the inode's last
            // write decides between "the inode is stale" and "the parent
            // never reached disk", and they need different fixes.
            if let Some(i) = d {
                let after = blocks
                    .iter()
                    .filter(|b| b.store_line.map(|l| l > i.last_write_line).unwrap_or(false))
                    .count();
                if after > 0 {
                    println!("    {after} stored AFTER the inode's last write");
                } else if i.written > 0 {
                    println!("    all stored BEFORE the inode's last write:");
                    println!("      the inode went to disk holding them, so the parent is at fault");
                }
            }
            println!();
        }

        println!("  to read an owning inode on the device, with T from inode_table_blk:");
        println!("    blk=$(( T + (I-1)/32 )); off=$(( ((I-1) % 32) * 128 ))");
        println!("    dd if=/dev/vdc bs=4096 skip=$blk count=1 | od -An -tx8 -j$off -N128");
    }
}

/// Analyse every capture under `root`, newest first.
pub fn analyse_all(root: &Path, limit: usize) -> Result<(), String> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .map_err(|e| format!("{}: {e}", root.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("lost.txt").exists())
        .collect();
    dirs.sort();
    dirs.reverse();

    if dirs.is_empty() {
        println!("  no captures under {}", root.display());
        return Ok(());
    }
    for d in dirs.iter().take(limit) {
        match analyse(d) {
            Ok(r) => {
                r.print(d);
                println!();
            }
            Err(e) => println!("  {}: {e}", d.display()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_read_off_a_line() {
        let s = "ino=150 parent=33190 slot=5 lvl=1";
        assert_eq!(field(s, "ino="), Some(150));
        assert_eq!(field(s, "parent="), Some(33190));
        assert_eq!(field(s, "lvl="), Some(1));
        assert_eq!(field(s, "absent="), None);
    }

    #[test]
    fn an_event_line_parses() {
        let l = "  xfs_io-123 [001] .... 6678.089: beamfs_block_alloc: ino=150 blk=26823 lvl=0";
        let e = parse(l).expect("parses");
        assert_eq!(e.kind, "block_alloc");
        assert_eq!(e.ino, 150);
        assert_eq!(field(e.fields, "blk="), Some(26823));
    }

    #[test]
    fn a_line_that_is_not_ours_is_skipped() {
        assert!(parse("  kworker-1 [000] .... 1.0: writeback_queue: sync_mode=1").is_none());
    }
}
