// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! A bpftrace script, running while a test runs.
//!
//! Everything else the harness keeps is read after the fact: a frozen
//! volume, a dmesg, a checker run. What is left to find are races --
//! a pointer installed and read back as zero twenty-five seconds later
//! -- and what a race leaves behind is not the race.
//!
//! The scripts are files under `scripts/`, not strings in here. They
//! get read by people, reviewed, changed without a rebuild, and sent to
//! whoever is running the filesystem under a beam.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::node::NodeConn;

/// Where the scripts live on the workstation.
///
/// Beside the binary's source rather than installed: they are edited
/// far more often than the harness is, and a script somebody changed
/// for one run is exactly what should not need a package rebuild.
pub fn script_root() -> PathBuf {
    if let Ok(p) = std::env::var("XFSTESTS_BPF_SCRIPTS") {
        return PathBuf::from(p);
    }
    // The repo, when running from it; the installed copy otherwise.
    for c in [
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts")),
        PathBuf::from("/usr/share/beamfs-xfstests/scripts"),
    ] {
        if c.is_dir() {
            return c;
        }
    }
    PathBuf::from("scripts")
}

/// What is available to run.
pub fn available() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let Ok(d) = std::fs::read_dir(script_root()) else { return v };
    for e in d.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(stem) = n.strip_suffix(".bt") {
            v.push(stem.to_string());
        }
    }
    v.sort();
    v
}

/// A script attached to a node, running until it is stopped.
pub struct Running {
    node: String,
    remote_out: String,
    pub name: String,
}

/// Start @script on the node.
///
/// The script is pushed to /tmp there and run with setsid so it
/// outlives the ssh channel that started it -- the harness's own
/// connection comes and goes, and a probe that dies with it measures
/// the first second of a test.
pub fn start(conn: &NodeConn, script: &str) -> Result<Running, String> {
    let local = script_root().join(format!("{script}.bt"));
    if !local.is_file() {
        return Err(format!("no such script: {}", local.display()));
    }

    // Is bpftrace even there? Answering now beats a run that measures
    // nothing and says so at the end.
    if conn.run("command -v bpftrace", Duration::from_secs(15)).is_err() {
        return Err("the node has no bpftrace".into());
    }

    let remote = format!("/tmp/bx-{script}.bt");
    let remote_out = format!("/tmp/bx-{script}.out");
    conn.push(&local.to_string_lossy(), &remote)
        .map_err(|e| format!("push {script}: {e:?}"))?;

    let cmd = format!(
        "sudo rm -f {remote_out}; \
         sudo setsid bpftrace {remote} > {remote_out} 2>&1 < /dev/null & \
         sleep 2; \
         grep -q 'Attaching' {remote_out} && echo attached || \
           (echo failed; cat {remote_out})");
    let out = conn.run(&cmd, Duration::from_secs(45))
        .map_err(|e| format!("start {script}: {e:?}"))?;
    if !out.contains("attached") {
        return Err(format!("{script} did not attach: {}", out.trim()));
    }

    Ok(Running {
        node: conn.node.host.clone(),
        remote_out,
        name: script.to_string(),
    })
}

impl Running {
    /// Stop it and bring its output back.
    ///
    /// SIGINT rather than SIGKILL: bpftrace prints its maps in the END
    /// block, and the counts are most of what the script is for.
    pub fn stop_into(self, conn: &NodeConn, dir: &Path) -> Option<(PathBuf, u64)> {
        let _ = conn.run(
            "sudo pkill -INT -x bpftrace || true",
            Duration::from_secs(20));
        // Give END time to print. A kill here truncates the maps.
        std::thread::sleep(Duration::from_secs(3));
        let _ = conn.run("sudo pkill -x bpftrace || true",
                         Duration::from_secs(15));

        let _ = std::fs::create_dir_all(dir);
        let local = dir.join(format!("bpf-{}.txt", self.name));
        conn.pull(&self.remote_out, &local.to_string_lossy()).ok()?;
        let _ = conn.run(&format!("sudo rm -f {}", self.remote_out),
                         Duration::from_secs(15));

        let size = std::fs::metadata(&local).ok()?.len();
        if size == 0 {
            let _ = std::fs::remove_file(&local);
            return None;
        }
        Some((local, size))
    }

    /// Where it is running, for a message.
    pub fn node(&self) -> &str {
        &self.node
    }
}

/// Say what a capture found, so the file is not the only record.
///
/// Narrow on purpose: the lines a script prints when it sees the thing
/// it watches for, and the totals. The histograms and the per-block
/// maps stay in the file.
pub fn speak(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };

    let mut findings: Vec<&str> = Vec::new();
    let mut totals: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("LOST ") || t.starts_with("  installed by ") {
            findings.push(t);
        } else if t.starts_with('@') && t.contains(':') && !t.contains('[') {
            totals.push(t);
        }
    }

    if findings.is_empty() && totals.is_empty() {
        return;
    }
    println!("    {} saw:", path.file_name().unwrap_or_default().to_string_lossy());
    for f in findings.iter().take(6) {
        println!("      {f}");
    }
    if findings.len() > 6 {
        println!("      and {} more", findings.len() - 6);
    }
    for t in totals.iter().take(10) {
        println!("      {t}");
    }
}
