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
use crate::wedge;
use crate::runpack;
use crate::bpf;
use crate::recovery::Recovery;
use crate::journal::Journal;
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
    // -C and current_dir both set the directory and git obeys -C, so
    // passing $HOME there sent it to a directory that is not a
    // repository: every run since has been recorded against "unknown",
    // which is a measurement attached to no code at all.
    let tree = std::env::var("BEAMFS_TREE").unwrap_or_else(|_| {
        format!("{}/git/beamfs", std::env::var("HOME").unwrap_or_default())
    });

    std::process::Command::new("git")
        .args(["-C", &tree])
        .args(["--no-pager", "log", "-1", "--format=%h"])
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
        evidence::speak(&case);
        if !t.trial.passed && !t.aborted {
            match evidence::freeze_volume(cfg, node, &case, &t.output) {
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
                        Ok(v) => volume::report_to(&v, Some(&case.dir)),
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


/// The domain to ask when a node stops answering.
///
/// One name because the x86 lab is one VM. A cluster would need this
/// per node, and the day it does the node struct is where it belongs.
const WEDGE_VM: &str = "beamfs-x86-01";

/// Set when the stop file appears: the loop finishes its test and stops.
///
/// A run killed outright loses its summary -- what passed, what failed,
/// what was never reached -- and that summary is most of what a sweep
/// produces. The evidence is written per test as it goes, so the
/// directories survive either way, but the tally does not.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Watch for a stop file instead of catching a signal.
///
/// This crate has no dependencies and catching SIGINT without libc
/// means writing the extern "C" handler and the sigaction call by hand,
/// which is a lot of unsafe for a flag. A watcher thread reading a path
/// every second does the same job in safe code, and has an advantage
/// the signal does not: the stop can come from another terminal, or
/// from a script, without finding the pid.
///
///   touch /tmp/beamfs-xfstests.stop
///
/// The file is removed when the run starts, so a leftover from a
/// previous campaign cannot stop the next one before it begins.
fn arm_stop() {
    let path = stop_path();
    let _ = std::fs::remove_file(&path);
    std::thread::spawn(move || loop {
        if path.exists() {
            STOP.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

fn stop_path() -> std::path::PathBuf {
    std::env::temp_dir().join("beamfs-xfstests.stop")
}

fn stopping() -> bool {
    STOP.load(std::sync::atomic::Ordering::SeqCst)
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

    // The node's tools, against the ones this repo builds.
    //
    // Every redeploy of the image puts its own mkfs.beamfs and
    // fsck.beamfs back. On 2026-09-13 a campaign reported 306 inodes
    // beyond correction on a sound volume because the checker
    // answering was the image's, 30840 bytes dated 2011, against the
    // 857568 built here -- and preflight had only asked whether a file
    // by that name existed.
    //
    // A wrong checker does not fail. It answers, and the answer is
    // taken for a finding.
    {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let local = vec![
            ("fsck.beamfs".to_string(),
             repo.join("beamfs/tools/fsck.beamfs/fsck.beamfs")
                 .to_string_lossy().into_owned()),
        ];
        // And the rest of what the run is about to assert: that the
        // kernel on the node is the one built here, and that nothing
        // is still mounted on the devices the checker will read raw.
        //
        // A mounted device gives a table of inodes half-updated and
        // every CRC in it wrong, which reads exactly like a filesystem
        // destroyed -- 306 of them on 2026-09-13, on a sound volume.
        let built = std::fs::metadata(
            std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
                .join("yocto/poky/build-qemux86/tmp/deploy/images/qemux86-64/bzImage"))
            .and_then(|m| m.modified())
            .ok();

        let wrong = c.ready_to_measure(&local, built);
        if !wrong.is_empty() {
            println!();
            for w in &wrong {
                println!("  {w}");
            }
            println!();
            return Err("the node is not what this run would claim it is -- \
                        fix the above before measuring anything".into());
        }
    }

    // ./check mounts TEST_DEV before it will list anything, even under
    // -n, so the node has to be prepared first. A freshly deployed image
    // has no /mnt/test and the enumeration comes back empty with a mount
    // error buried in output nobody reads.
    if let Err(e) = prepare(&c, &cfg.mkfs_options) {
        return Err(format!("cannot prepare the node: {e}"));
    }

    let tests = enumerate_tests(&c, selection)?;
    println!("  tests   : {} to run", tests.len());
    arm_stop();
    println!("  stop    : touch {} to finish the current test and stop",
             stop_path().display());
    println!();

    let root = evidence_root();
    let mut passed = 0usize;
    let mut failed: Vec<(String, usize)> = Vec::new();
    let mut aborted: Vec<String> = Vec::new();
    let t_start = std::time::Instant::now();

    let mut unreachable_run = 0usize;
    // The test the node last refused on, and how many times running.
    //
    // Refusals are counted per test rather than in a row: a node
    // wedged by generic/269 refuses generic/464 too, and treating that
    // as one long streak stopped the campaign with tests still unrun.
    let mut refused_test = String::new();
    let mut refused_times = 0u32;
    // Tests set aside because they wedge the node every time.
    let mut wedging: Vec<String> = Vec::new();
    // The case directories this run wrote, for the archive at the end.
    // Not the whole evidence tree: that holds every run before this one.
    let mut produced: Vec<std::path::PathBuf> = Vec::new();
    // How many times this sweep has revived the node. Recovery
    // escalates on it: a node killed twice and still blocked is
    // restarted rather than killed a third time.
    let mut wedge_attempts = 0u32;

    for (i, test) in tests.iter().enumerate() {
        if stopping() {
            println!();
            println!("  stopping after {} test(s): the stop file appeared", i);
            let _ = std::fs::remove_file(stop_path());
            break;
        }
        let p = Progress::start(
            &format!("{}/{} {test}", i + 1, tests.len()),
            &cfg.ssh_key,
            &format!("{}@{}", cfg.user, node.host),
        );

        if let Err(e) = prepare(&c, &cfg.mkfs_options) {
            p.finish(&format!("cannot prepare the node: {e}"));
            aborted.push(test.clone());
            unreachable_run += 1;

            if *test == refused_test {
                refused_times += 1;
            } else {
                refused_test = test.clone();
                refused_times = 1;
            }

            // First refusal only: the guest is asked once, while it is
            // still running and still has a console. By the third the
            // answer would be the same and the pty has moved on.
            if unreachable_run == 1 {
                println!("    the node stopped answering -- asking it why");
                // Its own directory: no Case exists here, because a
                // case is made when a test produces a verdict and this
                // one never will.
                let dir = root.join(format!(
                    "wedged-{}", test.replace('/', "-")));
                match wedge::capture_wedged(WEDGE_VM, &dir) {
                    Ok(n) => println!("    console kept: {} KiB in {}",
                                      n / 1024, dir.display()),
                    Err(e) => println!("    nothing captured: {e}"),
                }
            }
            // And then bring it back.
            //
            // Stopping here cost 272 tests three times over:
            // generic/464 wedges the node, the sweep gives up, and
            // everything after it is never measured. The recovery that
            // run and probe already use does the work -- kill, sysrq,
            // restart the domain -- and two minutes of boot is nothing
            // against three hours of campaign.
            if unreachable_run == 1 {
                println!("    bringing the node back");
                let rec = Recovery::new(cfg);
                let dom = rec.domain_for(&node.name);
                let dir = root.join(format!("wedged-{}",
                                            test.replace('/', "-")));
                let _ = std::fs::create_dir_all(&dir);
                let mut jr = Journal::create(&dir);
                let outcome = rec.recover_into(&c, &dom, wedge_attempts,
                                               &mut jr, Some(&dir));
                wedge_attempts += 1;
                println!("    recovery: {}", outcome.as_str());
                if outcome.usable() {
                    unreachable_run = 0;

                    // Twice on the same test means the test is what
                    // wedges it, not the one before. Retrying a third
                    // time costs another recovery and measures
                    // nothing; the rest of the list is worth more.
                    if refused_times >= 2 {
                        println!("    {test} wedges this node -- set aside, moving on");
                        wedging.push(test.clone());
                        refused_test.clear();
                        refused_times = 0;
                        continue;
                    }
                    continue;
                }
            }

            // Three refusals in a row after a recovery that did not
            // take. Seventeen tests once reported the same connection
            // timeout nine seconds apart, measuring nothing while the
            // list walked itself to the end.
            if unreachable_run >= 3 {
                println!();
                println!("  the node has refused {unreachable_run} times running; stopping");
                break;
            }
            continue;
        }
        unreachable_run = 0;

        let budget = std::env::var("XFSTESTS_TRIAL_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);

        // A bpftrace script, attached for the length of the test.
        //
        // XFSTESTS_BPF names one of scripts/*.bt. Nothing runs without
        // it: bpftrace instruments the kernel, and a campaign measuring
        // durations must not carry a probe it did not ask for.
        let probe = match std::env::var("XFSTESTS_BPF") {
            Ok(name) if !name.is_empty() => match bpf::start(&c, &name) {
                Ok(r) => {
                    println!("    {} attached on {}", r.name, r.node());
                    Some(r)
                }
                Err(e) => {
                    println!("    {name} did not start: {e}");
                    None
                }
            },
            _ => None,
        };
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

        // Emptied before anything writes into it.
        //
        // A case directory is named for the test, so a second run of
        // the same test lands in the first one's. What this run does
        // not produce -- a probe that did not start, a checker that
        // did not run -- stays behind and reads as if it had.
        //
        // On 2026-09-13 seven probe captures dated 13:03 sat beside a
        // dmesg dated 19:25, and four findings from the old ones were
        // read as this run's.
        let _ = std::fs::remove_dir_all(&case.dir);
        let _ = std::fs::create_dir_all(&case.dir);

        // The probe first: stopped before the checker runs, so what it
        // saw is the test rather than the test plus its verification.
        if let Some(r) = probe {
            match r.stop_into(&c, &case.dir) {
                Some((p, n)) => {
                    println!("    kept {} ({} KiB)",
                             p.file_name().unwrap_or_default().to_string_lossy(),
                             n / 1024);
                    bpf::speak(&p);
                }
                None => println!("    the probe brought nothing back"),
            }
        }

        evidence::collect(cfg, node, &case, &t.output);
        evidence::speak(&case);
        produced.push(case.dir.clone());
        if !t.trial.passed {
            if !t.reason.is_empty() {
                println!("    reason: {}", t.reason);
            }
            match evidence::freeze_volume(cfg, node, &case, &t.output) {
                Ok(sz) => {
                    println!("    volume kept: {} MiB compressed", sz / 1048576);
                    if let Ok(v) =
                        volume::inspect_compressed(&case.dir.join("scratch.img.zst"))
                    {
                        volume::report_to(&v, Some(&case.dir));
                    }
                }
                Err(e) => println!("    volume not kept: {e}"),
            }
        }
    }

    let done = passed + failed.len() + aborted.len();
    println!();
    if done < tests.len() {
        println!(
            "  === {} of {} passed, {} of {} run, in {} minutes ===",
            passed,
            done,
            done,
            tests.len(),
            t_start.elapsed().as_secs() / 60
        );
        println!("  {} test(s) never started", tests.len() - done);
    } else {
        println!(
            "  === {} of {} passed in {} minutes ===",
            passed,
            tests.len(),
            t_start.elapsed().as_secs() / 60
        );
    }
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
    if !wedging.is_empty() {
        println!();
        println!("  set aside, each wedged the node twice:");
        for t in &wedging {
            println!("    {t}");
        }
    }
    println!("  evidence under {}", root.display());

    // And one archive of it, in /tmp, without being asked.
    //
    // Building a tarball by hand was the step between a run finishing
    // and anybody looking at what it found, and a step in that place is
    // where findings are lost.
    //
    // The pruning above runs first on purpose: what it removed is what
    // nobody wanted carried, and the archive should not carry it either.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let kept: Vec<std::path::PathBuf> =
        produced.iter().filter(|p| p.exists()).cloned().collect();
    match runpack::pack_run(&kept, &format!("{stamp}")) {
        Some((path, size)) => runpack::announce(&path, size),
        None if kept.is_empty() => {}
        None => println!("  the archive could not be written"),
    }
    Ok(())
}
