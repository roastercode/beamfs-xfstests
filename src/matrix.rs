// SPDX-License-Identifier: GPL-2.0-only
//! Vary one condition at a time and see which ones the leak needs.
//!
//! generic/464 fails about three runs in ten while the same load driven
//! directly ran fifty-one loops clean. Something in the test's
//! surroundings, not its load, is a condition of the defect -- and
//! finding which meant a throwaway shell script per guess, each answering
//! one question and leaving nothing behind. The answers were worth
//! keeping and were not kept.
//!
//! So the conditions are named, each run is one cell of a matrix, and
//! the results go in the history with everything else. What has already
//! been established this way:
//!
//!   scratch alone                 51 loops, no leak
//!   scratch + a second beamfs     leaked at loop 5
//!   ... with scrub suspended      leaked at loop 10 -- scrub is not it
//!   ... second volume as ext2     leaked at loop 2 -- not beamfs-specific
//!
//! Which says the condition is a second filesystem being active at all:
//! memory pressure, inode eviction, a shared flusher. Not something in
//! beamfs that two beamfs mounts contend over.

use std::time::Duration;

use crate::config::{Config, Node};
use crate::load::{self, Load};
use crate::node::NodeConn;

/// One thing that can be varied around the load.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Nothing but the scratch volume.
    ScratchOnly,
    /// A second beamfs filesystem mounted alongside, as xfstests does.
    SecondBeamfs,
    /// A second filesystem of another type, to tell "a second mount"
    /// from "a second beamfs mount".
    SecondExt2,
    /// A second beamfs mount with its scrub suspended, to take the
    /// background reader out of the picture.
    SecondBeamfsNoScrub,
    /// Memory pressure without a second mount, to separate the effect of
    /// having two superblocks from the effect of the cache filling up.
    MemoryPressure,
}

