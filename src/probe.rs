// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! One test, watched from outside the machine running it.
//!
//! The node under test cannot be relied on to report its own death.
//! generic/027 takes the network with it every time, and everything
//! gathered inside the guest -- samples, stacks, dmesg -- dies with it
//! unless it has already been pulled out.
//!
//! So two channels run at once. ssh collects the detailed samples while
//! the node still answers, and the serial console is captured from the
//! host for the whole run, because it keeps working when the network
//! does not and is the only place the last words appear.
//!
//! And a poll that returns nothing means the node is gone, not that the
//! counter is zero. Treating an empty answer as a zero is how a watcher
//! sat for 396 seconds reporting "0 samples" at a machine that had been
//! dead for six minutes.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Erase the status line before printing anything else.
///
/// The status line is written with \r and no newline, so it leaves the
/// cursor mid-line. A println! after it starts where the cursor sits
/// and the output walks diagonally down the terminal, which is what
/// every run of this looked like.
/// Erase the status line without wrapping.
///
/// This padded to 100 columns and returned. On a terminal narrower
/// than that the padding wrapped, so the carriage return came back to
/// the start of the second line and every message after it began one
/// column further right -- a staircase down the screen for the length
/// of a run. \x1b[2K erases the line the cursor is on, whatever its
/// width, and moves nothing.
use crate::say;
use crate::say::clear_line;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::journal::Journal;
use crate::node::NodeConn;

/// Why the probe stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeEnd {
    /// The script finished and left its archive.
    Completed,
    /// The node stopped answering. The console log is what is left.
    NodeLost,
    /// The wall-clock limit was reached with the script still running.
    Timeout,
}

pub struct Probe<'a> {
    cfg: &'a Config,
    out: PathBuf,
}

impl<'a> Probe<'a> {
    #[must_use]
    pub fn new(cfg: &'a Config, out: &Path) -> Self {
        let _ = std::fs::create_dir_all(out);
        Self { cfg, out: out.to_path_buf() }
    }

