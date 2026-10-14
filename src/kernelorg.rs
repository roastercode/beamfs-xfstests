// SPDX-License-Identifier: GPL-2.0-only
//! The newest mainline kernel, against the one the build prefers.
//!
//! The lab works on the newest -rc: when kernel.org publishes one, the
//! layer moves to it. Nothing told a measurement that it ran on an rc
//! the lab had already left; this does, as a warning, since the
//! measurement still describes the kernel it ran.
//!
//! The tags are read with `git ls-remote`, which fetches nothing, under
//! `timeout`: kernel.org out of reach costs 20 seconds and a warning,
//! never a measurement.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::checkpoint::{Finding, Level};

const MAINLINE: &str = "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
const SECONDS: &str = "20";
const LINK: &str = "kernel.org -> build";

/// A mainline version. An -rc comes before the release it leads to,
/// which comes before the next merge window's -rc1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KVer {
    major: u32,
    minor: u32,
    /// The -rc number, or `RELEASE` for the release itself.
    stage: u32,
}

const RELEASE: u32 = u32::MAX;

impl KVer {
    /// "v7.3-rc7", "7.3-rc7", "v7.3", "7.3". Anything else, the
    /// three-part tags of the 2.6 years included, gives `None`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let (base, stage) = match s.split_once("-rc") {
            Some((b, n)) => (b, number(n).filter(|k| *k > 0)?),
            None => (s, RELEASE),
        };
        let (major, minor) = base.split_once('.')?;
        Some(Self {
            major: number(major)?,
            minor: number(minor)?,
            stage,
        })
    }
}

fn number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl fmt::Display for KVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.stage == RELEASE {
            write!(f, "{}.{}", self.major, self.minor)
        } else {
            write!(f, "{}.{}-rc{}", self.major, self.minor, self.stage)
        }
    }
}

/// The newest mainline version among the tags `git ls-remote` listed.
fn newest(listed: &str) -> Option<KVer> {
    listed
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(_, r)| r.trim().strip_prefix("refs/tags/"))
        .filter(|n| !n.ends_with("^{}"))
        .filter_map(|n| {
            let v = KVer::parse(n)?;
            (n == format!("v{v}")).then_some(v)
        })
        .max()
}

/// The kernel version a `local.conf` prefers, with the rule of bitbake
/// for the operators: `=` and `:=` set it, `?=` and `??=` only if
/// nothing did.
fn preferred(text: &str) -> Option<String> {
    let mut v: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("PREFERRED_VERSION_linux-mainline") else {
            continue;
        };
        let rest = rest.trim_start();
        let (weak, value) = if let Some(r) = rest.strip_prefix("??=") {
            (true, r)
        } else if let Some(r) = rest.strip_prefix("?=") {
            (true, r)
        } else if let Some(r) = rest.strip_prefix(":=") {
            (false, r)
        } else if let Some(r) = rest.strip_prefix('=') {
            (false, r)
        } else {
            continue;
        };
        let Some(value) = quoted(value.trim()) else {
            continue;
        };
        if !weak || v.is_none() {
            v = Some(value);
        }
    }
    v
}

fn quoted(s: &str) -> Option<String> {
    let q = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &s[1..];
    let end = body.find(q)?;
    Some(body[..end].to_string())
}

/// The build directory above the deploy directory: the first that
/// holds a `conf/local.conf`.
fn build_dir(deploy: &Path) -> Option<PathBuf> {
    deploy
        .ancestors()
        .find(|a| a.join("conf/local.conf").is_file())
        .map(Path::to_path_buf)
}

fn warn(detail: String) -> Finding {
    Finding {
        link: LINK,
        level: Level::Warn,
        detail,
    }
}

