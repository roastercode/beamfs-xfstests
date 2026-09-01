// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Run the kernel filesystem test suite against beamfs across the lab.
//!
//! `check -g generic` runs the suite in one process with no per-test
//! timeout. A test that wedges the filesystem takes the whole run with
//! it: on 2026-09-01 generic/285 sat in folio_wait_writeback for nine
//! hours and the other 127 tests in that batch never started. A night of
//! machine time produced one result.
//!
//! Here every test is its own invocation with its own deadline, results
//! are appended as they happen, and a run that is interrupted resumes
//! from what it had. Work is split across the nodes because a test that
//! takes ten seconds on real hardware takes two to three minutes under
//! TCG, and the suite is around 737 tests.

mod config;
mod journal;
mod node;
mod progress;
mod result;

use std::collections::BTreeMap;
use std::time::Duration;

use config::Config;
use journal::Journal;
use node::NodeConn;
use progress::{Progress, StallDetector};
use result::{Outcome, Summary, TestResult};

const RUNNER: &str = include_str!("runner.sh");
const SUITE_SIZE: usize = 737;
const POLL: Duration = Duration::from_secs(60);
/// Twenty polls of no progress with something blocked. Long enough that
/// the slowest measured test (583s) cannot trip it.
const STALL_LIMIT: u32 = 20;

fn main() -> std::process::ExitCode {
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("report") => report(&cfg),
        Some("stop") => stop(&cfg),
        Some("--help" | "-h") => usage(),
        _ => run(&cfg),
    }
}

fn usage() -> std::process::ExitCode {
    eprintln!(
        "usage: beamfs-xfstests [run|report|stop]\n\
         \n\
         run     shard the suite across the nodes and follow it (default)\n\
         report  summarise what the nodes have recorded so far\n\
         stop    kill the shards and release the mounts\n\
         \n\
         environment:\n\
         \x20 XFSTESTS_TIMEOUT        seconds per test, default 300\n\
         \x20 XFSTESTS_MKFS_OPTIONS   passed to mkfs.beamfs, default -N 16384\n\
         \x20 XFSTESTS_NODES          name:host:test_dev:scratch_dev, comma separated\n\
         \x20 XFSTESTS_NO_RESUME      start over instead of resuming\n"
    );
    std::process::ExitCode::from(2)
}

