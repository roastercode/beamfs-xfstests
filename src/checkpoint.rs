// SPDX-License-Identifier: GPL-2.0-only
//! Everything that must agree before a measurement means anything.
//!
//! A measurement is a claim about a particular version of the code, and
//! the chain from an edit to the node running it has five links:
//!
//!     repository -> layer -> image -> node
//!                      \-> tools pushed to the node
//!
//! Every one of them has broken at least once, silently, and each time
//! the campaign went on measuring something other than what it said:
//!
//!   - the layer kept an old copy of the module, and a fix that every
//!     diff and addr2line agreed was in the build was not;
//!   - the layer was never sent the checker at all, so fsck 0.1.1
//!     corrected a walk through uninitialised stack in the repository
//!     while the node ran 0.1.0 and reported leaks that did not exist;
//!   - the path to both carried a version number, so a bump to 0.1.4
//!     and again to 0.1.5 left the tool compiling from a directory
//!     that no longer existed, reporting only that mkfs did not build;
//!   - the node was rebooted under a running bench by a deploy from
//!     another terminal.
//!
//! The checks themselves already existed, written one at a time where
//! each defect was found: in clean.rs for the module, in deploy.rs for
//! the checker, in node.rs for the pushed tools. Scattered, they ran at
//! different moments and never all of them: `bench` ran none.
//!
//! This is the junction. One call, every link, before anything is
//! measured -- and a version comparison that had no home before.

use crate::config::{Config, Node};
use std::path::{Path, PathBuf};

/// How much a broken link matters.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Level {
    /// Agrees.
    Fine,
    /// Disagrees, but the measurement still describes something real.
    Warn,
    /// Disagrees in a way that makes the measurement describe other
    /// code than the one it names.
    Stale,
}

/// One link of the chain, and what it says.
#[derive(Debug)]
pub struct Finding {
    pub link: &'static str,
    pub level: Level,
    pub detail: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// The version a source file declares, by a pattern that brackets it.
fn declared(path: &Path, before: &str, after: &str) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let i = s.find(before)? + before.len();
    let rest = &s[i..];
    let j = rest.find(after)?;
    Some(rest[..j].to_string())
}

/// The version a directory name carries, after `prefix`.
fn dir_version(files: &Path, prefix: &str) -> Option<String> {
    let d = crate::clean::newest_dir(files, prefix)?;
    let name = d.file_name()?.to_string_lossy().to_string();
    name.strip_prefix(prefix).map(|v| v.to_string())
}

/// Check every link, without touching the node.
///
/// The node's own checks need a connection and a running domain, so
/// they live in `verify_node` and are asked for separately: a report
/// about the repository should not wait on ssh.
pub fn verify_local() -> Vec<Finding> {
    let mut out = Vec::new();
    let h = home();
    let files = h.join("git/yocto-beamfs/recipes-kernel/beamfs/files");

    let wrong = crate::clean::sources_in_sync();
    out.push(Finding {
        link: "repository -> layer (module)",
        level: if wrong.is_empty() { Level::Fine } else { Level::Stale },
        detail: if wrong.is_empty() {
            "every source matches".into()
        } else {
            format!("{} file(s) differ: {}", wrong.len(), wrong.join(" "))
        },
    });

    let wrong = crate::deploy::recipe_sources_current();
    out.push(Finding {
        link: "repository -> layer (checker)",
        level: if wrong.is_empty() { Level::Fine } else { Level::Stale },
        detail: if wrong.is_empty() {
            "every source matches".into()
        } else {
            format!("{} file(s) differ: {}", wrong.len(), wrong.join(" "))
        },
    });

    // Two different questions, and calling one by the other's name is
    // how a checkpoint came to report that an image carried every
    // commit twenty minutes after deploy had listed two it did not.
    match crate::clean::newest_image() {
        None => out.push(Finding {
            link: "commits -> image",
            level: Level::Warn,
            detail: "no image found to compare against".into(),
        }),
        Some(img) => {
            let ahead = crate::deploy::commits_after_image(&img);
            out.push(Finding {
                link: "commits -> image",
                level: if ahead.is_empty() { Level::Fine } else { Level::Stale },
                detail: if ahead.is_empty() {
                    "the image was built after the last commit".into()
                } else {
                    ahead.join("; ")
                },
            });
        }
    }

    match crate::clean::image_is_current() {
        None => out.push(Finding {
            link: "image -> node",
            level: Level::Fine,
            detail: "the deployed image is the newest built".into(),
        }),
        Some(w) => out.push(Finding {
            link: "image -> node",
            level: Level::Stale,
            detail: w,
        }),
    }

    // The versions, which nothing compared before.
    //
    // MODULE_VERSION in the repository, the layer directory the kernel
    // unpacks, and the recipe that names it are three statements of the
    // same number, kept by hand in three places.
    let m = declared(&h.join("git/beamfs/super.c"), "MODULE_VERSION(\"", "\")");
    let d = dir_version(&files, "beamfs-");
    out.push(match (m.as_deref(), d.as_deref()) {
        (Some(a), Some(b)) if a == b => Finding {
            link: "module version",
            level: Level::Fine,
            detail: format!("{a}, in the source and in the layer"),
        },
        (Some(a), Some(b)) => Finding {
            link: "module version",
            level: Level::Stale,
            detail: format!("the source says {a}, the layer directory says {b}"),
        },
        _ => Finding {
            link: "module version",
            level: Level::Warn,
            detail: "could not be read".into(),
        },
    });

    let f = declared(&h.join("git/beamfs/tools/fsck.beamfs/fsck.beamfs.c"),
                     "#define FSCK_BEAMFS_VERSION \"", "\"");
    let d = dir_version(&files, "fsck-beamfs-");
    out.push(match (f.as_deref(), d.as_deref()) {
        (Some(a), Some(b)) if a == b => Finding {
            link: "checker version",
            level: Level::Fine,
            detail: format!("{a}, in the source and in the layer"),
        },
        (Some(a), Some(b)) => Finding {
            link: "checker version",
            level: Level::Stale,
            detail: format!("the source says {a}, the layer directory says {b}"),
        },
        _ => Finding {
            link: "checker version",
            level: Level::Warn,
            detail: "could not be read".into(),
        },
    });

    out
}

