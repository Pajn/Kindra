//! Opt-in phase timings, for seeing where a command spends its time.
//!
//! With `KIN_TIMINGS` set to anything other than empty or `0`, every
//! [`phase`] prints how long it took to stderr when its guard drops, including
//! when the phase ends in an error. Phases opened inside another phase are
//! indented under it; since a phase prints when it ends, a nested phase
//! prints before the phase that contains it. Without the variable a phase
//! costs one cached check and prints nothing.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(
        || matches!(std::env::var("KIN_TIMINGS"), Ok(v) if !matches!(v.trim(), "" | "0")),
    )
}

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// A running phase; reports its duration when dropped.
#[must_use = "dropping the guard immediately ends the phase"]
pub struct Phase {
    label: &'static str,
    started: Option<(Instant, usize)>,
}

/// Start timing `label` until the returned guard drops.
pub fn phase(label: &'static str) -> Phase {
    let started = enabled().then(|| {
        let depth = DEPTH.with(|d| d.replace(d.get() + 1));
        (Instant::now(), depth)
    });
    Phase { label, started }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let Some((start, depth)) = self.started else {
            return;
        };
        DEPTH.with(|d| d.set(depth));
        eprintln!(
            "kin timing: {:indent$}{} {:.1?}",
            "",
            self.label,
            start.elapsed(),
            indent = depth * 2
        );
    }
}
