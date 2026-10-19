//! `cover`: the Testing paragraph of the series' cover letter, written
//! from whole, clean sweep records in place of `@@XFSTESTS@@`.
//!
//! The letter says what was measured and where the record is; the
//! numbers come from the records, never from a hand. A record is taken
//! only when its sweep finished (`=== end ===`), was clean
//! (`gate=clean`), recovered no node, ran the harness's default and
//! kept a verdict for every test, and every record names the same code
//! gate tree and measured sources. The link from those sources to the
//! series is the crate's own identity check, at the final run.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

const MARK: &str = "@@XFSTESTS@@";
/// kernel.org's limit for the letter, as beamfs-upstream checks it.
const WIDTH: usize = 75;

/// One whole, clean sweep record, as the letter needs it.
#[derive(Debug)]
pub(crate) struct Record {
    pub tag: String,
    pub machine: String,
    /// The running kernel's release, from uname.
    pub kernel: String,
    /// The kernel the image was built from, from the seal.
    pub kernel_release: String,
    pub bx_version: String,
    pub beamfs_version: String,
    pub tree: String,
    pub measured: String,
    pub tests: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_run: usize,
    pub minutes: u64,
    /// Why xfstests declined, by family, counted.
    pub reasons: BTreeMap<&'static str, usize>,
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .map(str::trim)
}

fn json_field(text: &str, key: &str) -> Option<String> {
    let k = format!("\"{key}\": \"");
    let i = text.find(&k)? + k.len();
    let rest = &text[i..];
    Some(rest[..rest.find('"')?].to_string())
}

/// The family of a reason xfstests gave for not running a test: what
/// the letter names instead of 90 distinct sentences.
pub(crate) fn family(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    let has = |s: &str| r.contains(s);
    if has("reflink") || has("dedupe") || has("duperemove") {
        "reflink and dedupe"
    } else if has("o_direct") {
        "O_DIRECT"
    } else if has("falloc") || has("fpunch") || has("fzero") || has("fcollapse") || has("finsert")
        || has("swapext") || has("exchrange")
    {
        "fallocate modes and exchange-range"
    } else if has("dm flakey") || has("dm thin") || has("dm snapshot") || has("logwrites")
        || has("scsi_debug") || has("dax")
    {
        "device-mapper targets, scsi_debug and DAX"
    } else if has("quota") {
        "quotas"
    } else if has("acl") {
        "ACLs"
    } else if has("encryption") || has("fsverity") {
        "encryption and fs-verity"
    } else if has("attr namespace") || has("chattr") || has("lsattr") {
        "xattr namespaces and inode flags"
    } else if has("shutdown") || has("freez") || has("journal") {
        "shutdown, freeze and journal replay"
    } else if has("swapfile") {
        "swap files"
    } else {
        "other features and test prerequisites"
    }
}

