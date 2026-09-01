// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! What happened to one test, and how a run is summarised.

use std::fmt;

/// The five outcomes a test can have here.
///
/// xfstests itself knows pass, fail and not-run. The other two are ours:
/// a test that had to be killed, and a device that would not mount
/// before the test began. Both were previously indistinguishable from
/// failure, which made a wedged filesystem look like a broken test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    /// The test declined: a feature this filesystem does not implement.
    ///
    /// Not a defect. Belongs in the scope section of a paper, not in a
    /// bug list -- no filesystem passes the whole suite, and the reasons
    /// here are design decisions.
    NotRun,
    /// Exceeded its timeout and was killed.
    Hang,
    /// The device could not be mounted; the test never started.
    MountFail,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotRun => "NOTRUN",
            Self::Hang => "HANG",
            Self::MountFail => "MOUNTFAIL",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PASS" => Some(Self::Pass),
            "FAIL" => Some(Self::Fail),
            "NOTRUN" => Some(Self::NotRun),
            "HANG" => Some(Self::Hang),
            "MOUNTFAIL" => Some(Self::MountFail),
            _ => None,
        }
    }

    /// Whether this outcome needs a human to look at it.
    ///
    /// NotRun does not: it is a statement about the filesystem's scope.
    #[must_use]
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Fail | Self::Hang | Self::MountFail)
    }
}

#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub outcome: Outcome,
    pub seconds: u64,
    pub node: String,
    /// For NotRun, the reason the suite gave. Empty otherwise.
    pub reason: String,
}

impl fmt::Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}s {}", self.name, self.outcome.as_str(), self.seconds, self.node)?;
        if !self.reason.is_empty() {
            write!(f, " {}", self.reason)?;
        }
        Ok(())
    }
}

impl TestResult {
    /// Parse one line of a node's result file.
    ///
    /// Format is deliberately flat: name, outcome, seconds, then an
    /// optional reason that may contain spaces. A run interrupted
    /// mid-write leaves a partial last line, which parses to None and is
    /// dropped rather than corrupting the resume.
    #[must_use]
    pub fn parse(line: &str, node: &str) -> Option<Self> {
        let mut p = line.split_whitespace();
        let name = p.next()?.to_string();
        let outcome = Outcome::parse(p.next()?)?;
        let seconds = p.next()?.trim_end_matches('s').parse().ok()?;
        let reason = p.collect::<Vec<_>>().join(" ");
        Some(Self { name, outcome, seconds, node: node.into(), reason })
    }
}

/// Counts across a whole run.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub pass: usize,
    pub fail: usize,
    pub notrun: usize,
    pub hang: usize,
    pub mountfail: usize,
    pub total_seconds: u64,
}

impl Summary {
    pub fn add(&mut self, r: &TestResult) {
        match r.outcome {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail => self.fail += 1,
            Outcome::NotRun => self.notrun += 1,
            Outcome::Hang => self.hang += 1,
            Outcome::MountFail => self.mountfail += 1,
        }
        self.total_seconds += r.seconds;
    }

    #[must_use]
    pub fn attempted(&self) -> usize {
        self.pass + self.fail + self.notrun + self.hang + self.mountfail
    }

    /// Of the tests that actually ran, how many passed.
    ///
    /// Excludes NotRun: including them would make a filesystem look
    /// better the fewer features it has, which is the wrong direction
    /// for a number anyone might quote.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let ran = self.pass + self.fail + self.hang;
        if ran == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                self.pass as f64 * 100.0 / ran as f64
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_round_trip() {
        for o in [Outcome::Pass, Outcome::Fail, Outcome::NotRun,
                  Outcome::Hang, Outcome::MountFail] {
            assert_eq!(Outcome::parse(o.as_str()), Some(o));
        }
    }

    #[test]
    fn notrun_is_not_actionable() {
        assert!(!Outcome::NotRun.is_actionable());
        assert!(Outcome::Hang.is_actionable());
        assert!(Outcome::MountFail.is_actionable());
    }

    #[test]
    fn a_line_parses() {
        let r = TestResult::parse("generic/013 PASS 164s", "master").unwrap();
        assert_eq!(r.name, "generic/013");
        assert_eq!(r.outcome, Outcome::Pass);
        assert_eq!(r.seconds, 164);
    }

    #[test]
    fn a_reason_survives_spaces() {
        let r = TestResult::parse(
            "generic/130 NOTRUN 2s O_DIRECT is not supported", "c1").unwrap();
        assert_eq!(r.reason, "O_DIRECT is not supported");
    }

    #[test]
    fn a_truncated_line_is_dropped() {
        // An interrupted run leaves one of these. Parsing it as a result
        // would let resume skip a test that never ran.
        assert!(TestResult::parse("generic/013 PA", "n").is_none());
        assert!(TestResult::parse("", "n").is_none());
    }

    #[test]
    fn pass_rate_ignores_notrun() {
        let mut s = Summary::default();
        for (n, o) in [("a", Outcome::Pass), ("b", Outcome::Pass),
                       ("c", Outcome::Fail), ("d", Outcome::NotRun)] {
            s.add(&TestResult {
                name: n.into(), outcome: o, seconds: 1,
                node: "n".into(), reason: String::new(),
            });
        }
        // 2 of 3 that ran, not 2 of 4 attempted.
        assert!((s.pass_rate() - 66.666).abs() < 0.01);
        assert_eq!(s.attempted(), 4);
    }
}
