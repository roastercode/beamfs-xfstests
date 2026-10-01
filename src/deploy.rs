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
    let dir = PathBuf::from(crate::lab::deploy_dir());
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
/// files/fsck-beamfs-<version>, which is what the image compiles. Nothing
/// kept them together.
///
/// On 2026-09-13 the image's copy was missing fsck_read.c and
/// fsck_pass6.c entirely -- half the checker's passes -- and three more
/// files had diverged. The binary in the image was 98368 bytes against
/// the 857568 built here, and it answered a campaign's questions for an
/// afternoon.
///
/// Returns the files that differ.
/// Seconds since the epoch of a file's last change, 0 when unknown.
fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The sources the kernel is built from, under a directory: what
/// do_inject_beamfs installs into fs/beamfs.
fn module_sources(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext == "c" || ext == "h" || name == "Kconfig" || name == "Makefile" {
            v.push(p);
        }
    }
    v.sort();
    v
}

/// Which of @files changed after @built, with how long after.
///
/// Its own function so the judgement can be tested without a build
/// directory: the comparison is what was wrong, not the paths.
#[must_use]
pub fn changed_after(files: &[(String, u64)], built: u64) -> Vec<String> {
    files
        .iter()
        .filter(|(_, when)| *when > built)
        .map(|(name, when)| format!("{name} changed {} min after the image was built",
                                    (when - built) / 60))
        .collect()
}

/// What the image was built from, against what this repository holds.
///
/// Judged on content, not on the clock. Until 2.3.19 this read the
/// time of the last commit touching beamfs sources and called the
/// image stale when a commit came after it; on 2026-09-22 the image
/// was built from the exact file_inline.c that was committed twenty
/// minutes later, and the guard refused a sweep that measured what it
/// named, while XFSTESTS_FORCE was the only way through. A guard that
/// is right by its rule and wrong in substance teaches the reader to
/// pass FORCE without looking.
///
/// Three things are asked instead, each with a remedy in its line:
/// does the layer's copy match the repository, file by file, for what
/// do_inject_beamfs installs (tools/sync-layer.sh if not); was the
/// layer's copy changed after the image was built (bitbake if so); and
/// were the layer's recipes and configs -- not the mirror -- committed
/// after the build (bitbake if so).
pub fn commits_after_image(image: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(home) = std::env::var("HOME") else { return out };
    let home = PathBuf::from(home);
    let built = mtime_secs(image);

    let repo = home.join("git/beamfs");
    let files = home.join("git/yocto-beamfs/recipes-kernel/beamfs/files");
    let layer = crate::clean::newest_dir(&files, "beamfs-")
        .unwrap_or_else(|| files.join("beamfs-0.1.5"));

    // 1. The layer's copy against the repository.
    if repo.is_dir() && layer.is_dir() {
        for p in module_sources(&repo) {
            let Some(name) = p.file_name() else { continue };
            let there = layer.join(name);
            let same = match (std::fs::read(&p), std::fs::read(&there)) {
                (Ok(x), Ok(y)) => x == y,
                (Ok(_), Err(_)) => false,
                _ => true,
            };
            if !same {
                out.push(format!("{} in the repository is not what the layer holds: \
                                  run tools/sync-layer.sh, then bitbake",
                                 name.to_string_lossy()));
            }
        }
    }

    // 2. The layer's copy against the image.
    if built > 0 && layer.is_dir() {
        let stamped: Vec<(String, u64)> = module_sources(&layer)
            .iter()
            .map(|p| (p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                      mtime_secs(p)))
            .collect();
        for l in changed_after(&stamped, built) {
            out.push(format!("the layer's {l}: run bitbake"));
        }
    }

    // 3. The layer's recipes and configs, by commit, the mirror left out.
    let yocto = home.join("git/yocto-beamfs");
    if let Ok(o) = Command::new("git")
        .args(["-C", &yocto.to_string_lossy(), "log", "-1", "--format=%ct %h %s", "--",
               ".", ":!recipes-kernel/beamfs/files"])
        .output()
    {
        if o.status.success() {
            let line = String::from_utf8_lossy(&o.stdout);
            let mut f = line.trim().splitn(3, ' ');
            let when: u64 = f.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let short = f.next().unwrap_or("");
            let subject = f.next().unwrap_or("");
            if built > 0 && when > built {
                out.push(format!(
                    "yocto-beamfs is {} min ahead of the image outside the mirror: {short} {}: run bitbake",
                    (when - built) / 60,
                    subject));
            }
        }
    }
    out
}

