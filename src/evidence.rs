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
        let _ = std::fs::write(self.dir.join(name), Self::collapse(body));
    }

    /// Keep the first of each kind of line and count the rest.
    ///
    /// generic/269's dmesg said the same thing 2305 times in 204 KiB,
    /// and the run before it kept a 704 KiB one. The sentence is worth
    /// keeping; the copies are what makes the file unreadable.
    ///
    ///     [12.3] beamfs: indirect block 9702 has no parity written yet
    ///       ... x2305
    ///
    /// Consecutive only: a line that returns after something else is a
    /// second occasion, and a log that reorders its own history is
    /// worse than a long one.
    fn collapse(body: &str) -> String {
        let mut out = String::with_capacity(body.len() / 4);
        let mut last: Option<String> = None;
        let mut n = 0u64;

        let flush = |out: &mut String, n: u64| {
            if n > 0 {
                out.push_str(&format!("  ... x{}\n", n + 1));
            }
        };

        for line in body.lines() {
            let sh = Self::shape(line);
            if last.as_deref() == Some(sh.as_str()) {
                n += 1;
                continue;
            }
            flush(&mut out, n);
            n = 0;
            last = Some(sh);
            out.push_str(line);
            out.push('\n');
        }
        flush(&mut out, n);
        out
    }

    /// A line's shape: timestamp removed, digit runs marked.
    ///
    /// The kernel's "[ 12.345678]" is dropped rather than marked: its
    /// digit runs differ with the width of the number, which split one
    /// finding into a group of 1684 and a group of 621.
    fn shape(line: &str) -> String {
        let rest = match (line.find('['), line.find(']')) {
            (Some(a), Some(b)) if a == 0 && b > a => &line[b + 1..],
            _ => line,
        };
        let mut out = String::with_capacity(rest.len());
        let mut in_num = false;
        for ch in rest.chars() {
            if ch.is_ascii_digit() {
                if !in_num {
                    out.push('#');
                    in_num = true;
                }
            } else {
                out.push(ch);
                in_num = false;
            }
        }
        out
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
/// What the files just written actually say.
///
/// Printed under the verdict, so the interesting line is on the screen
/// rather than in a directory. Deliberately narrow: the kernel's own
/// complaints, and the checker's named blocks. Everything else is in
/// the files.
/// How many inodes the checker found to own blocks, in @text.
///
/// The pass 6 line and no other. "N inode(s) walked" from pass 3 is
/// the size of the inode table -- 16384 on every volume of this lab,
/// full or empty -- and taking the largest number of either phrasing
/// read 16384 for a freshly made volume against 1602 for the one that
/// had been tested, which is the comparison backwards. Twice this
/// check stayed silent on the run it was written for; both times the
/// pattern had never been tried against a real file.
fn inodes_owning(text: &str) -> Option<u64> {
    text.match_indices("block ownership OK (")
        .filter_map(|(i, pat)| {
            text[i + pat.len()..]
                .split(|c: char| !c.is_ascii_digit())
                .find(|w| !w.is_empty())
                .and_then(|w| w.parse::<u64>().ok())
        })
        .max()
}

/// Did the volume get remade between the two checkers?
///
/// Returns the two inode counts when the one read afterwards is a
/// small fraction of the one read during the test, which is what a
/// fresh mkfs looks like: two inodes against sixteen hundred.
fn remade_after(full: &str, fsck: &str) -> Option<(u64, u64)> {
    let during = inodes_owning(full)?;
    let after = inodes_owning(fsck)?;
    (during > 16 && after * 8 < during).then_some((during, after))
}

pub fn speak(case: &Case) {
    let read = |name: &str| -> String {
        std::fs::read_to_string(case.dir.join(name)).unwrap_or_default()
    };

    // The volume is not judged here: freeze_volume runs after this,
    // and looking for its output before it exists reported every
    // failure as one whose volume could not be kept.

    // What the kernel said, deduplicated.
    //
    // A ratelimited message repeats; the same sentence forty times is
    // one finding, and printing it forty times buries the other one.
    let dmesg = read("dmesg");
    // What treecheck caught in the act, kept apart from the rest.
    let mut caught: Vec<String> = Vec::new();
    let mut kernel: Vec<String> = Vec::new();
    for line in dmesg.lines() {
        let Some(i) = line.find("beamfs") else { continue };
        let msg = &line[i..];
        // Mount and unmount lines are the run working, not a finding.
        /*
         * What the filesystem's own checker caught, first and whole.
         *
         * treecheck prints LOST POINTER with the parent, the slot, who
         * put the pointer there and who found it gone -- the defect
         * named at the moment it happens, which is what every probe in
         * this harness is trying to reconstruct afterwards.
         *
         * It fired on generic/083 and generic/476 in one sweep and the
         * summary said nothing, because the loop below keeps the first
         * few of each shape and these were neither first nor frequent.
         */
        if msg.contains("LOST POINTER") || msg.contains("treecheck:") {
            if !msg.contains("no lost pointer") {
                caught.push(msg.trim().to_string());
            }
            continue;
        }

        if msg.contains("mounted v") || msg.contains("bitmaps initialized")
            || msg.contains("module loaded") || msg.contains("unmounting") {
            continue;
        }
        // Collapse by shape: numbers differ, the sentence does not.
        let shape: String = msg.chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect();
        if !kernel.iter().any(|k| {
            let ks: String = k.chars()
                .map(|c| if c.is_ascii_digit() { '#' } else { c })
                .collect();
            ks == shape
        }) {
            kernel.push(msg.to_string());
        }
    }

    if !kernel.is_empty() {
        println!("    the kernel said, during this test:");
        for (n, k) in kernel.iter().take(6).enumerate() {
            let _ = n;
            println!("      {}", k.trim());
        }
        if kernel.len() > 6 {
            println!("      and {} more kind(s), in dmesg", kernel.len() - 6);
        }
    }

    if !caught.is_empty() {
        println!("    the filesystem's own checker caught it happening:");
        for c in caught.iter().take(6) {
            println!("      {c}");
        }
        if caught.len() > 6 {
            println!("      and {} more, in dmesg", caught.len() - 6);
        }
    }

    // What the checker named.
    //
    // Two checkers run over the same device and they do not always see
    // the same volume. xfstests runs its own from _check_generic_-
    // filesystem while the test is still finishing, and remakes the
    // TEST_DEV when that check fails so the next test starts from a
    // sound one. The harness's fsck runs after that, and then reads a
    // volume nobody tested.
    //
    // One generic/013 had 96 used-but-unreferenced blocks over 1637
    // inodes in full, and "bitmap consistent with inode table" over 2
    // inodes in fsck.verbose. Both were true of the device they saw.
    // Reading the second as the test's verdict is how a run with 96
    // leaked blocks was called clean.
    //
    // The inode counts tell them apart, so say so rather than print a
    // verdict that belongs to a fresh volume.
    let fsck = read("fsck.verbose");
    let full = read("full");
    if let Some((during, after)) = remade_after(&full, &fsck) {
        println!("    fsck.verbose describes a volume that was remade \
                  after the test: {after} inode(s) against {during} \
                  during it -- the verdict is the one in full");
    }
    let mut named: Vec<&str> = Vec::new();
    for line in fsck.lines() {
        if line.contains("never described")
            || line.contains("beyond correction")
            || line.contains("out-of-range")
            || line.contains("marked used but unreferenced")
        {
            named.push(line.trim());
        }
    }
    if !named.is_empty() {
        // Grouped by what was said, because four hundred identical
        // findings are one finding with four hundred addresses.
        let mut kinds: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for l in &named {
            let k = if l.contains("never described") {
                "indirect blocks with no parity ever written"
            } else if l.contains("beyond correction") {
                "indirect blocks whose parity disagrees"
            } else if l.contains("out-of-range") {
                "pointers outside the device"
            } else {
                "blocks marked used that nothing references"
            };
            *kinds.entry(k.to_string()).or_insert(0) += 1;
        }
        println!("    the checker named:");
        for (k, n) in &kinds {
            println!("      {n} {k}");
        }
        if let Some(first) = named.first() {
            println!("      first: {first}");
        }
        println!("      all of them in fsck.verbose");
    }
}

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
        // The checker again, verbose, naming every block it faults.
        //
        // The run the harness reads is the one xfstests makes, and that
        // one is quiet: "17 indirect blocks beyond correction" without
        // saying which. Naming them is the difference between a count
        // and a place to look.
        ("fsck.verbose", "@fsck".into()),
        // Where check left the volume it declared inconsistent.
        //
        // _check_generic_filesystem remakes the TEST_DEV when its own
        // check fails, so everything the harness reads afterwards --
        // its fsck, its frozen image -- describes a volume nobody
        // tested: two inodes against the sixteen hundred the test
        // created. With DUMP_CORRUPT_FS set, check keeps an image of
        // the filesystem as it found it, and that one is the state of
        // the defect.
        //
        // The path and the size, not the image: this collection copies
        // the output of a command into a file, and calling that file
        // corrupt.img would be one more piece of evidence that is not
        // what its name says.
        ("dumped-by-check", "@dumped".into()),
        // Which tracepoints were on, so a trace that is empty can be
        // told from a trace that was never enabled.
        ("tracing.state", "@tracing".into()),
        ("interrupts", "/proc/interrupts".into()),
        ("locks", "/proc/locks".into()),
        ("buddyinfo", "/proc/buddyinfo".into()),
        ("zoneinfo", "/proc/zoneinfo".into()),
    ];
    for (name, src) in files {
        let cmd = match src.as_str() {
            "@dmesg" => "sudo dmesg".to_string(),
            // Whatever check left behind for this test, if anything.
            // Where check leaves it, and what it does with the
            // variable when it does not leave one.
            //
            // DUMP_CORRUPT_FS is set and known to this xfstests, and
            // no image appeared where the results live. Guessing at
            // another path is how an afternoon goes; asking the
            // harness what its own _dump_fs_image does costs one
            // command and answers it.
            "@dumped" => format!(
                "sudo sh -c 'find /usr/xfstests/results /var/tmp /tmp \
                 -maxdepth 3 -name \"*{}*\" \\( -name \"*.img*\" -o \
                 -name \"*metadump*\" \\) -newermt \"-10 minutes\" \
                 -ls 2>/dev/null; \
                 echo \"--- what check does with DUMP_CORRUPT_FS ---\"; \
                 grep -n -A 12 \"DUMP_CORRUPT_FS\" \
                 /usr/xfstests/common/rc 2>/dev/null | head -40'",
                case.test.replace('/', "-")),
            "@mount" => "mount".to_string(),
            // Both devices: a test that fails on the test device and a
            // test that fails on the scratch one look the same from
            // here, and the checker is cheap.
            "@fsck" => format!(
                // /dev/ prefixed here: the node carries the bare
                // name, and fsck given "vdb" looks for a file called
                // vdb in the working directory and finds none.
                "for d in /dev/{} /dev/{}; do echo \"--- $d ---\"; \
                 sudo fsck.beamfs -v $d 2>&1 | head -400; done",
                node.test_dev, node.scratch_dev),
            "@tracing" => "sudo sh -c 'for e in /sys/kernel/debug/tracing/events/beamfs/*/enable; \
                do echo \"$(basename $(dirname $e)) $(cat $e)\"; done; \
                echo \"tracing_on $(cat /sys/kernel/debug/tracing/tracing_on)\"' 2>/dev/null || true".to_string(),
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

    // The ftrace buffer, if anything was enabled.
    //
    // Staged to /tmp on the node and pulled with rsync: read through ssh
    // it would come back as a String the size of the buffer, and the
    // buffer is sized in hundreds of megabytes.
    {
        let staged = "/tmp/ev-case-trace.txt";
        let prep = format!(
            "sudo sh -c 'cat /sys/kernel/debug/tracing/trace > {staged} 2>/dev/null; \
             chmod 644 {staged}' || true");
        if c.run(&prep, std::time::Duration::from_secs(120)).is_ok() {
            let local = case.dir.join("trace.txt");
            if c.pull(staged, &local.to_string_lossy()).is_ok() {
                if let Ok(m) = std::fs::metadata(&local) {
                    // A trace with nothing in it but its header is not
                    // evidence, and keeping it suggests otherwise.
                    if m.len() < 2048 {
                        let _ = std::fs::remove_file(&local);
                    }
                }
            }
            let _ = c.run(&format!("rm -f {staged}"),
                             std::time::Duration::from_secs(20));
        }
    }

}


/// Which devices a failure implicates, from what the harness said.
///
/// _check_generic_filesystem names the device it found inconsistent:
///
///   _check_generic_filesystem: filesystem on /dev/vdb is inconsistent
///
/// A test that works in TEST_DIR breaks TEST_DEV and leaves SCRATCH_DEV
/// untouched, so freezing the scratch produces a clean image and an
/// afternoon spent reading it. generic/310 is that test.
///
/// Falls back to the scratch when the output names nothing, which is
/// what every test that fails without a check line does.
fn implicated_devices(output: &str, node: &Node) -> Vec<String> {
    let mut devs: Vec<String> = Vec::new();

    for line in output.lines() {
        let Some(rest) = line.split("filesystem on ").nth(1) else { continue };
        let Some(dev) = rest.split_whitespace().next() else { continue };
        let name = dev.trim_start_matches("/dev/").to_string();
        if !name.is_empty() && !devs.contains(&name) {
            devs.push(name);
        }
    }

    /*
     * Both, when the output does not say which.
     *
     * "filesystem on /dev/vdX is inconsistent" names the volume; an
     * output mismatch names nothing, and the scratch alone was kept.
     * generic/013 fails that way and its defect was on the test
     * device: the frozen image showed two inodes on a volume that had
     * just been reformatted, and an afternoon went into reading it.
     *
     * A gigabyte each, compressed to a few megabytes. Keeping the one
     * that does not matter costs less than missing the one that does.
     */
    if devs.is_empty() {
        devs.push(node.test_dev.trim_start_matches("/dev/").to_string());
        devs.push(node.scratch_dev.trim_start_matches("/dev/").to_string());
    }
    devs
}

/// The volume as the failure left it, before anything reformats it.
///
/// Compressed on the node and streamed back: a 1 GiB scratch volume of
/// mostly zeroes is a few tens of megabytes, and the copy has to happen
/// before the next trial's mkfs, which is the reason none of the
/// earlier failures could be looked at twice.
pub fn freeze_volume(
    cfg: &Config,
    node: &Node,
    case: &Case,
    output: &str,
) -> Result<u64, String> {
    let c = NodeConn::new(node, cfg);
    let _ = std::fs::create_dir_all(&case.dir);

    // Unmounted first: an image taken from under a live mount is a
    // picture of neither state.
    let _ = c.run(
        "sudo sh -c 'umount /mnt/test /mnt/scratch 2>/dev/null; \
         umount -l /mnt/test /mnt/scratch 2>/dev/null; true'",
        Duration::from_secs(60),
    );

    let devs = implicated_devices(output, node);
    let scratch = node.scratch_dev.trim_start_matches("/dev/").to_string();
    let mut total = 0u64;
    let mut kept = 0usize;
    let mut last_err = String::new();

    for dev in &devs {
        // scratch.img.zst for the scratch device, whatever its name, so
        // every tool that already reads that path keeps working. Other
        // devices get their own name.
        let out = if *dev == scratch {
            case.dir.join("scratch.img.zst")
        } else {
            case.dir.join(format!("{dev}.img.zst"))
        };

        // The node's device name, with or without the /dev prefix: the
        // config carries "vdc" and a first version passed it to dd as a
        // relative path, which failed silently and left thirteen bytes
        // of compressed nothing in every case directory.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ssh -i {key} -o BatchMode=yes -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR {user}@{host} \
                 'sudo dd if=/dev/{dev} bs=1M 2>/dev/null | zstd -3 -T0 -c' > {out}",
                key = cfg.ssh_key,
                user = cfg.user,
                host = node.host,
                out = out.display()
            ))
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            last_err = format!("{dev} not frozen");
            continue;
        }
        let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        // A compressed empty stream is about a dozen bytes. Keeping one
        // looks like evidence and is not.
        if sz < 1024 {
            let _ = std::fs::remove_file(&out);
            last_err = format!("{dev}: capture produced {sz} bytes -- nothing was read");
            continue;
        }
        if *dev != scratch {
            println!("    {dev} frozen too: the harness named it, not the scratch");
        }
        total += sz;
        kept += 1;
    }

    if kept == 0 {
        return Err(if last_err.is_empty() {
            "volume not frozen".into()
        } else {
            last_err
        });
    }
    Ok(total)
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
    // saturating: a divergence at the first line means they agree for
    // none, and d.line - 1 underflows on an unsigned zero.
    println!("  the two traces agree for {} lines, then:", d.line.saturating_sub(1));
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

