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

    /// Bring `node` back, escalating until it answers or is written off.
    pub fn recover(&self, conn: &NodeConn, domain: &str, jr: &mut Journal)
        -> RecoveryOutcome
    {
        let name = conn.node.name.clone();
        jr.section(&format!("RECOVERY {name}"));

        // Step 0: evidence, before anything is disturbed.
        let (stacks, dmesg, mounts) = conn.stall_evidence();
        jr.stall_evidence(&name, "(stall)", &stacks, &dmesg, &mounts);

        // Step 1: the cheap one.
        println!("      step 1: killing the shard");
        jr.line(&format!("{name}: step 1, killing the shard"));
        conn.stop();
        if self.responsive(conn) {
            jr.line(&format!("{name}: recovered by kill"));
            return RecoveryOutcome::Killed;
        }

        // Step 2: sysrq. Runs in interrupt context, so it works when the
        // filesystem does not. 'w' dumps blocked tasks -- the only way
        // to get stacks out of a node that has stopped answering ssh --
        // and 'u' remounts everything read-only, which releases tasks
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
            print!("\r      boot {:3}s  {shown:<58}", i * 5);
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
                println!("\r      back after {}s{:40}", i * 5, " ");
                jr.line(&format!("{name}: back after {}s", i * 5));
                return RecoveryOutcome::Restarted;
            }

            // Silent console and no ssh for a minute: it is not booting,
            // it is stuck. Saying so beats waiting out the remaining
            // three minutes for the same answer.
            if quiet >= 12 && i > 8 {
                println!("\r      console silent for 60s, boot is stuck{:24}", " ");
                jr.line(&format!("{name}: console went silent during boot"));
                break;
            }
        }
        println!();

        jr.line(&format!("{name}: unrecoverable, redistributing its work"));
        RecoveryOutcome::Lost
    }

    /// Does the node answer a trivial command?
    fn responsive(&self, conn: &NodeConn) -> bool {
        conn.run("true", Duration::from_secs(12)).is_ok()
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
