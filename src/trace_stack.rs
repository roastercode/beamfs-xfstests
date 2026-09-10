// SPDX-License-Identifier: GPL-2.0-only
//! Trace the whole write path, guest and host, on one clock.
//!
//! The state module photographs: how many writes, how many flushes, how
//! much dirty memory. It cannot say what this defect needs, which is
//! *order* -- whether the bitmap reaches the medium before the pointer
//! that references the block it marks. Two days of analysis assumed an
//! ordering nobody had measured.
//!
//! So this films instead. blktrace inside the guest gives every request
//! with its sector, its flags and its completion; blktrace on the host
//! NVMe gives what btrfs actually did with those writes, which is not
//! the same thing -- the images sit on copy-on-write, so a 4 KiB write
//! at a fixed offset becomes an allocation somewhere else entirely.
//! perf gives where the kernel time went, and ftrace gives the call
//! sequence around it.
//!
//! ## The synchronisation point
//!
//! Guest and host keep different clocks and neither is the other's.
//! Aligning two blktrace streams needs one event visible in both,
//! emitted at a known instant: a write of a recognisable pattern to a
//! known sector on the scratch device. It leaves a mark in the guest
//! trace (the request) and in the host trace (the pread/pwrite QEMU
//! issues for it), and the offset between the two timestamps is the
//! clock difference for the rest of the run.
//!
//! It is done twice, once before the load and once after, because the
//! clocks drift: a single point gives an offset, two give an offset and
//! a rate, and the drift over a three-minute trial is not zero.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// The sector the sync mark is written to.
///
/// Past the superblock, past the inode table, inside the bitmap region
/// but on a block the allocator will rewrite anyway. Writing outside
/// the filesystem would not appear in the host trace at the same
/// offset, and writing to live data would corrupt the run.
const SYNC_SECTOR: u64 = 8;

/// How many times the mark is written.
///
/// One write can be merged, reordered or absorbed; eight in a row at
/// one-millisecond spacing produce a burst that is unmistakable in both
/// traces even when individual requests are coalesced.
const SYNC_REPEATS: u32 = 8;

pub struct Tracing {
    pub dir: PathBuf,
    /// The libvirt domain, for the hardware counters taken host-side.
    pub domain: String,
    /// Host block device under the images, e.g. nvme1n1.
    pub host_dev: String,
    /// Guest scratch device, e.g. vdc.
    pub guest_dev: String,
}

/// What the tools need, checked before a run rather than after.
///
/// A campaign that traces nothing because blktrace is absent from the
/// image looks exactly like a campaign that traced everything and found
/// nothing, and that mistake has already cost a day.
pub fn check(cfg: &Config, node: &Node) -> Vec<(String, bool, String)> {
    let c = NodeConn::new(node, cfg);
    let mut out = Vec::new();

    let guest = c
        .run(
            "for t in blktrace blkparse perf trace-cmd; do \
             printf '%s=%s\\n' $t $(command -v $t >/dev/null 2>&1 && echo yes || echo no); done; \
             printf 'debugfs=%s\\n' $(mountpoint -q /sys/kernel/debug && echo yes || echo no); \
             printf 'blk_dev=%s\\n' $(ls /sys/kernel/debug/block 2>/dev/null | head -1)",
            Duration::from_secs(30),
        )
        .unwrap_or_default();
    for line in guest.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            out.push((format!("guest.{k}"), v == "yes", v.to_string()));
        }
    }

    for t in ["blktrace", "blkparse", "perf"] {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {t}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        out.push((format!("host.{t}"), ok, if ok { "yes" } else { "no" }.into()));
    }
    out
}