#[cfg(test)]
mod collapse_tests {
    use super::*;

    #[test]
    fn the_timestamp_does_not_split_a_finding() {
        assert_eq!(
            Case::shape("[ 12.3] beamfs: indirect block 9702 has no parity"),
            Case::shape("[  4.5] beamfs: indirect block 10926 has no parity"));
    }

    #[test]
    fn different_sentences_stay_apart() {
        assert_ne!(
            Case::shape("[1.0] beamfs: block 1 has no parity"),
            Case::shape("[1.0] beamfs: block 1 beyond correction"));
    }

    #[test]
    fn a_repeat_is_counted_and_the_line_survives() {
        let body = "[1.0] beamfs: block 1 has no parity\n\
                    [1.1] beamfs: block 2 has no parity\n\
                    [1.2] beamfs: block 3 has no parity\n\
                    [1.3] something else\n";
        let out = Case::collapse(body);
        assert_eq!(out.matches("has no parity").count(), 1, "{out}");
        assert!(out.contains("... x3"), "{out}");
        assert!(out.contains("something else"), "{out}");
    }

    #[test]
    fn a_line_returning_later_is_a_second_occasion() {
        let body = "[1.0] A\n[1.1] B\n[1.2] A\n";
        let out = Case::collapse(body);
        // Not reordered into one group: the history is what it is.
        assert_eq!(out.matches("] A").count(), 2, "{out}");
    }

