// SPDX-License-Identifier: GPL-2.0-only
//! Which tests of a selection xfstests will run on a node, asked of
//! xfstests once and kept until the node changes (2.6.0).
//!
//! 612 of the 740 tests of the x86 sweep sweep-1791798230 were declined
//! by xfstests -- reflink, O_DIRECT, quotas, a dm target -- each after
//! the preparation, the trial and the evidence of a test that runs: 13
//! of the 19 hours of that sweep, for an answer that does not change
//! while the node does not.
//!
//! The answer is asked of xfstests, in each test's own context. Every
//! test is cut before its first line of work -- what comes before is
//! its preamble: sourcing, _begin_fstest, function definitions,
//! assignments, _require_*, _exclude_*, _supported_*, and the guards
//! that end in _notrun -- and ended by a _notrun of its own. All of them
//! run through one ./check, from a copy of check, common/ and tests/
//! under /var/tmp/bx-plan on the node; /usr/xfstests is not written. A
//! test whose notrun file holds the marker runs; one whose file holds
//! anything else is declined, with the reason xfstests gave; one with no
//! file has no answer and runs.
//!
//! A requirement written after the preamble is not seen: its test runs
//! and is declined as before. The error goes one way, a test run for
//! nothing, never a test hidden. Made by hand on x86-01 on 2026-10-14
//! and set beside sweep-1791798230, the plan took 923 s; no test that
//! ran in that sweep was declined, 563 were declined with the reason the
//! sweep gave, and 49 the sweep saw declined were run.
//!
//! The plan is kept under a key: the image sealed at deploy, the
//! kernel, check, common/ and tests/, the module, the tools, the node's
//! local.config and devices, the options of mkfs. A sweep under a known
//! key reuses its plan; under a new one it makes it first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{Config, Node};
use crate::node::NodeConn;

/// Bumped when what a plan is made of changes: an older plan is then
/// under another key and is not read as this one.
const FORMAT: u32 = 1;

/// What a cut test writes as its own _notrun.
const MARK: &str = "bx-plan: preamble met";

/// The copy on the node. /var/tmp is the image's volatile tmpfs:
/// nothing of it outlives a boot.
const DIR: &str = "/var/tmp/bx-plan";

/// Without a new answer from check for this long, the plan stops.
const STALL: Duration = Duration::from_secs(600);

/// What the plan says of one test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Planned {
    /// Its preamble met: xfstests runs it.
    Run,
    /// Declined by xfstests, with its reason.
    NotRun(String),
    /// No answer from check: it runs.
    Unknown(String),
}

/// The plan of one node under one key.
pub(crate) struct Plan {
    pub key: String,
    pub path: PathBuf,
    /// When it was last asked, seconds since the epoch.
    pub made: u64,
    /// How long asking took, the last time.
    pub secs: u64,
    /// What the key is the sha256 of.
    pub facts: String,
    /// What the kernel said while the plan was made, counted.
    pub kernel: Vec<String>,
    /// Per test: the lines kept, and the line its preamble ended on.
    pub cuts: BTreeMap<String, (usize, String)>,
    pub tests: BTreeMap<String, Planned>,
}

/// What one round of asking brought back.
struct Asked {
    tests: BTreeMap<String, Planned>,
    cuts: BTreeMap<String, (usize, String)>,
    kernel: Vec<String>,
    secs: u64,
}

impl Plan {
    fn empty(key: &str, path: &Path, facts: &str) -> Self {
        Self {
            key: key.to_string(),
            path: path.to_path_buf(),
            made: 0,
            secs: 0,
            facts: facts.to_string(),
            kernel: Vec::new(),
            cuts: BTreeMap::new(),
            tests: BTreeMap::new(),
        }
    }

    fn merge(&mut self, a: Asked) {
        self.tests.extend(a.tests);
        self.cuts.extend(a.cuts);
        self.kernel = a.kernel;
        self.secs = a.secs;
        self.made = unix_now();
    }
}

/// The tests a sweep runs, the ones it records as declined without
/// running them, and the declined ones it runs anyway as a control.
pub(crate) struct Split {
    /// In the order of the selection, the controls among them.
    pub run: Vec<String>,
    /// Test and the reason xfstests gave.
    pub planned: Vec<(String, String)>,
    pub control: Vec<String>,
}

fn note(s: &str) {
    crate::say::note(s);
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}

/// How many of the first lines of a test come before its first line of
/// work, and that line: "(end of file)" when there is none.
pub(crate) fn preamble(text: &str) -> (usize, String) {
    let mut keep = 0usize;
    let mut in_func = false;
    let mut cont = false;
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        // The rest of a line that ended with a backslash.
        if cont {
            keep = n;
            cont = l.ends_with('\\');
            continue;
        }
        // The body of a function, down to the brace in column 0.
        if in_func {
            keep = n;
            if l.starts_with('}') {
                in_func = false;
            }
            continue;
        }
        let t = l.trim_start_matches([' ', '\t']);
        if t.is_empty() || t.starts_with('#') {
            keep = n;
            continue;
        }
        if is_function(l) {
            keep = n;
            in_func = !l.trim_end_matches([' ', '\t']).ends_with('}');
            continue;
        }
        if is_header(l) {
            keep = n;
            cont = l.ends_with('\\');
            continue;
        }
        return (keep, l.to_string());
    }
    (keep, "(end of file)".to_string())
}

