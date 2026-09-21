// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! What has to be true before a campaign means anything.
//!
//! Each of these was discovered the same way: by spending hours on a
//! result that could not have been right.
//!
//! The kernel takes its beamfs sources from the Yocto layer, not from
//! the repository. A fix committed in one and not copied to the other
//! is simply not in the build, and the campaign measures the code the
//! fix was meant to replace -- while every commit, every diff and every
//! addr2line says the fix is there.
//!
//! The seal says which image was deployed. A build made afterwards sits
//! in the deploy directory unused, and a run started then tests the
//! previous kernel under the new kernel's name.
//!
//! Both are refusals rather than warnings. A warning at the top of a
//! seven-hour run is a warning nobody reads, and the result it produces
//! is indistinguishable from a real one afterwards.

use std::path::{Path, PathBuf};

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// The repository holding the filesystem sources.
fn repo_dir() -> PathBuf {
    std::env::var("BEAMFS_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home()).join("git/beamfs"))
}

/// The copy the kernel build actually compiles.
///
/// The version was written into this path, so every bump left the tool
/// compiling from a directory that no longer existed -- 0.1.4 and 0.1.5
/// both failed here, with a deploy reporting only that mkfs did not
/// build. The layer holds one beamfs-<version> directory; find it
/// rather than name it.
pub fn layer_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BEAMFS_LAYER_SRC") {
        return PathBuf::from(p);
    }
    let files = PathBuf::from(home())
        .join("git/yocto-beamfs/recipes-kernel/beamfs/files");
    newest_beamfs_dir(&files)
        .unwrap_or_else(|| files.join("beamfs-0.1.3"))
}

/// The highest-versioned beamfs-<version> directory under `files`.
///
/// Sorted by version number and not by name, so 0.1.10 comes after
/// 0.1.9 rather than before it.
pub fn newest_dir(files: &Path, prefix: &str) -> Option<PathBuf> {
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for e in std::fs::read_dir(files).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Some(v) = name.strip_prefix(prefix) else {
            continue;
        };
        if !e.path().is_dir() {
            continue;
        }
        let parts: Vec<u64> = v.split('.').filter_map(|x| x.parse().ok()).collect();
        if parts.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| parts > *b) {
            best = Some((parts, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

fn newest_beamfs_dir(files: &Path) -> Option<PathBuf> {
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for e in std::fs::read_dir(files).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Some(v) = name.strip_prefix("beamfs-") else {
            continue;
        };
        if !e.path().is_dir() {
            continue;
        }
        let parts: Vec<u64> = v.split('.').filter_map(|x| x.parse().ok()).collect();
        if parts.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| parts > *b) {
            best = Some((parts, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Every source file that differs between the repository and the layer.
///
/// Only the files the kernel build takes: .c, .h, Kconfig, Makefile.
/// The userspace tools have their own recipes and are not copied.
#[must_use]
pub fn sources_in_sync() -> Vec<String> {
    let repo = repo_dir();
    let layer = layer_dir();
    let mut out = Vec::new();

    let Ok(entries) = std::fs::read_dir(&repo) else {
        return vec![format!("cannot read {}", repo.display())];
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let take = name.ends_with(".c")
            || name.ends_with(".h")
            || name == "Kconfig"
            || name == "Makefile";
        if !take {
            continue;
        }
        // mkfs and fsck are built by their own recipes and the kernel
        // recipe deletes them after copying; a difference there means
        // nothing for the module under test.
        if name.starts_with("mkfs.") || name.starts_with("fsck.") {
            continue;
        }
        let there = layer.join(&name);
        if !there.exists() {
            out.push(format!("{name} is in the repository and not in the layer"));
        } else if !same_bytes(&e.path(), &there) {
            out.push(format!("{name} differs between the repository and the layer"));
        }
    }
    // The formatter and the shared decoder live under tools/ in the
    // repository and flat in the layer. They are not in the loop above
    // and were covered by nothing at all: mkfs writes the format, and
    // a divergence there is a volume laid out by one version and read
    // by another.
    for (there, here) in [
        ("mkfs.beamfs.c", "tools/mkfs.beamfs/mkfs.beamfs.c"),
        ("rs_decode.c", "tools/fsck.beamfs/rs_decode.c"),
        ("rs_decode.h", "tools/fsck.beamfs/rs_decode.h"),
        ("rs_decode_internal.h", "tools/fsck.beamfs/rs_decode_internal.h"),
    ] {
        let a = repo.join(here);
        let b = layer.join(there);
        if !a.exists() {
            out.push(format!("{here} is missing from the repository"));
        } else if !b.exists() {
            out.push(format!("{there} is missing from the layer"));
        } else if !same_bytes(&a, &b) {
            out.push(format!("{there} differs between the repository and the layer"));
        }
    }

    out.sort();
    out
}

/// Whether the image the seal names is still the newest one built.
///
/// Returns the complaint, or None when the deployed image is current.
#[must_use]
/// The most recently built image, by modification time.
///
/// Symlinks are skipped: the stable name points at the newest build
/// and would compare a file to itself under another name.
pub fn newest_image() -> Option<PathBuf> {
    let dir = PathBuf::from(crate::lab::deploy_dir());
    let entries = std::fs::read_dir(&dir).ok()?;

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.ends_with(".rootfs.beamfs") || n.contains("rootfs.beamfs.") {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        if m.file_type().is_symlink() {
            continue;
        }
        let Ok(t) = m.modified() else { continue };
        if newest.as_ref().is_none_or(|(bt, _)| t > *bt) {
            newest = Some((t, e.path()));
        }
    }
    newest.map(|(_, p)| p)
}

/// Whether the image now on the node is the newest one built.
///
/// This says nothing about the commits: an image can be the newest
/// there is and still predate every fix made since. That question is
/// `deploy::commits_after_image`, and calling this one by its name was
/// how a checkpoint came to report that an image carried every commit
/// twenty minutes after deploy had listed two it did not.
pub fn image_is_current() -> Option<String> {
    let path = newest_image()?;
    let now = crate::chain::sha256_file(&path)?;
    let seal = crate::chain::read()?;

    if seal.image_sha256 == now {
        None
    } else {
        Some(format!(
            "the newest image is not the one deployed: {} was built after the last deploy",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
}

/// Settings from a file, so a campaign is not a line of exports.
///
/// Read before the environment is consulted and never overriding it:
/// a variable set on the command line is the operator saying something
/// deliberate about this one run.
///
/// Format is one `key = value` per line, # for comments. Keys are the
/// variable names without the prefix, lowercased:
///
///     nodes   = x86-01:192.168.122.99:vdb:vdc
///     machine = qemux86-64
///     image   = beamfs-research-image
pub fn load_config_file() {
    let p = std::env::var("XFSTESTS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(home()).join(".config/beamfs-xfstests/config")
        });
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return;
    };
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = format!("XFSTESTS_{}", k.trim().to_uppercase());
        let val = v.trim();
        if val.is_empty() {
            continue;
        }
        if std::env::var(&key).is_err() {
            // Safety: single-threaded, before any work starts.
            unsafe { std::env::set_var(&key, val) };
        }
    }
}
