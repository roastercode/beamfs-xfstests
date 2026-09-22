// SPDX-License-Identifier: GPL-2.0-only
//! Say what a difference between two runs means, if anything.
//!
//! generic/464 fails intermittently. Counting passes out of ten and
//! comparing that to the last ten is what produced three reverted
//! patches in one day: the same commit measured 8 out of 10 in the
//! morning and 2 out of 10 in the evening, with no change between them,
//! so every difference attributed to a patch that day was smaller than
//! the spread of the measurement itself.
//!
//! A rate on its own is not a measurement. What is needed is the spread
//! on unchanged code, an interval around each rate, and the number of
//! trials it would take to resolve a difference of a given size. Those
//! are what this module computes, and it refuses to call a difference
//! real when the sample cannot carry it.
//!
//! It also keeps what a pass rate throws away. Losing 28 blocks and
//! losing 1014 are not the same event, and an outcome column cannot
//! tell them apart; the loss distribution is reported separately. So is
//! the correlation between blocks lost and pointers the tree checker
//! saw vanish, because a run where those two numbers match exactly is
//! a different defect from a run where hundreds are lost and the
//! checker sees nothing.

use std::collections::BTreeMap;

/// One trial: what the filesystem did, not what the harness said.
#[derive(Clone, Copy)]
pub struct Trial {
    pub passed: bool,
    /// Blocks fsck called used-but-unreferenced.
    pub lost: usize,
    /// Pointers the tree checker saw vanish during this trial.
    pub violations: usize,
    /// Blocks some inode still points at while the bitmap calls them
    /// free. Not a loss: the opposite of one, and the more dangerous,
    /// since the allocator can hand such a block out again.
    pub referenced_free: usize,
    /// Pointers fsck found outside the volume.
    pub out_of_range: usize,
    pub secs: u64,
}

/// A set of trials on one revision.
pub struct Series {
    pub commit: String,
    pub trials: Vec<Trial>,
}

impl Series {
    pub fn n(&self) -> usize {
        self.trials.len()
    }

    pub fn passes(&self) -> usize {
        self.trials.iter().filter(|t| t.passed).count()
    }

    pub fn rate(&self) -> f64 {
        if self.trials.is_empty() {
            return 0.0;
        }
        self.passes() as f64 / self.n() as f64
    }

    /// Wilson score interval, not the textbook normal one.
    ///
    /// At 8 of 10 the normal approximation runs past 1.0, and at 10 of
    /// 10 it collapses to a point — both are wrong in the direction
    /// that matters here, which is claiming more precision than ten
    /// trials hold. Wilson stays inside [0,1] and stays wide at the
    /// ends.
    pub fn interval(&self, z: f64) -> (f64, f64) {
        let n = self.n() as f64;
        if n == 0.0 {
            return (0.0, 1.0);
        }
        let p = self.rate();
        let d = 1.0 + z * z / n;
        let centre = (p + z * z / (2.0 * n)) / d;
        let half = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / d;
        ((centre - half).max(0.0), (centre + half).min(1.0))
    }

    /// Blocks lost, over failing trials only.
    pub fn losses(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.trials.iter().map(|t| t.lost).filter(|l| *l > 0).collect();
        v.sort_unstable();
        v
    }

    /// Trials where every lost block has a violation to account for it.
    ///
    /// When those two numbers match exactly the tree checker explains
    /// the whole leak; when hundreds are lost and it saw nothing, the
    /// pointers were right in memory and something after that lost
    /// them. Two defects, and a pass rate hides the difference.
    pub fn accounted(&self) -> (usize, usize, usize) {
        let mut exact = 0;
        let mut partial = 0;
        let mut none = 0;
        for t in self.trials.iter().filter(|t| t.lost > 0) {
            if t.violations == t.lost {
                exact += 1;
            } else if t.violations > 0 {
                partial += 1;
            } else {
                none += 1;
            }
        }
        (exact, partial, none)
    }
}

/// The spread of the measurement on unchanged code.
///
/// Several series of the same revision. Until this is known, no
/// difference between revisions means anything, and this is the number
/// that was missing for two weeks.
pub struct Baseline {
    pub commit: String,
    pub series: Vec<Series>,
}

