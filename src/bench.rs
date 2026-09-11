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
use crate::state::{self, Record};
use crate::evidence::{self, Case};
use crate::volume;
use crate::mem_trace;
use crate::trace_stack::{self, Tracing};
use crate::indicator::Progress;
use crate::node::NodeConn;

/// One trial, plus whether xfstests refused to start.
///
/// An abort says nothing about the filesystem, so it is carried beside
/// the trial rather than inside it and is excluded from every rate.
struct Attempt {
    trial: Trial,
    aborted: bool,
    /// What the harness objected to, empty when it passed.
    reason: String,
    /// Everything check printed, kept whole rather than filtered: the
    /// question asked of it changes and the output does not come back.
    output: String,
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
    // run_rc, not run: a failing test exits non-zero and that is the
    // result, not an error running it.
    //
    // `test` is passed through to ./check untouched, so everything the
    // harness understands works: one test, several, a -g group, or
    // nothing at all for the whole suite. A selection that takes hours
    // is measured the same way as one that takes three minutes.
    // The step file the indicator reads. Without it every trial shows
    // "start" for its whole duration and the spinner calls a healthy
    // three-minute run STALLED.
    let _ = c.run(
        "sudo sh -c 'echo check > /tmp/beamfs-step'",
        Duration::from_secs(20),
    );
    let (out, _rc) = c
        .run_rc(
            &format!("cd /usr/xfstests && sudo timeout -k 5 {} ./check {test} 2>&1",
                     deadline.as_secs().saturating_sub(30)),
            deadline,
        )
        .map_err(|e| e.to_string())?;