pub fn recipe_sources_current() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else { return Vec::new() };
    let repo = PathBuf::from(&home).join("git/beamfs/tools/fsck.beamfs");
    let files = PathBuf::from(&home)
        .join("git/yocto-beamfs/recipes-kernel/beamfs/files");
    // Found rather than named: the version used to be written into
    // this path, so a checker fix stayed in the repository while the
    // node went on running the previous one.
    let layer = crate::clean::newest_dir(&files, "fsck-beamfs-")
        .unwrap_or_else(|| files.join("fsck-beamfs-0.1.1"));

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

/// The tools this repo ships to the node, for the node's architecture.
///
/// A machine of this station's architecture gets a static build made
/// here: static because the node's libc is not this machine's, and a
/// tool that will not start is indistinguishable from one that found
/// nothing. A machine of another architecture gets the tools bitbake
/// built for it, found under the build tree the deploy directory
/// belongs to. Until 2.3.46 every machine got the static x86 build:
/// compute01 (aarch64) on 2026-09-26 carried an fsck.beamfs and an
/// mkfs.beamfs that answered "cannot execute binary file", check -n
/// died on its first mkfs, and the sweep said the harness listed no
/// tests.
fn build_tools(deploy_dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    if let Some(machine) = deploy_dir.file_name().and_then(|n| n.to_str()) {
        let target = match machine {
            "qemux86-64" | "qemux86" | "genericx86-64" => "x86_64",
            "qemuarm64" | "genericarm64" => "aarch64",
            other => return Err(format!("deploy: no architecture known for machine {other}")),
        };
        if target != std::env::consts::ARCH {
            return bitbake_tools(deploy_dir, machine, target);
        }
    }
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

    // The same directory the kernel build compiles, found rather than named.
    let mkfs_src = crate::clean::layer_dir();
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

/// The fsck.beamfs and mkfs.beamfs bitbake built for `machine`: the
/// newest `tmp/work/<tune>/<recipe>/<version>/image/usr/sbin/<tool>`
/// under the build tree that owns `deploy_dir`
/// (`<build>/tmp/deploy/images/<machine>`).
fn bitbake_tools(deploy_dir: &Path, machine: &str, target: &str)
    -> Result<Vec<(String, PathBuf)>, String>
{
    let tmp = deploy_dir
        .ancestors()
        .nth(3)
        .ok_or_else(|| format!("deploy: {} is not <build>/tmp/deploy/images/<machine>", deploy_dir.display()))?;
    let work = tmp.join("work");
    let mut v = Vec::new();
    for (recipe, tool) in [("fsck-beamfs", "fsck.beamfs"), ("mkfs-beamfs", "mkfs.beamfs")] {
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for tune in std::fs::read_dir(&work).map_err(|e| format!("{}: {e}", work.display()))?.flatten() {
            let r = tune.path().join(recipe);
            let Ok(versions) = std::fs::read_dir(&r) else { continue };
            for ver in versions.flatten() {
                let p = ver.path().join("image/usr/sbin").join(tool);
                if let Ok(m) = std::fs::metadata(&p) {
                    found.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), p));
                }
            }
        }
        found.sort();
        let Some((_, p)) = found.pop() else {
            return Err(format!(
                "deploy: no {tool} built by bitbake for {machine} under {}; \
                 bitbake {recipe} in that build first",
                work.display()));
        };
        v.push((tool.to_string(), p));
    }
    println!("  tools   : bitbake's, for {machine} ({target}); this station is {}",
             std::env::consts::ARCH);
    Ok(v)
}

fn md5(p: &Path) -> Option<String> {
    let o = Command::new("md5sum").arg(p).output().ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace().next().map(str::to_string)
}

/// The file this domain actually boots from.
///
/// Hardcoded as /var/lib/libvirt/images/x86/<domain>.beamfs before, which
/// is true only for as long as nobody moves the images. On 2026-09-21
/// they were moved to a directory carrying chattr +C, because btrfs
/// copy-on-write had fragmented the volumes into 180000 extents and was
/// dominating every measurement taken on them. deploy then wrote the new
/// rootfs to the old path, the domain booted from the new one, and it
/// printed "the node is ready" -- which is the failure the comment below
/// this call site already describes from a previous occurrence.
///
/// virsh knows. Asking costs one command and cannot drift.
fn rootfs_path_for(domain: &str) -> Result<String, String> {
    let out = Command::new("sudo")
        .args(["virsh", "domblklist", domain])
        .output()
        .map_err(|e| format!("virsh domblklist: {e}"))?;
    if !out.status.success() {
        return Err(format!("virsh domblklist {domain}: {}",
                           String::from_utf8_lossy(&out.stderr).trim()));
    }
    vda_from_domblklist(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| format!("{domain} lists no vda"))
}

