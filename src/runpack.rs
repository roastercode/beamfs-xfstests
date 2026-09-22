// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! One archive per run, written without being asked.
//!
//! The evidence directory holds everything a failure produced, and
//! moving it anywhere meant a tarball assembled by hand. A step between
//! a run and its analysis is where findings go missing, so the run
//! leaves one behind.
//!
//! /tmp, because that is where the operator's own logs go and because it
//! survives the session without surviving the machine.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Pack the cases this run produced.
///
/// @cases are the directories this run wrote -- not the whole evidence
/// tree, which holds every run before it. Returns the archive and its
/// size.
pub fn pack_run(cases: &[PathBuf], stamp: &str) -> Option<(PathBuf, u64)> {
    if cases.is_empty() {
        return None;
    }
    // xz, not zstd.
    //
    // The archive leaves this machine to be read somewhere else, and
    // zstd is not everywhere yet -- a reader without it gets "Cannot
    // exec: No such file or directory" and the run's whole record is
    // unreadable for want of a decompressor. xz is older and is on
    // everything.
    let out = PathBuf::from(format!("/tmp/beamfs-xfstests-{stamp}.tar.xz"));
    let root = cases[0].parent()?.to_path_buf();

    let mut args: Vec<String> = vec![
        "-C".into(), root.to_string_lossy().into_owned(),
        "-cJf".into(),
        out.to_string_lossy().into_owned(),
    ];
    for c in cases {
        if let Some(name) = c.file_name() {
            args.push(name.to_string_lossy().into_owned());
        }
    }
    if !Command::new("tar").args(&args).status().ok()?.success() {
        return None;
    }
    let size = std::fs::metadata(&out).ok()?.len();
    Some((out, size))
}

/// Delete archives older than a week.
///
/// A dozen of them, eleven megabytes each, accumulated in /tmp in one
/// day. The evidence they hold is still under the evidence root; what
/// the archive adds is the carrying, and a week is longer than anyone
/// waits to carry one.
fn prune(keep_days: u64) -> usize {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(keep_days * 86_400);
    let mut n = 0;
    let Ok(d) = std::fs::read_dir("/tmp") else { return 0 };
    for e in d.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("beamfs-xfstests-") || !name.ends_with(".tar.xz") {
            continue;
        }
        if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
            if t < cutoff && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Every text file of a case, concatenated, nothing dropped.
///
/// The digest picks and cuts, which is right for a first read and
/// wrong when the answer is in a file it does not pick. slabinfo held
/// 413940 live buffer_heads and vmstat held 2967695 pages dirtied
/// against 1998614 written -- a million pages dirtied and never
/// written on a volume that had run out of space -- and neither was in
/// any digest, through run after run.
///
/// This one drops nothing but the compressed images.
pub fn trace(dirs: &[PathBuf]) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = PathBuf::from(format!("/tmp/beamfs-xfstests-{stamp}.trace"));
    let mut f = std::fs::File::create(&out)?;

    for d in dirs {
        let name = d.file_name().unwrap_or_default().to_string_lossy();

        let mut files: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();

        for p in files {
            let n = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            // The images are megabytes of compressed bytes and belong
            // in the archive; everything else is text and stays.
            if n.ends_with(".zst") || n.ends_with(".img") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&p) else { continue };
            if body.trim().is_empty() {
                continue;
            }
            writeln!(f, "\n===== {name}/{n} ({} bytes) =====\n",
                     body.len())?;
            f.write_all(body.as_bytes())?;
        }
    }

    Ok(out)
}

