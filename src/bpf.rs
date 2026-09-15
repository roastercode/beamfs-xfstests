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

    // Not /tmp, and not named .out.
    //
    // xfstests' check does "rm -f /tmp/*.rawout /tmp/*.out /tmp/*.err
    // /tmp/*.time" at line 548, between the test and its verification.
    // A probe writing /tmp/bx-lostptr.out attaches, runs, records
    // everything, and has its file deleted by the harness it is
    // watching -- which is what "the probe brought nothing back" was.
    let dir = "/var/tmp/beamfs-bx";
    let remote = format!("{dir}/{script}.bt");
    let remote_out = format!("{dir}/{script}.trace");
    conn.run(&format!("sudo mkdir -p {dir} && sudo chmod 1777 {dir}"),
             Duration::from_secs(20))
        .map_err(|e| format!("{script}: cannot make {dir}: {e:?}"))?;
    conn.push(&local.to_string_lossy(), &remote)
        .map_err(|e| format!("{script}: could not be copied to the node: {e:?}"))?;

    // Arrived, and the right size.
    //
    // A transfer that reports success and lands nothing is what put a
    // stale checker on this node once already.
    let there = conn
        .run(&format!("stat -c %s {remote} 2>/dev/null || echo 0"),
             Duration::from_secs(20))
        .unwrap_or_default();
    let want = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
    if there.trim().parse::<u64>().unwrap_or(0) != want {
        return Err(format!(
            "{script}: {} bytes here, {} on the node",
            want, there.trim()));
    }

    let cmd = format!(
        // The redirection inside sudo, and no quotes on the pattern.
            //
            // Those quotes closed the ones sudo sh -c opened, so
            // the node received a malformed command: the probe
            // started, the check never ran, and the harness said
            // the script had not reached the node. Typed by hand it
            // worked, because the local shell re-quoted it.
        //
        // "sudo rm -f X; sudo cmd > X" has the unprivileged shell
        // create X after root removed it, and root's output goes
        // somewhere the shell cannot read back. The probe attached
        // fine and the harness reported "bpftrace said nothing at all"
        // twice.
        /*
         * Whatever is still attached, first.
         *
         * A bpftrace left from an earlier sweep holds the
         * tracepoints and the new one exits without attaching.
         * The same command works by hand because a hand starts
         * by killing what is there.
         */
        /*
         * Wait for it, rather than guess at it.
         *
         * Three seconds and a grep was wrong both ways: it reported
         * failure on a probe that had attached and was writing, and it
         * reported success on one that would die a second later. The
         * message that followed named a cause it could not know -- "the
         * script may not have reached the node" while the script sat on
         * the node -- and three fixes today moved that symptom without
         * removing it.
         *
         * bpftrace prints "Attaching N probes" when it is ready, so:
         * poll for it, up to fifteen seconds, and say what the file
         * holds if it never comes.
         */
        "sudo pkill -x bpftrace 2>/dev/null; sleep 1; \
         sudo sh -c 'rm -f {remote_out}; \
         setsid bpftrace {remote} > {remote_out} 2>&1 < /dev/null &' ; \
         for i in $(seq 1 30); do \
           if grep -q Attaching {remote_out} 2>/dev/null; then break; fi; \
           if ! pgrep -x bpftrace >/dev/null; then break; fi; \
           sleep 0.5; \
         done; \
         if grep -q Attaching {remote_out} 2>/dev/null && \
            pgrep -x bpftrace >/dev/null; then echo BX_ATTACHED; \
         else echo BX_FAILED; \
              echo \"--- bpftrace said ---\"; cat {remote_out} 2>/dev/null; \
              echo \"--- running: $(pgrep -c bpftrace) ---\"; fi");
    let out = conn.run(&cmd, Duration::from_secs(45))
        .map_err(|e| format!("start {script}: {e:?}"))?;
    if !out.contains("BX_ATTACHED") {
        /*
         * Kept anyway.
         *
         * The check can be wrong -- it looks for one word in output
         * that a warning can push out of reach -- and the capture is
         * on the node either way. Ten sweeps went by with the probe
         * attached, the capture sitting there, and the harness
         * reporting that the script had not arrived.
         */
        let keep = std::path::Path::new("/tmp")
            .join(format!("bpf-{script}-unclaimed.txt"));
        if conn.pull(&remote_out, &keep.to_string_lossy()).is_ok() {
            if let Ok(m) = std::fs::metadata(&keep) {
                if m.len() > 0 {
                    println!("    {script}: the capture is at {} ({} bytes)",
                             keep.display(), m.len());
                }
            }
        }

        // What bpftrace said, not that something went wrong.
        //
        // The first version reported "did not attach: failed" and the
        // reason -- a tracepoint field named slotval where the script
        // said val -- stayed on the node in a file nobody fetched.
        let why: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.contains("BX_FAILED"))
            .take(4)
            .collect();
        // Silence is not bpftrace's answer, it is the absence of one.
        //
        // "bpftrace said nothing at all" named the wrong culprit when
        // the push had failed and there was no script on the node to
        // run. What the harness knows is that it could not tell, and
        // saying so sends the reader to the right place.
        /*
         * What the node actually said.
         *
         * "may not have reached the node" was printed ten times in one
         * sweep while the script sat on the node and bpftrace was
         * attached to it: the check looks for "Attaching" and the
         * capture had scrolled past it, or a warning came first. A
         * message that names the wrong cause is worse than none.
         */
        return Err(if why.is_empty() {
            format!("{script}: the node gave no sign either way -- \
                     the capture is in the case directory")
        } else {
            format!("{script}: {}", why.join(" | "))
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture with findings in it is summarised; one without is
    /// not mistaken for one.
    #[test]
    fn speak_finds_what_a_script_reported() {
        let d = std::env::temp_dir().join(format!("bxb-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("bpf-lostptr.txt");
        std::fs::write(&p, "Attaching 4 probes...\n\
             LOST parent=18450 slot=433 held=18714 read as 0, now 18522\n\
               installed by fsstress, overwritten by kworker/u18:1\n\
             @lost: 62\n\
             @reads: 24856\n").unwrap();
        // No assertion on the printing itself -- it goes to stdout --
        // but it must not panic on a real capture, which is what a
        // malformed line would do.
        speak(&p);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An empty capture is read without complaint.
    #[test]
    fn speak_survives_an_empty_capture() {
        let d = std::env::temp_dir().join(format!("bxb2-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("bpf-none.txt");
        std::fs::write(&p, "").unwrap();
        speak(&p);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file that is not there is not a crash.
    #[test]
    fn speak_survives_a_missing_file() {
        speak(std::path::Path::new("/nonexistent/bpf-nothing.txt"));
    }

    /// available() lists stems, not filenames: XFSTESTS_BPF takes a
    /// name, and a user typing "lostptr.bt" should not be the one to
    /// discover that.
    #[test]
    fn available_lists_stems() {
        let d = std::env::temp_dir().join(format!("bxb3-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("one.bt"), "BEGIN{}").unwrap();
        std::fs::write(d.join("notascript.txt"), "x").unwrap();
        // SAFETY: single-threaded test, and the variable is read once
        // by script_root() below.
        unsafe { std::env::set_var("XFSTESTS_BPF_SCRIPTS", &d); }
        let v = available();
        assert_eq!(v, vec!["one".to_string()], "{v:?}");
        unsafe { std::env::remove_var("XFSTESTS_BPF_SCRIPTS"); }
        let _ = std::fs::remove_dir_all(&d);
    }
}