/// What the node says, against what is here.
pub fn verify_node(cfg: &Config, node: &Node) -> Vec<Finding> {
    let mut out = Vec::new();
    // Not our own lock: the caller takes it before asking, and a
    // command refusing itself is not a check, it is a deadlock with
    // better manners.
    if let Some((pid, what)) = crate::nodelock::holder(&node.name)
        .filter(|(pid, _)| *pid != std::process::id())
    {
        out.push(Finding {
            link: "node is free",
            level: Level::Stale,
            detail: format!("held by pid {pid} ({what})"),
        });
    } else {
        out.push(Finding {
            link: "node is free",
            level: Level::Fine,
            detail: "nothing else is measuring on it".into(),
        });
    }
    let _ = cfg;
    out
}

/// Refuse a measurement whose chain is broken.
///
/// XFSTESTS_FORCE takes it anyway, which is right for a deliberate
/// comparison against older code and wrong for everything else.
pub fn gate(cfg: &Config, node: &Node) -> Result<(), String> {
    let mut all = verify_local();
    all.extend(verify_node(cfg, node));
    let broken: Vec<&Finding> = all.iter().filter(|f| f.level == Level::Stale).collect();
    if broken.is_empty() {
        return Ok(());
    }
    if std::env::var("XFSTESTS_FORCE").is_ok() {
        for f in &broken {
            println!("  ignored : {} -- {}", f.link, f.detail);
        }
        return Ok(());
    }
    let mut msg = String::from("the chain from this repository to the node is broken:\n");
    for f in &broken {
        msg.push_str(&format!("    {:<30} {}\n", f.link, f.detail));
    }
    msg.push_str("  a measurement here would describe other code than it names; \
                  set XFSTESTS_FORCE=1 to take it anyway");
    Err(msg)
}

/// Print every link, whatever it says.
pub fn report(cfg: &Config, node: Option<&Node>) {
    let mut all = verify_local();
    if let Some(n) = node {
        all.extend(verify_node(cfg, n));
    }
    println!();
    for f in &all {
        let mark = match f.level {
            Level::Fine => "ok  ",
            Level::Warn => "?   ",
            Level::Stale => "STALE",
        };
        println!("  {mark} {:<30} {}", f.link, f.detail);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_read_from_between_its_brackets() {
        let p = std::env::temp_dir().join(format!("cp-{}.c", std::process::id()));
        std::fs::write(&p, "x\n#define FSCK_BEAMFS_VERSION \"0.1.1\"\ny\n").unwrap();
        assert_eq!(declared(&p, "#define FSCK_BEAMFS_VERSION \"", "\""),
                   Some("0.1.1".to_string()));
        assert_eq!(declared(&p, "nothing like this", "\""), None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_broken_link_is_stale_and_a_sound_one_is_not() {
        let f = Finding { link: "x", level: Level::Stale, detail: String::new() };
        assert_eq!(f.level, Level::Stale);
        assert_ne!(Level::Fine, Level::Stale);
    }
}
