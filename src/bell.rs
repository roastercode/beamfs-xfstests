// SPDX-License-Identifier: GPL-2.0-only
//! Ring when a run finishes.
//!
//! A sweep of the whole suite takes three hours and a single trial of
//! generic/269 takes twenty-two minutes. Nobody watches that; the
//! terminal gets checked every so often and a finished run sits there
//! unnoticed, which on a day with several campaigns is an hour lost to
//! not knowing.
//!
//! The sound is compiled in rather than read from a path. A campaign
//! that cannot find its own asset would either fail or go silent, and
//! neither is worth a config file for 79 KiB: include_bytes! puts the
//! wav in the binary and there is nothing left to install, move or get
//! wrong.
//!
//! Everything about the playback is best-effort. It runs detached, its
//! output goes nowhere, and a missing player or a machine with no sound
//! card changes nothing about the run -- the ring is a convenience and
//! must never be a reason a measurement fails or waits.

use std::io::Write;
use std::process::{Command, Stdio};

/// The sound itself, 79 KiB of 16-bit mono PCM at 44.1 kHz.
static BELL: &[u8] = include_bytes!("../assets/bell.wav");

/// Players, in the order they are tried.
///
/// paplay first because pipewire-pulse answers it and that is what this
/// lab runs. The rest are there so the binary is not tied to one sound
/// stack: a machine with ALSA alone has aplay, and one with sox has
/// play.
const PLAYERS: &[(&str, &[&str])] = &[
    ("paplay", &[]),
    ("pw-play", &[]),
    ("aplay", &["-q"]),
    ("play", &["-q", "-t", "wav", "-"]),
    ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet", "-"]),
];

/// Ring until someone presses Return.
///
/// Once is not enough. A sweep of the whole suite takes three hours and
/// a single generic/269 takes twenty-two minutes; a chime that plays
/// into an empty room while the person is elsewhere has told nobody
/// anything. It repeats every four seconds until acknowledged, and the
/// acknowledgement is Return because that is what a hand reaching for
/// the keyboard presses first.
///
/// Returns when Return is pressed, so the caller should already have
/// printed everything worth reading.
///
/// Silent when stdin is not a terminal -- under a scheduler or a pipe
/// there is nobody to press anything and the loop would never end --
/// and when BEAMFS_NO_BELL is set, for a run over a connection where
/// the sound would land on the wrong machine.
pub fn ring_until_acknowledged() {
    if std::env::var_os("BEAMFS_NO_BELL").is_some() || !stdin_is_a_terminal() {
        return;
    }

    println!();
    println!("  press Return to silence");

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let _ = tx.send(());
    });

    loop {
        ring_once();
        if rx.recv_timeout(std::time::Duration::from_secs(4)).is_ok() {
            return;
        }
    }
}

/// Is stdin a terminal?
///
/// Without libc this is the honest way to ask: a terminal has a size,
/// a pipe does not. Wrong only in the direction of staying quiet.
fn stdin_is_a_terminal() -> bool {
    std::process::Command::new("test")
        .arg("-t")
        .arg("0")
        .stdin(Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One chime, spawned and not waited for.
fn ring_once() {
    for (prog, args) in PLAYERS {
        // Feed the wav on stdin: no temporary file to write, name or
        // clean up, and nothing left behind if the process is killed
        // between spawning and playing.
        let Ok(mut child) = Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;   // not installed; try the next
        };

        if let Some(mut sink) = child.stdin.take() {
            // A player that exits early leaves a closed pipe and the
            // write fails with EPIPE. That is not worth reporting: the
            // sound either happened or it did not.
            let _ = sink.write_all(BELL);
        }

        // Reaped by a thread rather than waited for here, so the caller
        // returns at once and no zombie is left behind either.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return;
    }
}