    #[test]
    fn nothing_is_lost_when_nothing_repeats() {
        let body = "[1.0] A\n[1.1] B\n[1.2] C\n";
        assert_eq!(Case::collapse(body).lines().count(), 3);
    }
}

#[cfg(test)]
mod remade_tests {
    use super::*;

    /// The run that was called clean while it had leaked 96 blocks.
    #[test]
    fn a_volume_remade_between_the_two_checkers_is_named() {
        let full = "fsck.beamfs: pass 4: 0 referenced-but-free, 96 \
                    used-but-unreferenced block(s)\n\
                    fsck.beamfs: pass 6: directories, links and block \
                    ownership OK (1637 inode(s))";
        let fsck = "fsck.beamfs: pass 3: inode table OK (16384 inode(s) walked)\n\
                    fsck.beamfs: pass 6: directories, links and block \
                    ownership OK (2 inode(s))";
        assert_eq!(remade_after(full, fsck), Some((1637, 2)));
    }

    #[test]
    fn the_same_volume_twice_is_not_reported() {
        let t = "fsck.beamfs: pass 6: directories, links and block \
                 ownership OK (282 inode(s))";
        assert_eq!(remade_after(t, t), None);
    }

    /// A test that creates almost nothing leaves counts too close to
    /// tell apart, and a false alarm there would discredit the real one.
    #[test]
    fn a_small_test_is_left_alone() {
        let full = "pass 6: block ownership OK (12 inode(s))";
        let fsck = "pass 6: block ownership OK (2 inode(s))";
        assert_eq!(remade_after(full, fsck), None);
    }

    /// The phrasing xfstests gets, which the first version missed.
    #[test]
    fn the_closing_line_counts_too() {
        let full = "fsck.beamfs: pass 6: directories, links and block \
                    ownership OK (86 inode(s))";
        let fsck = "fsck.beamfs: pass 3: inode table OK (16384 inode(s) walked)\n\
                    fsck.beamfs: pass 6: directories, links and block \
                    ownership OK (2 inode(s))";
        assert_eq!(remade_after(full, fsck), Some((86, 2)));
    }

    #[test]
    fn a_missing_count_is_not_a_verdict() {
        assert_eq!(remade_after("nothing here", "block ownership OK (2 inode(s))"), None);
    }
}