/// Length of the shell name a line starts with, 0 when it starts with
/// none.
fn ident_len(s: &str) -> usize {
    let b = s.as_bytes();
    match b.first() {
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => b
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count(),
        _ => 0,
    }
}

/// `name()` in column 0, with or without its brace.
fn is_function(l: &str) -> bool {
    let n = ident_len(l);
    n > 0 && l[n..].trim_start_matches([' ', '\t']).starts_with("()")
}

/// A line of the preamble that is not a comment or a function.
fn is_header(l: &str) -> bool {
    let word = |w: &str| {
        l.strip_prefix(w)
            .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
    };
    let spaced = |w: &str| l.strip_prefix(w).is_some_and(|r| r.starts_with([' ', '\t']));
    spaced(".")
        || spaced("source")
        || word("_begin_fstest")
        || word("_register_cleanup")
        || ["_require_", "_exclude_", "_supported_", "_fixed_by_", "_wants_"]
            .iter()
            .any(|p| l.starts_with(*p))
        || is_assignment(l)
        || spaced("trap")
        || is_seqres_full_removal(l)
        || is_notrun_guard(l)
}

/// `name=...` or `export name=...`.
fn is_assignment(l: &str) -> bool {
    let rest = match l.strip_prefix("export") {
        Some(r) if r.starts_with([' ', '\t']) => r.trim_start_matches([' ', '\t']),
        _ => l,
    };
    let n = ident_len(rest);
    n > 0 && rest[n..].starts_with('=')
}

/// `rm -f $seqres.full`, quoted or not.
fn is_seqres_full_removal(l: &str) -> bool {
    let Some(r) = l.strip_prefix("rm -f ") else {
        return false;
    };
    let r = r.trim_end_matches([' ', '\t']);
    let r = r.strip_prefix('"').unwrap_or(r);
    let r = r.strip_suffix('"').unwrap_or(r);
    r == "$seqres.full"
}

/// `[ ... ] && _notrun ...`, `test ... || _notrun ...`.
fn is_notrun_guard(l: &str) -> bool {
    if !(l.starts_with('[') || l.starts_with("test ") || l.starts_with("test\t")) {
        return false;
    }
    ["&&", "||"].iter().any(|op| {
        l.match_indices(*op).any(|(i, _)| {
            l[i + 2..].trim_start_matches([' ', '\t']).starts_with("_notrun")
        })
    })
}

