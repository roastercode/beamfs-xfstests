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

/// What one short command on the node is allowed.
///
/// Read while the node is preparing the volume for the test, which
/// is the busiest it gets: two separate round trips of twenty
/// seconds each timed out there and the probe never attached.
const NODE_ASK_SECS: u64 = 45;

/// How long bpftrace is given to compile and attach, in seconds.
///
/// One number, because two were kept apart and drifted: a shell loop
/// polled for up to sixty seconds while the ssh call carrying it
/// allowed forty-five, so the connection expired before the loop could
/// finish and a probe still compiling was reported as a node that had
/// timed out. Raising one of the two only moves that.
///
/// Sixty rather than fifteen: a script with kstack on three
/// tracepoints took eighteen seconds, and one with six probes and two
/// struct accesses more. The loop exits as soon as bpftrace says it
/// has attached, so the ceiling costs nothing when the probe is quick.
const ATTACH_SECS: u64 = 60;

/// Half-second ticks the poll loop runs, derived from ATTACH_SECS.
const fn attach_ticks() -> u64 {
    ATTACH_SECS * 2
}

/// What the ssh call carrying that loop is allowed, derived from it.
///
/// The loop plus what the transport costs around it: the connection,
/// the pkill, and bpftrace's own exit. It must exceed the loop or the
/// loop's own verdict is never read.
const fn attach_budget() -> Duration {
    Duration::from_secs(ATTACH_SECS + 30)
}

/// A script attached to a node, running until it is stopped.
pub struct Running {
    node: String,
    remote_out: String,
    pub name: String,
}

