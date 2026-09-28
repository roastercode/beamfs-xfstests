// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! What each node is right now, without starting anything.
//!
//! Every other subcommand asks a node something on its way to doing
//! work, and what it learns dies with the command. When a capture then
//! disagrees with the node -- a volume the test called inconsistent and
//! fsck calls clean, a tracepoint file full of zeroes -- there is no way
//! to ask the plain question afterwards, and the gap gets filled by
//! guessing.
//!
//! On 2026-09-21 that gap cost four wrong conclusions in one analysis:
//! an inode cache read as a leak when it was the node's own beamfs
//! root, a fsck accused of giving up at four hundred lines when the
//! collector was truncating it, and two morts blamed on memory when
//! they were the long budget. Each of them was one question away.
//!
//! One round trip per node, because the answer must not disturb what it
//! measures. Nothing here writes, kills, mounts or reboots: it is safe
//! to run during a campaign.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;
use crate::nodestate;

/// The whole question, as one shell command.
///
/// Written as a raw string with @TEST@ and @SCRATCH@ standing in for
/// the device names: a format! here would have to double every brace
/// the shell needs, and the quoting is already three levels deep.
///
/// Every value is printed even when empty. A key that is absent means
/// the node would not answer it, which is not the same as a zero, and
/// conflating the two is how a tracepoint file full of zeroes was read
/// as "tracing off" rather than "never armed".
const PROBE: &str = r#"
echo "kernel=$(uname -r)"
echo "uptime=$(cut -d. -f1 /proc/uptime)"
echo "load=$(cut -d' ' -f1 /proc/loadavg)"
echo "blocked=$(ps -eo state= | grep -c '^D')"
echo "rootfs=$(grep ' / ' /proc/mounts | head -1 | cut -d' ' -f3)"
echo "beamfs_mounts=$(grep -c beamfs /proc/mounts)"
echo "test_size=$(lsblk -dno SIZE /dev/@TEST@ 2>/dev/null | tr -d ' ')"
echo "test_mounted=$(grep -c '^/dev/@TEST@ ' /proc/mounts)"
echo "scratch_size=$(lsblk -dno SIZE /dev/@SCRATCH@ 2>/dev/null | tr -d ' ')"
echo "scratch_mounted=$(grep -c '^/dev/@SCRATCH@ ' /proc/mounts)"
echo "xfstests=$(test -x /usr/xfstests/check && echo yes || echo no)"
echo "mkfs=$(command -v mkfs.beamfs >/dev/null && echo yes || echo no)"
echo "fsck=$(command -v fsck.beamfs >/dev/null && echo yes || echo no)"
echo "fsck_version=$(fsck.beamfs -V 2>&1 | head -1 | tr ' ' '_')"
echo "tracing_on=$(sudo sh -c 'cat /sys/kernel/debug/tracing/tracing_on 2>/dev/null')"
echo "tracing_events=$(sudo sh -c 'ls /sys/kernel/debug/tracing/events/beamfs/ 2>/dev/null | wc -l')"
echo "tracing_armed=$(sudo sh -c 'cat /sys/kernel/debug/tracing/events/beamfs/*/enable 2>/dev/null' | grep -c '^1')"
echo "inode_active=$(sudo sh -c 'grep ^beamfs_inode_cache /proc/slabinfo 2>/dev/null' | tr -s ' ' | cut -d' ' -f2)"
echo "inode_objects=$(sudo sh -c 'grep ^beamfs_inode_cache /proc/slabinfo 2>/dev/null' | tr -s ' ' | cut -d' ' -f3)"
echo "current_test=$(ps -eo args= | grep -oE 'generic/[0-9]+$' | head -1)"
echo "leftover=$(ps -eo args= | grep -cE '[c]heck generic/|[z]std |[b]pftrace |[f]sstress|[f]sx ')"
echo "results=$(wc -l < /var/lib/beamfs-xfstests/results.txt 2>/dev/null)"
echo "dump_corrupt=$(grep -h '^export DUMP_CORRUPT_FS=' /usr/xfstests/local.config 2>/dev/null | tail -1 | cut -d= -f2)"
"#;

#[must_use]
fn probe_for(node: &Node) -> String {
    PROBE
        .replace("@TEST@", &node.test_dev)
        .replace("@SCRATCH@", &node.scratch_dev)
}

/// key=value lines into a map, keeping empty values out.
///
/// An empty value is dropped rather than stored as "": the caller
/// prints "not answered" for a missing key, and a stored empty string
/// would print as a blank that reads like a legitimate zero.
#[must_use]
pub fn parse(raw: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for l in raw.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        if !v.is_empty() {
            m.insert(k.trim().to_string(), v.to_string());
        }
    }
    m
}

