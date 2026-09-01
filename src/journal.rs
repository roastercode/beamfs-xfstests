// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Everything a run produced, kept.
//!
//! The failure mode this exists to prevent: a thirty-minute run that
//! leaves an eight-line log. That happened on 2026-09-01, repeatedly.
//! Each step's output was piped through `tail -3` for the terminal and
//! the same three lines were all that reached the file, so a run that
//! ended in a hang left no record of what the node had been doing, no
//! stack, and no way to tell an unmountable device from a wedged one.
//!
//! Here the terminal gets a summary and the journal gets everything:
//! full stdout and stderr of every remote command, the stacks of every
//! blocked task at the moment a stall is declared, dmesg around the
//! failure, and the harness's own output for each failing test.
//!
//! Written append-only and flushed per entry, so a run killed at any
//! point keeps what it had.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Journal {
    path: PathBuf,
    file: Option<File>,
    /// Directory holding one file per failing test.
    artifacts: PathBuf,
}

impl Journal {
    /// Open a journal under `dir`, named for the moment it started.
    ///
    /// A new file per run rather than one appended forever: comparing
    /// two campaigns means reading two files, and a single growing log
    /// makes that a search problem.
    pub fn create(dir: &Path) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("xfstests-{stamp}.log"));
        let artifacts = dir.join(format!("xfstests-{stamp}.d"));
        let _ = std::fs::create_dir_all(&artifacts);
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        let mut j = Self { path, file, artifacts };
        j.section("RUN STARTED");
        j
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn artifacts(&self) -> &Path {
        &self.artifacts
    }

    fn stamp() -> String {
        // Seconds since boot would be nicer for correlating with dmesg,
        // but this runs on the orchestrator and dmesg is on the nodes;
        // wall clock is the only shared reference.
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let s = d.as_secs();
        format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
    }

    pub fn section(&mut self, title: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "\n=== {} {title} ===", Self::stamp());
            let _ = f.flush();
        }
    }

    pub fn line(&mut self, text: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{} {text}", Self::stamp());
            let _ = f.flush();
        }
    }

    /// Record a remote command and everything it produced.
    ///
    /// Untruncated on purpose. The output of a failing mount is four
    /// lines and the fourth is the one that matters.
    pub fn command(&mut self, node: &str, cmd: &str, output: &str, ok: bool) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "\n--- {} {node} {} ---",
                             Self::stamp(), if ok { "ok" } else { "FAILED" });
            let _ = writeln!(f, "$ {cmd}");
            for l in output.lines() {
                let _ = writeln!(f, "| {l}");
            }
            let _ = f.flush();
        }
    }

    /// Save the harness output for one failing test.
    ///
    /// One file per test rather than inline: a failure log runs to
    /// hundreds of lines of diff, and burying the next failure under it
    /// is how a second defect goes unnoticed.
    pub fn artifact(&mut self, test: &str, content: &str) {
        let name = test.replace('/', "-");
        let p = self.artifacts.join(format!("{name}.log"));
        if std::fs::write(&p, content).is_ok() {
            self.line(&format!("artifact {test} -> {}", p.display()));
        }
    }

    /// What a wedged node was doing when it stopped.
    ///
    /// The stacks are the evidence. Without them a hang is a word in a
    /// results file and the next run starts from nothing.
    pub fn stall_evidence(&mut self, node: &str, test: &str,
                          stacks: &str, dmesg: &str, mounts: &str) {
        self.section(&format!("STALL on {node} during {test}"));
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "blocked task stacks:");
            for l in stacks.lines() {
                let _ = writeln!(f, "  {l}");
            }
            let _ = writeln!(f, "\nkernel messages:");
            for l in dmesg.lines() {
                let _ = writeln!(f, "  {l}");
            }
            let _ = writeln!(f, "\nmounts:");
            for l in mounts.lines() {
                let _ = writeln!(f, "  {l}");
            }
            let _ = f.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journal_keeps_full_output() {
        let dir = std::env::temp_dir().join(format!("bxj{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut j = Journal::create(&dir);
        let long: String = (0..50).map(|i| format!("line {i}\n")).collect();
        j.command("master", "mount", &long, false);
        let got = std::fs::read_to_string(j.path()).unwrap_or_default();
        // Every line, not the last three.
        assert!(got.contains("line 0"));
        assert!(got.contains("line 49"));
        assert!(got.contains("FAILED"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artifacts_land_in_their_own_files() {
        let dir = std::env::temp_dir().join(format!("bxa{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut j = Journal::create(&dir);
        j.artifact("generic/013", "diff output here");
        let p = j.artifacts().join("generic-013.log");
        assert_eq!(std::fs::read_to_string(p).unwrap_or_default(), "diff output here");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
