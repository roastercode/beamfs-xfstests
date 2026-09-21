// SPDX-License-Identifier: GPL-2.0-only
//! The one thing allowed to write to the screen.
//!
//! There were two writers. The indicator thread drew a spinner with a
//! bare carriage return every 250 ms, and the main thread printed its
//! own lines whenever it had something to report. Neither erased what
//! the other had left, so a run reported
//!
//!   trial 1 volume kept: 6 MiB compressed82s  check
//!
//! where "82s  check" is the tail of a status line the message landed
//! on top of. Two writers on one screen is the defect; a mutex around
//! one block is the fix.
//!
//! [`draw`] is for what is true right now and will be replaced. [`note`]
//! is for what has become permanent. The distinction is the whole of
//! the display model: see the `liveblock` crate.

use liveblock::Block;
use std::io::Stdout;
use std::sync::{Mutex, OnceLock};

/// The screen.
fn screen() -> &'static Mutex<Block<Stdout>> {
    static UI: OnceLock<Mutex<Block<Stdout>>> = OnceLock::new();
    UI.get_or_init(|| Mutex::new(Block::stdout()))
}

/// Replace the live block with `body`, which may be several lines.
///
/// A poisoned lock is not worth failing a campaign over: the display
/// goes quiet and the run carries on.
pub fn draw(body: &str) {
    if let Ok(mut b) = screen().lock() {
        b.draw(body);
    }
}

/// Print a line that stays, above the live block.
pub fn note(line: &str) {
    if let Ok(mut b) = screen().lock() {
        b.note(line);
    }
}

/// Erase the live block.
pub fn clear_line() {
    if let Ok(mut b) = screen().lock() {
        b.clear();
    }
}

/// println!, but through the one writer rather than around it.
#[macro_export]
macro_rules! say {
    ($($a:tt)*) => {{ $crate::say::note(&format!($($a)*)); }};
}
