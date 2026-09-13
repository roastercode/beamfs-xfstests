// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! Put the image and the tools on a node, and prove they arrived.
//!
//! This was a shell block written fresh on every cycle, and it checked
//! none of its own transfers. An rsync failed silently on 2026-09-13,
//! the image's own fsck.beamfs answered in place of this repo's, and a
//! campaign reported 306 destroyed inodes on a volume that was sound.
//!
//! Every step here reports what it did, and the run stops at the first
//! one that did not.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// Where the freshly built image is.
fn newest_image() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HOME").ok()?)
        .join("yocto/poky/build-qemux86/tmp/deploy/images/qemux86-64");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.to_string_lossy().ends_with(".rootfs.beamfs") {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Are the Yocto recipe's sources the ones this repo builds from?
///
/// fsck.beamfs lives twice: once under beamfs/tools/fsck.beamfs, which
/// is what gets edited, and once under the layer's
/// files/fsck-beamfs-0.1.0, which is what the image compiles. Nothing
/// kept them together.
///
/// On 2026-09-13 the image's copy was missing fsck_read.c and
/// fsck_pass6.c entirely -- half the checker's passes -- and three more
/// files had diverged. The binary in the image was 98368 bytes against
/// the 857568 built here, and it answered a campaign's questions for an
/// afternoon.
///
/// Returns the files that differ.
fn recipe_sources_current() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else { return Vec::new() };
    let repo = PathBuf::from(&home).join("git/beamfs/tools/fsck.beamfs");
    let layer = PathBuf::from(&home)
        .join("git/yocto-beamfs/recipes-kernel/beamfs/files/fsck-beamfs-0.1.0");

    diff_trees(&repo, &layer)
}

/// Which .c and .h files under @a differ from @b, or are missing there.
///
/// Its own function so it can be tested without a checkout: the
/// comparison is what was missing, not the paths.
fn diff_trees(a: &Path, b: &Path) -> Vec<String> {
    let mut stale = Vec::new();
    let Ok(d) = std::fs::read_dir(a) else { return stale };
    for e in d.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext != "c" && ext != "h" {
            continue;
        }
        let Some(name) = p.file_name() else { continue };
        let there = b.join(name);
        let same = match (std::fs::read(&p), std::fs::read(&there)) {
            (Ok(x), Ok(y)) => x == y,
            (Ok(_), Err(_)) => {
                stale.push(format!("{} is not in the layer at all",
                                   name.to_string_lossy()));
                continue;
            }
            _ => true,
        };
        if !same {
            stale.push(format!("{} differs from the layer's copy",
                               name.to_string_lossy()));
        }
    }
    stale
}

/// Build the static tools this repo ships to the node.
///
/// Static because the node's libc is not this machine's, and a tool
/// that will not start is indistinguishable from one that found
/// nothing.
fn build_tools() -> Result<Vec<(String, PathBuf)>, String> {
    let home = PathBuf::from(std::env::var("HOME").map_err(|e| e.to_string())?);
    let fsck_dir = home.join("git/beamfs/tools/fsck.beamfs");

    // Quiet: a clean that prints its rm line in the middle of a
    // deploy adds nothing and hides the lines that matter.
    let _ = Command::new("make")
        .current_dir(&fsck_dir)
        .arg("clean")
        .output();
    let out = Command::new("make")
        .current_dir(&fsck_dir)
        .arg("LDFLAGS=-static")
        .output()
        .map_err(|e| format!("make fsck: {e}"))?;
    if !out.status.success() {
        return Err(format!("fsck did not build: {}",
                           String::from_utf8_lossy(&out.stderr)
                               .lines().rev().take(3)
                               .collect::<Vec<_>>().join(" | ")));
    }

    let mkfs_src = home.join("git/yocto-beamfs/recipes-kernel/beamfs/files/beamfs-0.1.3");
    let mkfs_out = PathBuf::from("/tmp/mkfs.beamfs.static");
    let out = Command::new("cc")
        .args(["-O2", "-std=gnu11", "-static"])
        .arg(format!("-I{}", mkfs_src.display()))
        .arg("-o").arg(&mkfs_out)
        .arg(mkfs_src.join("mkfs.beamfs.c"))
        .arg(mkfs_src.join("rs_decode.c"))
        .output()
        .map_err(|e| format!("cc mkfs: {e}"))?;
    if !out.status.success() {
        return Err(format!("mkfs did not build: {}",
                           String::from_utf8_lossy(&out.stderr)
                               .lines().rev().take(3)
                               .collect::<Vec<_>>().join(" | ")));
    }

    Ok(vec![
        ("fsck.beamfs".into(), fsck_dir.join("fsck.beamfs")),
        ("mkfs.beamfs".into(), mkfs_out),
    ])
}

fn md5(p: &Path) -> Option<String> {
    let o = Command::new("md5sum").arg(p).output().ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace().next().map(str::to_string)
}

