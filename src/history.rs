// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Results that survive the node they were produced on.
//!
//! They used to live in /tmp on each node. The image is redeployed after
//! every kernel change -- several times a day -- and each deploy wiped
//! them. So the question that matters after a fix, "did this test pass
//! before I touched it", had no answer, and a regression could only be
//! noticed by someone who remembered.
//!
//! Results are now copied to the orchestrator as they appear and kept
//! per run. A run can be compared against any earlier one, which is the
//! only way a test suite earns its cost: the absolute pass count says
//! little, the delta says everything.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::result::{Outcome, TestResult};

pub struct History {
    root: PathBuf,
}

/// What changed between two runs.
#[derive(Debug, Default)]
pub struct Delta {
    /// Passed before, does not now. The only category that stops a release.
    pub regressed: Vec<(String, Outcome)>,
    /// Failed before, passes now.
    pub fixed: Vec<String>,
    /// Not present in the baseline at all.
    pub new: Vec<(String, Outcome)>,
    /// In the baseline, absent here: the run did not get that far.
    pub missing: Vec<String>,
    pub unchanged: usize,
}

impl Delta {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.regressed.is_empty()
    }
}

impl History {
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let _ = fs::create_dir_all(root);
        Self { root: root.to_path_buf() }
    }

    /// Default location: alongside the source tree rather than in /tmp.
    ///
    /// /tmp is cleared on reboot and the orchestrator is a workstation
    /// that gets rebooted. History that does not outlive a reboot is not
    /// history.
    #[must_use]
    pub fn default_root() -> PathBuf {
        std::env::var("XFSTESTS_HISTORY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".local/share/beamfs-xfstests")
            })
    }

    /// Write a run's results under `tag`.
    pub fn save(&self, tag: &str, results: &[TestResult]) -> std::io::Result<PathBuf> {
        let p = self.root.join(format!("{tag}.results"));
        let mut body = String::with_capacity(results.len() * 48);
        for r in results {
            body.push_str(&format!(
                "{} {} {} {} {}\n",
                r.name, r.outcome.as_str(), r.seconds, r.node, r.reason
            ));
        }
        fs::write(&p, body)?;
        Ok(p)
    }

    /// Read a saved run.
    pub fn load(&self, tag: &str) -> Option<Vec<TestResult>> {
        let p = self.root.join(format!("{tag}.results"));
        let body = fs::read_to_string(p).ok()?;
        Some(
            body.lines()
                .filter_map(|l| {
                    // node is the fourth field; parse() takes it
                    // separately, so pass a placeholder and let the
                    // stored one win.
                    let mut it = l.split_whitespace();
                    let name = it.next()?;
                    let outcome = it.next()?;
                    let secs = it.next()?;
                    let node = it.next().unwrap_or("?");
                    let reason: Vec<&str> = it.collect();
                    TestResult::parse(
                        &format!("{name} {outcome} {secs} {}", reason.join(" ")),
                        node,
                    )
                })
                .collect(),
        )
    }

    /// Every saved run, newest first.
    #[must_use]
    pub fn runs(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.root)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter_map(|e| {
                        e.file_name().to_str().and_then(|n| {
                            n.strip_suffix(".results").map(ToString::to_string)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by(|a, b| b.cmp(a));
        v
    }

    /// The most recent run before `tag`, if any.
    #[must_use]
    pub fn baseline_for(&self, tag: &str) -> Option<String> {
        self.runs().into_iter().find(|r| r.as_str() < tag)
    }

    /// Compare two sets of results.
    ///
    /// A test that went from PASS to anything else is a regression and
    /// nothing else is. NOTRUN moving to PASS is a feature landing, not
    /// a fix; PASS moving to NOTRUN means a feature was lost, which
    /// counts as a regression because it is.
    #[must_use]
    pub fn compare(baseline: &[TestResult], current: &[TestResult]) -> Delta {
        let base: BTreeMap<&str, Outcome> =
            baseline.iter().map(|r| (r.name.as_str(), r.outcome)).collect();
        let cur: BTreeMap<&str, Outcome> =
            current.iter().map(|r| (r.name.as_str(), r.outcome)).collect();

        let mut d = Delta::default();
        for (name, &now) in &cur {
            match base.get(name) {
                None => d.new.push(((*name).to_string(), now)),
                Some(&was) if was == now => d.unchanged += 1,
                Some(&was) => {
                    if was == Outcome::Pass {
                        d.regressed.push(((*name).to_string(), now));
                    } else if now == Outcome::Pass {
                        d.fixed.push((*name).to_string());
                    } else {
                        // Both non-pass, different kinds: a FAIL that
                        // became a HANG is worth seeing but is not a
                        // regression in the sense that blocks anything.
                        d.new.push(((*name).to_string(), now));
                    }
                }
            }
        }
        for name in base.keys() {
            if !cur.contains_key(name) {
                d.missing.push((*name).to_string());
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, o: Outcome) -> TestResult {
        TestResult {
            name: name.into(), outcome: o, seconds: 1,
            node: "n".into(), reason: String::new(),
        }
    }

    #[test]
    fn pass_to_fail_is_a_regression() {
        let d = History::compare(
            &[r("generic/001", Outcome::Pass)],
            &[r("generic/001", Outcome::Fail)],
        );
        assert_eq!(d.regressed.len(), 1);
        assert!(!d.is_clean());
    }

    #[test]
    fn pass_to_notrun_is_also_a_regression() {
        // A feature that stopped being implemented is a regression, even
        // though the suite reports it as "not run" rather than failure.
        let d = History::compare(
            &[r("generic/020", Outcome::Pass)],
            &[r("generic/020", Outcome::NotRun)],
        );
        assert_eq!(d.regressed.len(), 1);
    }

    #[test]
    fn fail_to_pass_is_a_fix() {
        let d = History::compare(
            &[r("generic/069", Outcome::Fail)],
            &[r("generic/069", Outcome::Pass)],
        );
        assert_eq!(d.fixed, vec!["generic/069"]);
        assert!(d.is_clean());
    }

    #[test]
    fn a_run_that_stopped_early_shows_missing() {
        let d = History::compare(
            &[r("generic/001", Outcome::Pass), r("generic/002", Outcome::Pass)],
            &[r("generic/001", Outcome::Pass)],
        );
        assert_eq!(d.missing, vec!["generic/002"]);
        // Missing is not regression: the test did not fail, it did not run.
        assert!(d.is_clean());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("bxh{}", std::process::id()));
        let h = History::new(&dir);
        let rs = vec![
            r("generic/001", Outcome::Pass),
            TestResult {
                name: "generic/130".into(), outcome: Outcome::NotRun, seconds: 3,
                node: "c1".into(), reason: "O_DIRECT is not supported".into(),
            },
        ];
        h.save("20260901-1700", &rs).unwrap();
        let back = h.load("20260901-1700").unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].reason, "O_DIRECT is not supported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn baseline_is_the_previous_run() {
        let dir = std::env::temp_dir().join(format!("bxb{}", std::process::id()));
        let h = History::new(&dir);
        h.save("20260901-1000", &[r("a", Outcome::Pass)]).unwrap();
        h.save("20260901-1200", &[r("a", Outcome::Pass)]).unwrap();
        assert_eq!(h.baseline_for("20260901-1400").as_deref(), Some("20260901-1200"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