/// The source path of vda, out of what virsh domblklist prints.
#[must_use]
pub fn vda_from_domblklist(text: &str) -> Option<String> {
    for l in text.lines() {
        let mut f = l.split_whitespace();
        let (Some(t), Some(src)) = (f.next(), f.next()) else { continue };
        if t == "vda" && src.starts_with('/') {
            return Some(src.to_string());
        }
    }
    None
}


/// Replace the node's image, restart it, and push the tools.
///
/// Returns when the node answers and its tools match, or an error
/// naming the step that did not.
pub fn deploy(cfg: &Config, node: &Node, domain: &str) -> Result<(), String> {
    let image = newest_image().ok_or_else(|| {
        format!("no .rootfs.beamfs under {}", crate::lab::deploy_dir())
    })?;
    let age = std::fs::metadata(&image)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0);
    println!("  image   : {} ({} min old)",
             image.file_name().unwrap_or_default().to_string_lossy(), age);

    // A stale image is refused here, before anything is sealed or
    // pushed. On 2026-09-22 deploy printed "run bitbake first" six
    // times, sealed the old image as the current one, pushed it, and
    // returned success; the sweep that followed then refused the same
    // image for the same reason. A tool that knows the answer does not
    // hand the question on.
    let stale = commits_after_image(Path::new(&image));
    if !stale.is_empty() && std::env::var("XFSTESTS_FORCE").is_err() {
        for l in &stale {
            println!();
            println!("  {l}");
        }
        return Err("the image predates the code it is meant to carry: run bitbake \
                    first (bitbake -C rootfs <image> when it finds nothing to \
                    rebuild), or set XFSTESTS_FORCE=1 to deploy it anyway".into());
    }

    // What the chain held before this deploy. Between two campaigns
    // beamfs-bench may have sealed an image of its own, and a BX run
    // that silently overwrites it is how the two labs drifted onto
    // different kernels for ten weeks.
    if let Some(prev) = crate::chain::read() {
        let now = crate::chain::sha256_file(&image).unwrap_or_default();
        if !now.is_empty() && now != prev.image_sha256 {
            let a = &prev.image_sha256[..16.min(prev.image_sha256.len())];
            let b = &now[..16.min(now.len())];
            println!("  chain   : replacing {a} ({}, sealed by {}) with {b}",
                     prev.machine, prev.tool);
        }
    }

    // Open the chain. BX deploys first, so it records which image the
    // campaign is about to measure; beamfs-bench reads this back and
    // refuses to run against a different one.
    //
    // Not fatal here: BX opens the chain rather than checking it, and
    // a campaign that cannot write a file in ~/.local/share is still a
    // campaign. It says so, which is the part that matters.
    match crate::chain::write("beamfs-xfstests", &image) {
        Ok(s) => {
            let short = &s.image_sha256[..16.min(s.image_sha256.len())];
            println!("  chain   : sealed {short} on {} (kernel {})",
                     s.machine,
                     if s.kernel_release.is_empty() { "unknown" } else { &s.kernel_release });
        }
        Err(e) => println!("  chain   : seal NOT written: {e}"),
    }

    for line in commits_after_image(Path::new(&image)) {
        println!();
        println!("  {line}");
        println!("  run bitbake first (bitbake -C rootfs <image> when it finds nothing to rebuild)");
        println!();
    }

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
    let tools = build_tools(&PathBuf::from(crate::lab::deploy_dir()))?;
    for (n, p) in &tools {
        println!("  built   : {n} ({} bytes)",
                 std::fs::metadata(p).map(|m| m.len()).unwrap_or(0));
    }

    let st = Command::new("sudo").args(["virsh", "destroy", domain]).output();
    let _ = st;
    std::thread::sleep(Duration::from_secs(3));

    let target = rootfs_path_for(domain)?;
    let out = Command::new("sudo")
        .arg("cp").arg(&image).arg(&target)
        .output()
        .map_err(|e| format!("cp image: {e}"))?;
    if !out.status.success() {
        return Err(format!("the image did not copy: {}",
                           String::from_utf8_lossy(&out.stderr).trim()));
    }

    /*
     * And it is the same image, byte for byte.
     *
     * cp reports success on a short write to a full filesystem, and
     * the node then boots whatever was there before: six hours of
     * measurements went into a rootfs from an earlier build, with
     * deploy saying "the node is ready" each time. The tools are
     * checksummed after transfer for exactly this reason and the root
     * filesystem -- the larger thing, and the one carrying the
     * kernel's own format -- was not.
     */
    {
        let want = md5(&image).ok_or("cannot hash the built image")?;
        let got = Command::new("sudo")
            .args(["md5sum", &target])
            .output()
            .map_err(|e| format!("md5sum on the node image: {e}"))?;
        let got = String::from_utf8_lossy(&got.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if got != want {
            return Err(format!(
                "the image on the node is not the one just built: \
                 {} against {}", got, want));
        }
        println!("  rootfs  : {} ({} bytes)",
                 &want[..12.min(want.len())],
                 std::fs::metadata(&image).map(|m| m.len()).unwrap_or(0));
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

    /*
     * Answering is not ready.
     *
     * A node whose root filesystem came up read-only answers "true"
     * over ssh like any other; the tools then land in /tmp, the
     * install into /usr/sbin fails, and it is that step, or the sweep
     * after it, that names the fault, each in its own words. The node
     * is ready when it takes a write where the tools go; if it does
     * not, what the kernel said about its root is printed and deploy
     * stops here, before the pushes.
     */
    let rw = c.run("sudo sh -c 'echo bx > /usr/sbin/.bx-rw && rm -f /usr/sbin/.bx-rw' \
                    && echo BX_RW",
                   Duration::from_secs(20))
        .unwrap_or_default();
    if !rw.contains("BX_RW") {
        let mounts = c.run("grep ' / ' /proc/mounts", Duration::from_secs(20))
            .unwrap_or_default();
        let dm = c.run("sudo dmesg | grep -i 'remount\\|read-only\\|fs error' | tail -5",
                       Duration::from_secs(20))
            .unwrap_or_default();
        println!("  root    : {}", mounts.trim());
        for l in dm.lines() {
            println!("  dmesg   : {l}");
        }
        return Err("the node answers but its root filesystem does not take a write; \
                    not deployed".into());
    }
    println!("  root    : writable");

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

    /*
     * Which build, not which version.
     *
     * uname -r is 7.3.0-rc2 for every build of the day, and
     * /proc/version carries a KBUILD_BUILD_TIMESTAMP Yocto pins, so
     * neither tells two kernels apart. The symbol table does: every
     * build lays it out differently, and a node still running the
     * previous one answers with a different hash.
     *
     * A node booted before the kernel was built ran a day of
     * measurements against code none of the fixes were in.
     */
    let sym = c.run("sudo md5sum /proc/kallsyms | cut -d' ' -f1",
                    Duration::from_secs(30))
        .unwrap_or_default();
    let sym = sym.trim();
    println!("  kernel  : {} ({})", k.trim(),
             &sym[..12.min(sym.len())]);

    {
        let built = std::path::Path::new(crate::lab::kernel_image());
        let age = std::fs::metadata(built)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs());
        let boot = c.run("cut -d. -f1 /proc/uptime", Duration::from_secs(20))
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);

        if let Some(age) = age {
            if boot > age {
                return Err(format!(
                    "the node booted {boot}s ago and the kernel was built \
                     {age}s ago: it is running the previous one"));
            }
        }
    }

    // Written down, so the next command does not ask again.
    //
    // sweep checks the same tools and the same kernel before its first
    // test: four round trips to establish what this just did. The
    // node's uptime goes with it -- a node rebooted by hand in between
    // has a smaller one, and the record stops applying.
    let up = c.run("cut -d. -f1 /proc/uptime", Duration::from_secs(20))
        .unwrap_or_default();
    crate::nodestate::mark(&node.name,
                           up.trim().parse().unwrap_or(0),
                           k.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_changed_after_the_build_is_named_and_one_before_is_not() {
        let files = vec![("file_inline.c".to_string(), 1_000_u64),
                         ("scrub.c".to_string(), 5_000_u64)];
        let v = changed_after(&files, 2_000);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].starts_with("scrub.c changed 50 min"), "{v:?}");
    }

    #[test]
    fn the_root_device_is_read_out_of_virsh() {
        let out = " Target   Source\n\
                   ------------------------------------------\n\
                   vda      /var/lib/libvirt/images/x86-nocow/beamfs-x86-01.beamfs\n\
                   vdb      /var/lib/libvirt/images/x86-nocow/beamfs-x86-01-test.img\n";
        assert_eq!(vda_from_domblklist(out).as_deref(),
                   Some("/var/lib/libvirt/images/x86-nocow/beamfs-x86-01.beamfs"));
    }

    #[test]
    fn a_domain_without_vda_is_not_guessed_at() {
        let out = " Target   Source\n vdb      /tmp/x.img\n";
        assert!(vda_from_domblklist(out).is_none(),
                "a missing vda must fail loudly, not fall back to a guess");
    }

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
