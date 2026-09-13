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
    /// The shape of the last line written, and how many times it has
    /// repeated since. See `line`.
    last_shape: Option<String>,
    repeats: u64,
}

impl Drop for Journal {
    /// A journal ending on its thousandth identical line should say
    /// so, not end on the first one.
    fn drop(&mut self) {
        self.flush_repeats();
    }
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
        let mut j = Self {
            path, file, artifacts,
            last_shape: None,
            repeats: 0,
        };
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
        self.flush_repeats();
        self.last_shape = None;
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "\n=== {} {title} ===", Self::stamp());
            let _ = f.flush();
        }
    }

    /// The shape of a line: its text with every run of digits replaced.
    ///
    /// "region block 1244 subblock 9" and "region block 8104 subblock
    /// 6" are one finding with two addresses. Comparing the text
    /// itself made them two, and 1390 of them reached one journal.
    fn shape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut in_num = false;
        for ch in text.chars() {
            if ch.is_ascii_digit() {
                if !in_num {
                    out.push('#');
                    in_num = true;
                }
            } else {
                out.push(ch);
                in_num = false;
            }
        }
        out
    }

    /// Write a line, or count it when it repeats.
    ///
    /// A run wrote 1390 identical lines out of 3422, and a KCSAN
    /// report sat in the middle of them. The first of a kind is
    /// written in full; the rest are counted, and the count appears as
    /// soon as something else is written:
    ///
    ///     beamfs: region block 1244 encodes and decodes
    ///       ... x1390
    ///
    /// Nothing is lost: the line is there, the number of times is
    /// there, and what came after is readable.
    pub fn line(&mut self, text: &str) {
        let shape = Self::shape(text);

        if self.last_shape.as_deref() == Some(shape.as_str()) {
            self.repeats += 1;
            return;
        }

        self.flush_repeats();
        self.last_shape = Some(shape);

        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{} {text}", Self::stamp());
            let _ = f.flush();
        }
    }

    /// Write the pending repeat count, if any.
    ///
    /// Called before anything that is not a repeat, and at close: a
    /// journal ending on its thousandth identical line should say so
    /// rather than end on the first.
    fn flush_repeats(&mut self) {
        if self.repeats == 0 {
            return;
        }
        let n = self.repeats;
        self.repeats = 0;
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "  ... x{}", n + 1);
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

#[cfg(test)]
mod shape_tests {
    use super::*;

    #[test]
    fn numbers_do_not_make_two_findings() {
        assert_eq!(
            Journal::shape("region block 1244 subblock 9"),
            Journal::shape("region block 8104 subblock 6"));
    }

    #[test]
    fn different_sentences_stay_different() {
        assert_ne!(
            Journal::shape("region block 1244 encodes and decodes"),
            Journal::shape("region block 1244 beyond correction"));
    }

    #[test]
    fn a_line_with_no_numbers_is_itself() {
        assert_eq!(Journal::shape("no lost pointer seen"),
                   "no lost pointer seen");
    }

    #[test]
    fn a_run_of_digits_collapses_to_one_mark() {
        assert_eq!(Journal::shape("block 123456"), "block #");
    }

    #[test]
    fn repeats_are_counted_and_the_line_survives() {
        let d = std::env::temp_dir().join(format!("bxj-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        {
            let mut j = Journal::create(&d);
            j.line("region block 1 encodes and decodes");
            for k in 2..=1390 {
                j.line(&format!("region block {k} encodes and decodes"));
            }
            j.line("something else entirely");
        }
        let log = std::fs::read_dir(&d).unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "log"))
            .expect("a log");
        let text = std::fs::read_to_string(log).unwrap();

        // The finding is there once, its count is there, and what came
        // after it is readable.
        assert_eq!(text.matches("encodes and decodes").count(), 1);
        assert!(text.contains("... x1390"), "{text}");
        assert!(text.contains("something else entirely"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