/// Write the sync mark and return the guest's idea of when it happened.
///
/// dd with oflag=direct so the write reaches the device rather than
/// sitting in the page cache -- a mark that never leaves the guest is
/// not a mark. The timestamp is taken from the same clock blktrace
/// stamps its events with.
fn sync_mark(c: &NodeConn, dev: &str) -> Result<f64, String> {
    let out = c
        .run(
            &format!(
                "sudo sh -c 'awk \"{{print \\$1}}\" /proc/uptime; \
                 for i in $(seq 1 {SYNC_REPEATS}); do \
                   dd if=/dev/urandom of={dev} bs=512 count=1 seek={SYNC_SECTOR} \
                      oflag=direct conv=notrunc 2>/dev/null; \
                   sleep 0.001; \
                 done; \
                 awk \"{{print \\$1}}\" /proc/uptime'"
            ),
            Duration::from_secs(60),
        )
        .map_err(|e| e.to_string())?;
    let ts: Vec<f64> = out.lines().filter_map(|l| l.trim().parse().ok()).collect();
    if ts.len() < 2 {
        return Err("sync mark produced no timestamps".into());
    }
    Ok((ts[0] + ts[1]) / 2.0)
}

/// The host's clock at the same instant, as closely as we can take it.
fn host_now() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse().ok()))
        .unwrap_or(0.0)
}

/// The offset between the two clocks, and how fast it drifts.
pub struct Clocks {
    pub offset_start: f64,
    pub offset_end: f64,
    pub span: f64,
}

impl Clocks {
    /// Drift in microseconds per second of wall time.
    pub fn drift_us_per_s(&self) -> f64 {
        if self.span <= 0.0 {
            return 0.0;
        }
        (self.offset_end - self.offset_start) * 1e6 / self.span
    }

    /// A guest timestamp expressed on the host's clock.
    ///
    /// Interpolated between the two marks rather than using one: over a
    /// three-minute trial the drift is not zero, and an offset taken at
    /// the start is wrong by the end.
    pub fn to_host(&self, guest_t: f64, guest_start: f64) -> f64 {
        let frac = if self.span > 0.0 {
            ((guest_t - guest_start) / self.span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        guest_t + self.offset_start + frac * (self.offset_end - self.offset_start)
    }
}

/// Start tracing on both sides.
///
/// Returned handles must be stopped even on an error path, or blktrace
/// keeps writing until the disk fills -- it has no timeout of its own.
pub fn start(t: &Tracing, cfg: &Config, node: &Node) -> Result<f64, String> {
    let c = NodeConn::new(node, cfg);
    std::fs::create_dir_all(&t.dir).map_err(|e| e.to_string())?;

    // Guest: blktrace on the scratch device, plus perf on the whole
    // kernel. -a write and -a issue keep the volume down; a full trace
    // of twelve million requests is not readable and fills the guest.
    c.run(
        &format!(
            "sudo sh -c 'mkdir -p /var/trace && cd /var/trace && rm -f *; \
             setsid blktrace -d /dev/{} -a write -a issue -a complete -o guest \
               -D /var/trace </dev/null >/dev/null 2>&1 & \
             echo $! > /var/trace/blktrace.pid; \
             setsid perf record -a -g -F 199 -o /var/trace/perf.data \
               </dev/null >/dev/null 2>&1 & \
             echo $! > /var/trace/perf.pid; \
             sleep 2'",
            t.guest_dev
        ),
        Duration::from_secs(60),
    )
    .map_err(|e| format!("guest trace: {e}"))?;

    // Host: blktrace on the real device under the images.
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "mkdir -p {d} && cd {d} && rm -f host.blktrace.*; \
             sudo setsid blktrace -d /dev/{dev} -a write -a issue -a complete \
               -o host -D {d} </dev/null >/dev/null 2>&1 & \
             echo $! > {d}/host-blktrace.pid",
            d = t.dir.display(),
            dev = t.host_dev
        ))
        .spawn()
        .map_err(|e| e.to_string())?;

    std::thread::sleep(Duration::from_secs(1));

    // First sync mark, with the host clock read as close to it as the
    // ssh round trip allows.
    let h0 = host_now();
    let g0 = sync_mark(&c, &format!("/dev/{}", t.guest_dev))?;
    let h1 = host_now();
    let offset = (h0 + h1) / 2.0 - g0;

    std::fs::write(t.dir.join("clock-start"), format!("{g0} {offset}\n"))
        .map_err(|e| e.to_string())?;
    Ok(offset)
}