impl Condition {
    pub fn name(self) -> &'static str {
        match self {
            Condition::ScratchOnly => "scratch-only",
            Condition::SecondBeamfs => "second-beamfs",
            Condition::SecondExt2 => "second-ext2",
            Condition::SecondBeamfsNoScrub => "second-beamfs-no-scrub",
            Condition::MemoryPressure => "memory-pressure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "scratch-only" => Condition::ScratchOnly,
            "second-beamfs" => Condition::SecondBeamfs,
            "second-ext2" => Condition::SecondExt2,
            "second-beamfs-no-scrub" => Condition::SecondBeamfsNoScrub,
            "memory-pressure" => Condition::MemoryPressure,
            _ => return None,
        })
    }

    pub fn all() -> [Condition; 5] {
        [
            Condition::ScratchOnly,
            Condition::SecondBeamfs,
            Condition::SecondExt2,
            Condition::SecondBeamfsNoScrub,
            Condition::MemoryPressure,
        ]
    }

    /// What to run on the node before the loops start.
    ///
    /// Each returns the environment to the state the condition names,
    /// rather than assuming what the previous condition left behind: a
    /// cell that inherits a mount from the cell before it measures the
    /// wrong thing, and there is no way to tell from its result.
    fn setup(self) -> String {
        let common = "pkill -9 xfs_io 2>/dev/null\n\
                      umount -l /mnt/scratch /mnt/test 2>/dev/null\n\
                      sleep 1\n\
                      mkdir -p /mnt/scratch /mnt/test\n";
        match self {
            Condition::ScratchOnly => common.to_string(),
            Condition::SecondBeamfs => format!(
                "{common}\
                 mkfs.beamfs -N 16384 /dev/vdb >/dev/null 2>&1\n\
                 mount -t beamfs /dev/vdb /mnt/test || echo 'SETUP: second mount failed' >&2\n"
            ),
            Condition::SecondBeamfsNoScrub => format!(
                "{common}\
                 mkfs.beamfs -N 16384 /dev/vdb >/dev/null 2>&1\n\
                 mount -t beamfs /dev/vdb /mnt/test || echo 'SETUP: second mount failed' >&2\n\
                 # There is no enabled attribute; interval is what is\n\
                 # writable, and a day between passes is a stopped sweep\n\
                 # for a run measured in minutes. The cursor is reported\n\
                 # so a cell that failed to suspend it can be told apart\n\
                 # from one that did.\n\
                 for d in /sys/fs/beamfs/*/; do echo 86400000 > $d/interval 2>/dev/null; done\n\
                 printf 'SETUP: cursors '; for d in /sys/fs/beamfs/*/; do printf '%s ' $(cat $d/cursor 2>/dev/null); done; echo\n"
            ),
            Condition::SecondExt2 => format!(
                "{common}\
                 mkfs.ext2 -q -F /dev/vdb 2>/dev/null\n\
                 mount -t ext2 /dev/vdb /mnt/test || echo 'SETUP: ext2 mount failed' >&2\n"
            ),
            Condition::MemoryPressure => format!(
                "{common}\
                 # Fill the page cache without a second superblock, so the\n\
                 # cache filling up can be told apart from there being two\n\
                 # filesystems to flush.\n\
                 ( dd if=/dev/zero of=/dev/null bs=1M count=2000 2>/dev/null ) &\n\
                 echo 3 > /proc/sys/vm/drop_caches 2>/dev/null\n"
            ),
        }
    }

    /// Whether the node ended up in the state the condition names.
    ///
    /// A cell whose setup silently failed reports a leak or its absence
    /// under conditions nobody chose, and looks exactly like a cell that
    /// worked.
    fn check(self) -> String {
        match self {
            Condition::ScratchOnly => {
                "printf 'CHECK: mounts=%s\\n' \"$(mount | grep -c /mnt/test)\"".into()
            }
            Condition::SecondBeamfs | Condition::SecondBeamfsNoScrub => {
                "printf 'CHECK: beamfs=%s test=%s\\n' \
                 \"$(mount | grep -c 'type beamfs')\" \
                 \"$(mount | grep -c ' /mnt/test ')\""
                    .into()
            }
            Condition::SecondExt2 => {
                "printf 'CHECK: ext2=%s beamfs=%s\\n' \
                 \"$(mount | grep ' /mnt/test ' | grep -c ext2)\" \
                 \"$(mount | grep -c 'type beamfs')\""
                    .into()
            }
            Condition::MemoryPressure => {
                "printf 'CHECK: free=%sMB test=%s\\n' \
                 \"$(free -m | awk '/Mem:/{print $7}')\" \
                 \"$(mount | grep -c ' /mnt/test ')\""
                    .into()
            }
        }
    }
}

/// What one cell of the matrix produced.
pub struct Cell {
    pub condition: Condition,
    /// Loop at which the first leak appeared, if any.
    pub leak_at: Option<u32>,
    /// Blocks lost at that loop.
    pub lost: usize,
    /// Loops run.
    pub loops: u32,
    /// What the setup check reported, for a cell that did not do what
    /// it says.
    pub state: String,
}

impl Cell {
    pub fn verdict(&self) -> String {
        match self.leak_at {
            Some(l) => format!("leaked at loop {l} ({} blocks)", self.lost),
            None => format!("{} loops clean", self.loops),
        }
    }
}

/// Run one condition until it leaks or `max_loops` pass.
pub fn run_cell(
    cfg: &Config,
    node: &Node,
    cond: Condition,
    l: &Load,
    max_loops: u32,
) -> Result<Cell, String> {
    let c = NodeConn::new(node, cfg);

    let out = c
        .run(&format!("sudo sh -c {}", quote(&format!("{}\n{}", cond.setup(), cond.check()))),
             Duration::from_secs(180))
        .map_err(|e| format!("setup: {e}"))?;
    let state: String = out
        .lines()
        .filter(|l| l.starts_with("SETUP:") || l.starts_with("CHECK:"))
        .collect::<Vec<_>>()
        .join("; ");

    let mut cell = Cell { condition: cond, leak_at: None, lost: 0, loops: 0, state };

    for n in 1..=max_loops {
        cell.loops = n;
        print!("\r    {:<24} loop {n:<3} ", cond.name());
        let _ = std::io::Write::flush(&mut std::io::stdout());

        let r = load::run_loop(
            &c,
            l,
            "/dev/vdc",
            "/mnt/scratch",
            "-N 16384",
            n == 1,
            Duration::from_secs(300),
        )
        .map_err(|e| format!("loop {n}: {e}"))?;

        if !r.lost.is_empty() {
            cell.leak_at = Some(n);
            cell.lost = r.lost.len();
            break;
        }
    }
    println!("\r    {:<24} {}                    ", cond.name(), cell.verdict());
    Ok(cell)
}