/// One plain text file with everything worth reading in it.
///
/// The archive holds seventeen files per case and a compressed volume
/// image, which is right for keeping and wrong for reading: a reader
/// opens the one file they expect, misses the sixteen others, and
/// concludes from what they did not look at.
///
/// This is the same evidence as a single document -- what the checker
/// said, what the kernel said, what the probe counted -- with the
/// bulk left in the archive beside it.
pub fn digest(dirs: &[PathBuf]) -> std::io::Result<PathBuf> {
    use std::io::Write;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = PathBuf::from(format!("/tmp/beamfs-xfstests-{stamp}.md"));
    let mut f = std::fs::File::create(&out)?;

    writeln!(f, "# run {stamp}")?;

    for d in dirs {
        let name = d.file_name().unwrap_or_default().to_string_lossy();
        writeln!(f, "\n## {name}\n")?;

        // The verdict and the reason, first: everything else is
        // detail under them.
        /*
         * Whole, not sampled.
         *
         * The first digest kept 3884 bytes of the 46087 the case
         * holds in text -- the rest of the directory is a compressed
         * volume image, which has no place in a file meant to be
         * read. full is 66 lines, fsck.verbose 19, dmesg 173: there
         * was never anything to save by cutting them.
         *
         * The cap is high enough to be no cap at all in practice and
         * low enough that a runaway log cannot make this unreadable.
         */
        for (title, file, lines) in [
            ("what the checker found", "full", 400usize),
            ("what it named", "fsck.verbose", 400),
            ("what the runner saw before the kill", "runner.log", 400),
            ("why the test failed", "check.out", 40),
            ("where it was mounted", "mounts", 20),
            ("what the device did", "diskstats", 10),
            ("what memory looked like", "meminfo", 12),
            ("which tracepoints were on", "tracing.state", 12),
        ] {
            if file.is_empty() {
                continue;
            }
            let p = d.join(file);
            let Ok(body) = std::fs::read_to_string(&p) else { continue };
            let picked: Vec<&str> = body
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('+'))

                .take(lines)
                .collect();
            if picked.is_empty() {
                continue;
            }
            writeln!(f, "### {title}\n```")?;
            for l in picked {
                writeln!(f, "{l}")?;
            }
            writeln!(f, "```")?;
        }

        // The kernel, collapsed: a run says the same sentence
        // hundreds of times and the count is the information.
        if let Ok(body) = std::fs::read_to_string(d.join("dmesg")) {
            let mut seen: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for l in body.lines() {
                if !l.contains("beamfs") {
                    continue;
                }
                let msg = l.split_once("] ").map(|x| x.1).unwrap_or(l);
                let shape: String = msg
                    .chars()
                    .map(|c| if c.is_ascii_digit() { '#' } else { c })
                    .collect();
                *seen.entry(shape).or_insert(0) += 1;
            }
            if !seen.is_empty() {
                let mut v: Vec<(String, usize)> = seen.into_iter().collect();
                v.sort_by_key(|r| std::cmp::Reverse(r.1));
                writeln!(f, "### what the kernel said\n```")?;
                for (msg, n) in v.iter().take(60) {
                    writeln!(f, "{n:6}  {msg}")?;
                }
                writeln!(f, "```")?;
            }
        }

        // Whatever the probe left, by its counters only: a capture is
        // megabytes and its totals are ten lines.
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.starts_with("bpf-") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(e.path()) else { continue };
            let totals: Vec<&str> = body
                .lines()
                .filter(|l| l.starts_with('@') && l.contains(':'))
                .take(80)
                .collect();
            writeln!(f, "### {n}\n```")?;
            for l in totals {
                writeln!(f, "{l}")?;
            }
            writeln!(f, "```")?;
            /*
             * Where the rest of it is.
             *
             * The totals are ten lines and the capture is megabytes;
             * a reader who needs the stacks needs the path, and
             * looking for it on the node finds nothing -- the probe
             * pulls it here and removes it there.
             */
            writeln!(f, "\nthe whole capture: {}, {} bytes\n",
                     e.path().display(),
                     std::fs::metadata(e.path()).map(|m| m.len()).unwrap_or(0))?;
        }
    }

    Ok(out)
}

/// Say where it went.
pub fn announce(path: &Path, size: u64) {
    let gone = prune(7);
    println!();
    if gone > 0 {
        println!("  {gone} archive(s) older than a week removed from /tmp");
    }
    println!("  everything this run kept: {}", path.display());
    println!("    {} KiB, unpack with: tar -xf {}", size / 1024, path.display());
}
