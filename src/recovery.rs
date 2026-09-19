// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Getting a wedged node back into the run.
//!
//! Detecting a stall and stopping there is not enough. On 2026-09-01 two
//! nodes went to "No route to host" mid-campaign and stayed there: a
//! quarter of the work simply stopped, and nobody noticed until the
//! totals were read an hour later. A node that cannot be recovered is a
//! node that has to be recovered by hand, and a run that needs a human
//! at 3am is a run that does not happen.
//!
//! Four escalating steps, each tried only because the one before it did
//! not work:
//!
//!   1. kill the shard and lazy-unmount. Costs nothing, works when the
//!      test process is merely stuck rather than the filesystem.
//!   2. sysrq: dump every blocked task's stack, then emergency-remount
//!      read-only. Works through a wedged filesystem because sysrq runs
//!      in interrupt context; also the last chance to get evidence.
//!   3. destroy and restart the domain. Always works, costs ~2 minutes
//!      of boot, and loses whatever was in flight.
//!   4. give up on that node and redistribute its remaining tests.
//!
//! Evidence is collected at every step and before every action, because
//! each step destroys the state that explains the one before.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use crate::config::Config;
use crate::journal::Journal;
use crate::node::NodeConn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Killing the shard was enough; the node is usable again.
    Killed,
    /// sysrq freed it.
    Sysrq,
    /// The domain had to be restarted.
    Restarted,
    /// Nothing worked. Its remaining tests go to the other nodes.
    Lost,
}

impl RecoveryOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Killed => "killed",
            Self::Sysrq => "sysrq",
            Self::Restarted => "restarted",
            Self::Lost => "lost",
        }
    }

    /// Can the node take work again?
    #[must_use]
    pub fn usable(self) -> bool {
        !matches!(self, Self::Lost)
    }
}

pub struct Recovery<'a> {
    cfg: &'a Config,
}

impl<'a> Recovery<'a> {
    #[must_use]
    pub fn new(cfg: &'a Config) -> Self {
        Self { cfg }
    }

    /// Bring `node` back, escalating until it is usable or written off.
    ///
    /// `attempt` is how many times this node has already been through
    /// here during the run. Repeated recoveries are a signal in
    /// themselves: a node needing a third one is not having bad luck,
    /// it is failing in a way killing does not address, so the
    /// escalation starts further along.
    pub fn recover(&self, conn: &NodeConn, domain: &str, attempt: u32,
                   jr: &mut Journal) -> RecoveryOutcome
    {
        self.recover_into(conn, domain, attempt, jr, None)
    }

