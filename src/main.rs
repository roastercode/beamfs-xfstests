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

mod select;
mod checkpoint;
mod nodelock;
mod say;
mod wedge;
mod lab;
mod chain;
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
mod soak;
mod config;
mod console;
mod history;
mod journal;
mod node;
mod nodes;
mod nodestate;
mod probe;
mod recovery;
mod clean;
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

/// One lock for every test that writes to the process environment.
///
/// set_var changes the whole process, not the test that calls it. Eight
/// tests across five modules do it, cargo runs them on as many threads
/// as the machine has, and one of them read XFSTESTS_FSTYP as "ext2"
/// while another was halfway through setting it: cargo test failed
/// about one run in three, always on a different test, with nothing
/// wrong in the code it was testing. A suite that fails at random is a
/// suite nobody can use to decide anything.
#[cfg(test)]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn main() -> std::process::ExitCode {
    // Settings from a file before anything reads the environment,
    // so a campaign is a command and not a paragraph of exports.
    clean::load_config_file();

    // Which chain this run belongs to, before anything reads a path.
    //
    // The accessors in lab.rs memoise on first call, so the machine
    // has to be settled here or a later --machine would be read after
    // the paths it was meant to change. Two architectures are two
    // chains, run together or one without the other; this is how one
    // is named without exporting a variable for the whole shell.
    {
        let a: Vec<String> = std::env::args().collect();
        let picked = a.iter().position(|x| x == "--machine")
            .and_then(|i| a.get(i + 1).cloned())
            .or_else(|| a.iter().find_map(|x| x.strip_prefix("--machine=").map(str::to_string)));
        if let Some(m) = picked {
            let m = m.trim();
            if m.is_empty() {
                eprintln!("--machine needs a value, for instance qemux86-64 or qemuarm64");
                return std::process::ExitCode::FAILURE;
            }
            unsafe { std::env::set_var("XFSTESTS_MACHINE", m) };
        }
    }

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
    if let Some(i) = args.iter().position(|a| a == "--machine") {
        args.drain(i..=(i + 1).min(args.len() - 1));
    }
    args.retain(|a| !a.starts_with("--machine="));

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
             | "nodes" | "checkpoint" | "--help" | "-h" | "stop")
    );

    /*
     * Asking for help never acts, whatever its position.
     *
     * --help was matched on args[1] only, so a subcommand took
     * it for its own argument: 'probe --help' probed a node
     * called --help, found none, fell back to the default one,
     * booted it and ran its recovery; 'stop --help' killed a
     * live campaign. A tool that acts when asked what it does
     * cannot be explored safely.
     */
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        return usage();
    }

    let code = match args.get(1).map(String::as_str) {
        Some("report") => report(&cfg, args.get(2)),
        Some("probe") => do_probe(&cfg, args.get(2), args.get(3)),
        Some("history") => show_history(),
        Some("compare") => compare_runs(args.get(2), args.get(3)),
        Some("stop") => {
            let hard = args.iter().any(|a| a == "--hard");
            stop(&cfg, hard)
        }
        Some("trace") => do_trace(&cfg, args.get(2), args.get(3)),
        Some("analyse" | "analyze") => do_analyse(args.get(2)),
        Some("matrix") => do_matrix(&cfg, args.get(2), args.get(3)),
        Some("bench") => do_bench(&cfg, args.get(2), args.get(3)),
        Some("control") => do_control(&cfg, args.get(2), args.get(3), args.get(4)),
        Some("soak") => do_soak(&cfg, args.get(2)),
        Some("sweep") => do_sweep(&cfg, &args[2..]),
        Some("deploy") => do_deploy(&cfg, args.get(2)),
        Some("nodes") if args.get(2).map(String::as_str) == Some("exec") =>
            nodes::exec(&cfg, &args[3..]),
        Some("nodes") => nodes::status(&cfg, args.get(2)),
        Some("checkpoint") => {
            checkpoint::report(&cfg, cfg.nodes.first());
            std::process::ExitCode::SUCCESS
        }
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
        /* The usage text lists run as a command, so it is one. */
        Some("run") => run(&cfg),
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
        "usage: beamfs-xfstests [run|probe|report|history|compare|trace|analyse|matrix|bench|baseline|sweep|trend|nodes|deploy|stop]\n\
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
         control  run one test against beamfs and a sound filesystem,\n\
         \x20        same devices, and say which is implicated\n\
         \x20        control [test] [trials] [fstyp,fstyp]\n\
         sweep    run every test of a selection once, one verdict each:\n\
         \x20        passed, failed, not run by xfstests (with its reason)\n\
         \x20        or no verdict; each sweep kept whole in sweeps/<tag>/\n\
         soak     random writes on the bare scratch device, read back now and later
                  sweep [selection]   (default: the auto group, as ./check runs bare)\n\
                  a selection is written, not looped over:\n\
                    generic/013            one test\n\
                    generic/001-014        a range\n\
                    generic/074,075,083    a list\n\
                    @leaks                 every test that ever leaked\n\
                    @leaks:N               ... in its last N records\n\
                    @seen                  every test the history knows\n\
         trend    what a test has lost lately, or which tests lose most\n\
                  trend [test]        (no test: the worst first)\n\
         checkpoint say whether this repository, the layer, the image\n\
                  and the node still describe the same code\n\
deploy   put the newest image and this repo's tools on a node,\n\
                  and prove they arrived\n\
                  deploy [node]       (default: the first configured)\n\
         nodes    say what each configured node is, and change nothing\n\
                  nodes status        one round trip per node\n\
                  nodes exec <cmd>    one command on the first node, output and rc as they are\n\
         stop     kill the shards and release the mounts, and check\n\
                  that they are gone\n\
                  stop --hard         restart a node that will not let go\n\
         \n\
         options:\n\
           --no-bell               finish without ringing\n\
           --probes                list the bpftrace scripts and exit\n\
         \n\
         environment:\n\
         \x20 XFSTESTS_TIMEOUT        seconds per test, default 300\n\
         \x20 XFSTESTS_TRIAL_TIMEOUT  seconds per test in a sweep, default 1900\n\
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
    /*
     * One campaign at a time.
     *
     * Two orchestrators on one node share a scratch device and
     * a results file: the second wipes what the first proved
     * and both run tests against the same disk. Two hours of a
     * run were lost that way, and neither set of verdicts
     * meant anything afterwards.
     *
     * The lock holds a pid. A stale one -- the process is gone
     * -- is taken over rather than obeyed, so a crash does not
     * leave the tool refusing to start.
     */
    {
        let lock = std::path::Path::new("/tmp/beamfs-xfstests-campaign.lock");
        if let Ok(s) = std::fs::read_to_string(lock) {
            if let Ok(pid) = s.trim().parse::<u32>() {
                if pid != std::process::id()
                    && std::path::Path::new(&format!("/proc/{pid}")).exists()
                {
                    eprintln!("\n  a campaign is already running here (pid {pid})");
                    eprintln!("  stop it first, or remove {} if it is stale\n",
                              lock.display());
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
        let _ = std::fs::write(lock, std::process::id().to_string());
    }

    // Opened before anything is touched, so a run that dies during
    // preflight still says why. Thirty-minute runs leaving eight-line
    // logs is the specific thing this is here to stop.
    let mut jr = Journal::create(&std::env::temp_dir());
    println!();
    println!("  journal : {}", jr.path().display());
    /*
     * Bring each node to a known state before judging it.
     *
     * preflight only looks. It ran before anything was
     * stopped, so a node still holding a shard from the last
     * campaign was measured in that state -- and once a
     * blocked task makes a node unusable, a run refuses to
     * start over a condition a stop would have cleared.
     *
     * Everything here was being typed by hand before each
     * campaign, which is the definition of a missing step.
     */
    /*
     * Is the code about to be measured the code in the
     * repository, and is the image on the node the newest one
     * built? Neither was checked, and both have been false.
     *
     * A refusal, not a warning: a warning at the top of a
     * seven-hour run is read by nobody, and what it produces
     * cannot be told afterwards from a result that counts.
     */
    println!();
    println!("  === CHAIN ===");
    {
        let mut wrong = clean::sources_in_sync();
        if let Some(w) = clean::image_is_current() {
            wrong.push(w);
        }
        if wrong.is_empty() {
            println!("    sources and deployed image agree");
        } else {
            for w in &wrong {
                println!("    {w}");
            }
            if std::env::var("BEAMFS_CHAIN_IGNORE").is_ok() {
                println!("    BEAMFS_CHAIN_IGNORE set: running anyway");
                jr.section("CHAIN BROKEN, run continued on request");
            } else {
                eprintln!();
                eprintln!("  this run would not measure what it claims to");
                eprintln!("  rsync the layer, or rebuild and deploy, then start again");
                eprintln!();
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    println!();
    println!("  === PREPARE ===");
    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);
        c.stop();
        // A fresh campaign starts from nothing: old verdicts
        // would be resumed into, old failure logs read as this
        // run's, and the kernel ring still holds the markers of
        // the previous campaign -- which is how every test came
        // back with the same incident count.
        if !cfg.resume {
            let _ = c.run(
                "sudo rm -f /var/lib/beamfs-xfstests/results.txt; \
                 sudo rm -rf /tmp/xfs-failures; sudo dmesg -C",
                Duration::from_secs(30));
        }
        // Writeback left by the kill drains in seconds. Refusing
        // immediately on a non-zero count would reject a node
        // that is simply finishing what it was told to abandon.
        let mut waited = 0u64;
        let mut blocked = c.blocked_tasks();
        while blocked > 0 && waited < 60 {
            std::thread::sleep(Duration::from_secs(5));
            waited += 5;
            blocked = c.blocked_tasks();
        }
        println!("    {:<10} shard stopped, {} blocked after {}s{}",
                 n.name, blocked, waited,
                 if cfg.resume { "" } else { ", verdicts cleared" });
    }

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
                // A node with tasks in uninterruptible sleep is not ready.
                //
                // The first tests of a campaign then run against a device
                // something else still holds, and their verdicts say more
                // about the previous run than about this code. stop() has
                // reported a clean node with blocked=1 right after.
                let bad = bad || !info.contains("blocked=0");
                // And the rest of what the run is about to assert: the
                // tools answering are the ones built here, nothing is
                // mounted on the devices, and the kernel is this build.
                // bench has asked this since a checker dated 2011
                // reported 306 inodes beyond correction on a sound
                // volume; run never did.
                let local = vec![(
                    "fsck.beamfs".to_string(),
                    format!("{}/git/beamfs/tools/fsck.beamfs/fsck.beamfs",
                            std::env::var("HOME").unwrap_or_default()),
                )];
                let built = std::fs::metadata(
                    std::path::Path::new(crate::lab::kernel_image()))
                    .and_then(|m| m.modified()).ok();
                let wrong = c.ready_to_measure(&local, built);
                let bad = bad || !wrong.is_empty();
                for w in &wrong {
                    println!("    {:<10} {w}", n.name);
                }
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
        // node, all writing the same /var/lib/beamfs-xfstests/results.txt and all
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
             sleep 1; pgrep -f '[x]fs-runner.sh' >/dev/null && echo started || echo failed",
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
                    .run("pgrep -f '[x]fs-runner.sh' > /dev/null && echo yes || echo no",
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

    // Every verdict on its own line, as it lands.
    //
    // A bar redrawn in place says how many tests are done and
    // not which ones: a run that reported four failures showed
    // none of their names, and the only way to learn them was
    // to read the node's result file by hand. The bar stays as
    // a periodic summary; this is the record.
    let mut annonces: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

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

            for r in rs.iter() {
                if annonces.insert(format!("{}:{}", n.name, r.name)) {
                    println!("  {:>3}/{}  {:<14} {:<9} {:>5}s  {:<8} {}",
                             annonces.len(), SUITE_SIZE, r.name,
                             r.outcome.as_str(), r.seconds, r.node, r.reason);
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
            if let Ok(body) = c.run("cat /var/lib/beamfs-xfstests/results.txt 2>/dev/null",
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
            if let Ok(body) = c.run("cat /var/lib/beamfs-xfstests/results.txt 2>/dev/null",
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
    // Twenty shown out of a hundred and five on disk, with nothing
    // saying so: a capture looked for here and not found was read as a
    // run that never happened.
    let shown = runs.len();
    for tag in runs.iter().take(shown) {
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
    // Named, all of them: a "fixed 2" that names nobody cannot be
    // checked, and on 2026-09-22 the two it counted had to be found by
    // hand in the run report.
    for t in &d.fixed {
        println!("      {t:<16} fixed");
    }
    for (t, _) in &d.new {
        println!("      {t:<16} new");
    }
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

/// Other beamfs-xfstests processes on this machine.
///
/// stop frees the nodes and returns, which leaves the run that was
/// using them alive in whatever terminal launched it: it goes on
/// polling a node that has nothing left to report until its timeout
/// expires. On 2026-09-19 a probe sat for eleven minutes past a stop
/// that had already emptied the node, and the terminal stayed busy
/// throughout.
///
/// Read from /proc rather than pkill: the pattern would match the
/// shell carrying it, and this process must not kill itself.
fn local_runs() -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in dir.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else { continue };
        if pid == me {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default();
        if comm.trim() != "beamfs-xfstests" {
            continue;
        }
        let args = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ")
            .trim()
            .to_string();
        out.push((pid, args));
    }
    out
}

fn signal(pid: u32, sig: &str) {
    let _ = std::process::Command::new("kill")
        .args([sig, &pid.to_string()])
        .output();
}

/// Stop the work on every node, and prove it stopped.
///
/// This printed "stopped" and returned. On 2026-09-19 it left a check
/// running for twelve minutes, a bpftrace attached and a zstd
/// compressing a volume image -- every pattern it used matched its own
/// ssh command line, so the first pkill killed the shell running them
/// and none of the rest ever ran. The next sweep was refused the node,
/// which is how it was noticed at all.
///
/// So: kill, wait, look. What survives a SIGKILL is in uninterruptible
/// sleep and will not die until its I/O finishes; `hard` restarts the
/// domain instead of waiting for it.
/// Where a silent node's console is kept: beside the trial evidence,
/// named by node and by the moment, so two wedges do not overwrite
/// each other.
fn wedge_dir(node: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::path::PathBuf::from(home)
        .join(".local/share/beamfs-xfstests/evidence")
        .join(format!("wedge-{node}-{at}"))
}

fn stop(cfg: &Config, hard: bool) -> std::process::ExitCode {
    println!();
    let mut stubborn = 0usize;

    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);

        // A node that does not answer is not a node with nothing
        // running. leftover_work() turns an ssh failure into an empty
        // list, and on 2026-09-22 stop read that list as "stopped,
        // nothing left running" on a guest wedged at 5.7 GiB of held
        // folios; --hard sat behind the list and was never reached.
        // The console is the one channel such a guest still has, so
        // it is kept before the domain is touched.
        if let Err(e) = c.run("true", std::time::Duration::from_secs(12)) {
            println!("  {:<10} does not answer: {e}", n.name);
            if !hard {
                stubborn += 1;
                println!("               beamfs-xfstests stop --hard keeps its console");
                println!("               and restarts the domain");
                continue;
            }
            let r = Recovery::new(cfg);
            let domain = r.domain_for(&n.name);
            let dir = wedge_dir(&n.name);
            match wedge::capture_wedged(&domain, &dir) {
                Ok(bytes) => println!("               console kept: {bytes} bytes in {}",
                                      dir.display()),
                Err(e) => println!("               no console kept: {e}"),
            }
            println!("               restarting {domain}");
            let mut jr = Journal::create(&std::env::temp_dir());
            let outcome = r.recover(&c, &domain, 2, &mut jr);
            println!("               {}", outcome.as_str());
            if !outcome.usable() {
                stubborn += 1;
            }
            continue;
        }

        c.stop();

        // Up to fifteen seconds for the D-state work to finish and the
        // kill to take effect.
        let mut left = c.leftover_work();
        for _ in 0..5 {
            if left.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            c.stop();
            left = c.leftover_work();
        }

        if left.is_empty() {
            println!("  {:<10} stopped, nothing left running", n.name);
            continue;
        }

        println!("  {:<10} still running after the kill:", n.name);
        for l in &left {
            println!("               {l}");
        }

        if !hard {
            stubborn += 1;
            println!("               these are in uninterruptible sleep;");
            println!("               beamfs-xfstests stop --hard restarts the domain");
            continue;
        }

        let r = Recovery::new(cfg);
        let domain = r.domain_for(&n.name);
        println!("               restarting {domain}");
        let mut jr = Journal::create(&std::env::temp_dir());
        let outcome = r.recover(&c, &domain, 2, &mut jr);
        println!("               {}", outcome.as_str());
        if !outcome.usable() {
            stubborn += 1;
        }
    }

    // The nodes are free; whatever was driving them is not.
    let mut local = local_runs();
    if !local.is_empty() {
        println!();
        for (pid, args) in &local {
            println!("  local run {pid}: {args}");
        }
        for (pid, _) in &local {
            signal(*pid, "-TERM");
        }
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            local = local_runs();
            if local.is_empty() {
                break;
            }
        }
        for (pid, _) in &local {
            signal(*pid, "-KILL");
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        local = local_runs();
        if local.is_empty() {
            println!("  local runs stopped");
        } else {
            for (pid, _) in &local {
                println!("  local run {pid} survived SIGKILL");
            }
            stubborn += local.len();
        }
    }

    println!();
    if stubborn > 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
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
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }

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
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
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
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
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
/// Run one test against beamfs and against a filesystem known to be
/// sound, on the same devices, and print both.
fn do_control(cfg: &Config, test: Option<&String>, trials: Option<&String>,
              fstyps: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let t = test.map(|s| s.as_str()).unwrap_or("generic/083");
    let n: u32 = trials.and_then(|v| v.parse().ok()).unwrap_or(1);
    // beamfs first: the control is there to interpret it, and a run
    // interrupted half way is more useful with the subject measured
    // than with only the control.
    let list: Vec<String> = fstyps
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_else(|| vec!["beamfs".into(), "ext2".into()]);
    match bench::control(cfg, node, t, n, &list) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("control: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn do_baseline(cfg: &Config, test: Option<&String>, trials: Option<&String>,
               rounds: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
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
/// Random writes on the bare scratch device, read back at once and
/// again later: does the device keep what it is given? beamfs 0.1.18
/// on 2026-09-25 saw region blocks read back identical right after
/// their write and holding zeros later, with no write in between.
fn do_soak(cfg: &Config, secs: Option<&String>) -> std::process::ExitCode {
    let Some(node) = cfg.nodes.first() else {
        eprintln!("no nodes configured");
        return std::process::ExitCode::FAILURE;
    };
    if let Err(e) = bench::refuse_if_busy(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let secs = secs.and_then(|x| x.parse().ok()).unwrap_or(600);
    match soak::soak(cfg, node, secs) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("  {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn do_deploy(cfg: &Config, which: Option<&String>) -> std::process::ExitCode {
    let Some(node) = (match which.map(|s| s.as_str()) {
        Some(n) => cfg.nodes.iter().find(|x| x.name == n),
        None => cfg.nodes.first(),
    }) else {
        eprintln!("beamfs-xfstests: no such node");
        return std::process::ExitCode::FAILURE;
    };
    // Never reboot a node someone is measuring on.
    if let Err(e) = bench::refuse_if_busy(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
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
    // Held for as long as this command runs, so a deploy from another
    // terminal cannot reboot the node underneath it. Named for the
    // subcommand, so the refusal says what has the node.
    // The selection, in the forms a person writes rather than the one
    // the harness reads: ranges, lists, and the sets this tool knows
    // from its own history.
    let selection: Vec<String> = match select::expand(selection) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let selection = &selection[..];

    let _held = match nodelock::acquire(
        &node.name,
        &std::env::args().nth(1).unwrap_or_else(|| "a run".into()),
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("beamfs-xfstests: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // Nothing is measured until the chain from this repository to the
    // node agrees with itself.
    if let Err(e) = checkpoint::gate(cfg, node) {
        eprintln!("beamfs-xfstests: {e}");
        return std::process::ExitCode::FAILURE;
    }
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

#[cfg(test)]
mod shell_tests {
    /// The shell this tool carries is checked like the rest of it.
    ///
    /// clippy holds the Rust to zero warnings and -Werror holds the C,
    /// and the shell -- which runs as root on the nodes -- had never
    /// been looked at by anything. shellcheck was not even installed.
    /// Errors fail the build here; the style notes do not, because a
    /// wall of them would make the check be turned off within a week.
    ///
    /// Skipped where shellcheck is absent, so the suite still runs on
    /// a machine that does not have it.
    #[test]
    fn the_shell_has_no_errors() {
        let Ok(out) = std::process::Command::new("shellcheck")
            .args(["-f", "gcc", "-S", "error", "src/runner.sh"])
            .output()
        else {
            eprintln!("shellcheck absent: skipped");
            return;
        };
        let found = String::from_utf8_lossy(&out.stdout);
        assert!(
            found.trim().is_empty(),
            "shellcheck reports errors in src/runner.sh:\n{found}"
        );
    }
}
