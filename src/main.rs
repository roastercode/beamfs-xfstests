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

mod wedge;
mod deploy;
mod bisect;
mod bpf;
mod bell;
mod volume;
mod evidence;
mod mem_trace;
mod trace_stack;
mod state;
mod stats;
mod bench;
mod indicator;
mod matrix;
mod analyse;
mod load;
mod trace;
mod archive;
mod runpack;
mod scenario;
mod config;
mod console;
mod history;
mod journal;
mod node;
mod nodestate;
mod probe;
mod recovery;
mod progress;
mod result;

use std::collections::BTreeMap;
use std::time::Duration;

use config::Config;
use console::ConsoleSet;
use history::History;
use journal::Journal;
use node::{NodeConn, ShardState};
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
    // --no-bell is taken out of the arguments before anything reads
    // them, so it works after any command without every command having
    // to know about it.
    let mut args: Vec<String> = std::env::args().collect();
    // Asked and answered before anything else: somebody wanting the
    // list wants it now, not after a node check.
    if args.iter().any(|a| a == "--probes") {
        let v = bpf::available();
        if v.is_empty() {
            eprintln!("no scripts under {}", bpf::script_root().display());
        } else {
            eprintln!("scripts under {}:", bpf::script_root().display());
            for n in v {
                eprintln!("  {n}");
            }
            eprintln!("\nXFSTESTS_BPF=<name> attaches one for the length of each test");
        }
        return std::process::ExitCode::SUCCESS;
    }

    let quiet = args.iter().any(|a| a == "--no-bell");
    args.retain(|a| a != "--no-bell");

    // Which commands are worth waiting for.
    //
    // report, history and compare read what is already on disk and
    // return in a moment; ringing after them would be noise. The rest
    // drive a node for minutes or hours and are the reason the bell
    // exists.
    let long = !matches!(
        args.get(1).map(String::as_str),
        // Commands that read or prepare rather than measure. None of
        // them is long enough to walk away from, so none of them rings.
        Some("report" | "history" | "compare" | "trend" | "deploy"
             | "--help" | "-h" | "stop")
    );

    let code = match args.get(1).map(String::as_str) {
        Some("report") => report(&cfg, args.get(2)),
        Some("probe") => do_probe(&cfg, args.get(2), args.get(3)),
        Some("history") => show_history(),
        Some("compare") => compare_runs(args.get(2), args.get(3)),
        Some("stop") => stop(&cfg),
        Some("trace") => do_trace(&cfg, args.get(2), args.get(3)),
        Some("analyse" | "analyze") => do_analyse(args.get(2)),
        Some("matrix") => do_matrix(&cfg, args.get(2), args.get(3)),
        Some("bench") => do_bench(&cfg, args.get(2), args.get(3)),
        Some("sweep") => do_sweep(&cfg, &args[2..]),
        Some("deploy") => do_deploy(&cfg, args.get(2)),
        Some("scenario") => {
            let Some(node) = cfg.nodes.first() else {
                eprintln!("no nodes configured");
                return std::process::ExitCode::FAILURE;
            };
            let blocks = args.get(2).and_then(|x| x.parse().ok()).unwrap_or(32);
            let keep = args.get(3).and_then(|x| x.parse().ok()).unwrap_or(4);
            match scenario::partial_write(&cfg, node, blocks, keep) {
                Ok(()) => return std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("  {e}");
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
        Some("trend") => {
            match args.get(2) {
                Some(t) => bisect::trend(t, 12),
                None => bisect::worst(20),
            }
            // Reading a log is not a campaign: nothing rings and
            // nothing waits for a key.
            return std::process::ExitCode::SUCCESS;
        }
        Some("baseline") => do_baseline(&cfg, args.get(2), args.get(3), args.get(4)),
        Some("--help" | "-h") => usage(),
        // A typo must not start a campaign. "analyses" for "analyse"
        // fell through to run, which tried the aarch64 cluster -- powered
        // off -- and would have run the suite for hours had it answered.
        // Only a bare invocation means run.
        None => run(&cfg),
        Some(other) => {
            eprintln!("beamfs-xfstests: unknown command '{other}'");
            eprintln!();
            usage()
        }
    };

    if long && !quiet {
        bell::ring_until_acknowledged();
    }
    code
}

fn usage() -> std::process::ExitCode {
    eprintln!(
        "usage: beamfs-xfstests [run|probe|report|history|compare|trace|analyse|matrix|bench|baseline|sweep|trend|deploy|stop]\n\
         \n\
         run      shard the suite across the nodes and follow it (default)\n\
         probe    run one test with console capture and sampling\n\
         report   summarise what the nodes have recorded so far\n\
         history  list saved runs\n\
         compare  diff two runs; last two if unnamed\n\
         trace    reproduce the block leak under load and keep the trace\n\
         analyse  read a capture and say what happened to the lost blocks\n\
         matrix   vary one condition at a time and see which the leak needs\n\
         bench    measure a test, a group or the whole suite, and compare\n\
         baseline run the same code several times and report the spread\n\
         sweep    run every test of a selection once, one verdict each\n\
                  sweep [selection]   (default: the whole suite)\n\
         trend    what a test has lost lately, or which tests lose most\n\
                  trend [test]        (no test: the worst first)\n\
         deploy   put the newest image and this repo's tools on a node,\n\
                  and prove they arrived\n\
                  deploy [node]       (default: the first configured)\n\
         stop     kill the shards and release the mounts\n\
         \n\
         options:\n\
           --no-bell               finish without ringing\n\
           --probes                list the bpftrace scripts and exit\n\
         \n\
         environment:\n\
         \x20 XFSTESTS_TIMEOUT        seconds per test, default 300\n\
         \x20 XFSTESTS_MKFS_OPTIONS   passed to mkfs.beamfs, default -N 16384\n\
         \x20 XFSTESTS_FSTYP          filesystem to test, default beamfs\n\
         \x20 XFSTESTS_NODES          name:host:test_dev:scratch_dev, comma separated\n\
         \x20 XFSTESTS_NO_RESUME      start over instead of resuming\n\
         \x20 BEAMFS_NO_BELL          same as --no-bell, for a whole shell\n\
         \x20 XFSTESTS_BPF            a script from --probes, attached per test\n\
         \x20 XFSTESTS_BPF_SCRIPTS    where to look for them\n"
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
        // The watcher goes over first: the runner starts it, and a
        // shard that runs without one leaves the same blind spot every
        // failure this week was found in.
        let watch = std::env::temp_dir().join("xfs-watch.sh");
        let _ = std::fs::write(&watch, include_str!("watch.sh"));
        let _ = c.push(watch.to_str().unwrap_or_default(), "/tmp/xfs-watch.sh");
        let _ = c.run("chmod +x /tmp/xfs-watch.sh", Duration::from_secs(20));

        // Kill whatever is there before starting, always.
        //
        // Recovery restarts a shard after a kill it believes worked. It
        // does not always: a campaign ended up with three runners on one
        // node, all writing the same /tmp/xfs-results.txt and all
        // running tests against the same scratch device. The counters
        // disagreed with each other and with the node, and no result
        // from that run means anything.
        //
        // stop() is cheap and idempotent. Nothing is gained by asking
        // first whether it is needed, and a campaign was lost by
        // assuming it was not.
        c.stop();

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

    // Consoles first, and for every node, not just the one being
    // watched. A kernel that panics prints to the console and stops;
    // nothing reaches dmesg because there is no machine left to read
    // it. Opening the console after noticing the silence returns
    // nothing, which is how two nodes died overnight unexplained.
    let mut consoles = ConsoleSet::new(&std::env::temp_dir().join("beamfs-consoles"));
    println!();
    println!("  === CONSOLES ===");
    for n in &ready {
        let ok = consoles.start(&n.name, &format!("beamfs-{}", n.name));
        println!("    {:<10} {}", n.name,
                 if ok { "capturing" } else { "unavailable" });
    }

    println!();
    println!("  === RUNNING ===");
    let mut bar = Progress::new(SUITE_SIZE);
    let mut stalls: BTreeMap<String, StallDetector> = ready
        .iter()
        .map(|n| (n.name.clone(), StallDetector::new(STALL_LIMIT)))
        .collect();
    // Recoveries attempted per node. A node that has been through it
    // several times gets a shorter escalation, and one that has been
    // through it many times is written off rather than relaunched into
    // the same state for the rest of the run.
    let mut attempts: BTreeMap<String, u32> = BTreeMap::new();
    let mut written_off: BTreeMap<String, bool> = BTreeMap::new();
    // Failure logs already pulled, so each is fetched once.
    let mut have_log: BTreeMap<String, bool> = BTreeMap::new();

    // One file per verdict, written as it lands.
    //
    // A campaign runs seven to ten hours and everything it established
    // used to live only in memory until the end. A power cut at hour
    // six threw away six hours of verdicts that were never in doubt --
    // the tests had passed, the run had simply not finished.
    //
    // Records go under a directory named for the commit under test, so
    // two revisions never share a shelf and no table can end up mixing
    // them. A result without the revision that produced it is not
    // something a reviewer can check.
    let repo = std::env::var("BEAMFS_REPO").unwrap_or_else(|_| {
        format!("{}/git/beamfs", std::env::var("HOME").unwrap_or_default())
    });
    let arch_root = std::env::var("BEAMFS_ARCHIVE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| archive::Archive::default_root());
    let mut arch = match archive::Archive::open(
        &arch_root,
        std::path::Path::new(&repo),
    ) {
        Ok(a) => {
            println!("  archive : {} ({} already recorded)",
                     a.root().display(), a.known());
            Some(a)
        }
        Err(e) => {
            // Not fatal. A campaign that cannot shelve its verdicts is
            // still a campaign worth running; it just loses what it
            // proved if the power goes.
            println!("  archive unavailable: {e}");
            None
        }
    };

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
            let written = c.sectors_written();
            let cur = c.current_test().unwrap_or_default();
            let state = c.shard_state();

            // Unreachable and Stuck both need recovery; only Done is
            // finished. Counting Unreachable as working is what kept
            // the loop alive for hours after three shards had ended.
            let needs_recovery = matches!(
                state, ShardState::Unreachable | ShardState::Stuck);
            if state != ShardState::Done {
                active += 1;
            }

            if let Some(d) = stalls.get_mut(&n.name) {
                // Hangs excluded from progress: a node timing out every
                // test advances its count forever without achieving
                // anything, which kept a wedged node alive in the
                // detector's eyes all night.
                let stalled = d.update_with_hangs_io(count, hangs, blocked,
                                                     written);
                if written_off.get(&n.name).copied().unwrap_or(false) {
                    // Already given up on. Counted as finished so the
                    // loop can end, and left alone.
                    active = active.saturating_sub(1);
                } else if (stalled || needs_recovery) && state != ShardState::Done {
                    bar.clear();
                    println!("  >>> {} {:?}: {count} done, {blocked} blocked, on {cur}",
                             n.name, state);
                    // The console already holds the answer if the
                    // kernel died. Read it before touching anything:
                    // restarting the domain takes the pty with it.
                    if let Some(p) = consoles.panic_of(&n.name) {
                        println!("      KERNEL PANIC on {}:", n.name);
                        for l in p.lines().take(14) {
                            println!("        {l}");
                        }
                        jr.section(&format!("PANIC on {}", n.name));
                        for l in p.lines() {
                            jr.line(l);
                        }
                    } else {
                        let t = consoles.tail(&n.name, 12);
                        if !t.trim().is_empty() {
                            jr.section(&format!("CONSOLE TAIL {}", n.name));
                            for l in t.lines() {
                                jr.line(l);
                            }
                        }
                    }

                    // Recover rather than record and move on. A node
                    // left wedged is a quarter of the run silently
                    // stopped, which is how two nodes sat at "No route
                    // to host" for an hour on 2026-09-01 without anyone
                    // noticing.
                    let rec = Recovery::new(cfg);
                    let dom = rec.domain_for(&n.name);
                    // The pty dies with the domain, so stop reading it
                    // first and start again once it is back.
                    consoles.stop(&n.name);
                    let tries = attempts.entry(n.name.clone()).or_insert(0);
                    *tries += 1;
                    let this_try = *tries;

                    let outcome = rec.recover(&c, &dom, this_try - 1, &mut jr);
                    println!("      recovery: {} (attempt {this_try})",
                             outcome.as_str());
                    consoles.start(&n.name, &dom);

                    // Six recoveries and still stalling: the node is not
                    // going to finish, and relaunching it produces
                    // results from a machine in a state nobody would
                    // trust. One node did that for a whole run and
                    // returned 345 results for a 185-test shard.
                    if this_try >= 6 {
                        println!("      giving up on {} after {this_try} recoveries",
                                 n.name);
                        jr.line(&format!("{}: written off after {this_try} recoveries",
                                         n.name));
                        written_off.insert(n.name.clone(), true);
                        c.stop();
                        continue;
                    }
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
                        // Same rule as the initial launch: kill first,
                        // unconditionally. This path runs after a kill
                        // that was believed to have worked, which is
                        // exactly how one node ended up with three
                        // runners writing the same results file.
                        c.stop();
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
            // Pull each failure log as it appears, not at the end.
            //
            // Waiting cost this run its evidence: a node was restarted a
            // hundred and fifty times and its /tmp went with it every
            // time, so the logs recovered afterwards were whatever the
            // last incarnation happened to leave. A power cut or a
            // panicking node would have taken the lot the same way.
            //
            // One extra round trip per new failure, against seven and a
            // half hours of findings living nowhere but on the machines
            // producing them.
            for r in rs.iter().filter(|r| r.outcome.is_actionable()) {
                if have_log.contains_key(&r.name) {
                    continue;
                }
                have_log.insert(r.name.clone(), true);
                if let Some(body) = c.failure_log(&r.name) {
                    jr.artifact(&r.name, &body);
                }
            }

            // Shelve every verdict now, failures with their log
            // attached. A passing test needs nothing beyond the fact
            // that it passed.
            if let Some(a) = arch.as_mut() {
                for r in rs.iter() {
                    let log = if r.outcome.is_actionable() {
                        c.failure_log(&r.name)
                    } else {
                        None
                    };
                    a.record(r, log.as_deref());
                }
            }

            per_node.push((n.name.clone(), cur, blocked));
            all.extend(rs);
        }

        let mut s = Summary::default();
        for r in &all {
            s.add(r);
        }
        // The results file too: it is the only record of what ran, and
        // it lives on a machine that may not survive the night.
        for n in &ready {
            let c = NodeConn::new(n, cfg);
            if let Ok(body) = c.run("cat /tmp/xfs-results.txt 2>/dev/null",
                                    Duration::from_secs(30)) {
                if !body.trim().is_empty() {
                    let p = jr.artifacts().join(format!("results-{}.txt", n.name));
                    let _ = std::fs::write(p, body);
                }
            }
            // And the watcher's log, which is the only account of what
            // the node was doing between polls.
            if let Ok(body) = c.run("cat /tmp/xfs-watch.log 2>/dev/null",
                                    Duration::from_secs(45)) {
                if !body.trim().is_empty() {
                    let p = jr.artifacts().join(format!("watch-{}.log", n.name));
                    let _ = std::fs::write(p, body);
                }
            }
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
    consoles.stop_all();
    println!("  === DONE in {} min ===", bar.elapsed().as_secs() / 60);

    // The tally that matters is the one on disk. What the run holds in
    // memory disappears with the run; what is shelved under the commit
    // is what can still be shown tomorrow, and after a power cut it is
    // the only thing left.
    if let Some(a) = arch.as_ref() {
        let t = a.tally();
        let total: usize = t.iter().map(|(_, n)| n).sum();
        println!("  archived under {} : {total} verdicts", a.commit());
        for (outcome, n) in t {
            println!("    {outcome:<10} {n}");
        }
        println!("  {}", a.root().display());
    }
    for n in &ready {
        if let Some(p) = consoles.path_of(&n.name) {
            let sz = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            println!("    console {:<10} {sz} bytes  {}", n.name, p.display());
        }
    }

    // Pulled once at the end rather than as they happen: a failing test
    // produces hundreds of lines of diff, and four nodes' worth
    // interleaved into a live terminal helps nobody.
    // A last sweep for anything the loop did not see -- a failure
    // recorded between the final poll and the shard finishing.
    jr.section("FAILURE ARTIFACTS");
    let mut pulled = have_log.len();
    for n in &ready {
        let c = NodeConn::new(n, cfg);
        for t in c.failure_list() {
            if have_log.contains_key(&t) {
                continue;
            }
            if let Some(log) = c.failure_log(&t) {
                jr.artifact(&t, &log);
                have_log.insert(t, true);
                pulled += 1;
            }
        }
    }
    if pulled > 0 {
        println!("  {pulled} failure logs in {}", jr.artifacts().display());
    }

    // Everything the run produced, in one archive.
    //
    // A campaign leaves a journal, four console logs, a failure log per
    // failing test and the results themselves -- scattered across two
    // directories and four machines, and only useful together. Probe
    // has done this since it was written; run had not, so seven and a
    // half hours of evidence needed collecting by hand afterwards.
    {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stage = std::env::temp_dir().join(format!("xfstests-run-{stamp}"));
        let _ = std::fs::create_dir_all(&stage);

        // The per-node results, which live nowhere else once the VMs
        // are redeployed.
        for n in &ready {
            let c = NodeConn::new(n, cfg);
            if let Ok(body) = c.run("cat /tmp/xfs-results.txt 2>/dev/null",
                                    Duration::from_secs(60)) {
                let _ = std::fs::write(stage.join(format!("results-{}.txt", n.name)),
                                       body);
            }
        }
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cp -r {} {} {}/ 2>/dev/null;                  cp {}/console-*.log {}/ 2>/dev/null; true",
                jr.path().display(), jr.artifacts().display(),
                stage.display(),
                std::env::temp_dir().join("beamfs-consoles").display(),
                stage.display()))
            .output();

        let dest = std::env::temp_dir().join(format!("xfstests-run-{stamp}.tar.gz"));
        let ok = std::process::Command::new("tar")
            .arg("czf").arg(&dest)
            .arg("-C").arg(std::env::temp_dir())
            .arg(format!("xfstests-run-{stamp}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            let sum = std::process::Command::new("sha256sum")
                .arg(&dest).output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout)
                     .split_whitespace().next().unwrap_or("").to_string())
                .unwrap_or_default();
            println!();
            println!("  archive : {}", dest.display());
            println!("  size    : {size} bytes");
            if !sum.is_empty() {
                println!("  sha256  : {sum}");
            }
        }
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
    report(cfg, None)
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
            // First recovery for this node: the full escalation.
            let outcome = r.recover(&c, &r.domain_for(&node), 0, &mut jr);
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

fn report(cfg: &Config, tag: Option<&String>) -> std::process::ExitCode {
    let mut all: Vec<TestResult> = Vec::new();
    // Named, a run is read back from history; unnamed, report asks the
    // nodes what they still hold. A tag that matches no saved run is an
    // error and not an empty report: an empty report reads like a run
    // that found nothing, which is the one thing it never means. That
    // reading cost an hour -- "report <tag>" printed 0 of 737 and the
    // tag was simply ignored.
    if let Some(t) = tag {
        let h = History::new(&History::default_root());
        match h.load(t) {
            Some(rs) => all.extend(rs),
            None => {
                println!();
                println!("  no saved run named {t}");
                println!("  beamfs-xfstests history lists the saved runs");
                println!();
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        for n in &cfg.nodes {
            let c = NodeConn::new(n, cfg);
            all.extend(c.results().unwrap_or_default());
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let mut s = Summary::default();
    for r in &all {
        s.add(r);
    }

    match tag {
        Some(t) => println!("  === RESULT ({t}) ==="),
        None => println!("  === RESULT ==="),
    }
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

/// Reproduce the block leak under load and keep what it produces.
///
/// `trace [hours] [max]` -- defaults to one hour, ten captures. Each
/// capture is a directory under ~/.local/share/beamfs-xfstests/traces
/// holding the ring at the moment fsck found a lost block, the block
/// list, and the inode table location needed to read those inodes off
/// the device afterwards.
fn do_trace(cfg: &Config, hours: Option<&String>, max: Option<&String>) -> std::process::ExitCode {
    let h: f64 = hours.and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let m: u32 = max.and_then(|v| v.parse().ok()).unwrap_or(10);

    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };

    println!("  node    : {}", node.name);
    println!("  budget  : {h} h, up to {m} captures");
    println!("  keeping : {}", trace::default_root().display());
    println!();

    match trace::campaign(cfg, node, h, m) {
        Ok(caught) => {
            if caught.is_empty() {
                println!("  no leak reproduced");
            } else {
                println!();
                println!("  === captures ===");
                for c in &caught {
                    println!(
                        "    {:03}  {:>5} blocks  loop {:<3}  {:>8} events  {}",
                        c.seq,
                        c.lost,
                        c.loop_no,
                        c.events,
                        c.dir.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("trace: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Read a capture and say what happened to the blocks that went missing.
///
/// `analyse` with no argument takes the three newest captures; with a
/// directory, that one. The questions it answers -- which inode owned a
/// lost block, which parent holds its pointer, whether the pointer went
/// in before or after the inode was last written -- are the same every
/// time, and were being asked by hand.
fn do_analyse(arg: Option<&String>) -> std::process::ExitCode {
    let r = match arg {
        Some(p) => analyse::analyse(std::path::Path::new(p)).map(|r| {
            r.print(std::path::Path::new(p));
        }),
        None => analyse::analyse_all(&trace::default_root(), 3),
    };
    match r {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("analyse: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Vary one condition at a time and see which ones the leak needs.
///
/// `matrix [conditions] [loops]` -- all conditions and twelve loops by
/// default. Conditions are comma-separated names; the manual lists them.
fn do_matrix(cfg: &Config, which: Option<&String>, loops: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    let n: u32 = loops.and_then(|v| v.parse().ok()).unwrap_or(12);
    println!("  node    : {}", node.name);
    match matrix::run(cfg, node, which.map(|s| s.as_str()), n) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("matrix: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Measure one test's pass rate and compare it against the previous run.
///
/// `bench [test] [trials]` -- generic/464 and ten trials by default.
/// A single run of an intermittent test says nothing about a change;
/// the comparison with the last stored run is the point.
fn do_bench(cfg: &Config, test: Option<&String>, trials: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    // Anything ./check accepts: "generic/464", "generic/464 generic/589",
    // "-g auto", or "all" for the whole suite.
    let t = match test.map(|s| s.as_str()) {
        Some("all") | Some("") => "",
        Some(x) => x,
        None => "generic/464",
    };
    let n: u32 = trials.and_then(|v| v.parse().ok()).unwrap_or(10);
    match bench::run(cfg, node, t, n) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bench: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Run the same code several times and report the spread.
///
/// `baseline [test] [trials] [rounds]` -- generic/464, ten trials,
/// three rounds by default. Nothing changes between rounds, so the
/// difference between them is what the measurement does on its own.
fn do_baseline(cfg: &Config, test: Option<&String>, trials: Option<&String>,
               rounds: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    let t = test.map(|s| s.as_str()).unwrap_or("generic/464");
    let n: u32 = trials.and_then(|v| v.parse().ok()).unwrap_or(10);
    let k: u32 = rounds.and_then(|v| v.parse().ok()).unwrap_or(3);
    match bench::baseline(cfg, node, t, n, k) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("baseline: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Run a whole selection, one verdict per test.
///
/// `sweep` with no argument is the entire suite. The difference from
/// `bench` is that bench measures one test repeatedly to get a rate,
/// and sweep runs many tests once to find out which of them fail --
/// two different questions that were being asked with one command, and
/// answered badly for both.
/// Put the newest image and this repo's tools on a node.
///
/// A shell block written fresh on every cycle checked none of its own
/// transfers. An rsync failed silently on 2026-09-13, the image's own
/// fsck.beamfs -- 30840 bytes, dated 2011 -- answered in place of the
/// 857568 built here, and a campaign reported 306 destroyed inodes on
/// a volume that was sound.
fn do_deploy(cfg: &Config, which: Option<&String>) -> std::process::ExitCode {
    let Some(node) = (match which.map(|s| s.as_str()) {
        Some(n) => cfg.nodes.iter().find(|x| x.name == n),
        None => cfg.nodes.first(),
    }) else {
        eprintln!("beamfs-xfstests: no such node");
        return std::process::ExitCode::FAILURE;
    };
    // The domain is named for the node, as recovery names it.
    let domain = format!("beamfs-{}", node.name);
    match deploy::deploy(cfg, node, &domain) {
        Ok(()) => {
            println!();
            println!("  the node is ready");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!();
            eprintln!("  {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn do_sweep(cfg: &Config, selection: &[String]) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    // Every argument, not just the first.
    //
    // This read args.get(2) and dropped the rest in silence. On
    // 2026-09-18 `sweep generic/075 generic/083 generic/241
    // generic/589` ran generic/075 alone and announced "tests : 1 to
    // run" -- which reads, in a terminal, exactly like a selection
    // that was honoured, and three of the four failures under
    // investigation went unmeasured.
    //
    // ./check takes a space-separated list and enumerate_tests passes
    // the selection through untouched, so joining the arguments is the
    // whole fix.
    let joined = selection.join(" ");
    let sel = match joined.trim() {
        "all" | "" => "",
        x => x,
    };
    match bench::sweep(cfg, node, sel) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sweep: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
