// SPDX-License-Identifier: GPL-2.0-only
//! Measure one test's pass rate, and compare it against the last time.
//!
//! generic/464 fails intermittently, so a single run says nothing about
//! a change. On 2026-09-09 a locking patch took it from 8 passes in ten
//! to 4, and that was noticed three hours and two more patches later,
//! by which point which change had done it was guesswork.
//!
//! So: a run of N trials against a named commit, stored, and compared
//! with the stored run before it. The comparison is the point -- a rate
//! on its own is a number, a rate against the previous one is a verdict.
//!
//! Two things this is careful about, both learnt from getting them
//! wrong.
//!
//! An aborted trial is not a failed one. xfstests refuses to start when
//! the test volume is mounted twice, or when a previous run left results
//! behind, and reports that as a failure like any other. Ten such
//! aborts read as a catastrophic regression and sent a whole afternoon
//! after a defect that was a stale mount. Aborts are counted apart and
//! excluded from the rate.
//!
//! And a difference of one or two out of ten is not a signal. Ten
//! trials distinguish 30% from 80%, not 70% from 80%. The verdict says
//! so rather than inviting a conclusion the sample cannot carry.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::{Config, Node};
use crate::stats::{self, Baseline, Series, Trial};
use crate::indicator::Progress;
use crate::node::NodeConn;

/// One trial, plus whether xfstests refused to start.
///
/// An abort says nothing about the filesystem, so it is carried beside
/// the trial rather than inside it and is excluded from every rate.
struct Attempt {
    trial: Trial,
    aborted: bool,
}

/// A run of trials against one revision.
pub struct Run {
    pub commit: String,
    pub test: String,
    pub passed: usize,
    pub failed: usize,
    pub aborted: usize,
    /// Blocks lost per failing trial, in order.
    pub lost: Vec<usize>,
}

impl Run {
    /// Passes over trials that actually ran.
    pub fn rate(&self) -> f64 {
        let n = self.passed + self.failed;
        if n == 0 {
            return 0.0;
        }
        self.passed as f64 / n as f64
    }

    pub fn trials(&self) -> usize {
        self.passed + self.failed
    }

    fn line(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.commit,
            self.test,
            self.passed,
            self.failed,
            self.aborted,
            self.lost
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    fn parse(s: &str) -> Option<Run> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() < 5 {
            return None;
        }
        Some(Run {
            commit: f[0].into(),
            test: f[1].into(),
            passed: f[2].parse().ok()?,
            failed: f[3].parse().ok()?,
            aborted: f[4].parse().ok()?,
            lost: f
                .get(5)
                .map(|s| s.split(',').filter_map(|v| v.parse().ok()).collect())
                .unwrap_or_default(),
        })
    }
}

fn store() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("beamfs-xfstests/bench.log");
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".local/share/beamfs-xfstests/bench.log");
    }
    PathBuf::from("/var/tmp/beamfs-xfstests/bench.log")
}

/// The stored baseline for `test`, if `baseline` has ever been run.
///
/// Kept beside the run log. Until it exists, no comparison between
/// revisions means anything, and compare() says exactly that.
fn load_baseline(test: &str) -> Option<Baseline> {
    let p = store().parent()?.join(format!("baseline-{}.log", test.replace('/', "-")));
    let body = std::fs::read_to_string(p).ok()?;
    let series: Vec<Series> = body
        .lines()
        .filter_map(Run::parse)
        .map(|r| Series {
            commit: r.commit.clone(),
            trials: (0..r.passed)
                .map(|_| Trial { passed: true, lost: 0, violations: 0, secs: 0 })
                .chain(r.lost.iter().map(|&l| Trial {
                    passed: false,
                    lost: l,
                    violations: 0,
                    secs: 0,
                }))
                .collect(),
        })
        .collect();
    if series.len() < 2 {
        return None;
    }
    Some(Baseline { commit: series[0].commit.clone(), series })
}

/// The last stored run of `test`, whatever revision it was.
fn previous(test: &str) -> Option<Run> {
    let body = std::fs::read_to_string(store()).ok()?;
    body.lines()
        .filter_map(Run::parse)
        .rfind(|r| r.test == test)
}