/// Replace the node's image, restart it, and push the tools.
///
/// Returns when the node answers and its tools match, or an error
/// naming the step that did not.
pub fn deploy(cfg: &Config, node: &Node, domain: &str) -> Result<(), String> {
    let image = newest_image().ok_or("no image under deploy/images/qemux86-64")?;
    let age = std::fs::metadata(&image)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0);
    println!("  image   : {} ({} min old)",
             image.file_name().unwrap_or_default().to_string_lossy(), age);

    // The layer's copy of the checker, against this repo's.
    //
    // Not fatal: deploy pushes the binary built here, so the node ends
    // up correct either way. But the image carries the other one, and
    // anything that reads the image rather than the node -- a fresh
    // boot before deploy has run -- gets the stale checker.
    let stale = recipe_sources_current();
    if !stale.is_empty() {
        println!();
        println!("  the layer's fsck sources are behind this repo's:");
        for f in &stale {
            println!("    {f}");
        }
        println!("  the image will carry a checker that is not this one");
        println!();
    }

    // The tools first: a build that fails should not cost a reboot.
    let tools = build_tools()?;
    for (n, p) in &tools {
        println!("  built   : {n} ({} bytes)",
                 std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
    }

    let st = Command::new("sudo").args(["virsh", "destroy", domain]).output();
    let _ = st;
    std::thread::sleep(Duration::from_secs(3));

    let target = format!("/var/lib/libvirt/images/x86/{domain}.beamfs");
    let out = Command::new("sudo")
        .arg("cp").arg(&image).arg(&target)
        .output()
        .map_err(|e| format!("cp image: {e}"))?;
    if !out.status.success() {
        return Err(format!("the image did not copy: {}",
                           String::from_utf8_lossy(&out.stderr).trim()));
    }

    // The old host key, before the node comes back with a new one.
    //
    // Everything here passes UserKnownHostsFile=/dev/null, but an
    // operator's own ssh does not, and a warning about a changed key
    // is the last thing wanted mid-campaign.
    let _ = Command::new("ssh-keygen")
        .args(["-R", &node.host])
        .output();

    let out = Command::new("sudo")
        .args(["virsh", "start", domain])
        .output()
        .map_err(|e| format!("virsh start: {e}"))?;
    if !out.status.success() {
        return Err(format!("the node did not start: {}",
                           String::from_utf8_lossy(&out.stderr).trim()));
    }

    // Waited for, not slept through: a fixed sleep is either too short
    // on a kernel with KASAN or wasted on one without.
    let c = NodeConn::new(node, cfg);
    print!("  waiting :");
    let mut up = false;
    for i in 1..=40 {
        std::thread::sleep(Duration::from_secs(5));
        if c.run("true", Duration::from_secs(8)).is_ok() {
            println!(" up after {}s", i * 5);
            up = true;
            break;
        }
        if i % 6 == 0 {
            print!(" {}s", i * 5);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
    if !up {
        println!();
        return Err("the node never answered".into());
    }

    // rsync, and then the checksum: a transfer that reports success
    // and lands nowhere is what put a 2011 checker on the node.
    for (name, path) in &tools {
        let want = md5(path).ok_or_else(|| format!("cannot hash {name}"))?;
        c.push(&path.to_string_lossy(), &format!("/tmp/{name}"))
            .map_err(|e| format!("{name} did not transfer: {e:?}"))?;
        c.run(&format!("sudo cp /tmp/{name} /usr/sbin/{name} && \
                        sudo chmod 755 /usr/sbin/{name} && rm -f /tmp/{name}"),
              Duration::from_secs(30))
            .map_err(|e| format!("{name} did not install: {e:?}"))?;

        let got = c.run(&format!("md5sum /usr/sbin/{name} | cut -d' ' -f1"),
                        Duration::from_secs(20))
            .unwrap_or_default();
        if got.trim() != want {
            return Err(format!(
                "{name} on the node is {}, not the {} built here",
                &got.trim()[..got.trim().len().min(12)],
                &want[..want.len().min(12)]));
        }
        println!("  pushed  : {name} {}", &want[..12]);
    }

    let _ = c.run("sudo rm -rf /usr/xfstests/results; sudo dmesg -C; \
                   sudo mkdir -p /mnt/test /mnt/scratch; \
                   sudo mount -t debugfs none /sys/kernel/debug 2>/dev/null || true",
                  Duration::from_secs(30));

    let k = c.run("uname -r", Duration::from_secs(20)).unwrap_or_default();
    println!("  kernel  : {}", k.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison that was missing when a two-day-old checker
    /// answered a campaign's questions.
    #[test]
    fn a_file_that_differs_is_reported() {
        let d = std::env::temp_dir().join(format!("bxd-{}", std::process::id()));
        let a = d.join("repo");
        let b = d.join("layer");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("same.c"), "int main(void){return 0;}").unwrap();
        std::fs::write(b.join("same.c"), "int main(void){return 0;}").unwrap();
        std::fs::write(a.join("moved.c"), "new").unwrap();
        std::fs::write(b.join("moved.c"), "old").unwrap();
        std::fs::write(a.join("only-here.c"), "x").unwrap();

        let stale = diff_trees(&a, &b);
        assert!(stale.iter().any(|s| s.contains("moved.c")), "{stale:?}");
        assert!(stale.iter().any(|s| s.contains("only-here.c")), "{stale:?}");
        assert!(!stale.iter().any(|s| s.contains("same.c")), "{stale:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A tree with nothing in it is not a divergence: an operator
    /// running against an image whose tools are the reference should
    /// not be refused.
    #[test]
    fn a_missing_tree_is_not_a_divergence() {
        let d = std::env::temp_dir().join(format!("bxd2-{}", std::process::id()));
        assert!(diff_trees(&d.join("nowhere"), &d.join("nor-here")).is_empty());
    }
}