/// What in this reading would spoil a measurement, or explain one.
///
/// Not an error list: a node can be perfectly healthy and still carry
/// every one of these. They are the things that, left unsaid, turn up
/// later as a defect in the filesystem.
#[must_use]
pub fn concerns(m: &BTreeMap<String, String>) -> Vec<String> {
    let mut v = Vec::new();
    let get = |k: &str| m.get(k).map(String::as_str);

    if get("test_mounted") == Some("1") {
        v.push("the test device is mounted: fsck on it reads a moving target".into());
    }
    if get("scratch_mounted") == Some("1") {
        v.push("the scratch device is mounted: fsck on it reads a moving target".into());
    }
    if get("rootfs") == Some("beamfs") {
        v.push("this node's own root is beamfs: its inode cache counts the root's \
                inodes too, and a slab figure here is not about the test volumes"
            .into());
    }
    match (get("tracing_on"), get("tracing_armed")) {
        (Some("0"), _) => v.push("tracing is off: a capture will record zeroes".into()),
        (_, Some("0")) => v.push("no beamfs tracepoint is armed".into()),
        (None, _) => v.push("the node would not say whether tracing is on".into()),
        _ => {}
    }
    if get("dump_corrupt") == Some("0") {
        v.push("DUMP_CORRUPT_FS is 0: a failing test keeps no image of the volume".into());
    }
    if get("fsck") == Some("no") {
        v.push("fsck.beamfs is not on the node".into());
    }
    if get("mkfs") == Some("no") {
        v.push("mkfs.beamfs is not on the node".into());
    }
    if get("xfstests") == Some("no") {
        v.push("/usr/xfstests/check is not on the node".into());
    }
    if let Some(n) = get("leftover") {
        if n != "0" {
            v.push(format!("{n} process(es) from an earlier trial are still running"));
        }
    }
    v
}

/// What libvirt says, asked on the host rather than the guest.
///
/// A guest that does not answer ssh may be running, paused or gone, and
/// the three call for different things. Nothing here starts or stops a
/// domain.
#[must_use]
fn domstate(domain: &str) -> String {
    match Command::new("virsh").arg("domstate").arg(domain).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { "unknown".into() } else { s }
        }
        Ok(_) => "no such domain".into(),
        Err(_) => "virsh not available".into(),
    }
}

/// Print, whole, what the runner kept about each test that failed or
/// was killed, from every configured node.
///
/// The runner writes /tmp/xfs-failures/<test>.log as it goes: for a
/// test the budget killed, the watcher's last sample before the kill
/// and the stacks of the tasks in uninterruptible sleep -- the record
/// that names the lock a stalled writeback sits on. On 2026-09-22 the
/// evidence of generic/074 held the fsck of a mounted volume and none
/// of this, while the file sat on the node. Reads only.
pub fn failures(cfg: &Config) -> std::process::ExitCode {
    println!();
    if cfg.nodes.is_empty() {
        println!("  no nodes configured");
        println!();
        return std::process::ExitCode::SUCCESS;
    }
    for n in &cfg.nodes {
        let c = NodeConn::new(n, cfg);
        let list = c.failure_list();
        if list.is_empty() {
            println!("  {:<10} nothing under /tmp/xfs-failures, or the node did not answer: \
                      nodes status tells the two apart", n.name);
            println!();
            continue;
        }
        for t in &list {
            println!("  === {} {t} ===", n.name);
            match c.failure_log(t) {
                Some(body) => println!("{body}"),
                None => println!("  (empty)"),
            }
            println!();
        }
    }
    std::process::ExitCode::SUCCESS
}

