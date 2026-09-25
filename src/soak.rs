//! Does the scratch device keep what it is given?
//!
//! beamfs 0.1.18 on 2026-09-25: a region block written under the
//! buffer lock and read back from the device past the page cache comes
//! back identical, 405 465 times out of 405 504; it is never written
//! again; and a later read, while mounted or at fsck, finds it holding
//! zeros. Before that is the filesystem's fault the device answers for
//! itself, with no filesystem on it: `scripts/soak.py` on the node,
//! random O_DIRECT writes carrying their block number, a generation and
//! a CRC, read back at once, sampled every 30 seconds, and read whole
//! at the end. Anything but the last generation written is an anomaly.

use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

const SCRIPT: &str = include_str!("../scripts/soak.py");
const REMOTE: &str = "/var/tmp/beamfs-bx/soak.py";

/// Run the soak for `secs` seconds on the node's scratch device and
/// print what it found. The device must not be mounted.
pub fn soak(cfg: &Config, node: &Node, secs: u64) -> Result<(), String> {
    let conn = NodeConn::new(node, cfg);
    // The configuration names the device the way xfstests does, "vdc";
    // the script opens a path.
    let dev = if node.scratch_dev.starts_with('/') {
        node.scratch_dev.clone()
    } else {
        format!("/dev/{}", node.scratch_dev)
    };

    let mounted = conn
        .run(&format!("grep -c '^{dev} ' /proc/mounts; true"), Duration::from_secs(20))
        .map_err(|e| e.to_string())?;
    if mounted.trim() != "0" {
        return Err(format!("{dev} is mounted on {}; the soak needs the bare device", node.name));
    }
    conn.run("command -v python3 >/dev/null && mkdir -p /var/tmp/beamfs-bx",
             Duration::from_secs(20))
        .map_err(|_| format!("{}: no python3 on the node", node.name))?;

    let local = std::env::temp_dir().join(format!("bx-soak-{}.py", std::process::id()));
    std::fs::write(&local, SCRIPT).map_err(|e| format!("{}: {e}", local.display()))?;
    let pushed = conn.push(&local.to_string_lossy(), REMOTE);
    let _ = std::fs::remove_file(&local);
    pushed.map_err(|e| e.to_string())?;

    println!("  soaking {dev} on {} for {secs}s: random 4 KiB O_DIRECT writes, each read back at once,", node.name);
    println!("  a sample read again every 30 s, everything read again at the end");
    let out = conn
        // The block device belongs to root, as it does for the tests.
        .run(&format!("sudo -n python3 {REMOTE} {dev} {secs}"), Duration::from_secs(secs + 900))
        .map_err(|e| e.to_string())?;
    let report = out.lines().rev().find(|l| l.starts_with('{')).unwrap_or("").to_string();
    println!();
    println!("{}", summarize(&report));
    Ok(())
}

/// The report in a few lines. The JSON is small and flat enough that
/// picking fields out of it by name is safer than a parser dependency.
fn summarize(report: &str) -> String {
    let field = |name: &str| -> String {
        let key = format!("\"{name}\": ");
        report.find(&key).map_or_else(String::new, |i| {
            report[i + key.len()..]
                .chars()
                .take_while(|c| !matches!(c, ',' | '}'))
                .collect()
        })
    };
    if report.is_empty() {
        return "  the soak printed no report".to_string();
    }
    let mut s = String::new();
    s.push_str(&format!("  blocks on the device : {}\n", field("blocks")));
    s.push_str(&format!("  blocks written       : {} ({} in the region zone)\n",
                        field("written_blocks"), field("zone_blocks_written")));
    // "counts" and "bad" are nested objects: their fields are unique
    // names too, taken as the first occurrence after their object.
    let nested = |obj: &str, name: &str| -> String {
        let key = format!("\"{obj}\": {{");
        report.find(&key).map_or_else(String::new, |i| {
            let inner = &report[i..];
            let k = format!("\"{name}\": ");
            inner.find(&k).map_or_else(String::new, |j| {
                inner[j + k.len()..]
                    .chars()
                    .take_while(|c| !matches!(c, ',' | '}'))
                    .collect()
            })
        })
    };
    s.push_str(&format!("  writes               : {}\n", nested("counts", "writes")));
    s.push_str(&format!("  read back at once    : {} checked, {} wrong\n",
                        nested("counts", "immediate"), nested("bad", "immediate")));
    s.push_str(&format!("  sampled later        : {} checked, {} wrong\n",
                        nested("counts", "sampled"), nested("bad", "sampled")));
    s.push_str(&format!("  read at the end      : {} checked, {} wrong\n",
                        nested("counts", "final"), nested("bad", "final")));
    s.push_str(&format!("  wrong in region zone : {}\n", field("zone_bad")));
    let n = report.matches("\"when\":").count();
    if n > 0 {
        s.push_str(&format!("  anomalies (first {}):\n", n.min(12)));
        for a in report.split("{\"when\":").skip(1).take(12) {
            let cut: String = a.chars().take_while(|c| *c != '}').collect();
            s.push_str(&format!("    {}\n", cut.replace('"', "")));
        }
    }
    s
}
