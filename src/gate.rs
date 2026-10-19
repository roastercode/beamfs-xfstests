// SPDX-License-Identifier: GPL-2.0-only
//! The code gate, before anything is measured.
//!
//! Every command that drives a node starts here: the beamfs tree, as it
//! stands, goes through beamfs-upstream's full profile -- the checks
//! kernel.org puts a series through, builds included -- and a code that
//! does not pass is not measured. The verdict is kept by the hash of the
//! tree, so the same code is checked once; the record of the measurement
//! carries the line that names the verdict it ran under.
//!
//! XFSTESTS_UNCHECKED_CODE=1 measures anyway, and the record says so.
use std::sync::OnceLock;

static LINE: OnceLock<String> = OnceLock::new();

/// The beamfs tree, BEAMFS_TREE or ~/git/beamfs, as the rest of the tool reads it.
fn tree() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("BEAMFS_TREE").unwrap_or_else(|_| {
            format!("{}/git/beamfs", std::env::var("HOME").unwrap_or_default())
        }),
    )
}

/// Pass the code through the gate for `what`, or say why not.
pub fn require(what: &str) -> Result<(), String> {
    if std::env::var("XFSTESTS_UNCHECKED_CODE")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        println!("  code    : NOT CHECKED, XFSTESTS_UNCHECKED_CODE=1: this {what} measures code that may not pass kernel.org");
        let _ = LINE.set("code_gate=skipped XFSTESTS_UNCHECKED_CODE=1".to_string());
        return Ok(());
    }
    let repo = tree();
    let root = crate::history::History::default_root().join("code-gate");
    let g = beamfs_upstream::gate::Gate {
        repo: &repo,
        root: &root,
        tool: "beamfs-xfstests",
        placeholders: vec!["@@XFSTESTS@@".to_string()],
    };
    match beamfs_upstream::gate::require(&g) {
        Ok(p) => {
            println!(
                "  code    : passes kernel.org's checks as tree {}",
                &p.tree[..12]
            );
            let _ = LINE.set(p.meta_line());
            Ok(())
        }
        Err(e) => Err(format!("code gate: {e:#}")),
    }
}

/// The line the record carries; "code_gate=not run" for a command that
/// went through no gate.
pub fn meta_line() -> String {
    LINE.get()
        .cloned()
        .unwrap_or_else(|| "code_gate=not run".to_string())
}