/// The dev_t of this node's two volumes, as the kernel numbers them.
///
/// Every beamfs tracepoint carries the volume it belongs to, because
/// the node's own root is beamfs too: a probe with no filter counts
/// the rootfs's allocations alongside the test's, and 2796 allocations
/// on a volume of 262144 blocks were mostly inode 27192 of /dev/vda.
///
/// Read from the node rather than assumed: the harness names devices
/// by short name and their numbers are the kernel's to choose.
///
/// One call for both, and a second attempt: this runs while the node
/// is preparing the volume for the test, which is the busiest it gets.
/// Two separate round trips at twenty seconds each timed out there and
/// the probe never attached, which cost a run.
fn devnums(conn: &NodeConn, a: &str, b: &str) -> Result<(u64, u64), String> {
    let cmd = format!("stat -Lc '%t %T' /dev/{a} /dev/{b}");
    let mut last = String::new();

    for attempt in 1..=2 {
        match conn.run(&cmd, Duration::from_secs(NODE_ASK_SECS)) {
            Ok(out) => {
                let mut f = out.split_whitespace();
                let mut next = || -> Result<u64, String> {
                    let v = f.next().ok_or_else(|| format!("short answer: {out:?}"))?;
                    u64::from_str_radix(v, 16)
                        .map_err(|_| format!("not a device number: {v:?}"))
                };
                let (ma, mi) = (next()?, next()?);
                let (mb, nb) = (next()?, next()?);
                return Ok(((ma << 20) | mi, (mb << 20) | nb));
            }
            Err(e) => {
                last = format!("{e:?}");
                if attempt == 1 {
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }
    Err(format!("cannot read the device numbers of /dev/{a} and /dev/{b}: {last}"))
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
    // The volumes this test uses, substituted into the script.
    //
    // A script filters with `/args.dev == BX_SCRATCH_DEV/`; the tokens
    // are replaced here so the same file works on any node. A script
    // that names neither is pushed unchanged and sees every beamfs
    // mount, the node's root included.
    let mut text = std::fs::read_to_string(&local)
        .map_err(|e| format!("{script}: cannot be read: {e}"))?;
    let sent = if text.contains("BX_TEST_DEV") || text.contains("BX_SCRATCH_DEV") {
        let (t, s) = devnums(conn, &conn.node.test_dev,
                             &conn.node.scratch_dev)?;
        text = text
            .replace("BX_TEST_DEV", &t.to_string())
            .replace("BX_SCRATCH_DEV", &s.to_string());
        let tmp = std::env::temp_dir().join(format!("bx-{script}-{}.bt",
                                                    std::process::id()));
        std::fs::write(&tmp, &text)
            .map_err(|e| format!("{script}: cannot be staged: {e}"))?;
        tmp
    } else {
        local.clone()
    };

    conn.push(&sent.to_string_lossy(), &remote)
        .map_err(|e| format!("{script}: could not be copied to the node: {e:?}"))?;

    // Arrived, and the right size.
    //
    // A transfer that reports success and lands nothing is what put a
    // stale checker on this node once already.
    let there = conn
        .run(&format!("stat -c %s {remote} 2>/dev/null || echo 0"),
             Duration::from_secs(20))
        .unwrap_or_default();
    let want = std::fs::metadata(&sent).map(|m| m.len()).unwrap_or(0);
    if there.trim().parse::<u64>().unwrap_or(0) != want {
        return Err(format!(
            "{script}: {} bytes here, {} on the node",
            want, there.trim()));
    }

    let ticks = attach_ticks();
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
         * poll for it and say what the file holds if it never comes.
         *
         * ATTACH_SECS, not fifteen seconds: a script with kstack on three
         * tracepoints took eighteen to compile and attach, and was
         * reported as failed while it ran for the next twenty-three
         * minutes. The loop exits as soon as it sees the word, so the
         * ceiling costs nothing when the probe is quick.
         */
        "sudo pkill -x bpftrace 2>/dev/null; sleep 1; \
         sudo sh -c 'rm -f {remote_out}; \
         setsid env BPFTRACE_MAX_MAP_KEYS=1000000 bpftrace {remote} > {remote_out} 2>&1 < /dev/null &' ; \
         for i in $(seq 1 {ticks}); do \
           if grep -q Attaching {remote_out} 2>/dev/null; then break; fi; \
           if ! pgrep -x bpftrace >/dev/null; then break; fi; \
           sleep 0.5; \
         done; \
         sleep 1; \
         if grep -q Attaching {remote_out} 2>/dev/null && \
            ! grep -q ERROR {remote_out} 2>/dev/null && \
            pgrep -x bpftrace >/dev/null; then echo BX_ATTACHED; \
         else echo BX_FAILED; \
              echo \"--- bpftrace said ---\"; cat {remote_out} 2>/dev/null; \
              echo \"--- running: $(pgrep -c bpftrace) ---\"; fi");
    let out = conn.run(&cmd, attach_budget())
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
        // SIGINT, then wait for bpftrace to leave on its own: END
        // prints every map, and a script with tens of thousands of
        // keys needs longer than a fixed pause. On 2026-09-23 three
        // seconds cut the listing in the middle of @upd_ns, and the
        // two maps the run was for were never written. SIGKILL only
        // when it has not left after three minutes.
        let _ = conn.run(
            "sudo pkill -INT -x bpftrace || true; \
             for i in $(seq 1 360); do \
               pgrep -x bpftrace >/dev/null 2>&1 || break; \
               sleep 0.5; \
             done; \
             if pgrep -x bpftrace >/dev/null 2>&1; then \
               echo BX_BPF_KILLED; sudo pkill -x bpftrace || true; \
             fi",
            Duration::from_secs(200));

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
/// Whether bpftrace dropped entries it will never mention again.
///
/// A map past its key limit prints this once and carries on. Everything
/// printed afterwards looks like a complete answer and is not one, so a
/// script whose output says this has produced no usable list -- only a
/// lower bound.
pub fn map_overflowed(text: &str) -> bool {
    text.contains("Map full")
}

/// What a capture says, as lines to print.
///
/// Findings first (LOST, ERROR, WARNING), then every scalar map,
/// bracketed or not: `@with_buffers[1]: 521` was the figure that
/// closed 3.15 on 2026-09-22 and the report never showed it, because
/// a bracket was taken for a stack. A keyed map of more than eight
/// entries is counted rather than listed -- forty block numbers are a
/// list to read in the file. A histogram is printed with its rows;
/// its name alone said nothing. Stacks stay in the capture.
#[must_use]
pub fn summarise(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut findings: Vec<String> = Vec::new();
    let mut scalars: Vec<String> = Vec::new();
    let mut keyed: Vec<(String, String)> = Vec::new();
    let mut hists: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with("LOST ") || t.starts_with("installed by ")
            || t.starts_with("ERROR") || t.contains("WARNING:") {
            if !findings.iter().any(|f| f == t) {
                findings.push(t.to_string());
            }
        } else if t.starts_with('@') && t.ends_with(':') && !t.contains('[') {
            // A histogram: its rows follow until a blank line.
            let mut rows = vec![t.to_string()];
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().is_empty() {
                rows.push(format!("  {}", lines[j].trim()));
                j += 1;
            }
            if rows.len() > 1 && !hists.contains(&rows[0]) {
                hists.extend(rows);
            }
            i = j;
            continue;
        } else if t.starts_with('@') && !t.ends_with('[') {
            if let Some((k, v)) = t.rsplit_once(':') {
                if v.trim().parse::<i64>().is_ok() {
                    if let Some((name, _)) = k.split_once('[') {
                        if !keyed.iter().any(|(_, l)| l == t) {
                            keyed.push((name.to_string(), t.to_string()));
                        }
                    } else if !scalars.iter().any(|l| l == t) {
                        scalars.push(t.to_string());
                    }
                }
            }
        }
        i += 1;
    }

    let mut out = findings;
    out.extend(scalars);
    let mut names: Vec<String> = Vec::new();
    for (n, _) in &keyed {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    for n in names {
        let rows: Vec<&String> = keyed.iter().filter(|(k, _)| *k == n).map(|(_, l)| l).collect();
        if rows.len() <= 8 {
            out.extend(rows.into_iter().cloned());
        } else {
            out.push(format!("{n}: {} entries, in the capture", rows.len()));
        }
    }
    out.extend(hists);
    out
}

pub fn speak(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };

    if map_overflowed(&text) {
        println!("    {} OVERFLOWED its map: what follows is incomplete, \
                  and how incomplete is not knowable from it",
                 path.file_name().unwrap_or_default().to_string_lossy());
    }

    let lines = summarise(&text);
    if lines.is_empty() {
        return;
    }
    println!("    {} saw:", path.file_name().unwrap_or_default().to_string_lossy());
    for l in &lines {
        println!("      {l}");
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

    /// The figure that closes a case is a bracketed one as often as
    /// not, and a histogram's name is not its rows.
    #[test]
    fn a_keyed_count_is_spoken_a_long_map_is_counted_and_a_histogram_has_rows() {
        let text = "Attaching 3 probes...\n\
             watching folios leave the page cache\n\
             @held_from[\n        delete_from_page_cache_batch+278\n]: 8\n\
             @removed: 280813\n\
             @with_buffers[1]: 521\n\
             @held_ino[1]: 1\n@held_ino[2]: 1\n@held_ino[3]: 1\n@held_ino[4]: 1\n\
             @held_ino[5]: 1\n@held_ino[6]: 1\n@held_ino[7]: 1\n@held_ino[8]: 1\n\
             @held_ino[9]: 1\n@held_ino[10]: 1\n\
             @bh_count:\n[1]                  519 |@@@@|\n[2, 4)                 2 |    |\n\n\
             @excess:\n\n";
        let s = summarise(text);
        assert!(s.contains(&"@with_buffers[1]: 521".to_string()), "{s:?}");
        assert!(s.contains(&"@removed: 280813".to_string()), "{s:?}");
        assert!(s.iter().any(|l| l.starts_with("@held_ino: 10 entries")), "{s:?}");
        assert!(s.iter().any(|l| l.contains("[1]") && l.contains("519")), "{s:?}");
        assert!(!s.iter().any(|l| l.contains("delete_from_page_cache_batch")),
                "a stack was spoken: {s:?}");
        assert!(!s.iter().any(|l| l == "@excess:"), "an empty histogram was spoken: {s:?}");
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
        let _g = crate::env_lock();

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

#[cfg(test)]
mod attach_tests {
    use super::*;

    /// The ordering the two constants used to break by drifting apart.
    #[test]
    fn the_ssh_budget_outlasts_the_poll_loop() {
        let loop_secs = attach_ticks() / 2;
        assert!(
            attach_budget().as_secs() > loop_secs,
            "budget {}s does not outlast a loop of {}s",
            attach_budget().as_secs(),
            loop_secs
        );
    }

    /// And by enough to read the loop's verdict, not just to reach it.
    #[test]
    fn the_margin_is_not_a_hair() {
        let loop_secs = attach_ticks() / 2;
        assert!(attach_budget().as_secs() >= loop_secs + 15);
    }

    /// The loop is expressed in half-second ticks.
    #[test]
    fn ticks_are_half_seconds() {
        assert_eq!(attach_ticks(), ATTACH_SECS * 2);
    }
}


#[cfg(test)]
mod overflow_tests {
    use super::*;

    #[test]
    fn a_full_map_is_recognised() {
        assert!(map_overflowed("Map full; can't update element\n@owner[12]: 3\n"));
    }

    #[test]
    fn an_ordinary_capture_is_not_an_overflow() {
        assert!(!map_overflowed("Attaching 2 probes...\n@owner[12]: 3\n"));
    }
}
