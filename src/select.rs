// SPDX-License-Identifier: GPL-2.0-only
//! Naming a set of tests without writing a loop.
//!
//! A selection used to be built by the shell around the tool:
//!
//!     SEL=""; for i in $(seq -w 1 14); do SEL="$SEL generic/0$i"; done
//!     for t in 074 075 083; do SEL="$SEL generic/$t"; done
//!     beamfs-xfstests sweep "$SEL"
//!
//! which cannot be tested, cannot be documented, and has to be got
//! right again every time. Worse, the sets that matter -- the tests
//! that have ever leaked, the ones that leaked last time -- are known
//! to this tool and to nobody else, so the shell could only ever
//! restate them by hand and grow stale.
//!
//! The forms:
//!
//!     generic/013            one test, as before
//!     generic/001-014        a range, the width of the digits kept
//!     generic/074,075,083    a list within one family
//!     @leaks                 every test that has ever lost a block
//!     @leaks:20              ... within the last 20 records of each
//!     @seen                  every test the history has ever recorded
//!
//! Several forms combine, space separated, and the result is sorted
//! and deduplicated: asking for @leaks and generic/013 when 013 leaks
//! runs it once.

use std::collections::BTreeSet;

/// Where the loss history lives.
fn losses_path() -> std::path::PathBuf {
    crate::history::History::default_root().join("losses.log")
}

/// One line of the loss history: the test, and what it lost.
fn losses() -> Vec<(String, usize)> {
    let Ok(body) = std::fs::read_to_string(losses_path()) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let _when = f.next()?;
            let test = f.next()?.to_string();
            let lost: usize = f.next()?.parse().ok()?;
            Some((test, lost))
        })
        .collect()
}

/// Expand one `family/NNN-MMM` range, keeping the width of the digits.
fn range(spec: &str) -> Option<Vec<String>> {
    let (family, rest) = spec.split_once('/')?;
    let (from, to) = rest.split_once('-')?;
    if !from.chars().all(|c| c.is_ascii_digit()) || !to.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let width = from.len();
    let a: u32 = from.parse().ok()?;
    let b: u32 = to.parse().ok()?;
    if b < a {
        return None;
    }
    Some((a..=b).map(|n| format!("{family}/{n:0width$}")).collect())
}

/// Expand one `family/NNN,MMM,...` list.
fn list(spec: &str) -> Option<Vec<String>> {
    let (family, rest) = spec.split_once('/')?;
    if !rest.contains(',') {
        return None;
    }
    Some(
        rest.split(',')
            .filter(|x| !x.is_empty())
            .map(|x| format!("{family}/{x}"))
            .collect(),
    )
}

/// Expand one `@name` set, from what the history recorded.
fn named(spec: &str, rows: &[(String, usize)]) -> Result<Vec<String>, String> {
    let (name, arg) = match spec.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (spec, None),
    };
    match name {
        "@seen" => {
            let mut s: BTreeSet<String> = BTreeSet::new();
            for (t, _) in rows {
                s.insert(t.clone());
            }
            Ok(s.into_iter().collect())
        }
        "@leaks" => {
            let n: usize = match arg {
                Some(a) => a.parse().map_err(|_| format!("{spec}: not a number"))?,
                None => usize::MAX,
            };
            // The last n records of each test, so "@leaks:5" asks what
            // has leaked lately rather than what leaked in April.
            let mut per: std::collections::HashMap<&str, Vec<usize>> =
                std::collections::HashMap::new();
            for (t, l) in rows {
                per.entry(t.as_str()).or_default().push(*l);
            }
            let mut s: BTreeSet<String> = BTreeSet::new();
            for (t, v) in per {
                let tail = if v.len() > n { &v[v.len() - n..] } else { &v[..] };
                if tail.iter().any(|x| *x > 0) {
                    s.insert(t.to_string());
                }
            }
            Ok(s.into_iter().collect())
        }
        _ => Err(format!("{spec}: no such set (try @leaks, @leaks:N, @seen)")),
    }
}

/// Expand a whole selection.
///
/// An unknown word is returned as it stands: the harness resolves test
/// names, group names and its own flags, and second-guessing it here
/// would break `-g auto` for no gain.
pub fn expand(specs: &[String]) -> Result<Vec<String>, String> {
    let rows = losses();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for word in specs.iter().flat_map(|s| s.split_whitespace()) {
        if word.starts_with('@') {
            for t in named(word, &rows)? {
                out.insert(t);
            }
        } else if let Some(v) = range(word) {
            out.extend(v);
        } else if let Some(v) = list(word) {
            out.extend(v);
        } else {
            out.insert(word.to_string());
        }
    }
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_range_keeps_the_width_of_its_digits() {
        assert_eq!(
            range("generic/001-004").unwrap(),
            vec!["generic/001", "generic/002", "generic/003", "generic/004"]
        );
        assert_eq!(range("generic/008-010").unwrap().last().unwrap(), "generic/010");
    }

    #[test]
    fn a_backwards_range_is_not_a_range() {
        assert!(range("generic/014-001").is_none());
        assert!(range("generic/abc-def").is_none());
        assert!(range("generic/013").is_none());
    }

    #[test]
    fn a_list_belongs_to_one_family() {
        assert_eq!(
            list("generic/074,075,083").unwrap(),
            vec!["generic/074", "generic/075", "generic/083"]
        );
        assert!(list("generic/013").is_none());
    }

    #[test]
    fn what_is_not_a_form_is_passed_through() {
        let v = expand(&s(&["-g", "auto"])).unwrap();
        assert!(v.contains(&"-g".to_string()));
        assert!(v.contains(&"auto".to_string()));
    }

    #[test]
    fn a_selection_runs_each_test_once() {
        let v = expand(&s(&["generic/001-003", "generic/002"])).unwrap();
        assert_eq!(v, vec!["generic/001", "generic/002", "generic/003"]);
    }

    #[test]
    fn leaks_are_the_tests_that_lost_something() {
        // 013 leaked and was then clean; 083 leaked most recently.
        let rows = vec![
            ("generic/013".to_string(), 266),
            ("generic/013".to_string(), 0),
            ("generic/001".to_string(), 0),
            ("generic/083".to_string(), 33),
        ];
        let mut v = named("@leaks", &rows).unwrap();
        v.sort();
        assert_eq!(v, vec!["generic/013", "generic/083"],
                   "ever leaked means ever");
        // The last record of each, which is what "lately" has to mean
        // for the answer to change after a fix.
        assert_eq!(named("@leaks:1", &rows).unwrap(), vec!["generic/083"],
                   "013 was clean the last time it ran");
    }

    #[test]
    fn seen_is_every_test_the_history_knows() {
        let rows = vec![
            ("generic/013".to_string(), 0),
            ("generic/013".to_string(), 1),
            ("generic/001".to_string(), 0),
        ];
        assert_eq!(named("@seen", &rows).unwrap(), vec!["generic/001", "generic/013"]);
    }

    #[test]
    fn an_unknown_set_says_which_ones_exist() {
        let e = named("@nonsense", &[]).unwrap_err();
        assert!(e.contains("@leaks"), "{e}");
    }
}
