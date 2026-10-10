// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! The serial console of every node, captured for the whole run.
//!
//! A node that panics stops answering the network within the same
//! instant. The kernel prints the BUG, the registers and the call trace
//! to the console and stops; nothing of that reaches dmesg, because
//! there is no longer a machine to read dmesg on, and ssh gets "No
//! route to host" from a domain libvirt still reports as running.
//!
//! Reading the console after noticing the silence is too late: the
//! output has already gone past and a fresh read returns whatever
//! arrives next, which on a dead machine is nothing. Two nodes died
//! that way overnight with no record of why.
//!
//! So every node's console is written to a file from the moment the run
//! starts. When one goes quiet, the explanation is already on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

pub struct ConsoleSet {
    caps: BTreeMap<String, (Child, PathBuf)>,
    dir: PathBuf,
}

impl ConsoleSet {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self { caps: BTreeMap::new(), dir: dir.to_path_buf() }
    }

    /// Start capturing `node`'s console.
    ///
    /// Appends rather than truncates: a node restarted mid-run gets a
    /// second capture, and losing the first would lose the panic that
    /// caused the restart.
    pub fn start(&mut self, node: &str, domain: &str) -> bool {
        if self.caps.contains_key(node) {
            return true;
        }
        let Some(pty) = Self::pty_of(domain) else { return false };
        let path = self.dir.join(format!("console-{node}.log"));
        let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        else {
            return false;
        };
        // setsid so the reader is not in this process's terminal group:
        // a `sudo cat` on a pty that stays in the foreground group can
        // stop the shell that launched it.
        match Command::new("setsid")
            .args(["sudo", "cat", &pty])
            .stdin(Stdio::null())
            .stdout(Stdio::from(f))
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => {
                self.caps.insert(node.into(), (c, path));
                true
            }
            Err(_) => false,
        }
    }

    /// Stop capturing `node` -- before its domain is destroyed, since
    /// the pty goes with it.
    pub fn stop(&mut self, node: &str) {
        if let Some((mut c, _)) = self.caps.remove(node) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.caps.keys().cloned().collect();
        for n in names {
            self.stop(&n);
        }
    }

    /// The last `n` lines of a node's console.
    #[must_use]
    pub fn tail(&self, node: &str, n: usize) -> String {
        let Some((_, p)) = self.caps.get(node) else { return String::new() };
        let Ok(s) = std::fs::read_to_string(p) else { return String::new() };
        s.lines()
            .rev()
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Did this node's kernel die, and if so what did it say?
    ///
    /// Looks for the shapes a dying arm64 kernel prints. Returns the
    /// panic and the lines around it, which is the whole of what is
    /// ever recoverable from a node in this state.
    #[must_use]
    pub fn panic_of(&self, node: &str) -> Option<String> {
        let (_, p) = self.caps.get(node)?;
        let s = std::fs::read_to_string(p).ok()?;
        let idx = s.find("Kernel panic")
            .or_else(|| s.find("Internal error: Oops"))
            .or_else(|| s.find("kernel BUG at"))?;
        // From a little before the marker to the end: the BUG line comes
        // first, then registers and the trace, and the trace is the part
        // that says where.
        let from = s[..idx].rfind("cut here").unwrap_or(idx.saturating_sub(400));
        Some(s[from..].chars().take(4000).collect())
    }

    #[must_use]
    pub fn path_of(&self, node: &str) -> Option<&Path> {
        self.caps.get(node).map(|(_, p)| p.as_path())
    }

    fn pty_of(domain: &str) -> Option<String> {
        let out = Command::new("sudo")
            .args(["virsh", "-c", "qemu:///system", "ttyconsole", domain])
            .output()
            .ok()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        p.starts_with("/dev/pts/").then_some(p)
    }
}

impl Drop for ConsoleSet {
    /// Kill the readers even on an unusual exit.
    ///
    /// A `sudo cat` left behind holds a pty open and outlives the
    /// program; enough of them and the host runs out.
    fn drop(&mut self) {
        self.stop_all();
    }
}
