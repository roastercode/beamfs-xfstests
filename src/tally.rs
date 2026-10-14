// SPDX-License-Identifier: GPL-2.0-only
//! Lines said more than once, said once with their count.
//!
//! A reason that kept the first three messages of the kernel made a list
//! of expected messages that missed the rest; a reason that writes the
//! same line forty times is not read either. This keeps every line, in
//! the order it first came, and counts a repeat instead of writing it
//! again.

/// Every line, in the order it first came, a repeat counted, joined by
/// " | ".
pub(crate) fn tally<S: AsRef<str>>(lines: &[S]) -> String {
    let mut seen: Vec<(&str, usize)> = Vec::new();
    for l in lines {
        let l = l.as_ref();
        if let Some(e) = seen.iter_mut().find(|e| e.0 == l) {
            e.1 += 1;
        } else {
            seen.push((l, 1));
        }
    }
    seen.iter()
        .map(|&(l, n)| if n > 1 { format!("{l} (x{n})") } else { l.to_string() })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::tally;

    #[test]
    fn every_line_is_kept_and_a_repeat_is_counted() {
        assert_eq!(tally(&["a", "b", "a", "c", "a"]), "a (x3) | b | c");
        let none: [&str; 0] = [];
        assert_eq!(tally(&none), "");
        let many: Vec<String> = (0..10).map(|i| format!("m{i}")).collect();
        assert_eq!(tally(&many).split(" | ").count(), 10);
    }
}