/// What kernel.org says, against what the build of the machine prefers.
pub(crate) fn finding() -> Finding {
    let deploy = crate::lab::deploy_dir();
    let Some(build) = build_dir(Path::new(deploy)) else {
        return warn(format!("no build directory with a conf/local.conf above {deploy}"));
    };
    let conf = build.join("conf/local.conf");
    let name = conf.display();
    let text = std::fs::read_to_string(&conf).unwrap_or_default();
    let Some(wanted) = preferred(&text) else {
        return warn(format!("{name} names no PREFERRED_VERSION_linux-mainline"));
    };
    let listed = match Command::new("timeout")
        .args([SECONDS, "git", "ls-remote", "--tags", MAINLINE, "refs/tags/v*"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) if o.status.code() == Some(124) => {
            return warn(format!("kernel.org did not answer within {SECONDS} s"));
        }
        Ok(o) => {
            return warn(format!(
                "kernel.org not read: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return warn(format!("git ls-remote not run: {e}")),
    };
    let Some(top) = newest(&listed) else {
        return warn("kernel.org listed no mainline tag".to_string());
    };
    match KVer::parse(wanted.trim_end_matches('%')) {
        None => warn(format!(
            "{name} prefers {wanted}, not a mainline version; kernel.org is at {top}"
        )),
        Some(p) if p == top => Finding {
            link: LINK,
            level: Level::Fine,
            detail: format!("{name} prefers {p}, the newest tag on kernel.org"),
        },
        Some(p) if p < top => warn(format!("kernel.org has {top}; {name} prefers {p}")),
        Some(p) => warn(format!(
            "{name} prefers {p}, which kernel.org does not list (its newest is {top})"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> KVer {
        KVer::parse(s).unwrap()
    }

    #[test]
    fn versions_in_release_order() {
        assert_eq!(v("v7.3-rc7").to_string(), "7.3-rc7");
        assert_eq!(v("7.3-rc7"), v("v7.3-rc7"));
        assert_eq!(v("v7.3").to_string(), "7.3");
        assert!(v("7.3-rc6") < v("7.3-rc7"));
        assert!(v("7.3-rc9") < v("7.3-rc10"));
        assert!(v("7.3-rc7") < v("7.3"));
        assert!(v("7.3") < v("7.4-rc1"));
        assert!(v("7.9") < v("7.10-rc1"));
        for s in ["v2.6.12", "v7.3-rc", "v7.3-rc0", "v7", "v7.3-rc1x", "", "v.3"] {
            assert!(KVer::parse(s).is_none(), "{s}");
        }
    }

    #[test]
    fn the_newest_tag_listed() {
        let a = "1".repeat(40);
        let b = "2".repeat(40);
        let listed = format!(
            "{a}\trefs/tags/v7.3-rc7\n{b}\trefs/tags/v7.3-rc7^{{}}\n\
             {a}\trefs/tags/v7.3-rc10\n{a}\trefs/tags/v2.6.12\n\
             {a}\trefs/tags/v7.3-rc7-foo\n"
        );
        assert_eq!(newest(&listed), Some(v("7.3-rc10")));
        assert_eq!(newest("garbage\n"), None);
    }

    #[test]
    fn preferred_follows_bitbake() {
        let t = "PREFERRED_VERSION_linux-mainline ?= \"7.3-rc5\"\n\
                 # PREFERRED_VERSION_linux-mainline = \"7.3-rc1\"\n\
                 PREFERRED_VERSION_linux-mainline = \"7.3-rc7\"\n\
                 PREFERRED_VERSION_linux-mainline ?= \"7.3-rc2\"\n\
                 PREFERRED_VERSION_linux-mainline-rt = \"6.1\"\n";
        assert_eq!(preferred(t).as_deref(), Some("7.3-rc7"));
        assert_eq!(
            preferred("PREFERRED_VERSION_linux-mainline ?= '7.3-rc4'\n").as_deref(),
            Some("7.3-rc4")
        );
        assert_eq!(preferred("MACHINE = \"qemux86-64\"\n"), None);
    }

    #[test]
    fn the_build_directory_holds_the_local_conf() {
        let root = std::env::temp_dir().join(format!("bx-kernelorg-{}", std::process::id()));
        let deploy = root.join("build-x/tmp/deploy/images/qemux86-64");
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::create_dir_all(root.join("build-x/conf")).unwrap();
        std::fs::write(root.join("build-x/conf/local.conf"), "MACHINE = \"qemux86-64\"\n").unwrap();
        assert_eq!(build_dir(&deploy), Some(root.join("build-x")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