/// A reason as /usr/xfstests would have given it: the copy the plan
/// runs from is named in the reasons that name a path, and those are
/// the same reasons.
pub(crate) fn normalized(reason: &str) -> String {
    const COPY: &str = "/bx-plan/x/";
    reason
        .split(' ')
        .map(|w| match w.find(COPY) {
            Some(i) => {
                let s = w[..i].find('/').unwrap_or(i);
                format!("{}/usr/xfstests/{}", &w[..s], &w[i + COPY.len()..])
            }
            None => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The test a notrun file of check's results belongs to.
fn test_of_notrun(path: &str) -> Option<String> {
    let p = path.strip_suffix(".notrun")?;
    let mut it = p.rsplit('/');
    let num = it.next()?;
    let fam = it.next()?;
    if num.is_empty() || fam.is_empty() || fam == "." {
        return None;
    }
    Some(format!("{fam}/{num}"))
}

/// The line check printed for a test.
fn line_of(out: &str, test: &str) -> Option<String> {
    out.lines()
        .find(|l| {
            l.strip_prefix(test)
                .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
        })
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The kernel's lines without their timestamps, each with its count,
/// the most frequent first; check's own "run fstests" lines left out.
fn counted(dmesg: &str) -> Vec<String> {
    let mut m: BTreeMap<String, usize> = BTreeMap::new();
    for l in dmesg.lines() {
        let text = match l.split_once("] ") {
            Some((h, r)) if h.starts_with('[') => r,
            _ => l,
        };
        let text = text.trim();
        if text.is_empty() || text.starts_with("run fstests ") {
            continue;
        }
        *m.entry(text.to_string()).or_insert(0) += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.into_iter().map(|(t, n)| format!("{n} {t}")).collect()
}

/// The tests out of one read of the node, by name.
fn split_texts(out: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for chunk in out.split("\n\u{1e}BX ").skip(1) {
        let (name, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
        m.insert(name.trim().to_string(), body.to_string());
    }
    m
}

/// sha256 of a text, by the command every one of these hosts has.
fn sha256_text(s: &str) -> Result<String, String> {
    use std::io::Write;
    let mut ch = std::process::Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("sha256sum: {e}"))?;
    ch.stdin
        .take()
        .ok_or("sha256sum: no stdin")?
        .write_all(s.as_bytes())
        .map_err(|e| format!("sha256sum: {e}"))?;
    let o = ch.wait_with_output().map_err(|e| format!("sha256sum: {e}"))?;
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| "sha256sum said nothing".to_string())
}

/// The key of the node as it stands, and what it is the sha256 of.
fn key_of(c: &NodeConn, cfg: &Config, node: &Node) -> Result<(String, String), String> {
    let remote = c
        .run(
            &format!(
                "cat /proc/version; cd /usr/xfstests && sha256sum check local.config; \
                 echo \"common+tests $(find common tests -type f | LC_ALL=C sort \
                 | xargs sha256sum | sha256sum)\"; \
                 echo \"src $(ls src | LC_ALL=C sort | sha256sum)\"; \
                 m=$(modinfo -n beamfs 2>/dev/null); \
                 if [ -f \"$m\" ]; then sha256sum \"$m\"; else echo \"beamfs module ${{m:-none}}\"; fi; \
                 for t in mkfs.beamfs fsck.beamfs; do p=$(command -v $t); \
                 if [ -n \"$p\" ]; then sha256sum \"$p\"; else echo \"$t absent\"; fi; done; \
                 lsblk -b -d -n -o NAME,SIZE /dev/{} /dev/{}",
                node.test_dev, node.scratch_dev
            ),
            Duration::from_secs(120),
        )
        .map_err(|e| format!("asking the node what it is: {e}"))?;
    let seal = crate::chain::read().map_or_else(|| "none".to_string(), |s| s.image_sha256);
    let facts = format!(
        "format={FORMAT}\nnode={}\nfstyp={}\nmkfs_options={}\ntest_dev={}\nscratch_dev={}\n\
         seal_image={seal}\n{}",
        node.name,
        cfg.fstyp,
        cfg.mkfs_options,
        node.test_dev,
        node.scratch_dev,
        remote.trim_end()
    );
    let key = sha256_text(&facts)?;
    Ok((key, facts))
}

fn path_for(node: &str, key: &str) -> PathBuf {
    crate::history::History::default_root()
        .join("plans")
        .join(node)
        .join(format!("{key}.plan"))
}

fn render(p: &Plan) -> String {
    let mut s = String::from("# beamfs-xfstests plan: which tests xfstests runs on this node\n");
    s.push_str(&format!(
        "key {}\nmade {}\nsecs {}\nbx {}\n",
        p.key,
        p.made,
        p.secs,
        env!("CARGO_PKG_VERSION")
    ));
    for l in p.facts.lines() {
        s.push_str(&format!("fact {l}\n"));
    }
    for k in &p.kernel {
        s.push_str(&format!("kernel {k}\n"));
    }
    for (t, (n, stop)) in &p.cuts {
        s.push_str(&format!("cut {t}\t{n}\t{stop}\n"));
    }
    for (t, e) in &p.tests {
        match e {
            Planned::Run => s.push_str(&format!("test {t}\tRUN\n")),
            Planned::NotRun(r) => s.push_str(&format!("test {t}\tNOTRUN\t{r}\n")),
            Planned::Unknown(r) => s.push_str(&format!("test {t}\tUNKNOWN\t{r}\n")),
        }
    }
    s
}

fn parse(text: &str, path: &Path) -> Plan {
    let mut p = Plan::empty("", path, "");
    for l in text.lines() {
        if let Some(v) = l.strip_prefix("key ") {
            p.key = v.to_string();
        } else if let Some(v) = l.strip_prefix("made ") {
            p.made = v.parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("secs ") {
            p.secs = v.parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("fact ") {
            p.facts.push_str(v);
            p.facts.push('\n');
        } else if let Some(v) = l.strip_prefix("kernel ") {
            p.kernel.push(v.to_string());
        } else if let Some(v) = l.strip_prefix("cut ") {
            let mut f = v.splitn(3, '\t');
            if let (Some(t), Some(n), Some(s)) = (f.next(), f.next(), f.next()) {
                p.cuts.insert(t.to_string(), (n.parse().unwrap_or(0), s.to_string()));
            }
        } else if let Some(v) = l.strip_prefix("test ") {
            let mut f = v.splitn(3, '\t');
            let (Some(t), Some(k)) = (f.next(), f.next()) else {
                continue;
            };
            let r = f.next().unwrap_or("").to_string();
            let e = match k {
                "RUN" => Planned::Run,
                "NOTRUN" => Planned::NotRun(r),
                "UNKNOWN" => Planned::Unknown(r),
                _ => continue,
            };
            p.tests.insert(t.to_string(), e);
        }
    }
    p
}

/// The plan kept under `key`, when there is one.
fn load(path: &Path, key: &str) -> Option<Plan> {
    let text = std::fs::read_to_string(path).ok()?;
    let p = parse(&text, path);
    (p.key == key).then_some(p)
}

fn save(p: &Plan) -> Result<(), String> {
    if let Some(d) = p.path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::write(&p.path, render(p)).map_err(|e| format!("{}: {e}", p.path.display()))
}

/// Ask xfstests, on the node, which of `tests` it runs there.
fn ask(c: &NodeConn, tests: &[String]) -> Result<Asked, String> {
    let t0 = Instant::now();
    let mut asked = Asked {
        tests: BTreeMap::new(),
        cuts: BTreeMap::new(),
        kernel: Vec::new(),
        secs: 0,
    };
    if tests.is_empty() {
        return Ok(asked);
    }

    // The tests as the node has them: what runs there is what is cut.
    let read = c
        .run(
            &format!(
                "cd /usr/xfstests/tests && for t in {}; do printf '\\n\\036BX %s\\n' \"$t\"; \
                 cat \"$t\" 2>/dev/null; done; true",
                tests.join(" ")
            ),
            Duration::from_secs(300),
        )
        .map_err(|e| format!("reading the tests on the node: {e}"))?;
    let texts = split_texts(&read);
    for t in tests {
        match texts.get(t) {
            Some(body) if !body.is_empty() => {
                let (n, stop) = preamble(body);
                asked.cuts.insert(t.clone(), (n, stop.replace('\t', " ")));
            }
            _ => {
                asked
                    .tests
                    .insert(t.clone(), Planned::Unknown("not found on the node".into()));
            }
        }
    }
    if asked.cuts.is_empty() {
        asked.secs = t0.elapsed().as_secs();
        return Ok(asked);
    }

    // The copy, and every test in it cut and ended by the marker.
    let list: String = asked.cuts.iter().map(|(t, (n, _))| format!("{t} {n}\n")).collect();
    let out = c
        .run(
            &format!(
                "D={DIR}; X=$D/x; sudo rm -rf $D; mkdir -p $X/results || exit 1; \
                 cd /usr/xfstests || exit 1; \
                 for e in *; do case \"$e\" in tests|results|check|common|local.config) ;; \
                 *) ln -s \"/usr/xfstests/$e\" \"$X/$e\" ;; esac; done; \
                 cp -a check common tests \"$X/\" || exit 1; \
                 grep -v RESULT_BASE local.config > \"$X/local.config\"; \
                 cat > $D/cuts <<'BXEOF'\n{list}BXEOF\n\
                 awk '{{print $1}}' $D/cuts > $D/list; \
                 while read -r t n; do f=\"$X/tests/$t\"; \
                 head -n \"$n\" \"$f\" > \"$f.bx\" \
                 && printf '\\n_notrun \"%s\"\\n' '{MARK}' >> \"$f.bx\" \
                 && chmod 755 \"$f.bx\" && mv \"$f.bx\" \"$f\" \
                 || echo \"BX_CUT_FAILED $t\"; done < $D/cuts; \
                 echo \"BX_CUT $(cd $X/tests && grep -l -F '{MARK}' $(cat $D/list) | wc -l)\""
            ),
            Duration::from_secs(300),
        )
        .map_err(|e| format!("copying the tests on the node: {e}"))?;
    let failed: Vec<&str> = out
        .lines()
        .filter_map(|l| l.trim().strip_prefix("BX_CUT_FAILED "))
        .collect();
    let cut: usize = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("BX_CUT "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if !failed.is_empty() || cut != asked.cuts.len() {
        return Err(format!(
            "{cut} of {} tests cut on the node; not cut: {}",
            asked.cuts.len(),
            failed.join(" ")
        ));
    }
    note(&format!(
        "  plan    : {cut} test(s) cut before their first line of work, in {DIR} on the node"
    ));

    // Detached, as the listing is: one blocking call would say nothing
    // until it ended, and nothing at all if it did not.
    let started = c
        .run(
            &format!(
                "cd {DIR}/x && test -L src && test -f check || exit 1; \
                 sudo dmesg -C; rm -f {DIR}/rc {DIR}/check.txt; \
                 nohup sh -c 'sudo env RESULT_BASE={DIR}/x/results ./check $(cat {DIR}/list) \
                 > {DIR}/check.txt 2>&1; echo $? > {DIR}/rc' > /dev/null 2>&1 < /dev/null & \
                 echo started"
            ),
            Duration::from_secs(30),
        )
        .map_err(|e| format!("starting check on the node: {e}"))?;
    if !started.contains("started") {
        return Err(format!("check did not start on the node: {}", started.trim()));
    }

    let total = asked.cuts.len();
    let poll = format!(
        "test -f {DIR}/rc && echo BX_DONE; \
         echo \"BX_N $(grep -c -E '^[a-z]+/[0-9]' {DIR}/check.txt 2>/dev/null)\"; \
         echo \"BX_K $(grep -c -F '[not run]' {DIR}/check.txt 2>/dev/null)\"; true"
    );
    let mut last = (0usize, 0usize);
    let mut moved = Instant::now();
    let mut said = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let o = c.run(&poll, Duration::from_secs(20)).unwrap_or_default();
        let num = |p: &str| -> usize {
            o.lines()
                .find_map(|l| l.trim().strip_prefix(p))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        };
        let now = (num("BX_N"), num("BX_K"));
        if now != last {
            last = now;
            moved = Instant::now();
        }
        if o.lines().any(|l| l.trim() == "BX_DONE") {
            break;
        }
        if said.elapsed() >= Duration::from_secs(60) {
            note(&format!(
                "  plan    : {}/{total} asked, {} declined, {} min",
                now.0,
                now.1,
                t0.elapsed().as_secs() / 60
            ));
            said = Instant::now();
        }
        if moved.elapsed() >= STALL {
            let _ = c.run("sudo pkill -x check", Duration::from_secs(20));
            return Err(format!(
                "check gave no answer for {} min, at {}/{total}; stopped, its output is \
                 {DIR}/check.txt on the node",
                STALL.as_secs() / 60,
                now.0
            ));
        }
    }

    // The answers: a notrun file per test, the marker or a reason.
    let res = c
        .run(
            &format!(
                "cd {DIR}/x/results && find . -name '*.notrun' | while read -r f; do \
                 printf '\\036%s\\t' \"$f\"; tr '\\n\\t' '  ' < \"$f\"; echo; done; true"
            ),
            Duration::from_secs(120),
        )
        .map_err(|e| format!("reading what check declined: {e}"))?;
    let mut declined: BTreeMap<String, String> = BTreeMap::new();
    for l in res.lines() {
        let Some(rest) = l.strip_prefix('\u{1e}') else {
            continue;
        };
        let Some((path, why)) = rest.split_once('\t') else {
            continue;
        };
        if let Some(t) = test_of_notrun(path) {
            declined.insert(t, why.trim().to_string());
        }
    }
    let check_out = c
        .run(&format!("cat {DIR}/check.txt"), Duration::from_secs(60))
        .unwrap_or_default();
    for t in asked.cuts.keys() {
        let p = match declined.get(t) {
            Some(w) if w == MARK => Planned::Run,
            Some(w) => Planned::NotRun(normalized(w)),
            None => Planned::Unknown(normalized(
                &line_of(&check_out, t).unwrap_or_else(|| "no answer from check".into()),
            )),
        };
        asked.tests.insert(t.clone(), p);
    }
    if let Ok(d) = c.run("sudo dmesg", Duration::from_secs(30)) {
        asked.kernel = counted(&d);
    }
    asked.secs = t0.elapsed().as_secs();
    Ok(asked)
}

/// The plan a sweep runs under: the one kept under the node's key, made
/// first for the tests it does not answer. None when XFSTESTS_PLAN=0.
pub(crate) fn for_sweep(
    c: &NodeConn,
    cfg: &Config,
    node: &Node,
    tests: &[String],
) -> Result<Option<Plan>, String> {
    if std::env::var("XFSTESTS_PLAN").is_ok_and(|v| v.trim() == "0") {
        note("  plan    : off, XFSTESTS_PLAN=0: every test runs");
        return Ok(None);
    }
    let (key, facts) = key_of(c, cfg, node)?;
    let path = path_for(&node.name, &key);
    let mut plan = load(&path, &key).unwrap_or_else(|| Plan::empty(&key, &path, &facts));
    let missing: Vec<String> = tests
        .iter()
        .filter(|t| !plan.tests.contains_key(*t))
        .cloned()
        .collect();
    if missing.is_empty() {
        note(&format!(
            "  plan    : the one of this node, key {}, made {} min ago",
            short(&key),
            unix_now().saturating_sub(plan.made) / 60
        ));
        return Ok(Some(plan));
    }
    note(&format!(
        "  plan    : {} test(s) of this selection without an answer under key {}, \
         asking xfstests",
        missing.len(),
        short(&key)
    ));
    let asked = ask(c, &missing)?;
    plan.merge(asked);
    save(&plan)?;
    note(&format!("  plan    : made in {} s, kept in {}", plan.secs, path.display()));
    Ok(Some(plan))
}

/// The plan made again for `tests`, whatever the node's key holds.
pub(crate) fn make(
    c: &NodeConn,
    cfg: &Config,
    node: &Node,
    tests: &[String],
) -> Result<Plan, String> {
    let (key, facts) = key_of(c, cfg, node)?;
    let path = path_for(&node.name, &key);
    let mut plan = load(&path, &key).unwrap_or_else(|| Plan::empty(&key, &path, &facts));
    note(&format!(
        "  plan    : key {}, asking xfstests about {} test(s)",
        short(&key),
        tests.len()
    ));
    let asked = ask(c, tests)?;
    plan.merge(asked);
    save(&plan)?;
    Ok(plan)
}

/// How many declined tests a sweep runs anyway: XFSTESTS_PLAN_CONTROL,
/// or 10.
pub(crate) fn controls() -> usize {
    std::env::var("XFSTESTS_PLAN_CONTROL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10)
}

/// Every stride-th of `from`, from an offset the seed gives, at most k.
fn pick(from: &[String], k: usize, seed: u64) -> Vec<String> {
    if k == 0 || from.is_empty() {
        return Vec::new();
    }
    let stride = (from.len() / k).max(1);
    let offset = usize::try_from(seed).unwrap_or(0) % stride;
    from.iter()
        .enumerate()
        .filter(|(i, _)| i % stride == offset)
        .map(|(_, t)| t.clone())
        .take(k)
        .collect()
}

/// What a sweep runs of `tests` under `plan`.
pub(crate) fn split(tests: &[String], plan: Option<&Plan>, controls: usize, seed: u64) -> Split {
    let Some(p) = plan else {
        return Split { run: tests.to_vec(), planned: Vec::new(), control: Vec::new() };
    };
    let declined: Vec<(String, String)> = tests
        .iter()
        .filter_map(|t| match p.tests.get(t) {
            Some(Planned::NotRun(r)) => Some((t.clone(), r.clone())),
            _ => None,
        })
        .collect();
    let names: Vec<String> = declined.iter().map(|(t, _)| t.clone()).collect();
    let control = pick(&names, controls, seed);
    let run = tests
        .iter()
        .filter(|t| control.contains(*t) || !names.contains(*t))
        .cloned()
        .collect();
    let planned = declined
        .into_iter()
        .filter(|(t, _)| !control.contains(t))
        .collect();
    Split { run, planned, control }
}

/// What the plan says of a selection, said.
pub(crate) fn summary(p: &Plan, tests: &[String]) {
    let mut run = 0usize;
    let mut unknown: Vec<String> = Vec::new();
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for t in tests {
        match p.tests.get(t) {
            Some(Planned::Run) => run += 1,
            Some(Planned::NotRun(r)) => *by.entry(r.as_str()).or_insert(0) += 1,
            Some(Planned::Unknown(u)) => unknown.push(format!("{t}: {u}")),
            None => unknown.push(format!("{t}: not in the plan")),
        }
    }
    let declined: usize = by.values().sum();
    note(&format!(
        "  plan    : key {}, kept in {}, asked in {} s",
        short(&p.key),
        p.path.display(),
        p.secs
    ));
    note(&format!(
        "  plan    : {run} run, {declined} declined by xfstests, {} without an answer",
        unknown.len()
    ));
    let mut v: Vec<(&str, usize)> = by.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (why, n) in v {
        note(&format!("    {n:4}  {why}"));
    }
    for u in &unknown {
        note(&format!("    no answer: {u}"));
    }
    if !p.kernel.is_empty() {
        note("  plan    : the kernel, while the plan was made (count, line):");
        for k in &p.kernel {
            note(&format!("    {k}"));
        }
    }
}

/// The plan set beside the verdicts of a sweep.
#[derive(Default)]
pub(crate) struct Comparison {
    /// Ran there, run by the plan.
    pub kept: usize,
    /// Declined there and by the plan, for the same reason.
    pub agree: usize,
    /// Seconds those spent in check there.
    pub agree_secs: u64,
    pub differ: Vec<String>,
    /// Declined there, run by the plan, by the first word of the line
    /// that ended the preamble.
    pub for_nothing: BTreeMap<String, Vec<String>>,
    pub unknown: Vec<String>,
    pub other: Vec<String>,
    /// Ran there, declined by the plan: a test the plan would hide.
    pub hidden: Vec<String>,
}

fn kind(p: &Planned) -> &'static str {
    match p {
        Planned::Run => "run",
        Planned::NotRun(_) => "declined",
        Planned::Unknown(_) => "no answer",
    }
}

/// Set the plan beside verdicts.txt of a sweep.
pub(crate) fn compare(p: &Plan, tests: &[String], verdicts: &str) -> Comparison {
    let mut rec: BTreeMap<&str, (&str, u64, &str)> = BTreeMap::new();
    for l in verdicts.lines() {
        let mut it = l.splitn(4, ' ');
        let (Some(t), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let secs = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let why = it.next().unwrap_or("").trim();
        let why = why.strip_prefix("planned: ").unwrap_or(why);
        rec.entry(t).or_insert((v, secs, why));
    }
    let mut c = Comparison::default();
    for t in tests {
        let Some(planned) = p.tests.get(t) else {
            c.other.push(format!("{t}: not in the plan"));
            continue;
        };
        let Some(&(v, secs, why)) = rec.get(t.as_str()) else {
            c.other.push(format!("{t}: not in the record"));
            continue;
        };
        let ran = v == "PASS" || v == "FAIL";
        match planned {
            Planned::NotRun(r) if ran => {
                c.hidden.push(format!("{t}: {v} there, declined by the plan: {r}"));
            }
            _ if ran => c.kept += 1,
            Planned::NotRun(r) if v == "NOTRUN" && r == why => {
                c.agree += 1;
                c.agree_secs += secs;
            }
            Planned::NotRun(r) if v == "NOTRUN" => {
                c.differ.push(format!("{t}: plan \"{r}\", there \"{why}\""));
            }
            Planned::Run if v == "NOTRUN" => {
                let w = p
                    .cuts
                    .get(t)
                    .and_then(|(_, s)| s.split_whitespace().next())
                    .unwrap_or("?")
                    .to_string();
                c.for_nothing.entry(w).or_default().push(t.clone());
            }
            Planned::Unknown(u) if v == "NOTRUN" => c.unknown.push(format!("{t}: {u}")),
            x => c.other.push(format!("{t}: {v} there, {} in the plan", kind(x))),
        }
    }
    c
}

impl Comparison {
    fn print(&self, record: &Path) {
        note(&format!("  against : {}", record.display()));
        note(&format!("  ran there, run by the plan          : {}", self.kept));
        note(&format!(
            "  declined there and by the plan      : {}, same reason ({} s in check there)",
            self.agree, self.agree_secs
        ));
        note(&format!("  declined there and by the plan, another reason : {}", self.differ.len()));
        for d in &self.differ {
            note(&format!("    {d}"));
        }
        let n: usize = self.for_nothing.values().map(Vec::len).sum();
        note(&format!(
            "  declined there, run by the plan     : {n}, by the line that ended the preamble"
        ));
        let mut v: Vec<(&String, &Vec<String>)> = self.for_nothing.iter().collect();
        v.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        for (w, ts) in v {
            note(&format!("    {:3}  {w}: {}", ts.len(), ts.join(" ")));
        }
        note(&format!("  declined there, no answer in the plan : {}", self.unknown.len()));
        for u in &self.unknown {
            note(&format!("    {u}"));
        }
        note(&format!("  other                               : {}", self.other.len()));
        for o in &self.other {
            note(&format!("    {o}"));
        }
        note(&format!("  RAN THERE, DECLINED BY THE PLAN     : {}", self.hidden.len()));
        for h in &self.hidden {
            note(&format!("    {h}"));
        }
        if self.hidden.is_empty() {
            note("  === no test hidden by the plan ===");
        } else {
            note(&format!("  === THE PLAN HIDES {} TEST(S) ===", self.hidden.len()));
        }
    }
}

/// `plan --against`: the plan beside the verdicts of the sweep recorded
/// in `record`. Ok(true) when it hides no test that ran there.
pub(crate) fn against(p: &Plan, tests: &[String], record: &Path) -> Result<bool, String> {
    let f = record.join("verdicts.txt");
    let v = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
    let cmp = compare(p, tests, &v);
    cmp.print(record);
    Ok(cmp.hidden.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPICAL: &str = "#! /bin/bash\n\
        # SPDX-License-Identifier: GPL-2.0\n\
        #\n\
        . ./common/preamble\n\
        _begin_fstest auto quick\n\
        \n\
        # Import common functions.\n\
        . ./common/filter\n\
        \n\
        _cleanup()\n\
        {\n\
        \tcd /\n\
        \trm -f $tmp.*\n\
        }\n\
        \n\
        _supported_fs generic\n\
        _require_test\n\
        _require_xfs_io_command \"falloc\" \\\n\
        \t\"-k\"\n\
        testfile=$TEST_DIR/$seq.file\n\
        \n\
        echo \"Silence is golden\"\n\
        status=0\n\
        exit\n";

    #[test]
    fn a_preamble_ends_at_the_first_line_of_work() {
        assert_eq!(preamble(TYPICAL), (21, "echo \"Silence is golden\"".to_string()));
    }

    #[test]
    fn guards_one_line_functions_and_exports_are_preamble() {
        let t = "_cleanup() { rm -f $tmp.*; }\n\
            [ \"$FSTYP\" = \"btrfs\" ] && _notrun \"not here\"\n\
            rm -f $seqres.full\n\
            export FOO=1\n\
            _scratch_mkfs > /dev/null 2>&1\n\
            _require_scratch_reflink\n";
        assert_eq!(preamble(t), (4, "_scratch_mkfs > /dev/null 2>&1".to_string()));
    }

    #[test]
    fn a_test_that_is_all_preamble_ends_at_its_end() {
        assert_eq!(preamble("_require_test\n\n"), (2, "(end of file)".to_string()));
    }

    #[test]
    fn a_block_or_a_loop_ends_the_preamble() {
        assert_eq!(
            preamble("if [ \"$FSTYP\" = xfs ]; then\n_require_test\nfi\n"),
            (0, "if [ \"$FSTYP\" = xfs ]; then".to_string())
        );
        assert_eq!(
            preamble("test -x /bin/foo || _notrun \"no foo\"\n_require_test\nfor i in 1 2; do\n"),
            (2, "for i in 1 2; do".to_string())
        );
    }

    #[test]
    fn the_copy_is_not_named_in_a_reason() {
        assert_eq!(
            normalized("/var/volatile/tmp/bx-plan/x/src/dbtest not built"),
            "/usr/xfstests/src/dbtest not built"
        );
        assert_eq!(normalized("'/var/tmp/bx-plan/x/src/a' missing"), "'/usr/xfstests/src/a' missing");
        assert_eq!(
            normalized("Reflink not supported by test filesystem type: beamfs"),
            "Reflink not supported by test filesystem type: beamfs"
        );
    }

    #[test]
    fn a_notrun_file_names_its_test() {
        assert_eq!(test_of_notrun("./generic/033.notrun").as_deref(), Some("generic/033"));
        assert_eq!(test_of_notrun("./sec/generic/033.notrun").as_deref(), Some("generic/033"));
        assert_eq!(test_of_notrun("./033.notrun"), None);
        assert_eq!(test_of_notrun("./generic/033.full"), None);
    }

    #[test]
    fn kernel_lines_are_counted_without_their_time() {
        let d = "[    1.0] a\n[    2.0] a\n[    3.0] run fstests generic/001 at x\n[ 4.0] b\n";
        assert_eq!(counted(d), vec!["2 a".to_string(), "1 b".to_string()]);
    }

    #[test]
    fn the_texts_of_the_tests_are_told_apart() {
        let m = split_texts("\n\u{1e}BX generic/001\nline1\nline2\n\n\u{1e}BX generic/002\n");
        assert_eq!(m.get("generic/001").map(String::as_str), Some("line1\nline2\n"));
        assert_eq!(m.get("generic/002").map(String::as_str), Some(""));
    }

    fn sample() -> Plan {
        let mut p = Plan::empty("k", Path::new("/nonexistent/k.plan"), "a\nb\n");
        p.tests.insert("generic/001".into(), Planned::Run);
        p.tests.insert("generic/002".into(), Planned::NotRun("no reflink".into()));
        p.tests.insert("generic/003".into(), Planned::Run);
        p.tests.insert("generic/004".into(), Planned::NotRun("no O_DIRECT".into()));
        p.tests.insert("generic/005".into(), Planned::Unknown("failed".into()));
        p.cuts.insert("generic/003".into(), (12, "echo hi".into()));
        p.kernel.push("3 x".into());
        p
    }

    #[test]
    fn a_plan_reads_back_as_written() {
        let p = sample();
        let q = parse(&render(&p), &p.path);
        assert_eq!(q.key, "k");
        assert_eq!(q.facts, "a\nb\n");
        assert_eq!(q.tests, p.tests);
        assert_eq!(q.cuts, p.cuts);
        assert_eq!(q.kernel, p.kernel);
    }

    #[test]
    fn a_sweep_runs_what_is_not_declined_and_a_control() {
        let p = sample();
        let tests: Vec<String> = ["generic/001", "generic/002", "generic/004", "generic/005"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let s = split(&tests, Some(&p), 0, 0);
        assert_eq!(s.run, vec!["generic/001".to_string(), "generic/005".to_string()]);
        assert_eq!(s.planned.len(), 2);
        assert!(s.control.is_empty());
        let s = split(&tests, Some(&p), 1, 1);
        assert_eq!(s.control, vec!["generic/004".to_string()]);
        assert_eq!(s.planned, vec![("generic/002".to_string(), "no reflink".to_string())]);
        assert_eq!(s.run.len(), 3);
        let s = split(&tests, None, 10, 0);
        assert_eq!(s.run, tests);
    }

    #[test]
    fn a_test_that_ran_and_is_declined_is_hidden() {
        let p = sample();
        let tests: Vec<String> = ["generic/001", "generic/002", "generic/003", "generic/004"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let v = "generic/001 PASS 12 \n\
                 generic/002 NOTRUN 9 no reflink\n\
                 generic/003 NOTRUN 8 no quota\n\
                 generic/004 PASS 30 \n";
        let c = compare(&p, &tests, v);
        assert_eq!(c.kept, 1);
        assert_eq!((c.agree, c.agree_secs), (1, 9));
        assert_eq!(c.for_nothing.get("echo"), Some(&vec!["generic/003".to_string()]));
        assert_eq!(c.hidden.len(), 1);
        assert!(c.hidden[0].starts_with("generic/004"));
        assert!(c.differ.is_empty() && c.unknown.is_empty() && c.other.is_empty());
    }
}
