// SPDX-License-Identifier: GPL-2.0-only
//! The whole machine's state around one trial, kept whether it failed
//! or not.
//!
//! Two days went into asking why generic/464 fails intermittently, with
//! nothing recorded per trial except a pass flag and a count of lost
//! blocks. Everything that turned out to matter was found by accident
//! and late: that the guest had 2 GB and was in permanent writeback,
//! that beamfs is built into the kernel so rebuilding the module
//! measured nothing, that the images sit on btrfs with copy-on-write so
//! the order reaching the SSD is not the order beamfs emitted, and that
//! the scratch volume takes twelve million writes against seventeen
//! flushes. None of those were visible in a pass count.
//!
//! So: capture the state at every layer of the write path, before and
//! after each trial, and keep it for passing trials too -- a variable
//! only implicates itself when the runs that fail differ from the runs
//! that pass.
//!
//! The layers, top to bottom: the filesystem's own counters, the guest
//! kernel's memory and writeback pressure, the block layer and its
//! queues, the virtio boundary, and the host -- which libvirt reports
//! without anything installed in the guest.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// Everything measured at one instant.
///
/// A flat map rather than a struct per layer: the set of interesting
/// variables changes faster than the code does, and a field nobody
/// reads is worse than a key nobody queries.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub v: BTreeMap<String, i64>,
    /// Values that are not numbers -- scheduler name, mount options.
    pub s: BTreeMap<String, String>,
}

impl Snapshot {
    fn num(&mut self, k: &str, v: i64) {
        self.v.insert(k.into(), v);
    }

    pub fn get(&self, k: &str) -> Option<i64> {
        self.v.get(k).copied()
    }

    /// What changed between two instants, for counters that only rise.
    pub fn delta(&self, before: &Snapshot) -> Snapshot {
        let mut d = Snapshot::default();
        for (k, after) in &self.v {
            if let Some(b) = before.v.get(k) {
                d.v.insert(k.clone(), after - b);
            }
        }
        d.s = self.s.clone();
        d
    }
}

/// One trial with the state around it.
pub struct Record {
    pub n: u32,
    pub passed: bool,
    pub lost: usize,
    pub violations: usize,
    pub secs: u64,
    /// State before the load started.
    pub before: Snapshot,
    /// State after fsck ran.
    pub after: Snapshot,
    /// Counters that moved during the trial.
    pub during: Snapshot,
    /// Kernel messages the trial produced, whole, not grepped.
    pub dmesg: String,
}