fn append(r: &Run) -> std::io::Result<()> {
    let p = store();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
    writeln!(f, "{}", r.line())
}

/// The revision under test, so a rate is attached to something.
fn commit() -> String {
    std::process::Command::new("git")
        .args(["-C", &std::env::var("HOME").unwrap_or_default()])
        .args(["--no-pager", "log", "-1", "--format=%h"])
        .current_dir(
            std::env::var("BEAMFS_TREE").unwrap_or_else(|_| {
                format!("{}/git/beamfs", std::env::var("HOME").unwrap_or_default())
            }),
        )
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Put the node in a state xfstests will start from.
///
/// Every abort seen so far came from one of two things: the test volume
/// mounted more than once, or results from a previous run still on
/// disk. Both are cleared here rather than being diagnosed again.
fn prepare(c: &NodeConn, mkfs_opts: &str) -> Result<String, String> {
    let cmd = format!(
        "sudo sh -c 'pkill -9 xfs_io 2>/dev/null; \
         for i in 1 2 3 4 5; do \
           umount /mnt/test /mnt/scratch 2>/dev/null; \
           [ \"$(mount | grep -c \" /mnt/test \")\" -eq 0 ] && break; \
           umount -l /mnt/test /mnt/scratch 2>/dev/null; \
         done; \
         mkdir -p /mnt/test /mnt/scratch; \
         rm -f /usr/xfstests/results/generic/*.full /usr/xfstests/results/generic/*.out.bad; \
         mkfs.beamfs {mkfs_opts} /dev/vdb >/dev/null 2>&1; \
         mount -t beamfs /dev/vdb /mnt/test; \
         dmesg -C; \
         printf \"mounts=%s\\n\" \"$(mount | grep -cE \"vdb|vdc\")\"'"
    );
    c.run(&cmd, Duration::from_secs(180)).map_err(|e| e.to_string())
}

fn one_trial(c: &NodeConn, test: &str, deadline: Duration) -> Result<Attempt, String> {
    let t0 = Instant::now();
    let out = c
        .run(
            &format!("cd /usr/xfstests && sudo timeout -k 5 {} ./check {test} 2>&1",
                     deadline.as_secs().saturating_sub(30)),
            deadline,
        )
        .map_err(|e| e.to_string())?;

    // "aborting" is xfstests refusing to start, not a filesystem
    // verdict. Counting it as a failure is how a stale mount became an
    // afternoon of chasing a defect that was not there.
    let aborted = out.contains("aborting");
    let passed = out.contains("Passed all 1");

    let lost = c
        .run(
            "sudo grep -oE '[0-9]+ used-but-unreferenced' \
             /usr/xfstests/results/generic/*.full 2>/dev/null | \
             grep -oE '^[0-9]+' | head -1",
            Duration::from_secs(30),
        )
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // Pointers the tree checker saw vanish, when the kernel was built
    // with it. Without that number a leak of 28 blocks and a leak of
    // 1014 look like the same event.
    let violations = c
        .run("sudo dmesg | grep -c 'LOST POINTER'", Duration::from_secs(30))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    Ok(Attempt {
        trial: Trial { passed, lost, violations, secs: t0.elapsed().as_secs() },
        aborted,
    })
}

/// Run `trials` of `test` and report against the previous stored run.
pub fn run(
    cfg: &Config,
    node: &Node,
    test: &str,
    trials: u32,
) -> Result<Run, String> {
    let c = NodeConn::new(node, cfg);
    let rev = commit();
    let r_commit = rev.clone();

    println!("  node    : {}", node.name);
    println!("  test    : {test}");
    println!("  commit  : {rev}");
    println!("  trials  : {trials}");
    println!();

    let state = prepare(&c, &cfg.mkfs_options).map_err(|e| format!("prepare: {e}"))?;
    println!("  node    : {}", state.trim());
    println!();

    let mut series = Series { commit: r_commit.clone(), trials: Vec::new() };
    let mut r = Run {
        commit: rev,
        test: test.into(),
        passed: 0,
        failed: 0,
        aborted: 0,
        lost: Vec::new(),
    };

    for n in 1..=trials {
        let p = Progress::start(
            &format!("trial {n}/{trials}"),
            &cfg.ssh_key,
            &format!("{}@{}", cfg.user, node.host),
        );
        let t = match one_trial(&c, test, Duration::from_secs(600)) {
            Ok(t) => t,
            Err(e) => {
                p.finish(&format!("error: {e}"));
                continue;
            }
        };
        if t.aborted {
            r.aborted += 1;
            p.finish(&format!("ABORTED ({}s) -- not a verdict", t.trial.secs));
            // An abort usually leaves the node in a state the next
            // trial hits too. Clear it rather than aborting nine more
            // times.
            let _ = prepare(&c, &cfg.mkfs_options);
            continue;
        }
        // Every trial that ran, pass or fail, so the statistics module
        // has the losses and the checker's count and not just an
        // outcome column.
        series.trials.push(t.trial);
        if t.trial.passed {
            r.passed += 1;
            p.finish(&format!("pass ({}s)", t.trial.secs));
        } else {
            r.failed += 1;
            r.lost.push(t.trial.lost);
            p.finish(&format!(
                "FAIL ({}s, {} blocks lost, {} pointer(s) seen to vanish)",
                t.trial.secs, t.trial.lost, t.trial.violations
            ));
        }
    }

    println!();
    // The rate with its interval, the losses by size, and how much of
    // each leak the tree checker accounts for -- a pass count on its
    // own hides all three.
    stats::report(&series);
    report(&r);

    // The comparison against the last stored run, judged against the
    // spread on unchanged code rather than against nothing. Without a
    // baseline it says so instead of inventing a verdict -- which is
    // what happened on 2026-09-09, three times.
    if let Some(prev) = previous(&r.test) {
        let before = Series {
            commit: prev.commit.clone(),
            trials: (0..prev.passed)
                .map(|_| Trial { passed: true, lost: 0, violations: 0, secs: 0 })
                .chain(prev.lost.iter().map(|&l| Trial {
                    passed: false,
                    lost: l,
                    violations: 0,
                    secs: 0,
                }))
                .collect(),
        };
        let base = load_baseline(&r.test);
        stats::report_verdict(&stats::compare(&before, &series, base.as_ref()));
    }

    // Losses by order of magnitude: 28 blocks and 1014 are not one
    // phenomenon with a wide spread, and a list of numbers hides that.
    let all: Vec<usize> = series.trials.iter().map(|t| t.lost).collect();
    let m = stats::magnitudes(&all);
    if m.len() > 1 {
        println!("  losses by size:");
        for (d, n) in &m {
            let lo = 10usize.pow(*d);
            println!("    {lo}..{}: {n} trial(s)", lo * 10 - 1);
        }
        println!("  -- more than one order of magnitude: likely more than one mechanism");
        println!();
    }
    if let Err(e) = append(&r) {
        println!("  (not stored: {e})");
    }
    Ok(r)
}

/// Print the rate, and what it means against the previous run.
fn report(r: &Run) {
    println!("  === {} at {} ===", r.test, r.commit);
    println!(
        "  {} pass, {} fail out of {} trials  ({:.0}%)",
        r.passed,
        r.failed,
        r.trials(),
        r.rate() * 100.0
    );
    if r.aborted > 0 {
        println!(
            "  {} aborted and excluded: xfstests refused to start, which says",
            r.aborted
        );
        println!("  nothing about the filesystem");
    }
    if !r.lost.is_empty() {
        let total: usize = r.lost.iter().sum();
        println!(
            "  blocks lost per failure: {} (total {total})",
            r.lost.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    let Some(prev) = previous(&r.test) else {
        println!();
        println!("  no earlier run to compare against; this one is the baseline");
        return;
    };
    if prev.commit == r.commit {
        println!();
        println!("  previous run was the same commit ({}), so this is a repeat", prev.commit);
    }

    println!();
    println!(
        "  previous : {} pass / {} at {} ({:.0}%)",
        prev.passed,
        prev.trials(),
        prev.commit,
        prev.rate() * 100.0
    );

    // Ten trials separate 30% from 80%, not 70% from 80%. Saying which
    // of those this is matters more than the delta itself: a patch
    // judged on a two-trial difference is a patch judged on noise.
    let delta = r.rate() - prev.rate();
    let n = r.trials().min(prev.trials()) as f64;
    let margin = if n > 0.0 { 1.96 * (0.25f64 / n).sqrt() } else { 1.0 };

    if delta.abs() < margin {
        println!(
            "  change {:+.0} points, within what {} trials can resolve (+/-{:.0})",
            delta * 100.0,
            r.trials(),
            margin * 100.0
        );
        println!("  -- no verdict; run more trials if the answer matters");
    } else if delta < 0.0 {
        println!("  change {:+.0} points -- REGRESSION", delta * 100.0);
        println!("  -- the last change made it worse; revert before building on it");
    } else {
        println!("  change {:+.0} points -- improvement", delta * 100.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: usize, f: usize, a: usize) -> Run {
        Run { commit: "abc".into(), test: "generic/464".into(),
              passed: p, failed: f, aborted: a, lost: vec![] }
    }

    #[test]
    fn the_rate_ignores_aborted_trials() {
        // Eight of ten ran and six passed: 75%, not 60%.
        let x = r(6, 2, 2);
        assert_eq!(x.trials(), 8);
        assert!((x.rate() - 0.75).abs() < 0.001);
    }

    #[test]
    fn a_run_with_nothing_that_ran_has_no_rate() {
        assert_eq!(r(0, 0, 10).rate(), 0.0);
    }

    #[test]
    fn a_run_round_trips_through_its_stored_line() {
        let mut x = r(8, 2, 1);
        x.lost = vec![28, 609];
        let y = Run::parse(&x.line()).expect("parses");
        assert_eq!(y.passed, 8);
        assert_eq!(y.failed, 2);
        assert_eq!(y.aborted, 1);
        assert_eq!(y.lost, vec![28, 609]);
        assert_eq!(y.commit, "abc");
    }

    #[test]
    fn a_line_that_is_short_is_not_a_run() {
        assert!(Run::parse("abc generic/464").is_none());
    }
}

/// Run the same code several times over, and report the spread.
///
/// This is the number that was missing while three patches were judged
/// and reverted on differences smaller than the measurement itself. It
/// changes nothing between series -- same kernel, same node, same load
/// -- so whatever difference comes out is what the measurement does on
/// its own, and no comparison between revisions means anything until it
/// is known.
pub fn baseline(
    cfg: &Config,
    node: &Node,
    test: &str,
    trials: u32,
    rounds: u32,
) -> Result<Baseline, String> {
    println!("  node    : {}", node.name);
    println!("  test    : {test}");
    println!("  commit  : {}", commit());
    println!("  {rounds} series of {trials} trials, nothing changed between them");
    println!();

    let mut b = Baseline { commit: commit(), series: Vec::new() };
    for k in 1..=rounds {
        println!("  --- series {k}/{rounds} ---");
        let r = run(cfg, node, test, trials)?;
        // run() already printed the series; keep the rate for the spread.
        b.series.push(Series {
            commit: r.commit.clone(),
            trials: (0..r.passed)
                .map(|_| Trial { passed: true, lost: 0, violations: 0, secs: 0 })
                .chain(r.lost.iter().map(|&l| Trial {
                    passed: false,
                    lost: l,
                    violations: 0,
                    secs: 0,
                }))
                .collect(),
        });
    }
    // Store it: every later comparison is judged against this spread,
    // so it has to outlive the session that measured it.
    let p = store().parent().map(|d| d.join(format!("baseline-{}.log", test.replace('/', "-"))));
    if let Some(p) = p {
        let body: String = b
            .series
            .iter()
            .map(|s| {
                format!(
                    "{} {} {} {} 0 {}\n",
                    b.commit,
                    test,
                    s.passes(),
                    s.n() - s.passes(),
                    s.losses().iter().map(|l| l.to_string()).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        if let Err(e) = std::fs::write(&p, body) {
            println!("  (baseline not stored: {e})");
        } else {
            println!("  baseline stored at {}", p.display());
        }
    }
    stats::report_baseline(&b);
    Ok(b)
}