/// Stop tracing, take the second sync mark, and return the clock model.
pub fn stop(t: &Tracing, cfg: &Config, node: &Node, offset_start: f64)
    -> Result<Clocks, String>
{
    let c = NodeConn::new(node, cfg);

    let h0 = host_now();
    let g1 = sync_mark(&c, &format!("/dev/{}", t.guest_dev))?;
    let h1 = host_now();
    let offset_end = (h0 + h1) / 2.0 - g1;

    let g0: f64 = std::fs::read_to_string(t.dir.join("clock-start"))
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse().ok()))
        .unwrap_or(g1);

    // Stop by pid rather than pkill: another campaign's blktrace on a
    // different device is not ours to kill.
    let _ = c.run(
        "sudo sh -c 'for p in /var/trace/blktrace.pid /var/trace/perf.pid; do \
           [ -f $p ] && kill -INT $(cat $p) 2>/dev/null; done; sleep 2; \
           cd /var/trace && blkparse -i guest -d guest.bin > guest.txt 2>/dev/null; \
           perf report -i /var/trace/perf.data --stdio --sort symbol 2>/dev/null \
             | head -40 > /var/trace/perf.txt; \
           chmod -R a+r /var/trace'",
        Duration::from_secs(180),
    );

    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "[ -f {d}/host-blktrace.pid ] && sudo kill -INT $(cat {d}/host-blktrace.pid) 2>/dev/null; \
             sleep 2; cd {d} && blkparse -i host -d host.bin > host.txt 2>/dev/null",
            d = t.dir.display()
        ))
        .output();

    // Bring the guest trace back so both are in one place.
    let _ = std::process::Command::new("scp")
        .args(["-q", "-i", &cfg.ssh_key, "-o", "BatchMode=yes",
               "-o", "StrictHostKeyChecking=no"])
        .arg(format!("{}@{}:/var/trace/*", cfg.user, node.host))
        .arg(t.dir.as_os_str())
        .output();

    Ok(Clocks { offset_start, offset_end, span: (g1 - g0).max(0.001) })
}

/// One request, from either trace.
pub struct Req {
    pub t: f64,
    pub sector: u64,
    pub blocks: u32,
    /// W, WS, WFS, FWS -- the flags say whether it carried a barrier.
    pub rw: String,
    /// A for issue, C for complete.
    pub action: char,
}

impl Req {
    pub fn is_flush(&self) -> bool {
        self.rw.contains('F')
    }
    pub fn is_fua(&self) -> bool {
        self.rw.contains("FUA") || self.rw.ends_with('A')
    }
}

/// Read a blkparse dump.
///
/// blkparse output is column-oriented and its exact shape depends on
/// the version, so this is deliberately loose: find a timestamp, an
/// action and a sector, ignore anything that does not have all three.
pub fn parse_blkparse(text: &str) -> Vec<Req> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let Some(t) = f.get(3).and_then(|x| x.trim_end_matches(':').parse::<f64>().ok()) else {
            continue;
        };
        let action = f.get(5).and_then(|x| x.chars().next()).unwrap_or('?');
        if action != 'D' && action != 'C' && action != 'Q' {
            continue;
        }
        let rw = f.get(6).unwrap_or(&"").to_string();
        if !rw.starts_with('W') && !rw.starts_with('F') {
            continue;
        }
        let Some(sector) = f.get(7).and_then(|x| x.parse::<u64>().ok()) else {
            continue;
        };
        let blocks = f
            .get(9)
            .and_then(|x| x.parse::<u32>().ok())
            .unwrap_or(1);
        out.push(Req { t, sector, blocks, rw, action });
    }
    out
}