    /// Start capturing the guest's serial console into a file.
    ///
    /// Started before the test, not after it fails. By the time a node
    /// is unreachable the interesting output has already gone past, and
    /// a console read that begins then returns whatever happens to
    /// arrive afterwards -- which is nothing, because the machine is
    /// dead.
    fn console_capture(&self, domain: &str, dest: &Path) -> Option<Child> {
        // virsh ttyconsole gives the path directly. Parsing dumpxml
        // for it works but depends on quoting that is libvirt's to
        // change, and this is one call either way.
        let out = Command::new("sudo")
            .args(["virsh", "ttyconsole", domain])
            .output()
            .ok()?;
        let pty = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !pty.starts_with("/dev/pts/") {
            return None;
        }
        let pty = pty.as_str();

        let f = std::fs::File::create(dest).ok()?;
        Command::new("sudo")
            .args(["cat", pty])
            .stdout(Stdio::from(f))
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    /// Run `test` on `node` with everything watched.
    pub fn run(&self, node_name: &str, test: &str, limit: Duration, jr: &mut Journal)
        -> ProbeEnd
    {
        // Before the console is opened, so there is nothing to clean up
        // on this path. Every return after the capture starts goes
        // through finish(), which kills it.
        let Some(node) = self.cfg.nodes.iter().find(|n| n.name == node_name) else {
            eprintln!("  unknown node {node_name}");
            return ProbeEnd::NodeLost;
        };
        let conn = NodeConn::new(node, self.cfg);
        let domain = format!("beamfs-{node_name}");
        let safe = test.replace('/', "-");

        jr.section(&format!("PROBE {test} on {node_name}"));

        let console_path = self.out.join(format!("console-{safe}.log"));
        let mut console = self.console_capture(&domain, &console_path);
        if console.is_some() {
            say!("  console -> {}", console_path.display());
        } else {
            say!("  console unavailable; guest-side data only");
        }

        // The script named by XFSTESTS_BPF, attached for the length
        // of the test.
        //
        // probe ignored the variable entirely: it is honoured by the
        // campaign path and nowhere else, so every probe run asking
        // for a script got the seven ordinary artefacts and no
        // capture, with nothing saying the script had not started.
        //
        // Before the test starts, not after. Attached after the
        // launch call -- which ends in a sleep and can outlive its
        // own deadline -- the probe went on at the sixtieth second
        // of a test that ended at the sixty-third, and reported 0
        // updates and 0 verifies where bpftrace by hand counts
        // 11047 and 84948 in twenty seconds. A capture of the last
        // three seconds of a test reads exactly like a probe that
        // does not work.
        let bprobe = match std::env::var("XFSTESTS_BPF") {
            Ok(name) if !name.is_empty() => match crate::bpf::start(&conn, &name) {
                Ok(r) => {
                    say!("  {} attached on {}", r.name, r.node());
                    Some(r)
                }
                Err(e) => {
                    say!("  {name} did not start: {e}");
                    jr.line(&format!("bpf {name} did not start: {e}"));
                    None
                }
            },
            _ => None,
        };

        // Deploy and launch, detached from the ssh session so the
        // connection can close while the script keeps running.
        let script = include_str!("probe.sh");
        let tmp = std::env::temp_dir().join(format!("probe-{}.sh", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, script) {
            say!("  cannot stage the probe: {e}");
            if let Some(c) = console.as_mut() {
                let _ = c.kill();
            }
            return ProbeEnd::NodeLost;
        }
        // A failed copy is a copy problem, not a dead node. Treating it
        // as one destroyed a healthy machine and then declared its boot
        // stuck, which is two wrong answers from one bad assumption.
        if let Err(e) = conn.push(tmp.to_str().unwrap_or_default(), "/tmp/probe.sh") {
            say!("  cannot deploy the probe: {e}");
            jr.line(&format!("push failed: {e}"));
            let alive = conn.run("true", Duration::from_secs(15)).is_ok();
            say!("  node is {}", if alive { "alive; not touching it" } else { "unreachable" });
            if let Some(c) = console.as_mut() {
                let _ = c.kill();
            }
            let _ = std::fs::remove_file(&tmp);
            return if alive { ProbeEnd::Timeout } else { ProbeEnd::NodeLost };
        }
        let _ = std::fs::remove_file(&tmp);
        let launch = format!(
            "chmod +x /tmp/probe.sh && setsid /tmp/probe.sh {test} {} {} {} \
             < /dev/null > /tmp/probe.out 2>&1 & sleep 2; \
             pgrep -f '[p]robe.sh' > /dev/null && echo running || echo failed",
            node.test_dev, node.scratch_dev, limit.as_secs()
        );
        // The launch command ends in a sleep, so it can outlive a
        // short ssh deadline while the probe is in fact running. A
        // timeout here says nothing about whether it started; only
        // looking does.
        match conn.run(&launch, Duration::from_secs(60)) {
            Ok(o) if o.contains("running") => say!("  probe running"),
            Ok(o) => {
                say!("  probe did not start: {}", o.trim());
                jr.command(node_name, &launch, &o, false);
            }
            Err(e) => {
                jr.command(node_name, &launch, &e.to_string(), false);
                let up = conn
                    .run("pgrep -f '[p]robe.sh' > /dev/null && echo yes || echo no",
                         Duration::from_secs(20))
                    .map(|o| o.contains("yes"))
                    .unwrap_or(false);
                if up {
                    say!("  probe running (launch call timed out, process is there)");
                } else {
                    say!("  launch failed: {e}");
                }
            }
        }


        let start = Instant::now();
        let dir = format!("/tmp/probe-{safe}");
        let mut spin = 0u64;
        let mut misses = 0u32;
        let end;

        loop {
            spin += 1;
            let el = start.elapsed();

            // One command, so a slow node costs one round trip rather
            // than six. Empty output is the signal, not a zero.
            let poll = conn.run(
                &format!(
                    "s=$(grep -c '=== sample' {dir}/samples.txt 2>/dev/null || echo 0); \
                     z=$(stat -c%s {dir}/samples.txt 2>/dev/null || echo 0); \
                     p=$(pgrep -c -f '[p]robe.sh' 2>/dev/null || echo 0); \
                     d=$(ps -eo state | grep -c '^D'); \
                     w=$(grep ' {} ' /proc/diskstats | awk '{{print $8}}'); \
                     echo \"ALIVE $s $z $p $d ${{w:-0}}\"",
                    node.scratch_dev
                ),
                Duration::from_secs(20),
            );

            match poll {
                Ok(o) if o.contains("ALIVE") => {
                    misses = 0;
                    let f: Vec<&str> = o.split_whitespace().collect();
                    let g = |i: usize| f.get(i).copied().unwrap_or("?");
                    println!("  {} {:5}s  {:>4} samples {:>9} bytes  writes={:<10} D={:<3}",
                        ['|', '/', '-', '\\'][(spin % 4) as usize],
                        el.as_secs(), g(1), g(2), g(5), g(4)
                    );
                    let _ = std::io::stdout().flush();
                    jr.line(&format!("t={}s {}", el.as_secs(), o.trim()));

                    /*
                     * The only way this loop ends on its own.
                     *
                     * The pattern used to be 'probe.sh', unbracketed,
                     * so pgrep found the ssh shell carrying it and p
                     * was never 0: every probe ran to its deadline,
                     * and a probe of a node that had already been
                     * emptied by stop sat for eleven minutes with
                     * nothing to watch. Bracketed, p counts the probe
                     * and nothing else.
                     */
                    if g(3) == "0" && el > Duration::from_secs(30) {
                        end = ProbeEnd::Completed;
                        break;
                    }
                }
                _ => {
                    // Three consecutive misses, because one ssh can fail
                    // for reasons that are not the node dying.
                    misses += 1;
                    println!("  ? {:5}s  node not answering ({misses}/3)          ",
                           el.as_secs());
                    let _ = std::io::stdout().flush();
                    if misses >= 3 {
                        say!("  NODE LOST after {}s", el.as_secs());
                        end = ProbeEnd::NodeLost;
                        break;
                    }
                }
            }

            if el > limit + Duration::from_secs(120) {
                say!("  limit reached with the probe still running");
                end = ProbeEnd::Timeout;
                break;
            }
            std::thread::sleep(Duration::from_secs(10));
        }
        clear_line();

        // The probe first, so what it kept is the test and not the
        // test plus the checker that runs after it.
        if let Some(r) = bprobe {
            match r.stop_into(&conn, &self.out) {
                Some((p, sz)) => {
                    say!("  kept {} ({} KiB)",
                         p.file_name().unwrap_or_default().to_string_lossy(),
                         sz / 1024);
                    jr.line(&format!("bpf capture: {} ({sz} bytes)", p.display()));
                    crate::bpf::speak(&p);
                }
                None => {
                    say!("  the probe brought nothing back");
                    jr.line("bpf capture: nothing came back");
                }
            }
        }

        // Whatever the outcome, take what exists rather than waiting for
        // an archive a dead node will never produce.
        self.collect(&conn, &safe, end, jr);

        if let Some(c) = console.as_mut() {
            std::thread::sleep(Duration::from_secs(3));
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(m) = std::fs::metadata(&console_path) {
            say!("  console: {} bytes", m.len());
            if m.len() > 0 {
                if let Ok(s) = std::fs::read_to_string(&console_path) {
                    jr.section("SERIAL CONSOLE");
                    for l in s.lines().rev().take(60).collect::<Vec<_>>().into_iter().rev() {
                        jr.line(l);
                    }
                }
            }
        }

        self.archive(&safe);
        end
    }

    /// Roll everything into one archive.
    ///
    /// A probe leaves a journal, a console log, samples, stacks, fsck
    /// output and the harness transcript -- eight or ten files that are
    /// only useful together. Handing them over one at a time loses the
    /// correlation that is the whole point of collecting them at once.
    fn archive(&self, safe: &str) {
        let dest = std::env::temp_dir().join(format!("probe-{safe}.tar.gz"));
        let Some(parent) = self.out.parent() else { return };
        let Some(dir) = self.out.file_name() else { return };

        let ok = Command::new("tar")
            .arg("czf")
            .arg(&dest)
            .arg("-C")
            .arg(parent)
            .arg(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if ok {
            let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            let sum = Command::new("sha256sum")
                .arg(&dest)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout)
                     .split_whitespace()
                     .next()
                     .unwrap_or("")
                     .to_string())
                .unwrap_or_default();
            say!("");
            say!("  archive : {}", dest.display());
            say!("  size    : {size} bytes");
            if !sum.is_empty() {
                say!("  sha256  : {sum}");
            }
        } else {
            say!("  could not archive {}", self.out.display());
        }
    }

    /// Pull back what the guest managed to write.
    fn collect(&self, conn: &NodeConn, safe: &str, end: ProbeEnd, jr: &mut Journal) {
        let dir = format!("/tmp/probe-{safe}");
        if end == ProbeEnd::NodeLost {
            say!("  node is gone; nothing to pull from it");
            jr.line("collection skipped: node unreachable");
            return;
        }
        for f in ["verdict.txt", "samples.txt", "dmesg-test.txt",
                  "stacks-final.txt", "fsck.txt", "check.out", "baseline.txt"] {
            if let Ok(body) = conn.run(&format!("cat {dir}/{f} 2>/dev/null"),
                                       Duration::from_secs(60)) {
                if !body.trim().is_empty() {
                    let p = self.out.join(format!("{safe}-{f}"));
                    let _ = std::fs::write(&p, &body);
                    say!("  {} ({} bytes)", p.display(), body.len());
                }
            }
        }
    }
}