/// Print what every configured node is, and stop there.
/// One command on the first configured node, its output and exit code
/// shown as they are.
///
/// For looking at a node through the tool's own connection when one of
/// the tool's own commands misbehaves there: on 2026-09-28 the detached
/// listing of 2.3.50 produced nothing on x86-01 and worked on compute01,
/// and there was no way to ask x86-01 what it had done with it.
pub fn exec(cfg: &Config, words: &[String]) -> std::process::ExitCode {
    let Some(n) = cfg.nodes.first() else {
        eprintln!("  no nodes configured");
        return std::process::ExitCode::from(2);
    };
    if words.is_empty() {
        eprintln!("  nodes exec <command>");
        return std::process::ExitCode::from(2);
    }
    let c = NodeConn::new(n, cfg);
    match c.run_rc(&words.join(" "), Duration::from_secs(120)) {
        Ok((out, rc)) => {
            print!("{out}");
            if !out.ends_with('\n') {
                println!();
            }
            println!("  rc={rc}");
            if rc == 0 { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::from(1) }
        }
        Err(e) => {
            eprintln!("  {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn status(cfg: &Config, what: Option<&String>) -> std::process::ExitCode {
    if let Some(other) = what.map(String::as_str) {
        if other == "failures" {
            return failures(cfg);
        }
        if other != "status" {
            eprintln!();
            eprintln!("  nodes: no such subcommand \"{other}\". They are status and failures.");
            eprintln!();
            return std::process::ExitCode::from(2);
        }
    }

    println!();
    if cfg.nodes.is_empty() {
        println!("  no nodes configured");
        println!();
        return std::process::ExitCode::SUCCESS;
    }

    let mut unreachable = false;

    for n in &cfg.nodes {
        println!("  === {} ({}) ===", n.name, n.host);
        println!("    libvirt          {}", domstate(&format!("beamfs-{}", n.name)));

        let c = NodeConn::new(n, cfg);
        let raw = match c.run(&probe_for(n), Duration::from_secs(45)) {
            Ok(o) => o,
            Err(e) => {
                println!("    unreachable      {e}");
                println!();
                unreachable = true;
                continue;
            }
        };
        let m = parse(&raw);

        let show = |label: &str, key: &str| {
            match m.get(key) {
                Some(v) => println!("    {label:<17}{v}"),
                None => println!("    {label:<17}(not answered)"),
            }
        };

        show("kernel", "kernel");
        show("uptime s", "uptime");
        show("load", "load");
        show("tasks in D", "blocked");
        show("root fs", "rootfs");
        show("beamfs mounts", "beamfs_mounts");
        println!("    test dev         /dev/{} {} mounted={}",
                 n.test_dev,
                 m.get("test_size").map_or("?", String::as_str),
                 m.get("test_mounted").map_or("?", String::as_str));
        println!("    scratch dev      /dev/{} {} mounted={}",
                 n.scratch_dev,
                 m.get("scratch_size").map_or("?", String::as_str),
                 m.get("scratch_mounted").map_or("?", String::as_str));
        show("xfstests", "xfstests");
        show("mkfs.beamfs", "mkfs");
        show("fsck.beamfs", "fsck");
        show("fsck version", "fsck_version");
        show("DUMP_CORRUPT_FS", "dump_corrupt");
        println!("    tracing          on={} armed={}/{}",
                 m.get("tracing_on").map_or("?", String::as_str),
                 m.get("tracing_armed").map_or("?", String::as_str),
                 m.get("tracing_events").map_or("?", String::as_str));
        println!("    inode cache      active={} objects={}",
                 m.get("inode_active").map_or("?", String::as_str),
                 m.get("inode_objects").map_or("?", String::as_str));
        show("running test", "current_test");
        show("leftover procs", "leftover");
        show("results lines", "results");

        // What deploy last established, and whether it still holds.
        match nodestate::read(&n.name) {
            Some(v) => {
                let now: u64 = m.get("uptime").and_then(|s| s.parse().ok()).unwrap_or(0);
                if nodestate::still_current(&v, now) {
                    println!("    last verified    kernel {} , still the same boot", v.kernel);
                } else {
                    println!("    last verified    kernel {} , but the node has \
                              rebooted since: that record says nothing about this machine",
                             v.kernel);
                }
            }
            None => println!("    last verified    never"),
        }

        let c = concerns(&m);
        if c.is_empty() {
            println!("    nothing here would spoil a measurement");
        } else {
            println!("    worth knowing before you measure:");
            for x in &c {
                println!("      - {x}");
            }
        }
        println!();
    }

    if unreachable {
        std::process::ExitCode::from(1)
    } else {
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_value_is_absent_rather_than_zero() {
        let m = parse("tracing_on=\nblocked=0\n");
        assert!(!m.contains_key("tracing_on"),
                "an unanswered key must not become a value");
        assert_eq!(m.get("blocked").map(String::as_str), Some("0"));
    }

    #[test]
    fn the_probe_carries_both_device_names() {
        let n = Node {
            name: "x86-01".into(),
            host: "192.168.122.99".into(),
            test_dev: "vdb".into(),
            scratch_dev: "vdc".into(),
        };
        let p = probe_for(&n);
        assert!(p.contains("/dev/vdb"), "test device missing from the probe");
        assert!(p.contains("/dev/vdc"), "scratch device missing from the probe");
        assert!(!p.contains("@TEST@") && !p.contains("@SCRATCH@"),
                "a placeholder was left in the probe");
    }

    #[test]
    fn a_mounted_scratch_is_worth_saying() {
        let m = parse("scratch_mounted=1\ntracing_on=1\ntracing_armed=3\n");
        let c = concerns(&m);
        assert!(c.iter().any(|s| s.contains("scratch device is mounted")), "{c:?}");
    }

    #[test]
    fn a_beamfs_root_is_declared_because_it_skews_the_slab() {
        let m = parse("rootfs=beamfs\ntracing_on=1\ntracing_armed=1\n");
        let c = concerns(&m);
        assert!(c.iter().any(|s| s.contains("inode cache")), "{c:?}");
    }

    #[test]
    fn a_node_that_answers_nothing_about_tracing_says_so() {
        let m = parse("kernel=7.3.0\n");
        let c = concerns(&m);
        assert!(c.iter().any(|s| s.contains("would not say whether tracing")), "{c:?}");
    }

    #[test]
    fn a_clean_node_raises_nothing() {
        let m = parse(
            "test_mounted=0\nscratch_mounted=0\nrootfs=ext4\ntracing_on=1\n\
             tracing_armed=9\ndump_corrupt=1\nfsck=yes\nmkfs=yes\nxfstests=yes\nleftover=0\n");
        assert!(concerns(&m).is_empty(), "{:?}", concerns(&m));
    }
}