impl Baseline {
    pub fn rates(&self) -> Vec<f64> {
        self.series.iter().map(|s| s.rate()).collect()
    }

    pub fn spread(&self) -> (f64, f64) {
        let r = self.rates();
        let lo = r.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = r.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        (lo, hi)
    }

    pub fn mean(&self) -> f64 {
        let r = self.rates();
        if r.is_empty() {
            return 0.0;
        }
        r.iter().sum::<f64>() / r.len() as f64
    }

    /// Standard deviation across series, which is the honest error bar.
    ///
    /// Not the binomial standard error: that assumes every trial is an
    /// independent draw from one fixed rate, and the 8-then-2 result
    /// says they are not. Something between series moves the rate, so
    /// the spread between series is what a comparison has to clear.
    pub fn sd(&self) -> f64 {
        let r = self.rates();
        if r.len() < 2 {
            return f64::NAN;
        }
        let m = self.mean();
        (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (r.len() - 1) as f64).sqrt()
    }

    /// Trials per arm to resolve a difference of `delta`, at 80% power.
    ///
    /// Uses the between-series spread, so it answers the question that
    /// matters -- how many trials before a difference this size stops
    /// being noise -- rather than the one the binomial formula answers.
    pub fn trials_needed(&self, delta: f64) -> Option<usize> {
        let sd = self.sd();
        if !sd.is_finite() || delta <= 0.0 {
            return None;
        }
        if sd == 0.0 {
            return Some(self.series.first().map(|s| s.n()).unwrap_or(10));
        }
        // (z_{a/2} + z_b)^2 * 2 * sd^2 / delta^2, two-sided 5%, power 80%.
        let n = (1.96 + 0.84f64).powi(2) * 2.0 * sd * sd / (delta * delta);
        Some(n.ceil() as usize * self.series.first().map(|s| s.n()).unwrap_or(10))
    }
}

/// What a difference between two revisions amounts to.
pub enum Verdict {
    /// Inside the spread of the measurement itself.
    Noise { delta: f64, spread: f64, need: Option<usize> },
    Regression { delta: f64 },
    Improvement { delta: f64 },
    /// No baseline: nothing to compare a difference against.
    Unknown { delta: f64 },
    /// Too few trials on one side to carry the difference at all,
    /// whatever the spread of unchanged code says.
    TooFew { delta: f64, trials: usize, margin: f64 },
}

/// The half-width a two-sided 95% binomial interval has at `n` trials,
/// at the worst case p = 0.5: what a difference has to clear before
/// the trial count alone could have produced it.
#[must_use]
pub fn binomial_margin(n: usize) -> f64 {
    if n == 0 {
        return 1.0;
    }
    1.96 * (0.25f64 / n as f64).sqrt()
}

pub fn compare(before: &Series, after: &Series, base: Option<&Baseline>) -> Verdict {
    let delta = after.rate() - before.rate();
    let n = before.n().min(after.n());
    let margin = binomial_margin(n);

    let Some(b) = base else {
        // No spread to judge against; the trial count still can.
        if delta.abs() < margin {
            return Verdict::TooFew { delta, trials: n, margin };
        }
        return Verdict::Unknown { delta };
    };

    // Inside the spread of unchanged code: noise, and the spread is
    // the informative figure.
    let (lo, hi) = b.spread();
    let spread = hi - lo;
    if delta.abs() <= spread {
        return Verdict::Noise { delta, spread, need: b.trials_needed(delta.abs().max(0.05)) };
    }

    // Outside the spread but carried by too few trials: no verdict
    // either. On 2026-09-22 one trial against two, on the same commit,
    // was called a REGRESSION outside a 30-point spread, ten lines
    // under bench's own "within what 1 trials can resolve". A
    // difference one trial carries is not a difference.
    if delta.abs() < margin {
        return Verdict::TooFew { delta, trials: n, margin };
    }

    if delta < 0.0 {
        Verdict::Regression { delta }
    } else {
        Verdict::Improvement { delta }
    }
}