/// Read the record under `dir`, or say why it cannot stand in the letter.
pub(crate) fn read(dir: &Path) -> Result<Record, String> {
    let tag = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = std::fs::read_to_string(dir.join("meta.txt"))
        .map_err(|e| format!("{tag}: meta.txt: {e}"))?;
    let (start, end) = meta
        .split_once("=== end ===")
        .ok_or_else(|| format!("{tag}: no end: the sweep did not finish"))?;
    match field(end, "gate") {
        Some("clean") => {}
        Some(g) => return Err(format!("{tag}: not clean: {g}")),
        None => return Err(format!("{tag}: no verdict in meta.txt")),
    }
    let recoveries = field(end, "recoveries")
        .and_then(|v| v.split_whitespace().next())
        .unwrap_or("?");
    if recoveries != "0" {
        return Err(format!("{tag}: the node was recovered {recoveries} time(s)"));
    }
    if !field(start, "selection").is_some_and(|s| s.starts_with("(none: the harness's default")) {
        return Err(format!("{tag}: not the whole auto group"));
    }
    let tests: usize = field(start, "tests")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{tag}: no tests= in meta.txt"))?;
    let at = |s: &str| field(s, "at").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let minutes = at(end).saturating_sub(at(start)) / 60;
    let gate = field(start, "code_gate").unwrap_or("");
    if !gate.starts_with("passed") {
        return Err(format!("{tag}: the code gate did not pass: {gate}"));
    }
    let token = |k: &str| {
        gate.split_whitespace()
            .find_map(|w| w.strip_prefix(k))
            .map(str::to_string)
            .ok_or_else(|| format!("{tag}: code_gate= has no {k}"))
    };
    let tree = token("tree=")?;
    let measured = token("measured=")?;
    let verdicts = std::fs::read_to_string(dir.join("verdicts.txt"))
        .map_err(|e| format!("{tag}: verdicts.txt: {e}"))?;
    let mut rec = Record {
        tag: tag.clone(),
        machine: json_field(start, "machine").unwrap_or_else(|| "?".into()),
        kernel: field(start, "uname")
            .and_then(|u| u.split_whitespace().next())
            .unwrap_or("?")
            .to_string(),
        kernel_release: json_field(start, "kernel_release").unwrap_or_else(|| "?".into()),
        bx_version: field(start, "bx_version").unwrap_or("?").to_string(),
        beamfs_version: field(start, "beamfs_version").unwrap_or("?").to_string(),
        tree,
        measured,
        tests,
        passed: 0,
        failed: 0,
        not_run: 0,
        minutes,
        reasons: BTreeMap::new(),
    };
    let mut seen = 0usize;
    for l in verdicts.lines().filter(|l| !l.trim().is_empty()) {
        let mut it = l.splitn(4, ' ');
        let (Some(_t), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let _secs = it.next();
        let why = it.next().unwrap_or("").trim();
        let why = why
            .strip_prefix("not run by xfstests: ")
            .unwrap_or(why);
        let why = why
            .strip_prefix("planned: ")
            .or_else(|| why.strip_prefix("recorded: "))
            .unwrap_or(why);
        seen += 1;
        match v {
            "PASS" => rec.passed += 1,
            "FAIL" => rec.failed += 1,
            "NOTRUN" => {
                rec.not_run += 1;
                *rec.reasons.entry(family(why)).or_insert(0) += 1;
            }
            _ => {}
        }
    }
    if seen < tests {
        return Err(format!("{tag}: {seen} verdicts of {tests}: not whole"));
    }
    if rec.failed > 0 {
        return Err(format!("{tag}: {} failed", rec.failed));
    }
    Ok(rec)
}

/// `text` as lines of at most WIDTH columns, the first led by `first`,
/// the others by `rest`.
pub(crate) fn wrap(text: &str, first: &str, rest: &str) -> String {
    let mut out = String::new();
    let mut line = String::from(first);
    let mut lead = first.len();
    for w in text.split_whitespace() {
        if line.len() > lead && line.len() + 1 + w.len() > WIDTH {
            out.push_str(line.trim_end());
            out.push('\n');
            line = String::from(rest);
            lead = rest.len();
        }
        if line.len() > lead {
            line.push(' ');
        }
        line.push_str(w);
    }
    if line.len() > lead {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The image kernel's release, said as a tree: Yocto writes the branch
/// and the revision into it (`7.3-rc7+vfs0+9afee012f4`).
pub(crate) fn built_from(release: &str) -> String {
    match release.rsplit_once('+') {
        Some((head, sha)) if head.contains("vfs") => format!("vfs.git's vfs.all at {sha}"),
        Some((_, sha)) => format!("revision {sha}"),
        None => format!("release {release}"),
    }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

/// The paragraph the records make, or why they cannot stand together.
pub(crate) fn paragraph(recs: &[Record]) -> Result<String, String> {
    let Some(first) = recs.first() else {
        return Err("no record".into());
    };
    for r in recs {
        if r.tree != first.tree || r.measured != first.measured {
            return Err(format!(
                "{} and {} did not measure the same code: tree {} / {}, sources {} / {}",
                first.tag,
                r.tag,
                short(&first.tree),
                short(&r.tree),
                short(&first.measured),
                short(&r.measured)
            ));
        }
        if r.tests != first.tests || r.bx_version != first.bx_version
            || r.beamfs_version != first.beamfs_version
            || r.kernel_release != first.kernel_release
        {
            return Err(format!(
                "{} and {} differ in tests, beamfs-xfstests, module or kernel",
                first.tag, r.tag
            ));
        }
    }
    let mut out = wrap(
        &format!(
            "xfstests: the auto group as ./check runs it bare, {} tests, run whole \
             by beamfs-xfstests {} on {} machine{}, each test once. Every test that \
             ran passed, none is listed as a known failure, and no node needed \
             recovering. The code measured passed the code gate as tree {} (sources \
             {}, the ones in this series), kernel {} built from the series' base \
             with {}, beamfs built in, module {}; a test xfstests declines is recorded with the \
             reason it gives, a feature beamfs does not have or a tool the image \
             lacks:",
            first.tests,
            first.bx_version,
            recs.len(),
            if recs.len() == 1 { "" } else { "s" },
            short(&first.tree),
            short(&first.measured),
            first.kernel,
            built_from(&first.kernel_release),
            first.beamfs_version
        ),
        "",
        "",
    );
    for r in recs {
        let mut fam: Vec<(&&str, &usize)> = r.reasons.iter().collect();
        fam.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let chiefly: Vec<String> = fam
            .iter()
            .take(5)
            .map(|(f, n)| format!("{f} ({n})"))
            .collect();
        out.push_str(&wrap(
            &format!(
                "{}: {} ran, {} passed, {} failed; {} not run by xfstests, chiefly {}; \
                 {} min; record {}.",
                r.machine,
                r.passed + r.failed,
                r.passed,
                r.failed,
                r.not_run,
                chiefly.join(", "),
                r.minutes,
                r.tag
            ),
            "- ",
            "  ",
        ));
    }
    Ok(out)
}

/// `cover <sweep>...`: the letter's `@@XFSTESTS@@` replaced by what the
/// records say.
pub(crate) fn fill(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("cover: name the sweep records to write from: cover <sweep-tag>...");
        return ExitCode::FAILURE;
    }
    let sweeps = crate::history::History::default_root().join("sweeps");
    let mut recs = Vec::new();
    for a in args {
        match read(&sweeps.join(a)) {
            Ok(r) => recs.push(r),
            Err(e) => {
                eprintln!("cover: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let text = match paragraph(&recs) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cover: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = crate::clean::repo_dir().join("upstream").join("cover.txt");
    let cover = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cover: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    if !cover.contains(MARK) {
        eprintln!(
            "cover: {} has no {MARK}: filled already; put the marker back to fill again",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let filled = cover.replace(MARK, text.trim_end());
    if let Err(e) = std::fs::write(&path, filled) {
        eprintln!("cover: {}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    println!("  cover   : {} filled from {}", path.display(), args.join(" "));
    for l in text.lines() {
        println!("    {l}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_named_and_lines_stay_within_the_width() {
        assert_eq!(family("Reflink not supported by scratch filesystem type: beamfs"), "reflink and dedupe");
        assert_eq!(family("xfs_io falloc  failed (old kernel/wrong fs?)"), "fallocate modes and exchange-range");
        assert_eq!(family("This test requires dm flakey support"), "device-mapper targets, scsi_debug and DAX");
        assert_eq!(family("something else entirely"), "other features and test prerequisites");
        let w = wrap(&"word ".repeat(60), "- ", "  ");
        assert!(w.lines().all(|l| l.len() <= WIDTH), "{w}");
        assert!(w.starts_with("- word"));
        assert!(w.lines().nth(1).unwrap().starts_with("  word"));
    }

    #[test]
    fn a_whole_clean_record_is_read_and_a_short_one_refused() {
        let root = std::env::temp_dir().join(format!("bx-cover-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = root.join("sweep-1");
        std::fs::create_dir_all(&d).unwrap();
        let meta = "=== start ===\nat=1000\nbx_version=2.9.0\nnode=x86-01 10.0.0.1\n\
                    selection=(none: the harness's default, the auto group)\ntests=3\n\
                    code_gate=passed tree=abcdef0123456789 measured=0123456789abcdef summary=/x\n\
                    uname=7.3.0-rc7 #1 SMP\nbeamfs_version=0.1.26\n--- seal ---\n{\n  \
                    \"machine\": \"qemux86-64\",\n  \"kernel_release\": \"7.3-rc7+vfs0+9afee012f4\"\n}\n\
                    === end ===\nat=7000\nrecoveries=0 \ngate=clean\n";
        std::fs::write(d.join("meta.txt"), meta).unwrap();
        std::fs::write(
            d.join("verdicts.txt"),
            "generic/001 PASS 10 \ngeneric/002 NOTRUN 0 planned: O_DIRECT is not supported\n\
             generic/003 NOTRUN 0 recorded: ACLs not supported by this filesystem type: beamfs\n",
        )
        .unwrap();
        let r = read(&d).unwrap();
        assert_eq!((r.passed, r.failed, r.not_run, r.minutes), (1, 0, 2, 100));
        assert_eq!(r.machine, "qemux86-64");
        assert_eq!(r.reasons.get("O_DIRECT"), Some(&1));
        let p = paragraph(std::slice::from_ref(&r)).unwrap();
        assert!(p.lines().all(|l| l.len() <= WIDTH), "{p}");
        assert!(p.contains("record sweep-1."));
        let flat = p.replace('\n', " ");
        assert!(flat.contains("tree abcdef012345 (sources 0123456789ab"), "{p}");
        assert!(flat.contains("vfs.all at 9afee012f4"), "{p}");
        std::fs::write(d.join("verdicts.txt"), "generic/001 PASS 10 \n").unwrap();
        assert!(read(&d).unwrap_err().contains("not whole"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
