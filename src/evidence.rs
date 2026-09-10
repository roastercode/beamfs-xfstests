// SPDX-License-Identifier: GPL-2.0-only
//! Keep everything a trial produced, so the next question does not cost
//! another campaign.
//!
//! Two days of analysis were spent asking the machine one question at a
//! time through a filter written in advance around whatever hypothesis
//! was current. Each answer discarded the rest of the output, so the
//! next question needed a new run -- and the block count that drove
//! most of it turned out to come from a grep that never matched
//! anything.
//!
//! The rule here is the opposite: capture whole, filter later. Text is
//! small enough to keep from every trial; the volume image and the
//! block traces are large and are kept only when a trial fails.
//!
//! What makes it useful rather than merely large:
//!
//!   - the test runs under `set -x`, so the exact line where a failing
//!     trial diverged is written down rather than inferred;
//!   - the load is seeded, so a failing trial and a passing one did the
//!     same thing and can be compared line by line;
//!   - traces are normalised before comparison, or the diff drowns in
//!     pids and timestamps;
//!   - the scratch volume is frozen before anything reformats it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// Where one trial's evidence lives.
pub struct Case {
    pub dir: PathBuf,
    pub trial: u32,
    pub test: String,
}

impl Case {
    pub fn new(root: &Path, test: &str, trial: u32) -> Case {
        Case {
            dir: root.join(format!("{}-{trial:03}", test.replace('/', "-"))),
            trial,
            test: test.into(),
        }
    }

    /// Which trial this is, for a report that names it.
    pub fn trial(&self) -> u32 {
        self.trial
    }

    fn put(&self, name: &str, body: &str) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join(name), body);
    }
}

/// Make the test say what it does, and do the same thing twice.
///
/// Two changes to a copy of the test, never to the original:
///
/// `set -x` with a PS4 carrying the time and the line number. The line
/// number places the command -- one appearing four times in a script is
/// otherwise unplaceable -- and EPOCHREALTIME gives the microsecond
/// from bash itself, with no fork: `date +%s.%N` measured 1.7 ms per
/// call on this node, which over tens of thousands of traced lines
/// would add minutes to a test whose failures are a timing question.
/// Nanoseconds are available through XFSTESTS_TRACE_NS=1 for a run that
/// needs them and can afford the distortion.
///
/// RANDOM seeded from a fixed value, so the file numbers and write
/// sizes are the same on every trial. 464 picks both at random, which
/// means a failing trial and a passing one did different work and
/// comparing them compares the wrong thing. With a seed the two differ
/// only in what the kernel did, which is the question.
pub fn instrument(cfg: &Config, node: &Node, test: &str, seed: u32) -> Result<(), String> {
    let c = NodeConn::new(node, cfg);
    // Nanoseconds cost a fork per traced line; microseconds cost
    // nothing. The default is the one that does not move what it
    // measures.
    let ps4 = if std::env::var("XFSTESTS_TRACE_NS").is_ok_and(|v| v == "1") {
        r#"PS4="+\$(date +%s.%N) \${LINENO}| ""#
    } else {
        r#"PS4="+\${EPOCHREALTIME} \${LINENO}| ""#
    };
    let out = c
        .run(
            &format!(
                r#"sudo sh -c '
T=/usr/xfstests/tests/{test}
[ -f $T.orig ] || cp $T $T.orig
cp $T.orig $T
# After the shebang and before anything runs. The seed is set first so
# every RANDOM in the test draws from it.
sed -i "2i RANDOM={seed}" $T
sed -i "3i {ps4}" $T
# Redirected, not printed.
#
# set -x writes to stderr, which check captures and compares
# against the expected output -- 464 expects "Silence is golden"
# and got a megabyte and a half of trace, so every trial failed on
# the instrumentation rather than on anything the filesystem did.
# Sending fd 2 to a file leaves the comparison alone and keeps the
# trace, which is the whole point of taking it.
sed -i "4i exec 2>/tmp/beamfs-xtrace.\$\$" $T
sed -i "5i set -x" $T
head -8 $T'"#
            ),
            Duration::from_secs(60),
        )
        .map_err(|e| format!("instrument: {e}"))?;
    if !out.contains("set -x") {
        return Err("the test was not instrumented".into());
    }
    Ok(())
}

/// Put the test back as it was.
///
/// A campaign that leaves an instrumented test behind makes every later
/// run measure something else, and the difference is invisible.
pub fn restore(cfg: &Config, node: &Node, test: &str) {
    let c = NodeConn::new(node, cfg);
    let _ = c.run(
        &format!(
            "sudo sh -c '[ -f /usr/xfstests/tests/{test}.orig ] && \
             mv /usr/xfstests/tests/{test}.orig /usr/xfstests/tests/{test}'"
        ),
        Duration::from_secs(30),
    );
}