/// Print a series, its interval, and what its losses look like.
pub fn report(s: &Series) {
    let (lo, hi) = s.interval(1.96);
    println!("  === {} ===", s.commit);
    println!(
        "  {} of {} passed -- {:.0}%, and the true rate is somewhere in {:.0}..{:.0}%",
        s.passes(),
        s.n(),
        s.rate() * 100.0,
        lo * 100.0,
        hi * 100.0
    );

    let l = s.losses();
    if l.is_empty() {
        println!("  no blocks lost");
    } else {
        let total: usize = l.iter().sum();
        println!(
            "  blocks lost per failing trial: {} (median {}, total {total})",
            l.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "),
            l[l.len() / 2]
        );
        // A leak of 28 and a leak of 1014 are not one phenomenon with a
        // wide spread; saying so in the summary keeps them apart.
        if *l.last().unwrap() > 10 * l[0].max(1) {
            println!("  the largest is {}x the smallest: likely more than one mechanism",
                     l.last().unwrap() / l[0].max(1));
        }
    }

    let rf: usize = s.trials.iter().map(|t| t.referenced_free).sum();
    let oor: usize = s.trials.iter().map(|t| t.out_of_range).sum();
    if rf + oor > 0 {
        println!(
            "  fsck also saw: {rf} block(s) referenced but marked free, {oor} pointer(s) out of range"
        );
        println!("  -- not in the losses above, which count used-but-unreferenced only");
    }

    let (exact, partial, none) = s.accounted();
    if exact + partial + none > 0 {
        println!(
            "  leaks the checker accounts for: {exact} fully, {partial} partly, {none} not at all"
        );
        if none > 0 {
            println!("  -- where it accounts for none, the pointers were right in memory");
        }
    }
    println!();
}

/// Print the baseline: what the measurement does on unchanged code.
pub fn report_baseline(b: &Baseline) {
    let (lo, hi) = b.spread();
    println!("  === baseline at {} ===", b.commit);
    println!(
        "  {} series of the same code: {}",
        b.series.len(),
        b.rates()
            .iter()
            .map(|r| format!("{:.0}%", r * 100.0))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  spread {:.0}..{:.0} points, sd {:.1}",
        lo * 100.0,
        hi * 100.0,
        b.sd() * 100.0
    );
    println!();
    println!("  a difference has to clear {:.0} points to mean anything here", (hi - lo) * 100.0);
    for d in [0.10, 0.20, 0.30] {
        match b.trials_needed(d) {
            Some(n) => println!("    to resolve {:.0} points: about {n} trials per arm", d * 100.0),
            None => println!("    to resolve {:.0} points: not computable from one series", d * 100.0),
        }
    }
    println!();
}

/// Print the verdict of a comparison.
pub fn report_verdict(v: &Verdict) {
    match v {
        Verdict::Noise { delta, spread, need } => {
            println!("  change {:+.0} points, spread on unchanged code is {:.0}", delta * 100.0, spread * 100.0);
            println!("  -- no verdict: the difference is inside the measurement");
            if let Some(n) = need {
                println!("  -- about {n} trials per arm would settle it");
            }
        }
        Verdict::Regression { delta } => {
            println!("  change {:+.0} points -- REGRESSION, outside the spread", delta * 100.0);
        }
        Verdict::TooFew { delta, trials, margin } => {
            println!("  change {:+.0} points, but {trials} trial(s) resolve nothing under +/-{:.0}",
                     delta * 100.0, margin * 100.0);
            println!("  -- no verdict: the difference is smaller than the trial count can carry");
        }
        Verdict::Improvement { delta } => {
            println!("  change {:+.0} points -- improvement, outside the spread", delta * 100.0);
        }
        Verdict::Unknown { delta } => {
            println!("  change {:+.0} points, but no baseline exists", delta * 100.0);
            println!("  -- run `bench baseline` first: without it a difference means nothing");
        }
    }
    println!();
}

