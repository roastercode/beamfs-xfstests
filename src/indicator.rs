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
            let mut spin = liveblock::Spin::new();
            let mut shown_step = String::new();
            let mut shown_state = 3u8;
            let mut step = String::from("start");
            let mut step_since = Instant::now();

            let mut written: Option<u64> = None;

            while mine.load(Ordering::Relaxed) {
                if let Some((now, sectors)) = read_step(&k, &h) {
                    if now != step && !now.is_empty() {
                        step = now;
                        step_since = Instant::now();
                    }
                    /*
                     * The test devices writing is progress whatever
                     * the step file says. generic/074 on 2026-09-22
                     * was shown STALLED for thirty minutes while the
                     * node wrote 9 GiB: fstest prints nothing until it
                     * exits, and the step file only moves between
                     * phases of the harness.
                     */
                    if let Some(s) = sectors {
                        if written.is_some_and(|w| w != s) {
                            step_since = Instant::now();
                        }
                        written = Some(s);
                    }
                }
                let held = step_since.elapsed().as_secs();
                let total = started.elapsed().as_secs();

                /*
                 * One line when something changes, not four a second.
                 *
                 * This used to redraw a single line in place, and the
                 * redrawing was removed when the output started being
                 * pasted into reports -- without adding anything in
                 * its stead, so one test produced five hundred lines
                 * of spinner and a run was killed for being
                 * unreadable.
                 *
                 * A line is worth printing when the step changes or
                 * when its health changes. Between those, the spinner
                 * below says the program is alive.
                 */
                let state = if held >= STALLED_AFTER {
                    2u8
                } else if held >= SLOW_AFTER {
                    1u8
                } else {
                    0u8
                };
                /*
                 * No line on a timer.
                 *
                 * The thirty-second line was there when nothing turned;
                 * the spinner below says the program is alive now, so a
                 * periodic line only repeats what is already on screen.
                 * A step or a change of health is an event; elapsed
                 * time is not.
                 */
                if step != shown_step || state != shown_state {
                    let note = match state {
                        2 => format!("   STALLED {held}s in {step}"),
                        1 => format!("   slow, {held}s in {step}"),
                        _ => String::new(),
                    };

                    crate::say::note(&format!(
                        "  {shown:<34} {total:>4}s  {step:<10}{note}"));
                    shown_step = step.clone();
                    shown_state = state;
                }

                /*
                 * The spinner turns in place, on the line below.
                 *
                 * Two separate jobs: the lines above are the record and
                 * scroll, this says the program is alive between them.
                 * Printing the spinner as its own line was five hundred
                 * lines for one test; printing nothing at all left the
                 * terminal frozen for thirty seconds at a time.
                 */
                crate::say::draw(&format!(
                    "  {shown:<34} {} {total:>4}s  {step:<10}",
                    spin.tick()));
                std::thread::sleep(Duration::from_millis(250));
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
        crate::say::note(&format!(
            "  {:<34} {verdict}   {}s",
            self.label,
            self.started.elapsed().as_secs()
        ));
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
/// The step, and the sectors written so far to the test devices (every
/// virtio disk but the root's, vda: the lab's layout).
fn read_step(key: &str, host: &str) -> Option<(String, Option<u64>)> {
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
        .arg("cat /tmp/beamfs-step 2>/dev/null; echo; \
              awk '$3 ~ /^vd[b-z]$/ {s += $10} END {print s + 0}' /proc/diskstats")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut lines = text.lines();
            let step = lines.next().unwrap_or("").trim().to_string();
            let sectors = text.lines().last().and_then(|l| l.trim().parse::<u64>().ok());
            (step, sectors)
        })
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
