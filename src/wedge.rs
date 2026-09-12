// SPDX-License-Identifier: GPL-2.0-only
//! What the node was doing when it stopped answering.
//!
//! ssh is gone, the test is over, and the interesting part is inside a
//! guest that is still running. sysrq answers when nothing else does:
//! it is handled in the interrupt path, so a kernel deadlocked on a
//! mutex still prints.
//!
//! Three keys, in this order. `w` lists tasks in uninterruptible sleep
//! -- the ones waiting on I/O or a lock -- with a stack for each, and
//! that is usually the whole answer. `l` gives a backtrace per CPU,
//! which says whether anything is still running or they are all
//! parked. `m` dumps the memory state, which distinguishes a deadlock
//! from an allocation that cannot be satisfied.
//!
//! Everything here is best-effort. A capture that fails must not turn
//! a wedged node into a failed campaign -- the node is already lost
//! and the run is already stopping.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// How long to let the guest print before reading the console back.
///
/// sysrq-t on a busy node produces thousands of lines and the pty is
/// not fast. Under-waiting truncates the one stack that mattered.
const PRINT_SETTLE: Duration = Duration::from_secs(6);

/// Ask a wedged guest what it is doing, and keep the answer.
///
/// Returns the number of console bytes captured, or an error when
/// there was nothing to capture. Called with the VM still running:
/// the console is a pty and a destroyed domain has none.
pub fn capture_wedged(vm: &str, dir: &Path) -> Result<u64, String> {
    let _ = std::fs::create_dir_all(dir);

    let console = dir.join("console.txt");
    let out = std::fs::File::create(&console)
        .map_err(|e| format!("create {}: {e}", console.display()))?;

    // Reading starts first: sysrq output goes to the console as it is
    // produced, and a reader attached afterwards gets whatever the pty
    // buffer still holds, which is not all of it.
    let mut reader = Command::new("sudo")
        .args(["virsh", "console", vm, "--force"])
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone().map_err(|e| e.to_string())?)
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn virsh console: {e}"))?;

    std::thread::sleep(Duration::from_secs(1));

    for (key, what) in [
        ("KEY_W", "blocked tasks"),
        ("KEY_L", "per-CPU backtraces"),
        ("KEY_M", "memory state"),
    ] {
        println!("    asking the node for {what}");
        let _ = Command::new("sudo")
            .args(["virsh", "send-key", vm, "KEY_LEFTALT", "KEY_SYSRQ", key])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::thread::sleep(PRINT_SETTLE);
    }

    let _ = reader.kill();
    let _ = reader.wait();

    let size = std::fs::metadata(&console).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = std::fs::remove_file(&console);
        return Err("the console gave nothing back".into());
    }

    summarise(&console, dir);
    Ok(size)
}

/// Say what the capture contains, so the log carries the finding.
///
/// A file on disk nobody opens is not a diagnosis. The counts below
/// are what separated a deadlock from memory exhaustion on 2026-09-12:
/// one task in __mutex_lock and twenty in folio_wait_bit_common is a
/// lock-order problem, and every task in __alloc_pages is not.
fn summarise(console: &Path, dir: &Path) {
    let Ok(text) = std::fs::read_to_string(console) else { return };

    let mut blocked = 0usize;
    let mut in_mutex = 0usize;
    let mut in_folio = 0usize;
    let mut in_alloc = 0usize;
    let mut beamfs: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.contains("state:D") {
            blocked += 1;
        }
        if line.contains("__mutex_lock") {
            in_mutex += 1;
        }
        if line.contains("folio_wait_bit_common") {
            in_folio += 1;
        }
        if line.contains("__alloc_pages") || line.contains("alloc_frozen_pages") {
            in_alloc += 1;
        }
        if let Some(i) = line.find("beamfs_") {
            let f: String = line[i..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !beamfs.contains(&f) {
                beamfs.push(f);
            }
        }
    }

    if blocked == 0 && in_mutex == 0 && in_folio == 0 {
        println!("    console kept, nothing recognisable in it");
        return;
    }

    println!("    {blocked} task(s) in uninterruptible sleep");
    if in_mutex > 0 {
        println!("      {in_mutex} waiting on a mutex");
    }
    if in_folio > 0 {
        println!("      {in_folio} waiting on a folio");
    }
    if in_alloc > 0 {
        println!("      {in_alloc} in page allocation");
    }
    if in_mutex > 0 && in_folio > in_mutex {
        println!("      -- one holder, many waiters: a lock order, not memory");
    }
    if !beamfs.is_empty() {
        let shown: Vec<&String> = beamfs.iter().take(6).collect();
        println!("      beamfs frames: {}", shown.iter()
            .map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
    }

    let note = dir.join("wedge-summary.txt");
    let mut s = format!("blocked={blocked}\nmutex={in_mutex}\nfolio={in_folio}\nalloc={in_alloc}\n");
    for f in &beamfs {
        s.push_str(&format!("frame={f}\n"));
    }
    let _ = std::fs::write(note, s);
}
