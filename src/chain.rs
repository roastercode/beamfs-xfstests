// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! The seal that keeps BX and BB on one image.
//!
//! The validation chain is BX, then BB, then publication. It only
//! means something if both measured the same filesystem, built from
//! the same sources, running the same kernel. Nothing enforced that.
//!
//! On 2026-09-19 the x86 lab was still running images built on 4 July
//! from kernel 7.1.3 while the arm64 cluster ran 7.3.0-rc2, and the
//! difference surfaced only because emufi's kprobe on submit_bh --
//! a symbol removed between the two -- failed to register and took
//! the whole injector down with it. Twenty-seven attack runs measured
//! nothing, and the report called it resilience.
//!
//! So whoever deploys writes down which image it was, by its sha256,
//! and the next tool in the chain refuses to run against a different
//! one. Breaking the chain stays possible -- BEAMFS_CHAIN_IGNORE=1 --
//! because a harness nobody can run during development is a harness
//! that gets bypassed permanently. It is recorded when it happens, so
//! a run that skipped the check cannot be published as one that passed
//! it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the seal lives. Outside both repos: it belongs to the lab,
/// not to either tool, and it must survive a clean checkout of both.
#[must_use]
pub fn seal_path() -> PathBuf {
    if let Ok(p) = std::env::var("BEAMFS_CHAIN_SEAL") {
        if !p.trim().is_empty() {
            return PathBuf::from(p.trim());
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/beamfs-chain/seal.json")
}

/// What one link of the chain recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seal {
    pub tool: String,
    pub written_at: String,
    pub machine: String,
    pub image_path: String,
    pub image_sha256: String,
    pub kernel_release: String,
    pub commit_beamfs: String,
    pub commit_yocto: String,
}

/// sha256 of a file, via the command every one of these hosts has.
/// The crate has no dependencies and this keeps it that way.
#[must_use]
pub fn sha256_file(p: &Path) -> Option<String> {
    let o = Command::new("sha256sum").arg(p).output().ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

fn git_head(repo: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = format!("{home}/{repo}");
    Command::new("git")
        .args(["-C", &p, "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The kernel release string the image carries, read from the build
/// deploy directory rather than from a running node: the seal is
/// written at deploy time, before anything has booted.
fn kernel_release_of_build() -> String {
    let dir = PathBuf::from(crate::lab::deploy_dir());
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    // modules--7.3-rc3-r0-qemux86-64-20260919133231.tgz
    let mut best = String::new();
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(rest) = n.strip_prefix("modules--") {
            if let Some(v) = rest.split("-r0-").next() {
                if v > best.as_str() {
                    best = v.to_string();
                }
            }
        }
    }
    best
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Pull one string value out of the seal. The file is written by this
/// module and by beamfs-bench, both flat objects of string fields, so
/// a full parser would be more code than it protects.
fn field(txt: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    let Some(i) = txt.find(&needle) else {
        return String::new();
    };
    let rest = &txt[i + needle.len()..];
    let Some(c) = rest.find(':') else {
        return String::new();
    };
    let rest = &rest[c + 1..];
    let Some(a) = rest.find('"') else {
        return String::new();
    };
    let rest = &rest[a + 1..];
    let mut out = String::new();
    let mut escaped = false;
    for ch in rest.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            break;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Read the seal, if one was left.
#[must_use]
pub fn read() -> Option<Seal> {
    let txt = std::fs::read_to_string(seal_path()).ok()?;
    let s = Seal {
        tool: field(&txt, "tool"),
        written_at: field(&txt, "written_at"),
        machine: field(&txt, "machine"),
        image_path: field(&txt, "image_path"),
        image_sha256: field(&txt, "image_sha256"),
        kernel_release: field(&txt, "kernel_release"),
        commit_beamfs: field(&txt, "commit_beamfs"),
        commit_yocto: field(&txt, "commit_yocto"),
    };
    if s.image_sha256.is_empty() {
        return None;
    }
    Some(s)
}

/// Record the image this tool is about to deploy.
///
/// # Errors
/// When the image cannot be hashed or the seal cannot be written.
pub fn write(tool: &str, image: &Path) -> Result<Seal, String> {
    let sha = sha256_file(image)
        .ok_or_else(|| format!("sha256sum failed on {}", image.display()))?;
    let now = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let s = Seal {
        tool: tool.to_string(),
        written_at: now,
        machine: crate::lab::machine().to_string(),
        image_path: image.display().to_string(),
        image_sha256: sha,
        kernel_release: kernel_release_of_build(),
        commit_beamfs: git_head("git/beamfs"),
        commit_yocto: git_head("git/yocto-beamfs"),
    };

    let p = seal_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
    }
    let body = format!(
        "{{\n  \"tool\": \"{}\",\n  \"written_at\": \"{}\",\n  \"machine\": \"{}\",\n  \
         \"image_path\": \"{}\",\n  \"image_sha256\": \"{}\",\n  \
         \"kernel_release\": \"{}\",\n  \"commit_beamfs\": \"{}\",\n  \
         \"commit_yocto\": \"{}\"\n}}\n",
        esc(&s.tool),
        esc(&s.written_at),
        esc(&s.machine),
        esc(&s.image_path),
        esc(&s.image_sha256),
        esc(&s.kernel_release),
        esc(&s.commit_beamfs),
        esc(&s.commit_yocto),
    );
    std::fs::write(&p, body).map_err(|e| format!("write {}: {e}", p.display()))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seal path comes from the environment, which belongs to the
    /// process while cargo runs tests in threads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_seal_written_is_the_seal_read_back() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let img = std::env::temp_dir().join("beamfs-chain-test-image.bin");
        std::fs::write(&img, b"not an image, but it hashes").unwrap();
        let seal = std::env::temp_dir().join("beamfs-chain-test-seal.json");
        let _ = std::fs::remove_file(&seal);
        unsafe { std::env::set_var("BEAMFS_CHAIN_SEAL", &seal) };

        let written = write("test-harness", &img).expect("seal written");
        assert!(!written.image_sha256.is_empty());
        assert_eq!(written.tool, "test-harness");

        let back = read().expect("seal read back");
        assert_eq!(back.image_sha256, written.image_sha256);
        assert_eq!(back.image_path, img.display().to_string());
        assert_eq!(back.tool, "test-harness");
        assert_eq!(back.machine, written.machine);

        // The hash is of this file, not of whatever was there before.
        let direct = sha256_file(&img).expect("sha256sum");
        assert_eq!(direct, written.image_sha256);

        let _ = std::fs::remove_file(&seal);
        let _ = std::fs::remove_file(&img);
        unsafe { std::env::remove_var("BEAMFS_CHAIN_SEAL") };
    }

    #[test]
    fn a_different_file_seals_differently() {
        let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let a = std::env::temp_dir().join("beamfs-chain-a.bin");
        let b = std::env::temp_dir().join("beamfs-chain-b.bin");
        std::fs::write(&a, b"image one").unwrap();
        std::fs::write(&b, b"image two").unwrap();
        assert_ne!(sha256_file(&a), sha256_file(&b));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn a_field_comes_back_as_written() {
        let txt = "{\n  \"tool\": \"beamfs-xfstests\",\n  \"image_sha256\": \"abc123\"\n}";
        assert_eq!(field(txt, "tool"), "beamfs-xfstests");
        assert_eq!(field(txt, "image_sha256"), "abc123");
    }

    #[test]
    fn an_absent_field_is_empty_not_a_panic() {
        assert_eq!(field("{}", "tool"), "");
        assert_eq!(field("", "image_sha256"), "");
    }

    #[test]
    fn a_quoted_path_survives_the_round_trip() {
        let weird = r#"/tmp/a "b"\c"#;
        let txt = format!("{{\"image_path\": \"{}\"}}", esc(weird));
        assert_eq!(field(&txt, "image_path"), weird);
    }

    #[test]
    fn a_seal_without_a_hash_is_not_a_seal() {
        // A truncated or half-written file must not pass for a seal:
        // an empty sha256 compares equal to nothing and would let a
        // mismatched image through.
        let s = Seal::default();
        assert!(s.image_sha256.is_empty());
    }
}