    /// Recover, keeping whatever the node holds into @evidence first.
    ///
    /// The caller passes a directory when it has one -- a sweep does --
    /// and the guest's volatile record lands there before any step
    /// touches the machine.
    pub fn recover_into(&self, conn: &NodeConn, domain: &str, attempt: u32,
                        jr: &mut Journal,
                        evidence: Option<&std::path::Path>) -> RecoveryOutcome
    {
        let name = conn.node.name.clone();
        jr.section(&format!("RECOVERY {name}"));

        // Step 0: evidence, before anything is disturbed.
        let (stacks, dmesg, mounts) = conn.stall_evidence();
        jr.stall_evidence(&name, "(stall)", &stacks, &dmesg, &mounts);

        // And the volatile record, which a restart destroys.
        //
        // The ftrace buffer and the whole of dmesg live in the guest's
        // memory. On 2026-09-13 a restart took a 250 MB trace with it
        // and the campaign that produced it ran for three hours.
        if let Some(dir) = evidence {
            let kept = conn.drain_volatile(dir);
            if kept.is_empty() {
                jr.line(&format!("{name}: nothing volatile could be kept"));
                println!("      the node kept nothing back");
            } else {
                for (what, n) in &kept {
                    jr.line(&format!("{name}: kept {what} ({n} bytes)"));
                }
                let total: u64 = kept.iter().map(|(_, n)| n).sum();
                println!("      kept {} file(s), {} KiB, in {}",
                         kept.len(), total / 1024, dir.display());
            }
        }

        // Step 1, but only the first two times. A node that has
        // already been killed twice and come back blocked will come
        // back blocked a third time; going straight to the restart
        // saves the round trip and, more to the point, actually works.
        if attempt < 2 {
            println!("      step 1: killing the shard");
            jr.line(&format!("{name}: step 1, killing the shard (attempt {attempt})"));
            conn.stop();
            if self.responsive(conn) {
                jr.line(&format!("{name}: recovered by kill"));
                return RecoveryOutcome::Killed;
            }
        } else {
            println!("      attempt {attempt}: skipping the kill, it has not worked");
            jr.line(&format!("{name}: attempt {attempt}, escalating past the kill"));
        }

        // Step 2: sysrq, and only while the kill is still worth trying.
        // Runs in interrupt context, so it works when the filesystem
        // does not. 'w' dumps blocked tasks -- the only way to get
        // stacks out of a node that has stopped answering ssh -- and
        // 'u' remounts everything read-only, which releases tasks
        // waiting on writeback.
        println!("      step 2: sysrq w (task dump) then u (remount ro)");
        jr.line(&format!("{name}: step 2, sysrq w then u"));
        self.sysrq(domain, "w");
        std::thread::sleep(Duration::from_secs(2));
        self.sysrq(domain, "u");
        std::thread::sleep(Duration::from_secs(8));

        let console = self.console_tail(domain, 120);
        if !console.is_empty() {
            jr.section(&format!("SYSRQ OUTPUT {name}"));
            for l in console.lines() {
                jr.line(l);
            }
        }
        if self.responsive(conn) {
            jr.line(&format!("{name}: recovered by sysrq"));
            return RecoveryOutcome::Sysrq;
        }

        // Step 3: the hammer. Two minutes, and whatever the shard had
        // written to /tmp survives only if it was flushed -- which is
        // why results are copied to the orchestrator as they appear.
        println!("      step 3: restarting {domain}");
        jr.line(&format!("{name}: step 3, restarting domain {domain}"));
        self.virsh(&["destroy", domain]);
        std::thread::sleep(Duration::from_secs(5));
        self.virsh(&["start", domain]);

        // Watch the console while it boots rather than sleeping blind.
        //
        // Two reasons. The console says what the kernel is doing, so a
        // boot that is stuck looks different from one that is slow --
        // and the previous version simply waited four minutes in
        // silence either way. And "login:" appears well before sshd
        // accepts connections, so a machine that has reached userspace
        // can be declared back without waiting out the full timeout.
        let mut last_len = 0u64;
        let mut quiet = 0u32;
        for i in 1..=24 {
            std::thread::sleep(Duration::from_secs(5));
            let tail = self.console_tail(domain, 3);
            let len = tail.len() as u64;

            let last = tail.lines().last().unwrap_or("").trim();
            let shown: String = last.chars().take(58).collect();
            println!("      boot {:3}s  {shown:<58}", i * 5);
            let _ = std::io::stdout().flush();

            if len == last_len {
                quiet += 1;
            } else {
                quiet = 0;
                last_len = len;
                jr.line(&format!("{name} console: {last}"));
            }

            // Userspace is up; ssh is a formality from here.
            if tail.contains("login:") || tail.contains("systemd") {
                jr.line(&format!("{name}: userspace up after {}s", i * 5));
            }

            if self.responsive(conn) {
                
                println!("      back after {}s", i * 5);
                jr.line(&format!("{name}: back after {}s", i * 5));
                return RecoveryOutcome::Restarted;
            }

            // Silent console and no ssh for a minute: it is not booting,
            // it is stuck. Saying so beats waiting out the remaining
            // three minutes for the same answer.
            if quiet >= 12 && i > 8 {
                
                println!("      console silent for 60s, boot is stuck");
                jr.line(&format!("{name}: console went silent during boot"));
                break;
            }
        }
        println!();

        jr.line(&format!("{name}: unrecoverable, redistributing its work"));
        RecoveryOutcome::Lost
    }

