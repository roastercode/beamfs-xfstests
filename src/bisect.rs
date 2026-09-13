// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! Where a number changed, and whether it changed at all.
//!
//! A fix for one defect made three others worse on 2026-09-13, and two
//! campaigns went by before anybody noticed: the sweep prints one
//! number per test and the report prints a verdict, and neither says
//! "this was 26 last time".
//!
//! The losses log knows. This reads it.

use std::collections::BTreeMap;

use crate::history;

/// What a test has done lately, one line per run.
///
/// Not a bisection over commits -- rebuilding a kernel per commit is
/// twenty minutes each and nobody bisects that way -- but the thing a
/// bisection is usually for: seeing where a number moved.
pub fn trend(test: &str, n: usize) {
    let v = history::losses_for(test, n);
    if v.is_empty() {
        println!("  nothing recorded for {test} yet");
        return;
    }

    let lo = v.iter().min().copied().unwrap_or(0);
    let hi = v.iter().max().copied().unwrap_or(0);
    let mean = v.iter().sum::<usize>() as f64 / v.len() as f64;

    println!("  {test}, last {} run(s)", v.len());
    println!();

    // A bar per run, scaled to the worst. Reading twelve numbers in a
    // row is what a chart is for, and this is cheaper than a chart.
    for (i, &x) in v.iter().enumerate() {
        let width = x.checked_mul(40).and_then(|v| v.checked_div(hi)).unwrap_or(0);
        println!("    {:2}  {:>5}  {}", i + 1, x, "#".repeat(width));
    }

    println!();
    println!("    between {lo} and {hi}, mean {mean:.1}");

    // The spread is the finding when it is wide: a test losing 3 then
    // 333 is not a test that got worse, it is a race.
    if hi > 0 && lo * 4 < hi {
        println!("    the spread is wider than any single run: this is a race,");
        println!("    and one number from it means nothing on its own");
    } else if v.len() >= 4 {
        // Steady enough that a change in it is a change.
        let half = v.len() / 2;
        let a: usize = v[..half].iter().sum::<usize>() / half.max(1);
        let b: usize = v[half..].iter().sum::<usize>() / (v.len() - half).max(1);
        if b > a + a / 4 {
            println!("    the recent runs lose more than the earlier ones: {a} -> {b}");
        } else if a > b + b / 4 {
            println!("    the recent runs lose less than the earlier ones: {a} -> {b}");
        }
    }
}

/// Every test that has ever lost anything, worst first.
///
/// The report says which tests fail. This says which ones are worth
/// looking at first, and whether each is steady or a race.
pub fn worst(n: usize) {
    let Ok(body) = std::fs::read_to_string(
        history::History::default_root().join("losses.log"))
    else {
        println!("  nothing recorded yet");
        return;
    };

    let mut by_test: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for l in body.lines() {
        let mut f = l.split_whitespace();
        let (_when, name, lost) = (f.next(), f.next(), f.next());
        if let (Some(name), Some(lost)) = (name, lost) {
            if let Ok(x) = lost.parse::<usize>() {
                by_test.entry(name.to_string()).or_default().push(x);
            }
        }
    }

    let mut rows: Vec<(String, usize, usize, usize, usize)> = by_test
        .into_iter()
        .filter(|(_, v)| v.iter().any(|&x| x > 0))
        .map(|(k, v)| {
            let lo = v.iter().min().copied().unwrap_or(0);
            let hi = v.iter().max().copied().unwrap_or(0);
            let last = v.last().copied().unwrap_or(0);
            (k, hi, lo, last, v.len())
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));

    if rows.is_empty() {
        println!("  nothing has lost a block yet");
        return;
    }

    println!("  {:<18} {:>6} {:>6} {:>6} {:>5}  ", "test", "worst", "best", "last", "runs");
    for (name, hi, lo, last, runs) in rows.into_iter().take(n) {
        let shape = if lo * 4 < hi { "a race" } else { "steady" };
        println!("  {name:<18} {hi:>6} {lo:>6} {last:>6} {runs:>5}  {shape}");
    }
}
