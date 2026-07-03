//! Non-interactive / interactive mode resolution.
//!
//! Interactivity is resolved **once** at startup into a single [`Interaction`]
//! value (see [`resolve`] / [`init`]) and stored in a process-global
//! [`OnceLock`]. Everything downstream reads [`current`] instead of probing
//! `stdin().is_terminal()` at each prompt, so the whole binary agrees on whether
//! it may prompt, must fail loudly, or should auto-accept.
//!
//! The test-only scripted seam (the `KIN_TEST_*` env vars used by the
//! integration suite to drive prompts headlessly) lives entirely inside
//! [`ScriptedAnswers`]. The real non-interactive path
//! ([`Interaction::NonInteractive`]) contains no test branches.

use std::io::IsTerminal;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How the process should treat prompts, resolved once at startup.
#[derive(Debug)]
pub enum Interaction {
    /// A terminal is attached; prompts are shown and answered by a human.
    Interactive,
    /// No prompts. A prompt with a safe default uses it; a prompt that needs a
    /// real answer is a hard error ([`InputRequired`]). `assume_yes` (from
    /// `--yes`) flips confirmations to "accept the action" rather than erroring
    /// or denying.
    NonInteractive { assume_yes: bool },
    /// Deterministic scripted answers for headless integration tests. Emulates
    /// the keystrokes a human would make so the editor/draft/menu machinery can
    /// be exercised without a TTY.
    Scripted(ScriptedAnswers),
}

impl Interaction {
    /// True only when a human can be prompted.
    pub fn is_interactive(&self) -> bool {
        matches!(self, Interaction::Interactive)
    }

    /// True when `--yes` (or its env equivalent) asked us to accept prompted
    /// actions unattended. Honored in scripted runs too, so tests can exercise
    /// the `--yes` confirmation path.
    pub fn assume_yes(&self) -> bool {
        match self {
            Interaction::NonInteractive { assume_yes } => *assume_yes,
            Interaction::Scripted(answers) => answers.assume_yes,
            Interaction::Interactive => false,
        }
    }

    /// The scripted answers, if this is a test-driven run.
    pub fn scripted(&self) -> Option<&ScriptedAnswers> {
        match self {
            Interaction::Scripted(answers) => Some(answers),
            _ => None,
        }
    }
}

/// Error marker for "a prompt needed a real answer but none was available in
/// non-interactive mode". [`main`](crate) maps this to a distinct exit code so
/// scripts can tell missing-input apart from a genuine failure.
#[derive(Debug)]
pub struct InputRequired(pub String);

impl std::fmt::Display for InputRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InputRequired {}

/// Build an [`InputRequired`] error wrapped for `anyhow`.
pub fn input_required(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(InputRequired(message.into()))
}

static CURRENT: OnceLock<Interaction> = OnceLock::new();

/// Resolve the interaction mode from CLI flags, env, and TTY detection.
///
/// Precedence (highest first):
/// 1. `KIN_TEST_*` scripted seam (integration tests only)
/// 2. `--yes` / `--no-interactive`
/// 3. `KIN_INTERACTIVE=0|1` (the escape hatch for forcing interactive without a
///    TTY; there is no `--interactive` flag because `kin commit --interactive`
///    already owns that name)
/// 4. auto-detect: both stdin and stdout are TTYs → interactive, else not.
pub fn resolve(no_interactive: bool, yes: bool) -> Interaction {
    if let Some(mut scripted) = ScriptedAnswers::from_env() {
        // Carry `--yes` into scripted mode so `assume_yes()` (and thus
        // `prompt_confirm`) behaves the same as it does non-interactively.
        scripted.assume_yes = yes;
        return Interaction::Scripted(scripted);
    }

    if yes || no_interactive {
        return Interaction::NonInteractive { assume_yes: yes };
    }

    match std::env::var("KIN_INTERACTIVE").ok().as_deref() {
        Some("1") | Some("true") => return Interaction::Interactive,
        Some("0") | Some("false") => return Interaction::NonInteractive { assume_yes: false },
        _ => {}
    }

    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Interaction::Interactive
    } else {
        Interaction::NonInteractive { assume_yes: false }
    }
}