    /// Is the node fit to take work again?
    ///
    /// Answering ssh is not the same as being usable. A node with eighty
    /// tasks in uninterruptible sleep replies to every command and
    /// cannot run a test: the mounts are held, the next test inherits
    /// them, and it wedges too.
    ///
    /// Treating "replies" as "recovered" is why one node was killed and
    /// relaunched about a hundred and fifty times in a single run,
    /// staying at eighty blocked tasks throughout, and produced 345
    /// results for a 185-test shard -- every one of them from a machine
    /// that was never actually repaired.
    ///
    /// SIGKILL does not clear D-state, so if any remain after the kill
    /// the only remedy is a restart. The threshold is four rather than
    /// zero: a few tasks are briefly in D on any write, and demanding a
    /// perfectly idle node would restart healthy ones.
    fn responsive(&self, conn: &NodeConn) -> bool {
        if conn.run("true", Duration::from_secs(12)).is_err() {
            return false;
        }
        if conn.blocked_tasks() > 4 {
            return false;
        }

        // And nothing from the campaign may still be alive.
        //
        // D-state is not the only way a node stays unusable. On
        // 2026-09-19 a zstd compressing a 256 MB result file survived
        // SIGKILL, stayed in R, and burned a full core in kernel
        // context for thirteen minutes: no blocked task, ssh perfectly
        // responsive, and this function called the node repaired. The
        // restart it needed was never reached, because the sysrq step
        // returned first.
        //
        // The process was in the unlink of the file it had just
        // compressed -- freeing the blocks of a large file costs
        // minutes of CPU, and the signal is only handled once that
        // work ends. Surviving the kill is therefore the symptom to
        // test for, not a side effect to ignore.
        let left = conn.leftover_work();
        if !left.is_empty() {
            return false;
        }

        // And the root filesystem must still be writable.
        //
        // Step 2 of the recovery is sysrq u, an emergency remount
        // read-only. It releases tasks waiting on writeback, which is
        // what it is for, and it leaves the node unable to run a
        // single test: xfstests writes its results, its .out.bad and
        // its check.log to the root filesystem, so every test after
        // that fails on an output mismatch that is really a failed
        // write.
        //
        // Measured on 2026-09-19: a probe of generic/083 came back
        // FAIL in ten seconds with "cannot remove ...: Read-only file
        // system" on every line, on a node this function had just
        // called repaired. A node that has been remounted read-only
        // needs the restart, and saying so here is what reaches it.
        let rw = conn
            .run("findmnt -n -o OPTIONS / 2>/dev/null || cat /proc/mounts",
                 Duration::from_secs(15))
            .map(|o| {
                let first = o.lines().next().unwrap_or("");
                first.starts_with("rw,") || first == "rw"
                    || o.lines().any(|l| {
                        let mut f = l.split_whitespace();
                        f.next().is_some()
                            && f.next() == Some("/")
                            && f.next().is_some()
                            && f.next().is_some_and(|o| o.starts_with("rw"))
                    })
            })
            .unwrap_or(false);
        if !rw {
            return false;
        }
        true
    }

    /// Poke sysrq through the hypervisor.
    ///
    /// Through virsh rather than /proc/sysrq-trigger: the point is to
    /// reach a kernel that is no longer running userspace.
    fn sysrq(&self, domain: &str, key: &str) {
        let _ = Command::new("timeout")
            .arg("20")
            .arg("sudo")
            .args(["virsh", "send-key", domain, "--codeset", "linux"])
            .args(["KEY_LEFTALT", "KEY_SYSRQ", &format!("KEY_{}", key.to_uppercase())])
            .output();
    }

    /// What the serial console has said recently.
    ///
    /// The console keeps working when the network does not, which makes
    /// it the only channel that reaches a node in this state.
    fn console_tail(&self, domain: &str, lines: usize) -> String {
        let xml = Command::new("sudo")
            .args(["virsh", "dumpxml", domain])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let Some(pty) = xml
            .split_whitespace()
            .find_map(|w| w.strip_prefix("path='/dev/pts/").map(|r| {
                format!("/dev/pts/{}", r.trim_end_matches('\''))
            }))
        else {
            return String::new();
        };
        let out = Command::new("timeout")
            .args(["10", "sudo", "cat", &pty])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        out.lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn virsh(&self, args: &[&str]) {
        let _ = Command::new("timeout")
            .arg("60")
            .arg("sudo")
            .arg("virsh")
            .args(args)
            .output();
    }

    /// Domain name for a node, derived from its configured name.
    #[must_use]
    pub fn domain_for(&self, node_name: &str) -> String {
        let _ = self.cfg;
        format!("beamfs-{node_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lost_is_unusable() {
        assert!(RecoveryOutcome::Killed.usable());
        assert!(RecoveryOutcome::Sysrq.usable());
        assert!(RecoveryOutcome::Restarted.usable());
        assert!(!RecoveryOutcome::Lost.usable());
    }

    #[test]
    fn domains_follow_the_node_names() {
        let cfg = Config::default();
        let r = Recovery::new(&cfg);
        assert_eq!(r.domain_for("master"), "beamfs-master");
        assert_eq!(r.domain_for("compute01"), "beamfs-compute01");
    }
}