/// What the traces say about ordering, which is the whole point.
pub fn report(dir: &Path, clocks: &Clocks, domain: &str) {
    println!("  === trace of {domain}, guest and host on one clock ===");
    println!(
        "  clock offset {:.3}s at the start, {:.3}s at the end, drift {:.0} us/s",
        clocks.offset_start,
        clocks.offset_end,
        clocks.drift_us_per_s()
    );

    let guest = std::fs::read_to_string(dir.join("guest.txt")).unwrap_or_default();
    let host = std::fs::read_to_string(dir.join("host.txt")).unwrap_or_default();
    let g = parse_blkparse(&guest);
    let h = parse_blkparse(&host);

    println!("  guest: {} write requests, host: {} write requests", g.len(), h.len());
    if g.is_empty() {
        println!("  -- nothing traced in the guest: check blktrace is in the image");
        println!();
        return;
    }

    let gf = g.iter().filter(|r| r.is_flush()).count();
    let hf = h.iter().filter(|r| r.is_flush()).count();
    println!("  barriers: {gf} emitted by the guest, {hf} seen at the host device");
    if gf > 0 && hf == 0 {
        println!("  -- the guest asked for a flush and the device never saw one");
    }

    // Amplification: what one guest write becomes underneath. On
    // copy-on-write this is not 1, and the difference is the reordering
    // the filesystem's ordering assumptions do not survive.
    if !h.is_empty() {
        let gb: u64 = g.iter().map(|r| r.blocks as u64).sum();
        let hb: u64 = h.iter().map(|r| r.blocks as u64).sum();
        println!(
            "  {gb} blocks written by the guest became {hb} at the device ({:.1}x)",
            hb as f64 / gb.max(1) as f64
        );
    }

    // Reordering inside the guest: a request completing before one
    // issued earlier is the scheduler doing its job, but the count says
    // how much the emission order is worth as an ordering guarantee.
    let mut issued: Vec<&Req> = g.iter().filter(|r| r.action == 'D').collect();
    let completed: Vec<&Req> = g.iter().filter(|r| r.action == 'C').collect();
    issued.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    let mut inversions = 0usize;
    for w in completed.windows(2) {
        if w[1].sector < w[0].sector && w[1].t > w[0].t {
            inversions += 1;
        }
    }
    println!(
        "  {inversions} completions out of sector order across {} completions",
        completed.len()
    );

    // FUA writes bypass the device cache; a filesystem that relies on
    // ordering and issues none is relying on nothing.
    let fua = g.iter().filter(|r| r.is_fua()).count();
    println!("  {fua} of the guest's writes carried FUA");

    // The first and last guest write on the host's clock, so a request
    // in one trace can be found in the other.
    if let (Some(first), Some(last)) = (g.first(), g.last()) {
        println!(
            "  guest window on the host clock: {:.3}s to {:.3}s",
            clocks.to_host(first.t, first.t),
            clocks.to_host(last.t, first.t)
        );
    }
    println!();
    println!("  where the guest kernel spent its time:");
    let perf = std::fs::read_to_string(dir.join("perf.txt")).unwrap_or_default();
    for l in perf.lines().filter(|l| !l.trim_start().starts_with('#')).take(10) {
        if !l.trim().is_empty() {
            println!("    {}", l.trim());
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_is_per_second_of_span() {
        let c = Clocks { offset_start: 1.0, offset_end: 1.003, span: 100.0 };
        assert!((c.drift_us_per_s() - 30.0).abs() < 0.001);
    }

    #[test]
    fn a_guest_timestamp_is_interpolated_not_shifted() {
        // Offset moves by 1ms over the run: a timestamp halfway through
        // must take half of that, not all or none of it.
        let c = Clocks { offset_start: 10.0, offset_end: 10.001, span: 100.0 };
        let mid = c.to_host(50.0, 0.0);
        assert!((mid - 60.0005).abs() < 1e-6, "got {mid}");
    }

    #[test]
    fn a_flush_is_recognised_by_its_flags() {
        let r = Req { t: 1.0, sector: 8, blocks: 1, rw: "WFS".into(), action: 'D' };
        assert!(r.is_flush());
        let w = Req { t: 1.0, sector: 8, blocks: 1, rw: "W".into(), action: 'D' };
        assert!(!w.is_flush());
    }

    #[test]
    fn blkparse_lines_without_all_three_fields_are_skipped() {
        let text = "8,0 1 1 0.000000000 1234 D W 512 + 8 [dd]\ngarbage\n8,0 1 2 x D W\n";
        let r = parse_blkparse(text);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].sector, 512);
    }
}