    // "aborting" is xfstests refusing to start, not a filesystem
    // verdict. Counting it as a failure is how a stale mount became an
    // afternoon of chasing a defect that was not there.
    let aborted = out.contains("aborting");
    // Whether the check ran at all. Without it, a count of zero means
    // the question was never asked.
    let (checked, _) = c
        .run_rc(
            &format!(
                "sudo grep -c 'fsck' /usr/xfstests/results/{test}.full 2>/dev/null || echo 0"
            ),
            Duration::from_secs(30),
        )
        .unwrap_or_else(|_| ("0".into(), 0));
    let checked = checked.trim().parse::<u32>().unwrap_or(0) > 0;
    // "Passed all N" for any N, not just one: a group of ninety tests
    // that all passed says "Passed all 90", and matching only the
    // single-test wording scored every group run as a failure.
    let passed = out
        .lines()
        .any(|l| l.starts_with("Passed all ") && !l.contains(" 0 "));
    // How many ran and how many failed, for a selection larger than one.
    let ran = out
        .lines()
        .find_map(|l| l.strip_prefix("Ran: "))
        .map(|r| r.split_whitespace().count())
        .unwrap_or(1);
    let failed_names: Vec<String> = out
        .lines()
        .find_map(|l| l.strip_prefix("Failures: "))
        .map(|r| r.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    // grep over a glob prefixes each match with its filename, so
    // '/usr/.../464.full:512 used-but-unreferenced' never matched an
    // anchored '^[0-9]+' and the block count came from nowhere. Read
    // the number that precedes the phrase instead, and only from the
    // file this test wrote.
    //
    // 464 runs _check_scratch_fs in its cleanup, so a trial that dies
    // early never reaches fsck: no count is not the same as no leak,
    // and the two were being reported identically.
    let lost = c
        .run_rc(
            &format!(
                "sudo grep -oE '[0-9]+ used-but-unreferenced' \
                 /usr/xfstests/results/{test}.full 2>/dev/null | \
                 head -1 | grep -oE '[0-9]+' | head -1 || true"
            ),
            Duration::from_secs(30),
        )
        .ok()
        .and_then(|(s, _)| s.trim().parse().ok())
        .unwrap_or(0);

    // Pointers the tree checker saw vanish, when the kernel was built
    // with it. Without that number a leak of 28 blocks and a leak of
    // 1014 look like the same event.
    let violations = c
        .run("sudo dmesg | grep -c 'LOST POINTER'", Duration::from_secs(30))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // What the harness objected to, when it was not a leak. Trial 1 of
    // the last campaign failed with fsck reporting nothing, and the
    // reason was thrown away with the output.
    // Both failing trials of the last campaign lost no blocks at all:
    // fsck found nothing, the tree checker saw nothing, and 464 failed
    // anyway. Whatever it objects to in those runs is not a leak, and
    // the reason was being filtered away by a pattern that assumed one.
    // Keep the harness's own words instead.
    let complaint: String = if passed {
        String::new()
    } else {
        out.lines()
            .filter(|l| {
                l.contains("output mismatch")
                    || l.contains("_check")
                    || l.contains("aborting")
                    || l.starts_with('+')
                    || l.contains("Failures:")
                    || l.contains("failed")
            })
            .take(6)
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    };

    if !failed_names.is_empty() {
        println!();
        println!("    {} of {ran} failed: {}", failed_names.len(),
                 failed_names.join(" "));
    }


    Ok(Attempt {
        trial: Trial { passed, lost, violations, secs: t0.elapsed().as_secs() },
        aborted,
        output: out.clone(),
        // Carried rather than printed: the indicator owns the current
        // line until finish() clears it, and a println here lands on
        // top of it. The caller prints this after finishing.
        reason: if passed || aborted {
            String::new()
        } else if !checked {
            // The single most useful thing to know about a short
            // failure: the filesystem was never checked, so whatever
            // went wrong went wrong before fsck could have an opinion.
            format!(
                "the test did not reach its own fsck ({}s) -- not a leak: {}",
                t0.elapsed().as_secs(),
                out.lines().rev().take(4).collect::<Vec<_>>().join(" | ")
            )
        } else if complaint.is_empty() {
            out.lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .iter()
                .rev()
                .map(|l| l.trim().to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        } else {
            complaint
        },
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

    // Two campaigns on one node destroy each other: the second finds
    // the volumes mounted, xfstests deletes the first one's temporary
    // files, and both report failures that belong to neither. Refuse
    // rather than produce results nobody can trust.
    // run_rc, not run: grep exits non-zero when it matches nothing,
    // which run reports as an error -- so a clean node looked busy and
    // the campaign refused to start on it.
    if let Ok((m, _)) = c.run_rc(
        "mount | grep -c ' /mnt/scratch ' || true",
        Duration::from_secs(20),
    ) {
        if m.trim().parse::<u32>().unwrap_or(0) > 0 {
            return Err(
                "the scratch volume is already mounted -- another campaign is running on this node"
                    .into(),
            );
        }
    }

    // Seed the load and trace the script. Without a seed two trials
    // write different files at different sizes, so a failure and a
    // success cannot be compared -- which is why no comparison between
    // them has ever been possible.
    let seed: u32 = std::env::var("XFSTESTS_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20260910);
    // Only a single named test can be instrumented.
    //
    // The seed and the -x trace are edits to one test file. With an
    // empty selection -- the whole suite -- there is no file to edit,
    // and a first version built the path anyway and tried to sed a
    // directory: seven error lines per trial and no instrumentation.
    // A group is the same case.
    //
    // The suite still runs, still archives, still freezes volumes on
    // failure. It just runs each test as the harness wrote it, which
    // for a 737-test sweep is what is wanted anyway: the seed exists to
    // make two trials of ONE test comparable.
    let single = !test.trim().is_empty()
        && !test.contains(' ')
        && !test.starts_with('-');
    let instrumented = single
        && match evidence::instrument(cfg, node, test, seed) {
        Ok(()) => {
            println!("  seed    : {seed} (same load every trial, traced line by line)");
            true
        }
        Err(e) => {
            println!("  not instrumented: {e}");
            false
        }
        };
    if !single {
        println!("  seed    : not applied -- a whole-suite run edits no test file");
    }

    let state = prepare(&c, &cfg.mkfs_options).map_err(|e| format!("prepare: {e}"))?;
    println!("  node    : {}", state.trim());
    println!();

    let r_commit_test = test.to_string();
    let mut series = Series { commit: r_commit.clone(), trials: Vec::new() };
    // The state around every trial, kept for the ones that pass too: a
    // variable only implicates itself when the failures differ from the
    // successes, and a record of failures alone cannot show that.
    let mut records: Vec<Record> = Vec::new();
    let domain = std::env::var("XFSTESTS_DOMAIN")
        .unwrap_or_else(|_| format!("beamfs-{}", node.name));

    // Tracing is off unless asked for: blktrace and the function
    // profiler cost enough to move the timing of a race, and a
    // measurement that changes what it measures is worth nothing.
    let tracing = std::env::var("XFSTESTS_TRACE").is_ok_and(|v| v == "1");
    if tracing {
        for (what, ok, val) in trace_stack::check(cfg, node) {
            if !ok {
                println!("  missing: {what} ({val})");
            }
        }
        if let Err(e) = mem_trace::arm(cfg, node) {
            println!("  function profile not armed: {e}");
        }
    }
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
        // A single test is minutes; the whole suite is hours. The
        // deadline follows the selection rather than a constant that
        // would kill the long ones halfway -- and the trace window is
        // built from it, so it has to be known before tracing starts.
        let budget: u64 = std::env::var("XFSTESTS_TRIAL_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(if test.trim().is_empty() || test.contains("-g ") {
                14 * 3600
            } else {
                600
            });

        let before = state::capture(cfg, node, &domain);
        let mem_before = if tracing { mem_trace::capture(cfg, node) } else { Default::default() };
        let tr = if tracing {
            let t = Tracing {
                dir: store().parent().unwrap_or(std::path::Path::new("/tmp"))
                    .join(format!("trace-{}-{n}", r.test.replace('/', "-"))),
                domain: domain.clone(),
                host_dev: std::env::var("XFSTESTS_HOST_DEV")
                    .unwrap_or_else(|_| "nvme1n1".into()),
                guest_dev: node.scratch_dev.rsplit('/').next().unwrap_or("vdc").into(),
            };
            match trace_stack::start(&t, cfg, node, budget) {
                Ok(off) => Some((t, off)),
                Err(e) => {
                    println!("  trace not started: {e}");
                    None
                }
            }
        } else {
            None
        };
        let t = match one_trial(&c, test, Duration::from_secs(budget)) {
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
        // Before anything else, and before the next trial's mkfs: the
        // .full is overwritten by every trial, so a count read after
        // the fact belongs to whichever trial ran last. Every number
        // reported over the last two days came from a stale file that
        // way.
        let case = Case::new(&evidence_root(), &r.test, n);
        evidence::collect(cfg, node, &case, &t.output);
        if !t.trial.passed && !t.aborted {
            match evidence::freeze_volume(cfg, node, &case) {
                Ok(sz) => {
                    println!(
                        "    trial {} volume kept: {} MiB compressed",
                        case.trial(),
                        sz / 1048576
                    );
                    // Read the tree off the frozen image while the
                    // failure is in front of us. A count of lost blocks
                    // says how many; this says whether they are lost at
                    // all or sitting under one indirect block that was
                    // named and never written.
                    match volume::inspect_compressed(&case.dir.join("scratch.img.zst")) {
                        Ok(v) => volume::report(&v),
                        Err(e) => println!("    volume not inspected: {e}"),
                    }
                }
                Err(e) => println!("    volume not kept: {e}"),
            }
        }

        let after = state::capture(cfg, node, &domain);
        // The indicator owns the current line until finish() clears it.
        // Printing a report over it interleaves the two.
        p.finish(&if t.trial.passed {
            format!("pass ({}s)", t.trial.secs)
        } else {
            format!(
                "FAIL ({}s, {} blocks lost, {} pointer(s) seen to vanish)",
                t.trial.secs, t.trial.lost, t.trial.violations
            )
        });
        if !t.reason.is_empty() {
            println!("    reason: {}", t.reason);
        }
        if let Some((t2, off)) = tr {
            match trace_stack::stop(&t2, cfg, node, off) {
                Ok(clocks) => trace_stack::report(&t2.dir, &clocks, &t2.domain),
                Err(e) => println!("  trace not stopped cleanly: {e}"),
            }
        }
        if tracing {
            // Sampled over five seconds: long enough for the counters
            // to mean something, short enough not to stretch the trial.
            let mem_after = mem_trace::capture_with_hw(cfg, node, &domain, 5);
            let prof = mem_trace::profile(cfg, node);
            mem_trace::report(&mem_before, &mem_after, &prof);
            mem_trace::reset(cfg, node);
        }
        // Written as it happens, not at the end. Yesterday's campaign
        // was interrupted at trial four and everything it had measured
        // went with it.
        append_record(&r.test, n, &t.trial, &after.delta(&before));
        records.push(Record {
            n,
            passed: t.trial.passed,
            lost: t.trial.lost,
            violations: t.trial.violations,
            secs: t.trial.secs,
            during: after.delta(&before),
            before,
            after,
            dmesg: c
                .run("sudo dmesg | tail -200", Duration::from_secs(30))
                .unwrap_or_default(),
        });
        if t.trial.passed {
            r.passed += 1;
        } else {
            r.failed += 1;
            r.lost.push(t.trial.lost);
        }
    }

    if instrumented {
        evidence::restore(cfg, node, test);
    }

    // The first failing trial against the first passing one. With the
    // same seed they ran the same script over the same files, so the
    // first line that differs is the divergence itself rather than a
    // hypothesis about it.
    let root = evidence_root();
    let first = |want_pass: bool| -> Option<std::path::PathBuf> {
        records
            .iter()
            .find(|r| r.passed == want_pass)
            .map(|r| Case::new(&root, &r_commit_test, r.n).dir)
    };
    if let (Some(p), Some(f)) = (first(true), first(false)) {
        match evidence::diverge(&p, &f) {
            Some(d) => evidence::report_divergence(&d),
            None => println!("  the two traces never diverge: the failure is not in what ran\n"),
        }
    }

    // What the campaign left on disk, and enough room for the next one.
    let removed = evidence::prune(&root, 6);
    let inv = evidence::inventory(&root);
    if !inv.is_empty() {
        let total: u64 = inv.iter().map(|(_, s, _)| s).sum();
        println!(
            "  evidence: {} trial(s), {} MiB, {} volume image(s) kept{}",
            inv.len(),
            total / 1048576,
            inv.iter().filter(|(_, _, f)| *f).count(),
            if removed > 0 { format!(", {removed} pruned") } else { String::new() }
        );
        println!("  under {}", root.display());
        println!();
    }

    println!();
    // The rate with its interval, the losses by size, and how much of
    // each leak the tree checker accounts for -- a pass count on its
    // own hides all three.
    stats::report(&series);
    report(&r);
    state::report(&records);

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

/// Append one trial's state to the log as soon as it is measured.
///
/// One line per variable, prefixed by the trial: greppable, appendable,
/// and complete even when the run is killed halfway.
fn append_record(test: &str, n: u32, t: &Trial, during: &crate::state::Snapshot) {
    let Some(dir) = store().parent().map(|d| d.to_path_buf()) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("state-{}.log", test.replace('/', "-")));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut body = format!(
        "{now} {n} outcome={} lost={} violations={} secs={}\n",
        if t.passed { "pass" } else { "fail" },
        t.lost,
        t.violations,
        t.secs
    );
    for (k, v) in &during.v {
        body.push_str(&format!("{now} {n} {k}={v}\n"));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(body.as_bytes());
    }
}


/// The tests a selection covers, as the harness itself resolves it.
///
/// ./check -n prints what it would run and runs nothing. Parsing that
/// is better than globbing the tests directory: the harness applies its
/// own exclusions, its group files and its _requires, and a list built
/// here would disagree with it in ways that only show up as spurious
/// failures.
fn enumerate_tests(c: &NodeConn, selection: &str) -> Result<Vec<String>, String> {
    let (out, _) = c
        .run_rc(
            &format!("cd /usr/xfstests && sudo ./check -n {selection} 2>&1 || true"),
            Duration::from_secs(300),
        )
        .map_err(|e| e.to_string())?;

    let mut v: Vec<String> = out
        .split_whitespace()
        .filter(|w| {
            // "generic/464", "shared/002" -- a family, a slash, digits.
            w.contains('/')
                && w.rsplit('/').next().is_some_and(|n| {
                    !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                })
        })
        .map(String::from)
        .collect();
    v.sort();
    v.dedup();

    if v.is_empty() {
        return Err("the harness listed no tests for this selection".into());
    }
    Ok(v)
}

/// Where the evidence for a campaign lives.
fn evidence_root() -> std::path::PathBuf {
    store()
        .parent()
        .map(|d| d.join("evidence"))
        .unwrap_or_else(|| std::path::PathBuf::from("/var/tmp/beamfs-evidence"))
}

/// Run every test of a selection once, and report what failed.
///
/// This is what a whole-suite run should always have been. Each test is
/// its own trial: its own archive, its own frozen volume on failure,
/// its own block count. A campaign that dies halfway still has
/// everything it measured up to that point, written as it went.
pub fn sweep(cfg: &Config, node: &Node, selection: &str) -> Result<(), String> {
    let c = NodeConn::new(node, cfg);

    println!("  node    : {}", node.name);
    println!("  commit  : {}", commit());

    let tests = enumerate_tests(&c, selection)?;
    println!("  tests   : {} to run", tests.len());
    println!();

    let root = evidence_root();
    let mut passed = 0usize;
    let mut failed: Vec<(String, usize)> = Vec::new();
    let mut aborted: Vec<String> = Vec::new();
    let t_start = std::time::Instant::now();

    for (i, test) in tests.iter().enumerate() {
        let p = Progress::start(
            &format!("{}/{} {test}", i + 1, tests.len()),
            &cfg.ssh_key,
            &format!("{}@{}", cfg.user, node.host),
        );

        if let Err(e) = prepare(&c, &cfg.mkfs_options) {
            p.finish(&format!("cannot prepare the node: {e}"));
            aborted.push(test.clone());
            continue;
        }

        let budget = std::env::var("XFSTESTS_TRIAL_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1800);

        let t = match one_trial(&c, test, Duration::from_secs(budget)) {
            Ok(t) => t,
            Err(e) => {
                p.finish(&format!("error: {e}"));
                aborted.push(test.clone());
                continue;
            }
        };

        if t.aborted {
            p.finish(&format!("skipped ({}s)", t.trial.secs));
            aborted.push(test.clone());
            continue;
        }

        if t.trial.passed {
            passed += 1;
            p.finish(&format!("pass ({}s)", t.trial.secs));
        } else {
            failed.push((test.clone(), t.trial.lost));
            p.finish(&format!(
                "FAIL ({}s, {} blocks lost)",
                t.trial.secs, t.trial.lost
            ));
        }

        // Archived whichever way it went: a passing test's state is
        // what a failing one has to be compared against.
        let case = Case::new(&root, test, 1);
        evidence::collect(cfg, node, &case, &t.output);
        if !t.trial.passed {
            if !t.reason.is_empty() {
                println!("    reason: {}", t.reason);
            }
            match evidence::freeze_volume(cfg, node, &case) {
                Ok(sz) => {
                    println!("    volume kept: {} MiB compressed", sz / 1048576);
                    if let Ok(v) =
                        volume::inspect_compressed(&case.dir.join("scratch.img.zst"))
                    {
                        volume::report(&v);
                    }
                }
                Err(e) => println!("    volume not kept: {e}"),
            }
        }
    }

    println!();
    println!(
        "  === {} of {} passed in {} minutes ===",
        passed,
        tests.len(),
        t_start.elapsed().as_secs() / 60
    );
    if !failed.is_empty() {
        println!();
        println!("  failed:");
        for (t, lost) in &failed {
            if *lost > 0 {
                println!("    {t}  ({lost} blocks lost)");
            } else {
                println!("    {t}");
            }
        }
    }
    if !aborted.is_empty() {
        println!();
        println!("  not run ({}): {}", aborted.len(), aborted.join(" "));
    }

    // Prune here rather than per test: a sweep that fails often would
    // otherwise keep one image per failure and fill the disk it is
    // running on.
    let removed = evidence::prune(&root, 12);
    if removed > 0 {
        println!();
        println!("  {removed} older volume image(s) pruned");
    }
    println!("  evidence under {}", root.display());
    Ok(())
}
