// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! What was last verified about a node, and when.
//!
//! deploy checks the tools' checksums, waits for the node and reads its
//! kernel. sweep then asks the same questions before its first test:
//! four round trips to learn what the command that ran a minute ago
//! established.
//!
//! Worse than slow, the two can disagree -- a node deployed and then
//! rebooted by hand passes one and fails the other -- and nothing said
//! which was right.

use std::path::PathBuf;

/// What a deploy left behind.
pub struct Verified {
    /// Seconds since the epoch when the check passed.
    pub at: u64,
    /// The node's uptime in seconds at that moment.
    ///
    /// This is what makes the record safe to trust: a node whose
    /// uptime is now less than it was has rebooted since, and
    /// everything below it is about a machine that no longer exists.
    pub uptime: u64,
    pub kernel: String,
}

fn path(node: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".local/share/beamfs-xfstests")
        .join(format!("verified-{node}"))
}

/// Record that this node has just been checked.
pub fn mark(node: &str, uptime: u64, kernel: &str) {
    let p = path(node);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(&p, format!("{at} {uptime} {kernel}\n"));
}

/// What was recorded, if anything.
#[must_use]
pub fn read(node: &str) -> Option<Verified> {
    let body = std::fs::read_to_string(path(node)).ok()?;
    let mut f = body.split_whitespace();
    Some(Verified {
        at: f.next()?.parse().ok()?,
        uptime: f.next()?.parse().ok()?,
        kernel: f.next().unwrap_or_default().to_string(),
    })
}

/// Does the record still describe this node?
///
/// True only when the node has not rebooted since: its uptime must
/// have grown by roughly the time that has passed. A node restarted in
/// between has a smaller uptime, and one restored from a snapshot has
/// one that does not match the elapsed time either.
#[must_use]
pub fn still_current(v: &Verified, uptime_now: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(v.at);

    // The uptime must have moved forward by about the elapsed time.
    // Ten seconds of slack for the round trips on either side.
    uptime_now >= v.uptime
        && uptime_now.saturating_sub(v.uptime) <= elapsed + 10
        && uptime_now.saturating_sub(v.uptime) + 10 >= elapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_that_kept_running_is_still_current() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let v = Verified { at: now - 100, uptime: 500, kernel: "7.3.0".into() };
        assert!(still_current(&v, 600));
    }

    #[test]
    fn a_node_that_rebooted_is_not() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let v = Verified { at: now - 100, uptime: 500, kernel: "7.3.0".into() };
        // Uptime went backwards: a different boot.
        assert!(!still_current(&v, 30));
    }

    #[test]
    fn a_node_whose_uptime_jumped_is_not() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let v = Verified { at: now - 100, uptime: 500, kernel: "7.3.0".into() };
        // Restored from a snapshot: uptime moved more than the clock.
        assert!(!still_current(&v, 5000));
    }
}
