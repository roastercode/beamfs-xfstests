// SPDX-License-Identifier: GPL-2.0-only
//! Which local process owns a node.
//!
//! The node was asked whether it was busy, by counting `check`
//! processes on it. That is an heuristic and it failed the first time
//! it mattered: a deploy rebooted a node in the middle of a bench, so
//! `check` was gone, so the node reported itself free, so a second
//! deploy rebooted it again while the bench was still waiting on an ssh
//! that would never answer. The node cannot report a run it has just
//! been robbed of.
//!
//! The fact lives on this machine: the process that started the run.
//! A file names it, and a pid that is gone releases it.

use std::path::PathBuf;

fn path(node: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/beamfs-xfstests-node-{node}.lock"))
}

/// The live owner of `node`, if any: its pid and what it is doing.
///
/// A file whose pid no longer exists is stale and is removed, so a run
/// killed with its terminal does not hold a node for ever.
pub fn holder(node: &str) -> Option<(u32, String)> {
    let body = std::fs::read_to_string(path(node)).ok()?;
    let mut parts = body.splitn(2, ' ');
    let pid: u32 = parts.next()?.trim().parse().ok()?;
    let what = parts.next().unwrap_or("a run").trim().to_string();
    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
        Some((pid, what))
    } else {
        let _ = std::fs::remove_file(path(node));
        None
    }
}

/// Ownership of a node, released when dropped.
pub struct Held {
    node: String,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(path(&self.node));
    }
}

/// Take `node` for `what`, or say who has it.
pub fn acquire(node: &str, what: &str) -> Result<Held, String> {
    if std::env::var("XFSTESTS_FORCE").is_ok() {
        let _ = std::fs::remove_file(path(node));
    } else if let Some((pid, other)) = holder(node) {
        return Err(format!(
            "{node} is held by pid {pid} ({other}); \
             set XFSTESTS_FORCE=1 to take it anyway"
        ));
    }
    std::fs::write(path(node), format!("{} {what}", std::process::id()))
        .map_err(|e| format!("cannot write the node lock: {e}"))?;
    Ok(Held { node: node.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_node_names_its_holder() {
        let n = format!("test-held-{}", std::process::id());
        let h = acquire(&n, "a bench").expect("free to begin with");
        let (pid, what) = holder(&n).expect("held now");
        assert_eq!(pid, std::process::id());
        assert_eq!(what, "a bench");
        assert!(acquire(&n, "a deploy").is_err(), "a second taker is refused");
        drop(h);
        assert!(holder(&n).is_none(), "dropping releases it");
    }

    #[test]
    fn a_dead_holder_holds_nothing() {
        let n = format!("test-dead-{}", std::process::id());
        // pid 0 never names a live process in /proc.
        std::fs::write(path(&n), "0 a run that died").unwrap();
        assert!(holder(&n).is_none(), "a stale lock is not a lock");
        assert!(!path(&n).exists(), "and it is cleaned up");
    }
}