fn run(cfg: &Config) -> std::process::ExitCode {
    // Opened before anything is touched, so a run that dies during
    // preflight still says why. Thirty-minute runs leaving eight-line
    // logs is the specific thing this is here to stop.
    let mut jr = Journal::create(&std::env::temp_dir());
    println!();
    println!("  journal : {}", jr.path().display());
    println!();
    println!("  === PREFLIGHT ===");

    // Checked before launching, not discovered afterwards: a node
    // without its scratch device produces a shard of MOUNTFAIL that
    // reads exactly like a filesystem that cannot mount.
    let mut ready = Vec::new();
    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);
        match c.preflight() {
            Ok(info) => {
                let bad = info.contains("MISSING") || info.contains("=NO");
                println!("    {:<10} {}{}", n.name, info.trim(),
                         if bad { "   <-- unusable" } else { "" });
                jr.command(&n.name, "preflight", &info, !bad);
                if !bad {
                    ready.push(n);
                }
            }
            Err(e) => {
                println!("    {:<10} {e}", n.name);
                jr.command(&n.name, "preflight", &e.to_string(), false);
            }
        }
    }
    if ready.is_empty() {
        eprintln!("\n  no usable node, nothing to do\n");
        return std::process::ExitCode::FAILURE;
    }
    println!("    {} of {} nodes usable", ready.len(), cfg.nodes.len());

    println!();
    println!("  === LAUNCH ===");
    let tmp = std::env::temp_dir().join("xfs-runner.sh");
    if std::fs::write(&tmp, RUNNER).is_err() {
        eprintln!("    cannot stage the runner");
        return std::process::ExitCode::FAILURE;
    }

    let nshard = ready.len();
    for (idx, n) in ready.iter().enumerate() {
        let c = NodeConn::new(n, cfg);
        if let Err(e) = c.push(tmp.to_str().unwrap_or_default(), "/tmp/xfs-runner.sh") {
            println!("    {:<10} push failed: {e}", n.name);
            continue;
        }
        let cmd = format!(
            "chmod +x /tmp/xfs-runner.sh && \
             nohup /tmp/xfs-runner.sh {} {} {idx} {nshard} {} '{}' {} \
             > /tmp/xfs-shard.log 2>&1 & echo started",
            n.test_dev, n.scratch_dev,
            cfg.per_test_timeout.as_secs(), cfg.mkfs_options,
            u8::from(cfg.resume),
        );
        match c.run(&cmd, Duration::from_secs(30)) {
            Ok(o) => {
                println!("    {:<10} shard {idx}/{nshard}", n.name);
                jr.command(&n.name, &cmd, &o, true);
            }
            Err(e) => {
                println!("    {:<10} launch failed: {e}", n.name);
                jr.command(&n.name, &cmd, &e.to_string(), false);
            }
        }
    }

    println!();
    println!("  === RUNNING ===");
    let mut bar = Progress::new(SUITE_SIZE);
    let mut stalls: BTreeMap<String, StallDetector> = ready
        .iter()
        .map(|n| (n.name.clone(), StallDetector::new(STALL_LIMIT)))
        .collect();

    loop {
        let mut all = Vec::new();
        let mut per_node = Vec::new();
        let mut active = 0usize;

        for n in &ready {
            let c = NodeConn::new(n, cfg);
            let rs = c.results().unwrap_or_default();
            let count = rs.len();
            let blocked = c.blocked_tasks();
            let cur = c.current_test().unwrap_or_default();
            let done = c.is_done();

            if !done {
                active += 1;
            }
            if let Some(d) = stalls.get_mut(&n.name) {
                if d.update(count, blocked) && !done {
                    bar.clear();
                    println!("  >>> {} wedged: {count} tests done, {blocked} blocked, on {cur}",
                             n.name);
                    // Gathered before stopping anything: killing the
                    // shard first destroys the state that explains why
                    // it stopped.
                    let (stacks, dmesg, mounts) = c.stall_evidence();
                    jr.stall_evidence(&n.name, &cur, &stacks, &dmesg, &mounts);
                    for l in stacks.lines().take(10) {
                        println!("      {l}");
                    }
                    println!("      full evidence in {}", jr.path().display());
                    c.stop();
                }
            }
            per_node.push((n.name.clone(), cur, blocked));
            all.extend(rs);
        }

        let mut s = Summary::default();
        for r in &all {
            s.add(r);
        }
        bar.draw(s.attempted(),
                 (s.pass, s.fail, s.notrun, s.hang, s.mountfail),
                 &per_node);

        if active == 0 {
            break;
        }
        std::thread::sleep(POLL);
    }

    bar.clear();
    println!("  === DONE in {} min ===", bar.elapsed().as_secs() / 60);

    // Pulled once at the end rather than as they happen: a failing test
    // produces hundreds of lines of diff, and four nodes' worth
    // interleaved into a live terminal helps nobody.
    jr.section("FAILURE ARTIFACTS");
    let mut pulled = 0usize;
    for n in &ready {
        let c = NodeConn::new(n, cfg);
        for t in c.failure_list() {
            if let Some(log) = c.failure_log(&t) {
                jr.artifact(&t, &log);
                pulled += 1;
            }
        }
    }
    if pulled > 0 {
        println!("  {pulled} failure logs in {}", jr.artifacts().display());
    }
    println!();
    report(cfg)
}

fn stop(cfg: &Config) -> std::process::ExitCode {
    println!();
    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);
        c.stop();
        println!("  {:<10} stopped", n.name);
    }
    println!();
    std::process::ExitCode::SUCCESS
}

fn report(cfg: &Config) -> std::process::ExitCode {
    let mut all: Vec<TestResult> = Vec::new();
    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);
        all.extend(c.results().unwrap_or_default());
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let mut s = Summary::default();
    for r in &all {
        s.add(r);
    }

    println!("  === RESULT ===");
    println!("    attempted   {:>4} of {SUITE_SIZE}", s.attempted());
    println!("    passed      {:>4}", s.pass);
    println!("    failed      {:>4}", s.fail);
    println!("    not run     {:>4}   (features beamfs does not implement)", s.notrun);
    println!("    hung        {:>4}", s.hang);
    println!("    mount fail  {:>4}", s.mountfail);
    println!("    pass rate   {:>7.1}%  (of those that ran)", s.pass_rate());
    println!("    test time   {:>4} min", s.total_seconds / 60);

    let actionable: Vec<&TestResult> =
        all.iter().filter(|r| r.outcome.is_actionable()).collect();
    if !actionable.is_empty() {
        println!();
        println!("  === NEEDS ATTENTION ({}) ===", actionable.len());
        for r in &actionable {
            println!("    {:<16} {:<10} {:>4}s  {}",
                     r.name, r.outcome.as_str(), r.seconds, r.node);
        }
    }

    // Grouped, because the individual test numbers matter less than the
    // shape: eight tests declining for want of O_DIRECT is one design
    // decision, not eight problems.
    let notrun: Vec<&TestResult> =
        all.iter().filter(|r| r.outcome == Outcome::NotRun).collect();
    if !notrun.is_empty() {
        let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &notrun {
            *by_reason.entry(r.reason.trim()).or_default() += 1;
        }
        let mut v: Vec<_> = by_reason.into_iter().collect();
        v.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        println!();
        println!("  === NOT RUN, BY REASON ===");
        for (why, n) in v.iter().take(12) {
            println!("    {n:>3}  {}", if why.is_empty() { "(unstated)" } else { why });
        }
    }
    println!();

    if s.fail > 0 || s.hang > 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
