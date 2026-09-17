// SPDX-License-Identifier: GPL-2.0-only
//! A live indicator for work happening on a node.
//!
//! Distinct from progress.rs, which follows the test suite: this one
//! follows one loop on one node, driven by the step file the remote
//! script writes.
//!
//! Written once here rather than in each command that runs loops. It
//! existed in `trace` and not in `matrix`, so the matrix printed a loop
//! number and then sat still for fifteen seconds a loop -- which reads
//! as a hung program, and got a run killed for looking stuck. The same
//! mistake twice is a sign the code was in the wrong place.
//!
//! What turns here is the work, not a timer. The remote loop writes the
//! step it has reached to /tmp/beamfs-step and this reads it: an ssh
//! call that has hung leaves a timer spinning exactly as it spins during
//! useful work, so a timer answers the wrong question.
//!
//! Thresholds come from measurement. Loops on the x86 node run 11 to 16
//! seconds, median 14. Past 30 seconds in one step something is slow and
//! the step is named; past 60 it is stuck, and the marker blinks a hash
//! rather than spinning, because a spinning marker cannot be told from
//! progress.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/*
 * What counts as slow, and what counts as stuck.
 *
 * These were set when a generic/464 trial took ninety seconds. With
 * indirect parity on, a healthy trial now runs four hundred, and the
 * longest tests in the suite run far past that -- generic/522 takes
 * nineteen minutes on its own. A run overnight printed "STALLED 8068s"
 * about a campaign that was working perfectly, which is the same as
 * printing nothing: a threshold that fires on normal behaviour stops
 * being a signal.
 *
 * Slow is now five minutes and stuck is thirty. A test that has made no
 * progress for half an hour is worth looking at; one that has been
 * working for ten minutes is just working.
 */
const SLOW_AFTER: u64 = 300;
const STALLED_AFTER: u64 = 1800;

/// A running indicator. Dropping it, or calling `finish`, stops it.
pub struct Progress {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    started: Instant,
    label: String,
}

impl Progress {
    /// Start an indicator labelled `label`, polling the step file on
    /// `host` with `key`.
    ///
    /// The thread gets owned strings: it outlives the borrows of
    /// whatever config it was started from.
    pub fn start(label: &str, key: &str, host: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(true));
        let mine = stop.clone();
        let (k, h, l) = (key.to_string(), host.to_string(), label.to_string());
        let started = Instant::now();
        let shown = l.clone();

        let handle = std::thread::spawn(move || {
            let frames = ['|', '/', '-', '\\'];
            let mut i = 0usize;
            let mut step = String::from("start");
            let mut step_since = Instant::now();

            while mine.load(Ordering::Relaxed) {
                if let Some(now) = read_step(&k, &h) {
                    if now != step && !now.is_empty() {
                        step = now;
                        step_since = Instant::now();
                    }
                }
                let held = step_since.elapsed().as_secs();
                let total = started.elapsed().as_secs();

                let mark = if held >= STALLED_AFTER {
                    if i % 4 < 2 {
                        '#'
                    } else {
                        ' '
                    }
                } else {
                    frames[i % 4]
                };
                let note = if held >= STALLED_AFTER {
                    format!("  STALLED {held}s in {step}")
                } else if held >= SLOW_AFTER {
                    format!("  slow, {held}s in {step}")
                } else {
                    String::new()
                };

                print!("\r  {shown:<34} {mark} {total:>3}s  {step:<9}{note}          ");
                let _ = std::io::stdout().flush();
                i += 1;
                // Slower when a step is dragging: a fast spinner next to
                // the word STALLED is a mixed message.
                std::thread::sleep(Duration::from_millis(if held >= SLOW_AFTER {
                    600
                } else {
                    250
                }));
            }
        });

        Progress { stop, handle: Some(handle), started, label: label.to_string() }
    }

    /// Stop the indicator and replace its line with `verdict`.
    pub fn finish(mut self, verdict: &str) {
        self.stop.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        println!(
            "\r  {:<34} {verdict}   {}s                              ",
            self.label,
            self.started.elapsed().as_secs()
        );
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        // An early return or a `?` must not leave a thread drawing over
        // whatever is printed next.
        self.stop.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The step the remote loop has reached, at most once a second.
///
/// Returns None when it is not time to ask again, so the caller keeps
/// what it had rather than blanking the display between polls. Adding
/// load to the node under test would change what is being measured.
fn read_step(key: &str, host: &str) -> Option<String> {
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};
    static LAST: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now == LAST.load(Ordering::Relaxed) {
        return None;
    }
    LAST.store(now, Ordering::Relaxed);

    std::process::Command::new("timeout")
        // The same options NodeConn uses.
        //
        // A redeploy gives the node a new host key, and a caller
        // checking known_hosts stops on a warning that is not about
        // this: it appeared in the middle of evidence collection and
        // read like a finding.
        .args(["5", "ssh", "-i", key])
        .args(["-o", "StrictHostKeyChecking=no"])
        .args(["-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "LogLevel=ERROR"])
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "ConnectTimeout=3"])
        .args(["-o", "StrictHostKeyChecking=no"])
        .args(["-o", "UserKnownHostsFile=/dev/null"])
        .args(["-o", "LogLevel=ERROR"])
        .arg(host)
        .arg("cat /tmp/beamfs-step 2>/dev/null")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thresholds_are_ordered() {
        const { assert!(SLOW_AFTER < STALLED_AFTER) };
    }

    #[test]
    fn an_indicator_stops_when_dropped() {
        let p = Progress::start("test", "/nonexistent", "nobody@invalid");
        std::thread::sleep(Duration::from_millis(50));
        drop(p);
        // Reaching here without hanging is the assertion: Drop joins the
        // thread, so a thread that ignored the stop flag would block.
    }
}