/// Store the resolved mode. Called once from `main` after CLI parsing. A second
/// call would be silently ignored (the first value wins), which points to a
/// double-initialization bug, so surface it in debug builds.
pub fn init(mode: Interaction) {
    if CURRENT.set(mode).is_err() {
        debug_assert!(false, "interaction::init called more than once");
    }
}

/// The resolved interaction mode. Falls back to a fresh TTY-based resolution if
/// [`init`] was never called (e.g. unit tests that call a prompt helper
/// directly).
pub fn current() -> &'static Interaction {
    CURRENT.get_or_init(|| resolve(false, false))
}

/// Test-only scripted answers, parsed from `KIN_TEST_*` env vars.
///
/// This is the single home for the integration-test prompt seams. It is
/// consulted only from the [`Interaction::Scripted`] match arms in the prompt
/// helpers, keeping the production non-interactive path free of test logic.
#[derive(Debug, Default)]
pub struct ScriptedAnswers {
    /// Indices consumed in order by successive `prompt_select` calls
    /// (`KIN_TEST_SELECTIONS`).
    selections: Vec<usize>,
    selection_cursor: AtomicUsize,
    /// A single index for the amend-commit picker (`KIN_TEST_SELECTION`).
    single_selection: Option<usize>,
    /// Indices for `prompt_multi_select` (`KIN_TEST_MULTI_SELECTIONS`).
    multi_selections: Vec<usize>,
    /// Emulated menu choice for the PR body prompt (`KIN_TEST_PR_BODY_ACTION`).
    pr_body_action: Option<String>,
    /// Title override for the PR edit flow (`KIN_TEST_PR_EDIT_TITLE`).
    pr_edit_title: Option<String>,
    /// Whether `--yes` was passed; set by [`resolve`], not from the environment.
    assume_yes: bool,
}

impl ScriptedAnswers {
    /// Parse the scripted seam from the environment. Returns `None` when no
    /// `KIN_TEST_*` prompt var is set, so normal runs never enter scripted mode.
    fn from_env() -> Option<Self> {
        let selections = parse_index_list("KIN_TEST_SELECTIONS");
        let single_selection = std::env::var("KIN_TEST_SELECTION")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok());
        let multi_selections = parse_index_list("KIN_TEST_MULTI_SELECTIONS");
        let pr_body_action = std::env::var("KIN_TEST_PR_BODY_ACTION").ok();
        let pr_edit_title = std::env::var("KIN_TEST_PR_EDIT_TITLE").ok();

        let any = !selections.is_empty()
            || single_selection.is_some()
            || !multi_selections.is_empty()
            || pr_body_action.is_some()
            || pr_edit_title.is_some();

        any.then(|| ScriptedAnswers {
            selections,
            selection_cursor: AtomicUsize::new(0),
            single_selection,
            multi_selections,
            pr_body_action,
            pr_edit_title,
            assume_yes: false,
        })
    }

    /// The next `prompt_select` index in sequence, if the script provides one.
    pub fn next_selection(&self) -> Option<usize> {
        let call_index = self.selection_cursor.fetch_add(1, Ordering::Relaxed);
        self.selections.get(call_index).copied()
    }

    /// The scripted index for the single-commit amend picker.
    pub fn single_selection(&self) -> Option<usize> {
        self.single_selection
    }

    /// The scripted indices for a multi-select prompt.
    pub fn multi_selections(&self) -> &[usize] {
        &self.multi_selections
    }

    /// The scripted PR-body menu action, if any.
    pub fn pr_body_action(&self) -> Option<&str> {
        self.pr_body_action.as_deref()
    }

    /// The scripted PR-edit title override, if any.
    pub fn pr_edit_title(&self) -> Option<&str> {
        self.pr_edit_title.as_deref()
    }
}

fn parse_index_list(var: &str) -> Vec<usize> {
    std::env::var(var)
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_mode_honors_assume_yes() {
        // `--yes` must survive into scripted runs so `prompt_confirm` sees it.
        let with_yes = Interaction::Scripted(ScriptedAnswers {
            assume_yes: true,
            ..ScriptedAnswers::default()
        });
        assert!(with_yes.assume_yes());

        let without_yes = Interaction::Scripted(ScriptedAnswers::default());
        assert!(!without_yes.assume_yes());
    }
}