/// Read everything the guest can tell us in one ssh round trip.
///
/// One command rather than twenty: each ssh call is a second of the
/// trial's own timing, and a probe that perturbs what it measures is
/// worse than no probe. The output is key=value lines, parsed below.
fn guest_probe(scratch: &str, test: &str) -> String {
    format!(
        r#"sudo sh -c '
# --- memory and writeback pressure ---
awk "/^MemTotal:/{{print \"mem.total=\" \$2}}
     /^MemFree:/{{print \"mem.free=\" \$2}}
     /^MemAvailable:/{{print \"mem.avail=\" \$2}}
     /^Dirty:/{{print \"mem.dirty=\" \$2}}
     /^Writeback:/{{print \"mem.writeback=\" \$2}}
     /^Buffers:/{{print \"mem.buffers=\" \$2}}
     /^Cached:/{{print \"mem.cached=\" \$2}}
     /^Slab:/{{print \"mem.slab=\" \$2}}
     /^SReclaimable:/{{print \"mem.slab_recl=\" \$2}}" /proc/meminfo

# The thresholds the load is measured against: a run that spends its
# time above dirty_ratio is a different run from one that never
# reaches dirty_background_ratio, and the load is the same either way.
echo "wb.dirty_ratio=$(cat /proc/sys/vm/dirty_ratio)"
echo "wb.dirty_bg_ratio=$(cat /proc/sys/vm/dirty_background_ratio)"
echo "wb.dirty_expire=$(cat /proc/sys/vm/dirty_expire_centisecs)"

# --- pressure stall: how much time was lost waiting, not working ---
if [ -r /proc/pressure/io ]; then
  awk "/^some/{{print \"psi.io.some=\" \$5}} /^full/{{print \"psi.io.full=\" \$5}}" /proc/pressure/io | sed "s/total=//"
  awk "/^some/{{print \"psi.mem.some=\" \$5}}" /proc/pressure/memory | sed "s/total=//"
  awk "/^some/{{print \"psi.cpu.some=\" \$5}}" /proc/pressure/cpu | sed "s/total=//"
fi

# --- inode and dentry caches: eviction is what mmb_invalidate rides on ---
awk "{{print \"vfs.inodes_used=\" \$1 - \$2}}" /proc/sys/fs/inode-nr
awk "{{print \"vfs.dentries=\" \$1}}" /proc/sys/fs/dentry-state

# --- block layer, per device ---
for d in {scratch} {test}; do
  n=$(basename $d)
  # Fields 4..14 of /proc/diskstats, by number because their names are
  # not in the file: 4 reads, 6 sectors read, 8 writes, 10 sectors
  # written, 11 ms spent writing, 12 requests in flight, 13 ms with at
  # least one in flight, 14 weighted ms = the integral of the queue
  # depth over time. 11 and 13 are what turn counters into a latency
  # and an occupancy; 14 was previously collected under the name
  # io_ticks, which is field 13, so every reading of it was a
  # different quantity from the one its name claimed.
  set -- $(awk -v D="$n" "\$3==D {{print \$4, \$6, \$8, \$10, \$11, \$12, \$13, \$14}}" /proc/diskstats)
  echo "blk.$n.rd_ios=${{1:-0}}"
  echo "blk.$n.rd_sectors=${{2:-0}}"
  echo "blk.$n.wr_ios=${{3:-0}}"
  echo "blk.$n.wr_sectors=${{4:-0}}"
  echo "blk.$n.wr_ticks=${{5:-0}}"
  echo "blk.$n.in_flight=${{6:-0}}"
  echo "blk.$n.io_ticks=${{7:-0}}"
  echo "blk.$n.weighted_ms=${{8:-0}}"
  [ -r /sys/block/$n/queue/nr_requests ] && echo "blk.$n.nr_requests=$(cat /sys/block/$n/queue/nr_requests)"
  [ -r /sys/block/$n/queue/scheduler ] && echo "s:blk.$n.scheduler=$(sed "s/.*\[//;s/\].*//" /sys/block/$n/queue/scheduler)"
  [ -r /sys/block/$n/queue/write_cache ] && echo "s:blk.$n.write_cache=$(cat /sys/block/$n/queue/write_cache)"
  [ -r /sys/block/$n/queue/rotational ] && echo "blk.$n.rotational=$(cat /sys/block/$n/queue/rotational)"
done

# --- cpu: a spinlock held across a migration is a different run ---
awk "/^ctxt/{{print \"cpu.ctxt=\" \$2}} /^processes/{{print \"cpu.forks=\" \$2}}" /proc/stat
echo "cpu.count=$(nproc)"
awk "{{print \"cpu.load1=\" \$1 * 100}}" /proc/loadavg

# --- beamfs itself ---
for d in /sys/fs/beamfs/*/; do
  [ -d "$d" ] || continue
  n=$(basename $d)
  for a in cursor passes corrected uncorrectable error_budget blocks; do
    [ -r "$d$a" ] && echo "fs.$n.$a=$(cat $d$a 2>/dev/null | tr -dc 0-9)"
  done
done
# Mounted beamfs volumes: the leak needs a second filesystem, measured.
echo "fs.mounts=$(mount | grep -c \"type beamfs\")"
echo "fs.mounts_any=$(mount | grep -cE \" /mnt/(test|scratch) \")"

# --- the tree checker, when the kernel carries it ---
echo "tc.violations=$(dmesg | grep -c \"LOST POINTER\")"
echo "tc.zeroed=$(dmesg | grep -c \"ZEROED IN SERVICE\")"
echo "tc.present=$(grep -c beamfs_tc_store /proc/kallsyms)"
'"#
    )
}

/// Parse key=value, with a `s:` prefix marking the non-numeric ones.
fn parse(out: &str) -> Snapshot {
    let mut snap = Snapshot::default();
    for line in out.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        if let Some(k) = k.strip_prefix("s:") {
            snap.s.insert(k.into(), v.into());
        } else if let Ok(n) = v.trim().parse::<i64>() {
            snap.num(k, n);
        }
    }
    snap
}

/// What libvirt reports about the guest, from the host.
///
/// This is the layer the guest cannot see and where two of the day's
/// surprises lived: the balloon size that made 2 GB look like plenty,
/// and the flush count that turned out to be seventeen against twelve
/// million writes. Nothing to install in the guest, and it cannot be
/// perturbed by the load.
fn host_probe(domain: &str) -> Snapshot {
    let out = std::process::Command::new("sudo")
        .args(["virsh", "domstats", domain])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let mut snap = Snapshot::default();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for line in out.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        if let Some(idx) = k.strip_prefix("block.").and_then(|r| r.strip_suffix(".name")) {
            names.insert(idx.into(), v.into());
        }
    }
    for line in out.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let Ok(n) = v.trim().parse::<i64>() else { continue };
        // balloon.rss is what the guest really costs the host; the
        // guest's own MemTotal says nothing about that.
        if k.starts_with("balloon.") || k.starts_with("vcpu.") {
            snap.num(&format!("host.{k}"), n);
        } else if let Some(rest) = k.strip_prefix("block.") {
            let Some((idx, field)) = rest.split_once('.') else { continue };
            if field == "name" {
                continue;
            }
            let dev = names.get(idx).cloned().unwrap_or_else(|| idx.into());
            snap.num(&format!("host.blk.{dev}.{field}"), n);
        }
    }
    snap
}

/// The host's own state: a VM on a loaded host does not schedule like a
/// VM on an idle one, and the 8-then-2 pair differed in exactly that.
fn host_self() -> Snapshot {
    let mut snap = Snapshot::default();
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(first) = s.split_whitespace().next() {
            if let Ok(f) = first.parse::<f64>() {
                snap.num("hostself.load1", (f * 100.0) as i64);
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for l in s.lines() {
            let Some((k, v)) = l.split_once(':') else { continue };
            let key = match k {
                "MemFree" => "hostself.mem_free",
                "MemAvailable" => "hostself.mem_avail",
                "Dirty" => "hostself.dirty",
                _ => continue,
            };
            if let Some(n) = v.split_whitespace().next().and_then(|x| x.parse::<i64>().ok()) {
                snap.num(key, n);
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/pressure/io") {
        for l in s.lines() {
            if let Some(rest) = l.strip_prefix("some") {
                if let Some(t) = rest.split_whitespace().find_map(|w| w.strip_prefix("total=")) {
                    if let Ok(n) = t.parse::<i64>() {
                        snap.num("hostself.psi_io", n);
                    }
                }
            }
        }
    }
    snap
}

/// Everything, at one instant, from both sides.
pub fn capture(cfg: &Config, node: &Node, domain: &str) -> Snapshot {
    let c = NodeConn::new(node, cfg);
    let mut snap = c
        .run(&guest_probe(&node.scratch_dev, &node.test_dev), Duration::from_secs(60))
        .map(|o| parse(&o))
        .unwrap_or_default();
    for (k, v) in host_probe(domain).v {
        snap.v.insert(k, v);
    }
    for (k, v) in host_self().v {
        snap.v.insert(k, v);
    }
    snap
}

/// Which variables separate the trials that failed from the ones that
/// passed.
///
/// Not a claim of cause. With ten trials and forty variables something
/// will always look separated, and saying so plainly is the difference
/// between a lead and the three patches that got reverted. What this
/// gives is an ordering: look at the top of this list first.
pub fn discriminate(records: &[Record]) -> Vec<(String, f64, i64, i64)> {
    let (fail, pass): (Vec<&Record>, Vec<&Record>) =
        records.iter().partition(|r| !r.passed);
    if fail.is_empty() || pass.is_empty() {
        return Vec::new();
    }

    let mut keys: Vec<String> = records
        .first()
        .map(|r| r.during.v.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();

    let mut out = Vec::new();
    for k in keys {
        let fv: Vec<i64> = fail.iter().filter_map(|r| r.during.get(&k)).collect();
        let pv: Vec<i64> = pass.iter().filter_map(|r| r.during.get(&k)).collect();
        if fv.is_empty() || pv.is_empty() {
            continue;
        }
        let fm = fv.iter().sum::<i64>() as f64 / fv.len() as f64;
        let pm = pv.iter().sum::<i64>() as f64 / pv.len() as f64;
        if fm == pm {
            continue;
        }
        // Separation in units of the pooled spread, so a variable that
        // differs by a lot but scatters by more does not come top.
        let var = |v: &Vec<i64>, m: f64| {
            if v.len() < 2 {
                return 0.0;
            }
            v.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64
        };
        let sd = ((var(&fv, fm) + var(&pv, pm)) / 2.0).sqrt();
        let score = if sd > 0.0 { (fm - pm).abs() / sd } else { f64::INFINITY };
        out.push((k, score, fm as i64, pm as i64));
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(12);
    out
}

/// What the block layer did during a trial, in the terms that mean
/// something.
///
/// The counters are cumulative, so only their difference over the
/// trial says anything, and `during` already holds that difference.
/// Three derived numbers answer three separate questions:
///
///   - latency: ms spent writing divided by writes. How long one write
///     took, which is a property of the storage under the guest.
///   - occupancy: ms with at least one request in flight over the
///     trial's wall clock. Whether the device was ever idle.
///   - depth: weighted ms over the trial's wall clock, the integral of
///     the queue length over time. A value pinned at 1.0 with high
///     occupancy means every write waited for the one before it --
///     nothing the filesystem submits is ever in flight together, and
///     the throughput ceiling that follows is arithmetic, not luck.
///
/// Measured on generic/083 against beamfs: depth 0.96 to 1.02 across
/// four windows, occupancy 83 to 85 per cent, 72 to 142 ms per write.
/// Whether that ceiling belongs to the filesystem or to the host is
/// what a control run on another filesystem settles; this only makes
/// the question askable without rediscovering the field numbers.
pub fn io_summary(r: &Record) -> Vec<(String, f64, f64, f64, f64)> {
    let mut out = Vec::new();
    let ms = (r.secs as f64) * 1000.0;
    if ms <= 0.0 {
        return out;
    }
    let devs: Vec<String> = r
        .during
        .v
        .keys()
        .filter_map(|k| {
            k.strip_prefix("blk.")
                .and_then(|r| r.strip_suffix(".wr_ios"))
                .map(std::string::ToString::to_string)
        })
        .collect();
    for d in devs {
        let get = |f: &str| r.during.get(&format!("blk.{d}.{f}")).unwrap_or(0) as f64;
        let wio = get("wr_ios");
        if wio <= 0.0 {
            continue;
        }
        // Every read of `get` happens before `d` moves into the tuple:
        // the closure borrows `d` to build the key, so the borrow has
        // to end first.
        let latency = get("wr_ticks") / wio;
        let occupancy = get("io_ticks") * 100.0 / ms;
        let depth = get("weighted_ms") / ms;
        let rate = get("wr_sectors") / 2.0 * 1000.0 / ms;
        out.push((d, latency, occupancy, depth, rate));
    }
    out
}

/// Print what separated the failures, and what it does not mean.
pub fn report(records: &[Record]) {
    let fails = records.iter().filter(|r| !r.passed).count();
    println!("  === state across {} trials, {fails} failing ===", records.len());

    if let Some(r) = records.first() {
        // Conditions that hold for the whole run: worth stating once,
        // because two of them explained a day of wrong measurements.
        for (k, label) in [
            ("tc.present", "tree checker in the running kernel"),
            ("fs.mounts", "beamfs volumes mounted"),
            ("mem.total", "guest memory, kB"),
            ("host.balloon.rss", "guest resident on the host, kB"),
        ] {
            if let Some(v) = r.before.get(k) {
                println!("  {label}: {v}");
            }
        }
        for (k, v) in &r.before.s {
            println!("  {k}: {v}");
        }
        println!();
    }

    // Every failing trial in full: its number, what it lost, how long
    // it took and what the kernel said. A leak of 28 blocks and one of
    // 1014 are different events, and the summary above hides which
    // trial was which.
    for r in records.iter().filter(|r| !r.passed) {
        println!(
            "  trial {}: {} blocks lost, {} pointer(s) seen to vanish, {}s",
            r.n, r.lost, r.violations, r.secs
        );
        // Only the lines the trial itself produced, and only the ones
        // that say something went wrong -- the whole tail is noise.
        let notable: Vec<&str> = r
            .dmesg
            .lines()
            .filter(|l| {
                l.contains("LOST POINTER")
                    || l.contains("ZEROED IN SERVICE")
                    || l.contains("WARNING")
                    || l.contains("BUG")
                    || l.contains("beamfs/")
            })
            .collect();
        for l in notable.iter().take(4) {
            println!("      {}", l.trim());
        }
        if notable.len() > 4 {
            println!("      ... and {} more", notable.len() - 4);
        }
        // The state it ended in, for the volume it was writing to.
        for (k, label) in [
            ("mem.dirty", "dirty at the end, kB"),
            ("fs.mounts", "beamfs volumes mounted"),
        ] {
            if let Some(v) = r.after.get(k) {
                println!("      {label}: {v}");
            }
        }
    }
    if records.iter().any(|r| !r.passed) {
        println!();
    }

    // The block layer, per trial, in derived terms. Counters alone do
    // not show a queue that never holds more than one request.
    let mut any = false;
    for r in records {
        for (dev, lat, occ, depth, kbs) in io_summary(r) {
            if !any {
                println!("  what the block layer did:");
                println!(
                    "    {:<6} {:<8} {:>9} {:>10} {:>9} {:>10}",
                    "trial", "device", "ms/write", "busy %", "depth", "kB/s"
                );
                any = true;
            }
            println!(
                "    {:<6} {:<8} {lat:>9.2} {occ:>10.0} {depth:>9.2} {kbs:>10.0}",
                r.n, dev
            );
        }
    }
    if any {
        println!("  depth is the mean number of requests in flight: at 1.00 with");
        println!("  a busy device, every write waited for the one before it.");
        println!();
    }

    let d = discriminate(records);
    if d.is_empty() {
        println!("  every trial went the same way: nothing to separate");
        println!();
        return;
    }

    println!("  what differs between failing and passing trials:");
    println!("    {:<34} {:>7}  {:>12} {:>12}", "variable", "sep", "when failing", "when passing");
    for (k, score, fm, pm) in &d {
        let sep = if score.is_finite() { format!("{score:.1}") } else { "inf".into() };
        println!("    {k:<34} {sep:>7}  {fm:>12} {pm:>12}");
    }
    println!();
    println!("  sep is the gap in units of the spread within each group.");
    println!("  with this many variables and this few trials, the top of");
    println!("  the list is where to look, not what to conclude.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pairs: &[(&str, i64)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (k, v) in pairs {
            s.num(k, *v);
        }
        s
    }

    fn rec(n: u32, passed: bool, during: &[(&str, i64)]) -> Record {
        Record {
            n,
            passed,
            lost: if passed { 0 } else { 40 },
            violations: 0,
            secs: 100,
            before: Snapshot::default(),
            after: Snapshot::default(),
            during: snap(during),
            dmesg: String::new(),
        }
    }

    #[test]
    fn a_delta_is_taken_only_for_keys_present_in_both() {
        let before = snap(&[("a", 10), ("b", 5)]);
        let after = snap(&[("a", 30), ("c", 7)]);
        let d = after.delta(&before);
        assert_eq!(d.get("a"), Some(20));
        assert_eq!(d.get("c"), None);
    }

    #[test]
    fn parsing_separates_numbers_from_strings() {
        let s = parse("mem.free=1234\ns:blk.vdc.scheduler=none\njunk\nx=notanumber");
        assert_eq!(s.get("mem.free"), Some(1234));
        assert_eq!(s.s.get("blk.vdc.scheduler").map(|x| x.as_str()), Some("none"));
        assert_eq!(s.get("x"), None);
    }

    #[test]
    fn a_variable_that_separates_the_groups_comes_top() {
        let rs = vec![
            rec(1, false, &[("mem.dirty", 300), ("cpu.ctxt", 1000)]),
            rec(2, false, &[("mem.dirty", 310), ("cpu.ctxt", 4000)]),
            rec(3, true, &[("mem.dirty", 50), ("cpu.ctxt", 2000)]),
            rec(4, true, &[("mem.dirty", 60), ("cpu.ctxt", 3000)]),
        ];
        let d = discriminate(&rs);
        assert_eq!(d[0].0, "mem.dirty");
    }

    #[test]
    fn nothing_is_separated_when_every_trial_passed() {
        let rs = vec![
            rec(1, true, &[("mem.dirty", 300)]),
            rec(2, true, &[("mem.dirty", 50)]),
        ];
        assert!(discriminate(&rs).is_empty());
    }

    /// A device that never held more than one request has depth 1.
    ///
    /// The numbers are the ones measured on generic/083: 259 writes in
    /// thirty seconds, 30 600 ms spent writing, 25 000 ms with a
    /// request in flight, 30 500 weighted ms. Latency comes out near
    /// 118 ms, occupancy near 83 per cent, depth near 1.0 -- and a
    /// depth of one with a busy device is the whole finding.
    #[test]
    fn a_serialised_device_has_a_queue_depth_of_one() {
        let r = rec(
            1,
            true,
            &[
                ("blk.vdc.wr_ios", 259),
                ("blk.vdc.wr_sectors", 3300),
                ("blk.vdc.wr_ticks", 30600),
                ("blk.vdc.io_ticks", 25000),
                ("blk.vdc.weighted_ms", 30500),
            ],
        );
        let mut r = r;
        r.secs = 30;
        let s = io_summary(&r);
        assert_eq!(s.len(), 1);
        let (dev, latency, occupancy, depth, _rate) = &s[0];
        assert_eq!(dev, "vdc");
        assert!((latency - 118.1).abs() < 0.5, "latency was {latency}");
        assert!((occupancy - 83.3).abs() < 0.5, "occupancy was {occupancy}");
        assert!((depth - 1.016).abs() < 0.01, "depth was {depth}");
    }

    /// A trial with no writes to a device says nothing about it rather
    /// than dividing by zero.
    #[test]
    fn a_device_with_no_writes_is_left_out() {
        let mut r = rec(1, true, &[("blk.vdb.wr_ios", 0), ("blk.vdb.wr_ticks", 0)]);
        r.secs = 30;
        assert!(io_summary(&r).is_empty());
    }

    /// A trial of zero seconds has no rate to report.
    #[test]
    fn a_trial_of_no_duration_yields_nothing() {
        let mut r = rec(1, true, &[("blk.vdc.wr_ios", 100), ("blk.vdc.wr_ticks", 100)]);
        // rec() gives a trial a duration; a rate per second needs one
        // that is zero, which is what this is about.
        r.secs = 0;
        assert!(io_summary(&r).is_empty());
    }

}
