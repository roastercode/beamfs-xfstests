// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! One file per test, written the moment its verdict arrives.
//!
//! A campaign is seven to ten hours and everything it establishes used
//! to exist only in memory until the end. A power cut at hour six threw
//! away six hours of verdicts that were never in doubt -- the tests had
//! passed, the run had simply not finished.
//!
//! So each verdict is archived as it lands: the test, its outcome, how
//! long it took, which node ran it, and the commit it was obtained on.
//! Nothing waits for the run to end, and an interrupted campaign leaves
//! behind exactly what it had proved.
//!
//! The commit matters as much as the verdict. A result without the
//! revision that produced it says nothing a reviewer can check, and
//! results from two revisions in one table are worse than no table.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::result::TestResult;

pub struct Archive {
    root: PathBuf,
    commit: String,
    seen: BTreeSet<String>,
}

impl Archive {
    /// Where a run's per-test records live: one directory per commit, so
    /// two revisions never share a shelf.
    pub fn open(base: &Path, repo: &Path) -> std::io::Result<Self> {
        let commit = Self::describe(repo);
        let root = base.join(&commit);
        fs::create_dir_all(&root)?;

        // Anything already archived for this commit is a verdict that
        // stands: a resumed campaign does not need to prove it twice.
        let mut seen = BTreeSet::new();
        if let Ok(rd) = fs::read_dir(&root) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    if let Some(stem) = n.strip_suffix(".txt") {
                        seen.insert(stem.replace('-', "/"));
                    }
                }
            }
        }

        Ok(Self { root, commit, seen })
    }

    /// `git describe`-style identity of the tree under test.
    ///
    /// The short hash, plus `-dirty` when the working tree has changes
    /// that are not committed -- because a result obtained on an
    /// uncommitted tree cannot be reproduced by anyone else, and saying
    /// so is more useful than a hash that lies.
    fn describe(repo: &Path) -> String {
        let sha = Command::new("git")
            .args(["--no-pager", "rev-parse", "--short", "HEAD"])
            .current_dir(repo)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let dirty = Command::new("git")
            .args(["--no-pager", "status", "--porcelain"])
            .current_dir(repo)
            .output()
            .ok()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);

        if dirty {
            format!("{sha}-dirty")
        } else {
            sha
        }
    }

    pub fn commit(&self) -> &str {
        &self.commit
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Verdicts already on the shelf for this commit.
    pub fn known(&self) -> usize {
        self.seen.len()
    }

    /// Record a verdict, once. Returns true if this one was new.
    ///
    /// Failures get their log alongside; a passing test needs nothing
    /// beyond the fact that it passed.
    pub fn record(&mut self, r: &TestResult, log: Option<&str>) -> bool {
        if self.seen.contains(&r.name) {
            return false;
        }
        self.seen.insert(r.name.clone());

        let path = self.root.join(format!("{}.txt", r.name.replace('/', "-")));
        let mut body = format!(
            "test: {}\noutcome: {}\nseconds: {}\nnode: {}\ncommit: {}\n",
            r.name,
            r.outcome.as_str(),
            r.seconds,
            r.node,
            self.commit
        );
        if !r.reason.is_empty() {
            body.push_str(&format!("reason: {}\n", r.reason));
        }
        if let Some(l) = log {
            body.push_str("\n--- log ---\n");
            body.push_str(l);
        }
        let _ = fs::write(&path, body);
        true
    }

    /// What the shelf holds, counted by outcome.
    pub fn tally(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                if let Ok(body) = fs::read_to_string(e.path()) {
                    for line in body.lines() {
                        if let Some(v) = line.strip_prefix("outcome: ") {
                            *counts.entry(v.to_string()).or_insert(0) += 1;
                            break;
                        }
                    }
                }
            }
        }
        counts.into_iter().collect()
    }
}
