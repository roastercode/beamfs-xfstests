// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! One line, rewritten in place, that says whether anything is moving.
//!
//! A run of this suite under emulation takes hours, and the operator's
//! question throughout is not "how far along" but "is it still alive".
//! A percentage answers neither: it can sit unchanged for twenty minutes
//! during one slow test and look identical to a dead node.
//!
//! So the line carries three separate things. A bar that moves with
//! completed tests. A spinner that turns on every poll, which stops
//! turning if the loop itself is stuck. And the test each node is
//! currently on, which changes even when the totals do not.

use std::io::Write;
use std::time::{Duration, Instant};

pub struct Progress {
    start: Instant,
    ticks: u64,
    total: usize,
    width: usize,
}

impl Progress {
    #[must_use]
    pub fn new(total: usize) -> Self {
        Self { start: Instant::now(), ticks: 0, total, width: 20 }
    }

    /// Redraw. `per_node` is (name, current test, blocked task count).
    pub fn draw(&mut self, done: usize, counts: (usize, usize, usize, usize, usize),
                per_node: &[(String, String, usize)]) {
        self.ticks += 1;
        let (pass, fail, notrun, hang, mountfail) = counts;
        let pct = done.checked_mul(100).and_then(|v| v.checked_div(self.total)).unwrap_or(0);
        let filled = pct * self.width / 100;

        let mut bar = String::with_capacity(self.width);
        for i in 0..self.width {
            bar.push(if i < filled { '|' } else { ' ' });
        }

        let spin = ['|', '/', '-', '\\'][(self.ticks % 4) as usize];
        let mins = self.start.elapsed().as_secs() / 60;

        // Node columns last: they are the widest and the least likely to
        // be read at a glance, so truncation eats them first.
        let mut nodes = String::new();
        for (name, test, blocked) in per_node {
            let short = name.get(..4).unwrap_or(name);
            let t = if test.is_empty() { "-" } else { test.as_str() };
            if *blocked > 0 {
                nodes.push_str(&format!(" {short}:{t}(D{blocked})"));
            } else {
                nodes.push_str(&format!(" {short}:{t}"));
            }
        }

        print!(
            "\r  [{bar}]{pct:3}% {spin} {mins:4}min {done:3}/{total} \
             OK={pass:<3} KO={fail:<3} NR={notrun:<3} HG={hang:<2} MF={mountfail:<2}{nodes}",
            total = self.total,
        );
        let _ = std::io::stdout().flush();
    }

    /// Clear the line so whatever prints next starts clean.
    pub fn clear(&self) {
        print!("\r{:width$}\r", " ", width = 130);
        let _ = std::io::stdout().flush();
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Decides when a node has stopped making progress.
///
/// Two conditions together, never one alone: the count of finished tests
/// has not moved, and something is stuck in uninterruptible sleep. Either
/// on its own is normal -- a slow test moves nothing for minutes, and a
/// D-state task appears briefly on any write -- and acting on one alone
/// kills healthy runs.
pub struct StallDetector {
    last_count: Option<usize>,
    stalls: u32,
    limit: u32,
}

impl StallDetector {
    /// `limit` is in polls, not seconds; the caller owns the interval.
    #[must_use]
    pub fn new(limit: u32) -> Self {
        // None rather than a sentinel: usize::MAX differs from every
        // real count, so the first poll always looked like progress and
        // the stall counter could never start.
        Self { last_count: None, stalls: 0, limit }
    }

    /// True when the node should be considered wedged.
    pub fn update(&mut self, count: usize, blocked: usize) -> bool {
        let moved = self.last_count != Some(count);
        self.last_count = Some(count);
        if moved {
            self.stalls = 0;
            return false;
        }
        if blocked > 0 {
            self.stalls += 1;
        } else {
            // No progress but nothing blocked: a long test, not a hang.
            self.stalls = 0;
        }
        self.stalls >= self.limit
    }

    /// How many consecutive polls have seen no progress.
    ///
    /// Not used by the run loop, which only cares about the threshold.
    /// Kept because the tests assert on it and because a caller printing
    /// "stalled for N minutes" needs it.
    #[must_use]
    #[allow(dead_code)]
    pub fn stalled_polls(&self) -> u32 {
        self.stalls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_alone_is_never_a_stall() {
        // Alternating counts: tests keep finishing, so however many
        // tasks sit in D at any moment, nothing is wedged.
        let mut d = StallDetector::new(3);
        for _ in 0..10 {
            assert!(!d.update(5, 2));
            assert!(!d.update(6, 2));
        }
    }

    #[test]
    fn the_first_poll_starts_the_count() {
        // usize::MAX as the initial value made the first poll look like
        // progress, which reset the counter before it could ever run.
        let mut d = StallDetector::new(2);
        assert!(!d.update(0, 1));
        assert_eq!(d.stalled_polls(), 0);
        assert!(!d.update(0, 1));
        assert!(d.update(0, 1));
        // usize::MAX as the initial value never matched a real count, so
        // every poll looked like progress and the counter stayed at zero
        // forever.
    }

    #[test]
    fn blocked_without_progress_eventually_stalls() {
        // The first poll only establishes the reference: a node that has
        // just started has not failed to progress, it has not been asked
        // to yet. Three stalls therefore need four polls.
        let mut d = StallDetector::new(3);
        assert!(!d.update(5, 1));
        assert!(!d.update(5, 1));
        assert!(!d.update(5, 1));
        assert!(d.update(5, 1));
    }

    #[test]
    fn a_slow_test_is_not_a_stall() {
        // Nothing finishing, nothing blocked: generic/247 took 583s.
        let mut d = StallDetector::new(3);
        for _ in 0..20 {
            assert!(!d.update(5, 0));
        }
    }

    #[test]
    fn recovery_resets_the_count() {
        let mut d = StallDetector::new(3);
        d.update(5, 1);   // reference
        d.update(5, 1);   // 1
        d.update(5, 1);   // 2
        assert_eq!(d.stalled_polls(), 2);
        d.update(6, 0);
        assert_eq!(d.stalled_polls(), 0);
    }
}