/// Everything small, from every trial, whatever the outcome.
///
/// A few hundred kilobytes each. The passing trials matter as much as
/// the failing ones: a variable only implicates itself when the two
/// differ, and half the evidence cannot show that.
pub fn collect(cfg: &Config, node: &Node, case: &Case, check_output: &str) {
    let c = NodeConn::new(node, cfg);
    case.put("check.out", check_output);

    let files = [
        ("full", format!("/usr/xfstests/results/{}.full", case.test)),
        ("out.bad", format!("/usr/xfstests/results/{}.out.bad", case.test)),
        ("dmesg", "@dmesg".into()),
        ("mounts", "@mount".into()),
        ("meminfo", "/proc/meminfo".into()),
        ("vmstat", "/proc/vmstat".into()),
        ("diskstats", "/proc/diskstats".into()),
        ("slabinfo", "/proc/slabinfo".into()),
    ];
    for (name, src) in files {
        let cmd = match src.as_str() {
            "@dmesg" => "sudo dmesg".to_string(),
            "@mount" => "mount".to_string(),
            p => format!("sudo cat {p} 2>/dev/null || true"),
        };
        if let Ok((body, _)) = c.run_rc(&cmd, Duration::from_secs(60)) {
            case.put(name, &body);
        }
    }

    // The -x trace, from where the test redirected it. Kept beside
    // the rest rather than left on the node: it is the only record of
    // which line the trial reached.
    if let Ok((t, _)) = c.run_rc(
        "sudo sh -c 'cat /tmp/beamfs-xtrace.* 2>/dev/null; rm -f /tmp/beamfs-xtrace.*' || true",
        Duration::from_secs(120),
    ) {
        if !t.trim().is_empty() {
            case.put("xtrace", &t);
        }
    }
}

/// The volume as the failure left it, before anything reformats it.
///
/// Compressed on the node and streamed back: a 1 GiB scratch volume of
/// mostly zeroes is a few tens of megabytes, and the copy has to happen
/// before the next trial's mkfs, which is the reason none of the
/// earlier failures could be looked at twice.
pub fn freeze_volume(cfg: &Config, node: &Node, case: &Case) -> Result<u64, String> {
    let c = NodeConn::new(node, cfg);
    let _ = std::fs::create_dir_all(&case.dir);

    // Unmounted first: an image taken from under a live mount is a
    // picture of neither state.
    let _ = c.run(
        "sudo sh -c 'umount /mnt/scratch 2>/dev/null || umount -l /mnt/scratch 2>/dev/null; true'",
        Duration::from_secs(60),
    );

    let out = case.dir.join("scratch.img.zst");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ssh -i {key} -o BatchMode=yes -o StrictHostKeyChecking=no {user}@{host} \
             'sudo dd if={dev} bs=1M 2>/dev/null | zstd -3 -T0 -c' > {out}",
            key = cfg.ssh_key,
            user = cfg.user,
            host = node.host,
            dev = node.scratch_dev,
            out = out.display()
        ))
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("volume not frozen".into());
    }
    std::fs::metadata(&out).map(|m| m.len()).map_err(|e| e.to_string())
}

/// Strip what differs between any two runs but means nothing.
///
/// Pids, timestamps, kernel addresses and inode numbers change on every
/// run and would fill a diff with lines that carry no information. What
/// is left is the shape of what happened.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut s = String::with_capacity(line.len());
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '0' && chars.peek() == Some(&'x') {
                // 0xffff93f8c5798958 -- an address, never comparable.
                chars.next();
                while chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    chars.next();
                }
                s.push_str("0xADDR");
                continue;
            }
            if ch.is_ascii_digit() {
                let mut n = String::new();
                n.push(ch);
                while chars.peek().is_some_and(|c| c.is_ascii_digit() || *c == '.') {
                    n.push(chars.next().unwrap());
                }
                // Short numbers are line numbers, slots, small counts --
                // the things worth comparing. Long ones are pids,
                // timestamps and block numbers.
                if n.len() >= 5 || n.contains('.') {
                    s.push('N');
                } else {
                    s.push_str(&n);
                }
                continue;
            }
            s.push(ch);
        }
        out.push_str(&s);
        out.push('\n');
    }
    out
}

/// Where a failing trial stopped doing what a passing one did.
///
/// With the load seeded the two ran the same script over the same
/// files, so the first line that differs is the divergence itself --
/// not a hypothesis about it. This is the whole point of instrumenting
/// the test.
pub struct Divergence {
    pub line: usize,
    pub in_pass: String,
    pub in_fail: String,
    /// The twenty lines of the failing trace that follow.
    pub after: Vec<String>,
    /// How far each trial got.
    pub pass_lines: usize,
    pub fail_lines: usize,
}