/// Run every condition, or the ones named, and print the matrix.
pub fn run(
    cfg: &Config,
    node: &Node,
    which: Option<&str>,
    max_loops: u32,
) -> Result<Vec<Cell>, String> {
    // An empty argument means every condition, not a condition named
    // "": `matrix "" 8` is how a shell passes "all, eight loops" and it
    // must not be read as a name nobody could have meant.
    let conds: Vec<Condition> = match which.filter(|s| !s.trim().is_empty()) {
        Some(s) => s
            .split(',')
            .filter_map(Condition::parse)
            .collect(),
        None => Condition::all().to_vec(),
    };
    if conds.is_empty() {
        return Err("no known condition named; see the manual".into());
    }

    let l = Load::from_env();
    println!("  load    : {}", l.describe());
    println!("  budget  : up to {max_loops} loops per condition");
    println!();

    let mut cells = Vec::new();
    for cond in conds {
        // The load module mounts the test volume itself when it finds it
        // absent, which is right for `trace` and wrong here: a cell that
        // says scratch-only must have nothing else mounted. The setup
        // above unmounts it; run_loop must not put it back.
        std::env::set_var(
            "XFSTESTS_LOAD_NO_TEST_MOUNT",
            if cond == Condition::SecondBeamfs || cond == Condition::SecondBeamfsNoScrub {
                "0"
            } else {
                "1"
            },
        );
        match run_cell(cfg, node, cond, &l, max_loops) {
            Ok(c) => cells.push(c),
            Err(e) => println!("    {:<24} {e}", cond.name()),
        }
    }

    println!();
    println!("  === matrix ===");
    for c in &cells {
        println!("    {:<24} {:<32} [{}]", c.condition.name(), c.verdict(), c.state);
    }
    println!();

    // The point is the comparison, so say what it shows rather than
    // leaving it to be read off the rows.
    let clean: Vec<&Cell> = cells.iter().filter(|c| c.leak_at.is_none()).collect();
    let leaked: Vec<&Cell> = cells.iter().filter(|c| c.leak_at.is_some()).collect();
    if !clean.is_empty() && !leaked.is_empty() {
        println!("  clean under: {}", names(&clean));
        println!("  leaked under: {}", names(&leaked));
        println!("  -- the defect needs what separates the second list from the first");
    } else if leaked.len() == cells.len() {
        println!("  every condition leaked: none of them is the trigger");
    } else {
        println!("  no condition leaked: the load alone does not reproduce it");
    }
    Ok(cells)
}

fn names(cells: &[&Cell]) -> String {
    cells.iter().map(|c| c.condition.name()).collect::<Vec<_>>().join(", ")
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_argument_is_not_a_condition_name() {
        assert!(Condition::parse("").is_none());
        assert!(Condition::parse("  ").is_none());
    }

    #[test]
    fn every_condition_round_trips_through_its_name() {
        for c in Condition::all() {
            assert_eq!(Condition::parse(c.name()).map(|p| p.name()), Some(c.name()));
        }
    }

    #[test]
    fn a_second_mount_condition_mounts_something() {
        assert!(Condition::SecondBeamfs.setup().contains("mount -t beamfs /dev/vdb"));
        assert!(Condition::SecondExt2.setup().contains("mount -t ext2 /dev/vdb"));
        assert!(!Condition::ScratchOnly.setup().contains("mount -t"));
    }

    #[test]
    fn every_condition_unmounts_what_the_last_one_left() {
        for c in Condition::all() {
            assert!(c.setup().contains("umount -l /mnt/scratch /mnt/test"));
        }
    }

    #[test]
    fn the_no_scrub_condition_reports_the_cursor() {
        let s = Condition::SecondBeamfsNoScrub.setup();
        assert!(s.contains("interval"));
        assert!(s.contains("cursors"));
    }
}
