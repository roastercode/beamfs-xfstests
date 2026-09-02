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
mod history;
mod journal;
mod node;
mod probe;
mod recovery;
mod progress;
mod result;

use std::collections::BTreeMap;
use std::time::Duration;

use config::Config;
use history::History;
use journal::Journal;
use node::NodeConn;
use probe::Probe;
use recovery::Recovery;
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
        Some("probe") => do_probe(&cfg, args.get(2), args.get(3)),
        Some("history") => show_history(),
        Some("compare") => compare_runs(args.get(2), args.get(3)),
        Some("stop") => stop(&cfg),
        Some("--help" | "-h") => usage(),
        _ => run(&cfg),
    }
}

fn usage() -> std::process::ExitCode {
    eprintln!(
        "usage: beamfs-xfstests [run|probe|report|history|compare|stop]\n\
         \n\
         run      shard the suite across the nodes and follow it (default)\n\
         probe    run one test with console capture and sampling\n\
         report   summarise what the nodes have recorded so far\n\
         history  list saved runs\n\
         compare  diff two runs; last two if unnamed\n\
         stop     kill the shards and release the mounts\n\
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
        // setsid and all three descriptors redirected, or ssh waits
        // for the shard to finish: nohup detaches from the terminal but
        // leaves stdout attached to the connection, so the session stays
        // open for the whole run and the launch times out while the
        // shard is in fact running perfectly well.
        let cmd = format!(
            "chmod +x /tmp/xfs-runner.sh && \
             setsid /tmp/xfs-runner.sh {} {} {idx} {nshard} {} '{}' {} \
             < /dev/null > /tmp/xfs-shard.log 2>&1 & \
             sleep 1; pgrep -f xfs-runner.sh >/dev/null && echo started || echo failed",
            n.test_dev, n.scratch_dev,
            cfg.per_test_timeout.as_secs(), cfg.mkfs_options,
            u8::from(cfg.resume),
        );
        // A launch command that ends in a sleep keeps the ssh session
        // open past a short deadline, so a timeout here says nothing
        // about whether the shard started. Asking is the only way to
        // know, and reporting failure without asking is how four
        // healthy shards were declared dead while they ran all night.
        match c.run(&cmd, Duration::from_secs(60)) {
            Ok(o) => {
                println!("    {:<10} shard {idx}/{nshard}", n.name);
                jr.command(&n.name, &cmd, &o, true);
            }
            Err(e) => {
                jr.command(&n.name, &cmd, &e.to_string(), false);
                let up = c
                    .run("pgrep -f xfs-runner.sh > /dev/null && echo yes || echo no",
                         Duration::from_secs(20))
                    .map(|o| o.contains("yes"))
                    .unwrap_or(false);
                if up {
                    println!("    {:<10} shard {idx}/{nshard} (launch call timed out, running)",
                             n.name);
                } else {
                    println!("    {:<10} launch failed: {e}", n.name);
                }
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
            let hangs = rs.iter().filter(|r| r.outcome == Outcome::Hang).count();
            let blocked = c.blocked_tasks();
            let cur = c.current_test().unwrap_or_default();
            let done = c.is_done();

            if !done {
                active += 1;
            }
            if let Some(d) = stalls.get_mut(&n.name) {
                // Hangs excluded from progress: a node timing out every
                // test advances its count forever without achieving
                // anything, which kept a wedged node alive in the
                // detector's eyes all night.
                if d.update_with_hangs(count, hangs, blocked) && !done {
                    bar.clear();
                    println!("  >>> {} wedged: {count} done, {blocked} blocked, on {cur}",
                             n.name);
                    // Recover rather than record and move on. A node
                    // left wedged is a quarter of the run silently
                    // stopped, which is how two nodes sat at "No route
                    // to host" for an hour on 2026-09-01 without anyone
                    // noticing.
                    let rec = Recovery::new(cfg);
                    let dom = rec.domain_for(&n.name);
                    let outcome = rec.recover(&c, &dom, &mut jr);
                    println!("      recovery: {}", outcome.as_str());
                    if outcome.usable() {
                        // Redeploy first: a restarted domain comes back
                        // with a fresh filesystem and no runner on it.
                        // Relaunching without this fails silently and
                        // the node sits idle for the rest of the run.
                        let _ = c.push(tmp.to_str().unwrap_or_default(),
                                       "/tmp/xfs-runner.sh");
                        let cmd = format!(
                            "chmod +x /tmp/xfs-runner.sh; \
                             setsid /tmp/xfs-runner.sh {} {} {} {} {} '{}' 1 \
                             < /dev/null > /tmp/xfs-shard.log 2>&1 & \
                             sleep 1; echo restarted",
                            n.test_dev, n.scratch_dev,
                            ready.iter().position(|x| x.name == n.name).unwrap_or(0),
                            ready.len(),
                            cfg.per_test_timeout.as_secs(), cfg.mkfs_options);
                        match c.run(&cmd, Duration::from_secs(40)) {
                            Ok(_) => println!("      shard restarted"),
                            Err(e) => println!("      restart failed: {e}"),
                        }
                        // Fresh detector, or the next poll sees the same
                        // count it saw before the recovery and declares
                        // the node wedged again immediately.
                        *d = StallDetector::new(STALL_LIMIT);
                    }
                    println!("      evidence: {}", jr.path().display());
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

    // Saved before the report, and compared against the last run.
    // Absolute counts say little; the delta is what a fix is judged on.
    let hist = History::new(&History::default_root());
    let mut all: Vec<result::TestResult> = Vec::new();
    for n in &ready {
        all.extend(NodeConn::new(n, cfg).results().unwrap_or_default());
    }
    let tag = run_tag();
    match hist.save(&tag, &all) {
        Ok(p) => println!("  results saved as {tag} in {}", p.display()),
        Err(e) => println!("  could not save results: {e}"),
    }
    if let Some(base) = hist.baseline_for(&tag) {
        if let Some(prev) = hist.load(&base) {
            let d = History::compare(&prev, &all);
            println!();
            println!("  === AGAINST {base} ===");
            println!("    unchanged {:>4}", d.unchanged);
            println!("    fixed     {:>4}", d.fixed.len());
            println!("    new       {:>4}", d.new.len());
            println!("    missing   {:>4}", d.missing.len());
            println!("    REGRESSED {:>4}", d.regressed.len());
            for (t, o) in d.regressed.iter().take(20) {
                println!("      {t:<16} passed before, now {}", o.as_str());
            }
            for t in d.fixed.iter().take(10) {
                println!("      {t:<16} fixed");
            }
        }
    }
    println!();
    report(cfg)
}

/// A sortable tag for this run: comparisons rely on lexical order.
fn run_tag() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{s:012}")
}

/// Run one test with everything watched.
///
/// For a test that is known to misbehave: the run subcommand records
/// what happened, this records what the machine was doing while it did.
fn do_probe(cfg: &Config, test: Option<&String>, node: Option<&String>)
    -> std::process::ExitCode
{
    let Some(test) = test else {
        eprintln!("\n  usage: beamfs-xfstests probe generic/NNN [node]\n");
        return std::process::ExitCode::from(2);
    };
    let node = node.map_or_else(
        || cfg.nodes.first().map_or_else(String::new, |n| n.name.clone()),
        Clone::clone,
    );
    let out = std::env::temp_dir().join("beamfs-probe");
    let mut jr = Journal::create(&out);
    println!();
    println!("  probing {test} on {node}");
    println!("  journal : {}", jr.path().display());
    println!();

    // Configurable, because 300 was short enough to class slow tests
    // as hangs: generic/027 needs more than 900 seconds under TCG and
    // is not stuck while it takes them.
    let secs = std::env::var("XFSTESTS_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let p = Probe::new(cfg, &out);
    let end = p.run(&node, test, Duration::from_secs(secs), &mut jr);

    // The status line is written with \r and no newline; anything
    // printed after it without clearing walks across the terminal.
    print!("\r{:100}\r", " ");
    println!("  outcome : {end:?}");

    // A lost node is left recovered, not left dead. The next command
    // should not have to start with a reboot.
    if end == probe::ProbeEnd::NodeLost {
        println!();
        println!("  === RECOVERY ===");
        if let Some(n) = cfg.nodes.iter().find(|n| n.name == node) {
            let c = NodeConn::new(n, cfg);
            let r = Recovery::new(cfg);
            let outcome = r.recover(&c, &r.domain_for(&node), &mut jr);
            println!("  {}", outcome.as_str());
        }
    }
    println!();
    println!("  artefacts in {}", out.display());
    println!();
    std::process::ExitCode::SUCCESS
}

fn show_history() -> std::process::ExitCode {
    let h = History::new(&History::default_root());
    let runs = h.runs();
    println!();
    if runs.is_empty() {
        println!("  no saved runs in {}", History::default_root().display());
        return std::process::ExitCode::SUCCESS;
    }
    println!("  === SAVED RUNS ===");
    for tag in runs.iter().take(20) {
        if let Some(rs) = h.load(tag) {
            let mut s = result::Summary::default();
            for r in &rs {
                s.add(r);
            }
            println!("    {tag}  {:>4} tests  {:>4} pass  {:>3} fail  {:>3} hang",
                     s.attempted(), s.pass, s.fail, s.hang);
        }
    }
    println!();
    std::process::ExitCode::SUCCESS
}

fn compare_runs(a: Option<&String>, b: Option<&String>) -> std::process::ExitCode {
    let h = History::new(&History::default_root());
    let runs = h.runs();
    let (base, cur) = match (a, b) {
        (Some(x), Some(y)) => (x.clone(), y.clone()),
        (Some(x), None) => (x.clone(), runs.first().cloned().unwrap_or_default()),
        _ => {
            if runs.len() < 2 {
                eprintln!("
  need two runs to compare
");
                return std::process::ExitCode::from(2);
            }
            (runs[1].clone(), runs[0].clone())
        }
    };
    let (Some(p), Some(c)) = (h.load(&base), h.load(&cur)) else {
        eprintln!("
  cannot load {base} or {cur}
");
        return std::process::ExitCode::from(2);
    };
    let d = History::compare(&p, &c);
    println!();
    println!("  === {base} -> {cur} ===");
    println!("    unchanged {:>4}", d.unchanged);
    println!("    fixed     {:>4}", d.fixed.len());
    println!("    new       {:>4}", d.new.len());
    println!("    missing   {:>4}", d.missing.len());
    println!("    REGRESSED {:>4}", d.regressed.len());
    for (t, o) in &d.regressed {
        println!("      {t:<16} was PASS, now {}", o.as_str());
    }
    println!();
    if d.is_clean() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
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