pub fn diverge(pass: &Path, fail: &Path) -> Option<Divergence> {
    let p = std::fs::read_to_string(pass.join("check.out")).ok()?;
    let f = std::fs::read_to_string(fail.join("check.out")).ok()?;
    let pn = normalise(&p);
    let fn_ = normalise(&f);

    let pl: Vec<&str> = pn.lines().collect();
    let fl: Vec<&str> = fn_.lines().collect();
    let raw_f: Vec<&str> = f.lines().collect();

    for (i, (a, b)) in pl.iter().zip(fl.iter()).enumerate() {
        if a != b {
            return Some(Divergence {
                line: i + 1,
                in_pass: (*a).to_string(),
                in_fail: (*b).to_string(),
                after: raw_f.iter().skip(i).take(20).map(|s| s.to_string()).collect(),
                pass_lines: pl.len(),
                fail_lines: fl.len(),
            });
        }
    }
    // Identical as far as the shorter one goes: the failure is that it
    // stopped, and where it stopped is the answer.
    if pl.len() != fl.len() {
        let at = pl.len().min(fl.len());
        return Some(Divergence {
            line: at,
            in_pass: pl.get(at).unwrap_or(&"(ended)").to_string(),
            in_fail: fl.get(at).unwrap_or(&"(ended)").to_string(),
            after: raw_f.iter().skip(at.saturating_sub(4)).take(20).map(|s| s.to_string()).collect(),
            pass_lines: pl.len(),
            fail_lines: fl.len(),
        });
    }
    None
}

/// Print where the two trials parted company.
pub fn report_divergence(d: &Divergence) {
    println!("  === where the failing trial diverged ===");
    println!("  the two traces agree for {} lines, then:", d.line - 1);
    println!("    passing: {}", d.in_pass.trim());
    println!("    failing: {}", d.in_fail.trim());
    println!(
        "  the passing trial ran {} lines, the failing one {}",
        d.pass_lines, d.fail_lines
    );
    println!();
    println!("  what the failing trial did next:");
    for l in d.after.iter().take(14) {
        println!("    {}", l.trim());
    }
    println!();
}

/// What is on disk for this campaign, and what it cost.
pub fn inventory(root: &Path) -> Vec<(String, u64, bool)> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let size: u64 = std::fs::read_dir(&p)
            .map(|d| {
                d.flatten()
                    .filter_map(|x| x.metadata().ok().map(|m| m.len()))
                    .sum()
            })
            .unwrap_or(0);
        let frozen = p.join("scratch.img.zst").exists();
        v.push((
            p.file_name().unwrap_or_default().to_string_lossy().into(),
            size,
            frozen,
        ));
    }
    v.sort();
    v
}

/// Keep the disk from filling with volume images.
///
/// The text is worth keeping indefinitely -- it is kilobytes. The
/// images are not: `keep` of them, newest first, and the rest deleted.
/// A campaign that fills the disk stops mid-run, and the run that
/// stopped is the one nobody has evidence for.
pub fn prune(root: &Path, keep: usize) -> usize {
    let mut imgs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return 0 };
    for e in rd.flatten() {
        let img = e.path().join("scratch.img.zst");
        if let Ok(m) = std::fs::metadata(&img) {
            if let Ok(t) = m.modified() {
                imgs.push((t, img));
            }
        }
    }
    imgs.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    let mut removed = 0;
    for (_, p) in imgs.into_iter().skip(keep) {
        if std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalising_removes_what_changes_between_runs() {
        let a = normalise("+42| xfs_io -c pwrite 1048576 [pid 31337] 0xffff93f8c5798958");
        let b = normalise("+42| xfs_io -c pwrite 2097152 [pid 44444] 0xffff93f8c5790000");
        // Same line number, same command, different sizes and addresses.
        assert_eq!(a, b);
    }

    #[test]
    fn a_line_number_is_short_enough_to_survive() {
        // The whole point of PS4: the line number must be comparable.
        let n = normalise("+42| something");
        assert!(n.starts_with("+42|"), "got {n}");
    }

    #[test]
    fn a_trial_that_stopped_early_diverges_where_it_stopped() {
        let d = tempdir();
        let p = d.join("pass");
        let f = d.join("fail");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(p.join("check.out"), "+1| a\n+2| b\n+3| c\n+4| d\n").unwrap();
        std::fs::write(f.join("check.out"), "+1| a\n+2| b\n").unwrap();
        let div = diverge(&p, &f).expect("diverges");
        assert_eq!(div.pass_lines, 4);
        assert_eq!(div.fail_lines, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_identical_traces_do_not_diverge() {
        let d = tempdir();
        let p = d.join("p");
        let f = d.join("f");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&f).unwrap();
        // Different pids, same shape: seeded runs differ only in noise.
        std::fs::write(p.join("check.out"), "+1| run 11111\n").unwrap();
        std::fs::write(f.join("check.out"), "+1| run 22222\n").unwrap();
        assert!(diverge(&p, &f).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "beamfs-ev-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }
}
