// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Talking to one machine over ssh.
//!
//! Every call has a wall-clock deadline. A node whose kernel has wedged
//! stops answering its network entirely -- "No route to host" -- and a
//! command without a deadline waits on it until someone notices. That
//! happened twice on 2026-09-01 and cost the run both times.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::{Config, Node};
use crate::result::TestResult;

#[derive(Debug)]
pub enum NodeError {
    Unreachable(String),
    Command { rc: i32, stderr: String },
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(h) => write!(f, "{h} unreachable"),
            Self::Command { rc, stderr } => write!(f, "rc={rc}: {}", stderr.trim()),
        }
    }
}

pub struct NodeConn<'a> {
    pub node: &'a Node,
    key: String,
    user: String,
}

impl<'a> NodeConn<'a> {
    #[must_use]
    pub fn new(node: &'a Node, cfg: &Config) -> Self {
        Self { node, key: cfg.ssh_key.clone(), user: cfg.user.clone() }
    }

    /// Run a command on the node, giving up after `deadline`.
    ///
    /// ConnectTimeout covers a machine that refuses the connection;
    /// BatchMode stops ssh asking for a password nobody is there to
    /// type. Neither covers a connection that opens and then never
    /// answers, which is what a wedged filesystem produces, so the
    /// deadline is enforced here as well.
    pub fn run(&self, cmd: &str, deadline: Duration) -> Result<String, NodeError> {
        let out = Command::new("timeout")
            .arg(format!("{}", deadline.as_secs()))
            .arg("ssh")
            .args(["-i", &self.key])
            .args(["-o", "BatchMode=yes"])
            .args(["-o", "ConnectTimeout=8"])
            .args(["-o", "StrictHostKeyChecking=no"])
            .args(["-o", "UserKnownHostsFile=/dev/null"])
            .args(["-o", "LogLevel=ERROR"])
            .arg(format!("{}@{}", self.user, self.node.host))
            .arg(cmd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| NodeError::Unreachable(format!("{}: {e}", self.node.host)))?;

        // 124 is timeout(1)'s "the command outlived its deadline".
        if out.status.code() == Some(124) {
            return Err(NodeError::Unreachable(format!("{} timed out", self.node.host)));
        }
        if !out.status.success() {
            return Err(NodeError::Command {
                rc: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Copy a local file to the node.
    pub fn push(&self, local: &str, remote: &str) -> Result<(), NodeError> {
        let out = Command::new("timeout")
            .arg("30")
            .arg("scp")
            .args(["-i", &self.key])
            .args(["-o", "BatchMode=yes"])
            .args(["-o", "StrictHostKeyChecking=no"])
            .args(["-o", "UserKnownHostsFile=/dev/null"])
            .args(["-o", "LogLevel=ERROR"])
            .arg(local)
            .arg(format!("{}@{}:{remote}", self.user, self.node.host))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| NodeError::Unreachable(format!("{}: {e}", self.node.host)))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(NodeError::Command {
                rc: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Is the node up, and does it have what the suite needs?
    ///
    /// Checked before launching rather than discovered afterwards: a
    /// node missing its scratch device produces a run of MOUNTFAIL that
    /// looks exactly like a filesystem that cannot mount.
    pub fn preflight(&self) -> Result<String, NodeError> {
        self.run(
            &format!(
                "echo \"kernel=$(uname -r) \
                 blocked=$(ps -eo state | grep -c '^D') \
                 test={} scratch={} \
                 xfstests=$(test -x /usr/xfstests/check && echo yes || echo NO) \
                 mkfs=$(command -v mkfs.beamfs >/dev/null && echo yes || echo NO)\"",
                self.probe_dev(&self.node.test_dev),
                self.probe_dev(&self.node.scratch_dev),
            ),
            Duration::from_secs(20),
        )
    }

    fn probe_dev(&self, dev: &str) -> String {
        format!("$(lsblk -dno SIZE /dev/{dev} 2>/dev/null | tr -d ' ' || echo MISSING)")
    }

    /// Everything the node has recorded so far.
    ///
    /// Read fresh each time rather than tailed: the file is small, the
    /// poll interval is a minute, and a partial read of a line being
    /// written is dropped by the parser anyway.
    pub fn results(&self) -> Result<Vec<TestResult>, NodeError> {
        let raw = self.run("cat /tmp/xfs-results.txt 2>/dev/null", Duration::from_secs(20))?;
        Ok(raw
            .lines()
            .filter(|l| !l.starts_with("DONE"))
            .filter_map(|l| TestResult::parse(l, &self.node.name))
            .collect())
    }

    /// Has this node finished its shard?
    pub fn is_done(&self) -> bool {
        self.run("grep -qc '^DONE' /tmp/xfs-results.txt 2>/dev/null && echo 1 || echo 0",
                 Duration::from_secs(15))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// The test currently running, if any.
    pub fn current_test(&self) -> Option<String> {
        self.run("ps -eo args | grep -oE 'generic/[0-9]+$' | head -1",
                 Duration::from_secs(15))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Tasks stuck in uninterruptible sleep.
    ///
    /// A non-zero count with no progress is the signature of a wedged
    /// filesystem, and it is worth surfacing before the node stops
    /// answering altogether.
    pub fn blocked_tasks(&self) -> usize {
        self.run("ps -eo state | grep -c '^D'", Duration::from_secs(15))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Kill the shard and release the mounts.
    pub fn stop(&self) {
        let _ = self.run(
            "sudo pkill -9 -f xfs-runner.sh 2>/dev/null; \
             sudo pkill -9 -f 'tests/generic' 2>/dev/null; \
             sudo pkill -9 -f '/usr/xfstests/check' 2>/dev/null; \
             sleep 1; sudo umount -l /mnt/test /mnt/scratch 2>/dev/null; true",
            Duration::from_secs(30),
        );
    }
}