/// Group losses by order of magnitude.
///
/// Twenty-eight blocks and a thousand are different events; a histogram
/// says so where a mean does not.
pub fn magnitudes(all: &[usize]) -> BTreeMap<u32, usize> {
    let mut m = BTreeMap::new();
    for &l in all.iter().filter(|l| **l > 0) {
        let d = (l as f64).log10().floor() as u32;
        *m.entry(d).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(commit: &str, outcomes: &[(bool, usize, usize)]) -> Series {
        Series {
            commit: commit.into(),
            trials: outcomes
                .iter()
                .map(|&(passed, lost, violations)| Trial { passed, lost, violations, referenced_free: 0, out_of_range: 0, secs: 100 })
                .collect(),
        }
    }

    #[test]
    fn a_block_referenced_but_free_is_not_a_loss() {
        // generic/464 and 476 fail on hundreds of these and were
        // recorded as losing nothing. They still are not losses; they
        // are counted, and said, apart.
        let mut x = s("abc", &[(false, 0, 0)]);
        x.trials[0].referenced_free = 400;
        assert!(x.losses().is_empty());
        assert_eq!(x.trials[0].referenced_free, 400);
    }

    #[test]
    fn the_interval_stays_inside_zero_and_one() {
        let all = s("x", &[(true, 0, 0); 10]);
        let (lo, hi) = all.interval(1.96);
        assert!(lo >= 0.0 && hi <= 1.0);
        // Ten of ten is not certainty, and the interval must not say so.
        assert!(lo < 1.0);
    }

    #[test]
    fn a_difference_inside_the_spread_is_not_a_verdict() {
        let base = Baseline {
            commit: "x".into(),
            series: vec![
                s("x", &[(true, 0, 0); 8]),
                s("x", &[(true, 0, 0), (false, 5, 0), (false, 5, 0), (false, 5, 0),
                         (false, 5, 0), (false, 5, 0), (false, 5, 0), (false, 5, 0)]),
            ],
        };
        // 8/10 against 5/10 -- the exact comparison that got three
        // patches reverted on a day when the spread was 6 points wider.
        let before = s("a", &[(true, 0, 0), (true, 0, 0), (true, 0, 0), (true, 0, 0),
                              (true, 0, 0), (true, 0, 0), (true, 0, 0), (true, 0, 0),
                              (false, 9, 0), (false, 9, 0)]);
        let after = s("b", &[(true, 0, 0), (true, 0, 0), (true, 0, 0), (true, 0, 0),
                             (true, 0, 0), (false, 9, 0), (false, 9, 0), (false, 9, 0),
                             (false, 9, 0), (false, 9, 0)]);
        assert!(matches!(compare(&before, &after, Some(&base)), Verdict::Noise { .. }));
    }

    #[test]
    fn one_trial_against_two_is_too_few_whatever_the_spread() {
        assert!(binomial_margin(1) > 0.9, "{}", binomial_margin(1));
        assert!(binomial_margin(100) < 0.1, "{}", binomial_margin(100));
        assert!(binomial_margin(0) >= 1.0);
    }

    #[test]
    fn without_a_baseline_there_is_no_verdict() {
        let before = s("a", &[(true, 0, 0); 10]);
        let after = s("b", &[(false, 1, 0); 10]);
        assert!(matches!(compare(&before, &after, None), Verdict::Unknown { .. }));
    }

    #[test]
    fn a_leak_the_checker_fully_accounts_for_is_counted_apart() {
        let x = s("x", &[(false, 28, 28), (false, 1014, 0), (false, 40, 3), (true, 0, 0)]);
        let (exact, partial, none) = x.accounted();
        assert_eq!((exact, partial, none), (1, 1, 1));
    }

    #[test]
    fn magnitudes_separate_a_leak_of_28_from_one_of_1014() {
        // The real numbers from 2026-09-09: two leaks in the tens and
        // two in the hundreds and thousands, which is the point -- they
        // are not one phenomenon with a wide spread.
        let m = magnitudes(&[28, 1014, 33, 912, 2]);
        assert_eq!(m.get(&0), Some(&1));   // 2
        assert_eq!(m.get(&1), Some(&2));   // 28, 33
        assert_eq!(m.get(&2), Some(&1));   // 912
        assert_eq!(m.get(&3), Some(&1));   // 1014
    }
}
