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

/// What a shard is doing, as far as can be told from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardState {
    Running,
    /// Finished its list.
    Done,
    /// Gave up: tasks stuck in uninterruptible sleep, which no amount
    /// of killing clears. Needs the domain restarted and the shard
    /// relaunched, not to be counted as finished.
    Stuck,
    /// No answer. Not the same as finished, and not the same as
    /// working.
    Unreachable,
}

#[derive(Debug)]
pub enum NodeError {
    Unreachable(String),
    /// The connection was made and the command outlived its budget.
    ///
    /// Not the same as unreachable, and calling it that sent an
    /// afternoon looking for a network fault on a node that answered
    /// ping and had port 22 open: the loop simply took longer than the
    /// budget allowed, because the kernel under it carries a sanitizer.
    TimedOut { host: String, secs: u64 },
    /// A command that ran and exited non-zero.
    ///
    /// Both streams are carried. A failing xfstests run writes its
    /// verdict to stdout, and keeping only stderr meant every archived
    /// check.out was empty -- three failures went unexplained for a day
    /// because the output had been dropped one layer below where it was
    /// looked for.
    Command { rc: i32, stdout: String, stderr: String },
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(h) => write!(f, "{h} unreachable"),
            Self::TimedOut { host, secs } => {
                write!(f, "{host} did not finish within its {secs}s budget")
            }
            Self::Command { rc, stderr, .. } => {
                // stdout is carried for the caller, not for the message:
                // a failing test writes megabytes there and an error
                // line is not the place for them.
                write!(f, "rc={rc}: {}", stderr.trim())
            }
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
    /// Run a command whose non-zero exit is an answer, not a failure.
    ///
    /// `run` treats rc != 0 as an error, which is right for a mount or
    /// an mkfs and wrong for a test: generic/464 exits non-zero when it
    /// fails, and counting that as an execution error dropped three
    /// real failures out of ten and reported the run as 100% passing.
    /// This returns the output and the code, and lets the caller decide
    /// which is which.
    pub fn run_rc(&self, cmd: &str, deadline: Duration)
        -> Result<(String, i32), NodeError>
    {
        // A failing test writes its verdict to stdout and its exit code
        // is non-zero, so returning stderr here threw away the very
        // thing the caller asked for: every archived check.out was zero
        // bytes, and three failures went unexplained for a day because
        // of it. Both streams are kept, stdout first.
        match self.run(cmd, deadline) {
            Ok(out) => Ok((out, 0)),
            Err(NodeError::Command { rc, stdout, stderr }) => {
                let mut both = stdout;
                if !stderr.trim().is_empty() {
                    if !both.is_empty() {
                        both.push('\n');
                    }
                    both.push_str(&stderr);
                }
                Ok((both, rc))
            }
            Err(e) => Err(e),
        }
    }

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
            return Err(NodeError::TimedOut {
                host: self.node.host.clone(),
                secs: deadline.as_secs(),
            });
        }
        if !out.status.success() {
            return Err(NodeError::Command {
                rc: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Copy a local file to the node.
    pub fn push(&self, local: &str, remote: &str) -> Result<(), NodeError> {
        let out = Command::new("timeout")
            .arg("30")
            .arg("rsync")
            // rsync over ssh rather than scp.
            //
            // scp's -O forces the original protocol, which OpenSSH
            // deprecated in 9.0 for the reasons its own release notes
            // give. rsync says what it transferred and what it did not,
            // which scp does not, and resumes rather than restarting.
            .arg("-e")
            .arg(format!(
                "ssh -i {} -o BatchMode=yes -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
                 -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
                self.key))
            .arg("-q")
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
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Copy a file from the node.
    ///
    /// rsync rather than `cat` through run(): the ftrace buffer is
    /// hundreds of megabytes and run() returns a String, which means
    /// the whole thing in the harness's memory on a host that is
    /// already short of it.
    pub fn pull(&self, remote: &str, local: &str) -> Result<(), NodeError> {
        let out = Command::new("timeout")
            .arg("300")
            .arg("rsync")
            // rsync over ssh rather than scp.
            //
            // scp's -O forces the original protocol, which OpenSSH
            // deprecated in 9.0 for the reasons its own release notes
            // give. rsync says what it transferred and what it did not,
            // which scp does not, and resumes rather than restarting.
            .arg("-e")
            .arg(format!(
                "ssh -i {} -o BatchMode=yes -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
                 -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
                self.key))
            .arg("-q")
            .arg(format!("{}@{}:{remote}", self.user, self.node.host))
            .arg(local)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| NodeError::Unreachable(format!("{}: {e}", self.node.host)))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(NodeError::Command {
                rc: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Everything the guest holds that a restart would destroy.
    ///
    /// Written into @dir, best-effort, each piece independent of the
    /// others: a node too far gone to answer one of these is usually
    /// still able to answer the next, and half the record beats none.
    ///
    /// Returns what it managed to keep, for the log to say so.
    pub fn drain_volatile(&self, dir: &std::path::Path) -> Vec<(String, u64)> {
        let mut kept = Vec::new();
        let _ = std::fs::create_dir_all(dir);

        // Copied to /tmp on the node first: reading a debugfs file over
        // ssh holds it open for as long as the transfer takes, and the
        // ftrace buffer is not a file that likes being read slowly.
        let staged = [
            ("trace.txt",
             "sudo cat /sys/kernel/debug/tracing/trace > /tmp/ev-trace.txt 2>/dev/null; \
              sudo chmod 644 /tmp/ev-trace.txt",
             "/tmp/ev-trace.txt"),
            ("dmesg.txt",
             "sudo dmesg > /tmp/ev-dmesg.txt 2>/dev/null; sudo chmod 644 /tmp/ev-dmesg.txt",
             "/tmp/ev-dmesg.txt"),
            ("tracing-state.txt",
             "sudo sh -c 'for e in /sys/kernel/debug/tracing/events/beamfs/*/enable; do \
                echo \"$(basename $(dirname $e)) $(cat $e)\"; done' > /tmp/ev-state.txt 2>/dev/null; \
              sudo chmod 644 /tmp/ev-state.txt",
             "/tmp/ev-state.txt"),
        ];

        for (name, prep, remote) in staged {
            if self.run(prep, Duration::from_secs(60)).is_err() {
                continue;
            }
            let local = dir.join(name);
            if self.pull(remote, &local.to_string_lossy()).is_ok() {
                if let Ok(m) = std::fs::metadata(&local) {
                    if m.len() > 0 {
                        kept.push((name.to_string(), m.len()));
                    }
                }
            }
        }

        // Small enough to come back through run().
        let inline = [
            ("meminfo.txt", "cat /proc/meminfo"),
            ("slabinfo.txt", "sudo cat /proc/slabinfo"),
            ("mounts.txt", "cat /proc/mounts"),
            ("loadavg.txt", "cat /proc/loadavg; uptime"),
        ];
        for (name, cmd) in inline {
            if let Ok(out) = self.run(cmd, Duration::from_secs(20)) {
                if !out.is_empty() {
                    let local = dir.join(name);
                    if std::fs::write(&local, &out).is_ok() {
                        kept.push((name.to_string(), out.len() as u64));
                    }
                }
            }
        }
        kept
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

    /// Are the node's tools the ones this repo builds?
    ///
    /// Every redeploy of the image puts its own mkfs.beamfs and
    /// fsck.beamfs back, and they are not these. On 2026-09-13 a
    /// campaign reported 306 inodes beyond correction on a sound
    /// volume because the checker answering was the image's, 30840
    /// bytes dated 2011, against the 857568 built here.
    ///
    /// Existence was all preflight asked for, and it is not enough: a
    /// wrong checker does not fail, it answers, and an afternoon goes
    /// to a defect that was never there.
    ///
    /// @local maps a tool name under /usr/sbin to the binary this repo
    /// built. Returns the ones that differ; empty means all match.
    pub fn tools_match(&self, local: &[(String, String)]) -> Vec<String> {
        let mut wrong = Vec::new();

        for (name, path) in local {
            // No local copy is not a mismatch: the operator may be
            // running against an image whose tools are the reference.
            let want = match std::process::Command::new("md5sum").arg(path).output() {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                _ => continue,
            };
            if want.is_empty() {
                continue;
            }

            let got = self
                .run(
                    &format!("md5sum /usr/sbin/{name} 2>/dev/null | cut -d' ' -f1"),
                    Duration::from_secs(20),
                )
                .unwrap_or_default();
            let got = got.trim().to_string();

            if got.is_empty() {
                wrong.push(format!("{name} is not on the node"));
            } else if got != want {
                wrong.push(format!(
                    "{name}: the node has {}, this repo built {}",
                    &got[..got.len().min(12)],
                    &want[..want.len().min(12)]
                ));
            }
        }
        wrong
    }

    /// Does the kernel on the node carry the commit the run reports?
    ///
    /// commit() reads the git tree on the workstation. Nothing links
    /// it to the kernel that is actually running: an image built two
    /// commits ago boots, answers, and every result is filed under a
    /// commit whose code never ran.
    ///
    /// beamfs is builtin, so there is no module version to read. What
    /// there is: the kernel's build timestamp, which moves with every
    /// compile. @built is the mtime of the vmlinux this workstation
    /// last produced; a node older than that is running something else.
    ///
    /// Returns a description of the mismatch, or None when they agree.
    pub fn kernel_is_current(&self, built: std::time::SystemTime) -> Option<String> {
        // /proc/sys/kernel/version holds the build string, which ends
        // in the date the kernel was linked.
        let on_node = self
            .run("cat /proc/sys/kernel/version 2>/dev/null", Duration::from_secs(20))
            .unwrap_or_default();
        let on_node = on_node.trim().to_string();
        if on_node.is_empty() {
            return Some("the node would not say when its kernel was built".into());
        }

        // Compare against the local build by seconds since the epoch,
        // which is what both sides can agree on without parsing a date
        // in whatever locale the node happens to use.
        let want = built
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let got = self
            .run("stat -c %Y /boot/vmlinuz* 2>/dev/null | sort -rn | head -1",
                 Duration::from_secs(20))
            .unwrap_or_default();
        let got: u64 = got.trim().parse().unwrap_or(0);

        // A node whose kernel is newer than the local build is not a
        // mismatch: somebody may have built elsewhere. Older is.
        if got != 0 && want != 0 && got + 60 < want {
            return Some(format!(
                "the node's kernel is {} seconds older than the one built here",
                want - got));
        }
        None
    }

    /// Are the test and scratch devices unmounted?
    ///
    /// fsck reads the raw device. Reading one the kernel is still
    /// writing gives a table of inodes half-updated, and every CRC in
    /// it is wrong -- which reads exactly like a filesystem destroyed.
    ///
    /// Returns the ones still mounted.
    pub fn devices_quiet(&self) -> Vec<String> {
        let mut busy = Vec::new();
        for d in [&self.node.test_dev, &self.node.scratch_dev] {
            let out = self
                .run(&format!("grep -c '^/dev/{d} ' /proc/mounts || true"),
                     Duration::from_secs(20))
                .unwrap_or_default();
            if out.trim() != "0" && !out.trim().is_empty() {
                busy.push(format!("/dev/{d} is still mounted"));
            }
        }
        busy
    }

    /// Everything a run should prove before it starts.
    ///
    /// One call, because a check that has to be remembered separately
    /// is a check that gets forgotten. Returns every complaint; empty
    /// means the node is what the run is about to claim it is.
    pub fn ready_to_measure(&self, tools: &[(String, String)],
                            built: Option<std::time::SystemTime>) -> Vec<String> {
        let mut bad = self.tools_match(tools);
        bad.extend(self.devices_quiet());
        if let Some(t) = built {
            if let Some(k) = self.kernel_is_current(t) {
                bad.push(k);
            }
        }
        bad
    }

    /// What the node is doing right now, in one round trip.
    ///
    /// Sectors written to the scratch device, tasks in D, and the load.
    /// One call because each is an ssh round trip and a sweep samples
    /// this every thirty seconds for hours.
    pub fn vitals(&self) -> Option<(u64, usize, f32)> {
        let out = self
            .run(
                &format!(
                    // Both devices. generic/074 runs on TEST_DEV and the
                    // watcher read SCRATCH_DEV alone, so a test device
                    // that stopped for twenty-eight minutes was watched
                    // through a scratch device that had nothing to say.
                    "printf '%s %s %s\\n' \
                     \"$(awk '/ {} / || / {} / {{s += $10}} END {{print s + 0}}' /proc/diskstats)\" \
                     \"$(ps -eo state= | grep -c '^D')\" \
                     \"$(cut -d' ' -f1 /proc/loadavg)\"",
                    self.node.test_dev, self.node.scratch_dev
                ),
                Duration::from_secs(15),
            )
            .ok()?;

        let f: Vec<&str> = out.split_whitespace().collect();
        if f.len() < 3 {
            return None;
        }
        Some((
            f[0].parse().unwrap_or(0),
            f[1].parse().unwrap_or(0),
            f[2].parse().unwrap_or(0.0),
        ))
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
        let raw = self.run("cat /var/lib/beamfs-xfstests/results.txt 2>/dev/null", Duration::from_secs(20))?;
        Ok(raw
            .lines()
            .filter(|l| !l.starts_with("DONE"))
            .filter_map(|l| TestResult::parse(l, &self.node.name))
            .collect())
    }

    /// Where a shard stands.
    ///
    /// Three states, not two. A node that cannot be reached is neither
    /// working nor finished, and calling it either is wrong in a way
    /// that costs hours: unwrap_or(false) made an unreachable node
    /// "still working", so the run loop waited on it forever while
    /// three shards that had actually finished sat idle.
    pub fn shard_state(&self) -> ShardState {
        match self.run(
            "if grep -q '^STUCK' /var/lib/beamfs-xfstests/results.txt 2>/dev/null; then echo stuck; \
             elif grep -q '^DONE' /var/lib/beamfs-xfstests/results.txt 2>/dev/null; then echo done; \
             else echo running; fi",
            Duration::from_secs(15),
        ) {
            Ok(o) if o.contains("stuck") => ShardState::Stuck,
            Ok(o) if o.contains("done") => ShardState::Done,
            Ok(_) => ShardState::Running,
            Err(_) => ShardState::Unreachable,
        }
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
    /// Sectors written to the scratch device since boot.
    ///
    /// The stall detector needs to tell a node that is working from one
    /// that is wedged, and the count of tasks in D does not do it: a
    /// single writer in balance_dirty_pages sits in D for as long as the
    /// write lasts, which is exactly what a sustained test looks like.
    /// Two nodes were killed and restarted mid-campaign on that
    /// evidence, and each restart wiped the results file it was reading.
    ///
    /// A device whose write counter is moving is not stuck, whatever its
    /// tasks are doing.
    pub fn sectors_written(&self) -> u64 {
        self.run(
            &format!("grep ' {} ' /proc/diskstats | awk '{{print $10}}'",
                     self.node.scratch_dev),
            Duration::from_secs(15),
        )
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
    }

    pub fn blocked_tasks(&self) -> usize {
        self.run("ps -eo state | grep -c '^D'", Duration::from_secs(15))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Everything worth knowing about a node that has stopped moving.
    ///
    /// Collected in one round trip while the node still answers. A
    /// wedged filesystem takes the network with it soon after -- twice
    /// on 2026-09-01 the node went to "No route to host" before anyone
    /// had asked it anything -- so the window to gather evidence is
    /// short and closes without warning.
    ///
    /// Returns (stacks, dmesg, mounts). Any part may be empty if the
    /// node was already too far gone; empty strings are more useful
    /// than a failed call that returns nothing at all.
    pub fn stall_evidence(&self) -> (String, String, String) {
        // Every task in D and its whole stack, then SysRq w for the
        // ones /proc/<pid>/stack cannot show. Five tasks and twelve
        // frames was a choice made before anyone had read one: a
        // stack cut at twelve frames stops in the block layer, above
        // the filesystem function that took the lock.
        let stacks = self
            .run(
                "for p in $(ps -eo pid,state | awk '$2 ~ /D/ {print $1}'); do \
                 echo \"--- pid $p $(ps -o comm= -p $p) $(ps -o etime= -p $p) ---\"; \
                 sudo cat /proc/$p/stack 2>/dev/null; done; \
                 echo '--- sysrq w ---'; \
                 echo w | sudo tee /proc/sysrq-trigger >/dev/null 2>&1; sleep 1; \
                 echo '--- dirty ---'; grep -E '^(Dirty|Writeback|NFS_Unstable):' /proc/meminfo; \
                 for d in /sys/block/vd*/inflight; do echo \"$d $(cat $d)\"; done",
                Duration::from_secs(40),
            )
            .unwrap_or_default();

        // The whole ring. It was cleared when the test started, so
        // everything in it belongs to this test, including what SysRq
        // just added.
        let dmesg = self
            .run("sudo dmesg", Duration::from_secs(25))
            .unwrap_or_default();

        let mounts = self
            .run("mount | grep -E ' /mnt/(test|scratch) '; echo '--- df ---'; df -h /mnt/test /mnt/scratch 2>&1",
                 Duration::from_secs(20))
            .unwrap_or_default();

        (stacks, dmesg, mounts)
    }

    /// The harness output for a test that failed.
    ///
    /// The runner writes these under /tmp/xfs-failures as it goes. Read
    /// back rather than streamed, because a failing test produces
    /// hundreds of lines of diff and interleaving four nodes' worth of
    /// that into one terminal helps nobody.
    pub fn failure_log(&self, test: &str) -> Option<String> {
        let name = test.replace('/', "-");
        self.run(&format!("cat /tmp/xfs-failures/{name}.log 2>/dev/null"),
                 Duration::from_secs(20))
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Which failure logs the node has.
    pub fn failure_list(&self) -> Vec<String> {
        self.run("ls /tmp/xfs-failures/ 2>/dev/null | sed 's/\\.log$//'",
                 Duration::from_secs(20))
            .map(|s| s.lines().map(|l| l.replace('-', "/")).collect())
            .unwrap_or_default()
    }

    /// Kill the shard and release the mounts.
    ///
    /// Everything the trial started, not only the test script.
    ///
    /// check is launched as `timeout -k 5 1870 ./check generic/083`,
    /// a relative path, so the '/usr/xfstests/check' pattern never
    /// matched it and the parent outlived every stop. The capture
    /// phase outlives it too: dd copies a gigabyte of volume, zstd
    /// compresses it, bpftrace sits attached -- on 2026-09-19 a stop
    /// reported success while zstd ran on for another three minutes
    /// and the next run was refused the node it had just been told
    /// was free.
    pub fn stop(&self) {
        /*
         * Bracketed patterns, or the shell kills itself first.
         *
         * pkill -f matches against every process's whole command
         * line, and the ssh shell running these very commands carries
         * all of them in its own. The first pkill therefore found
         * itself and died, none of the later ones ever ran, and stop
         * reported success while the node stayed busy -- measured on
         * 2026-09-19: zstd still compressing a volume image six
         * minutes after a stop the harness called done, and the next
         * run refused the node it had just been told was free.
         *
         * "[x]fs-runner.sh" matches xfs-runner.sh and not the literal
         * text of this line. The trick is old and it is the only one
         * that works without knowing the shell's own pid.
         */
        let _ = self.run(
            "sudo pkill -9 -A -f '[x]fs-runner.sh' 2>/dev/null; \
             sudo pkill -9 -A -f '[t]ests/generic' 2>/dev/null; \
             sudo pkill -9 -A -f '[/]usr/xfstests/check' 2>/dev/null; \
             sudo pkill -9 -A -f '[c]heck generic/' 2>/dev/null; \
             sudo pkill -9 -A -f '[c]heck.img' 2>/dev/null; \
             sudo pkill -9 -A -x bpftrace 2>/dev/null; \
             sudo pkill -9 -A -x zstd 2>/dev/null; \
             sudo pkill -9 -A -x fsstress 2>/dev/null; \
             sudo pkill -9 -A -x fsx 2>/dev/null; \
             sleep 1; sudo umount -l /mnt/test /mnt/scratch 2>/dev/null; true",
            Duration::from_secs(30),
        );
    }

    /// What a trial left running, if anything.
    ///
    /// stop printed "stopped" whatever happened, so a node it had not
    /// freed looked exactly like one it had -- and the next run was
    /// refused by the claim guard with no explanation the caller could
    /// act on. This is what stop checks before it says it is done.
    ///
    /// Bracketed patterns here too: the grep would otherwise find the
    /// ssh command carrying it.
    #[must_use]
    pub fn leftover_work(&self) -> Vec<String> {
        let out = self.run(
            "ps -eo pid,etime,stat,args --no-headers | \
             grep -E '[c]heck generic/|[t]ests/generic|[z]std |[b]pftrace |[f]sstress|[f]sx |[x]fs-runner' \
             || true",
            Duration::from_secs(20),
        );
        out.map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn a_budget_overrun_does_not_claim_the_node_is_unreachable() {
        let e = NodeError::TimedOut { host: "10.0.0.1".into(), secs: 300 };
        let s = e.to_string();
        assert!(s.contains("300s"), "{s}");
        assert!(!s.contains("unreachable"), "{s}");
    }

    #[test]
    fn a_node_that_cannot_be_reached_still_says_so() {
        let e = NodeError::Unreachable("10.0.0.1".into());
        assert!(e.to_string().contains("unreachable"));
    }
}
