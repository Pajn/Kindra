//! Repository hooks: shell commands from the `[hooks]` section of repository
//! config that Kindra runs at fixed points of a command.
//!
//! ```toml
//! [hooks]
//! after_pr = ["some-command --flag"]
//! ```
//!
//! Each hook command runs through the platform shell (`sh -c` on Unix) from
//! the worktree root, with the event's JSON payload on stdin and
//! `KINDRA_HOOK_EVENT` naming the event. Output is inherited so the user sees
//! it. Commands run in order and the first failure stops the list.

use anyhow::{Context, Result, anyhow};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// The `[hooks]` section of repository config. Strict, so a misspelt event
/// name is an error rather than a hook that silently never runs.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksSection {
    #[serde(default)]
    after_pr: Vec<String>,
}

fn section(repo: &Repository) -> Result<HooksSection> {
    Ok(crate::config::repo_config(repo)?
        .section::<HooksSection>("hooks")?
        .unwrap_or_default())
}

/// The configured `after_pr` commands, in order. Empty when none are set.
pub fn after_pr_commands(repo: &Repository) -> Result<Vec<String>> {
    Ok(section(repo)?.after_pr)
}

/// The JSON payload `after_pr` hooks receive on stdin. Field names and
/// meanings are documented in `docs/cli_reference.md`; keep them stable.
#[derive(Debug, Serialize)]
pub struct AfterPrPayload {
    pub event: &'static str,
    pub trunk: String,
    pub remote: Option<String>,
    pub remote_url: Option<String>,
    pub branches: Vec<AfterPrBranch>,
}

#[derive(Debug, Serialize)]
pub struct AfterPrBranch {
    pub name: String,
    pub parent: String,
    pub head_sha: String,
    pub fork_point: String,
    pub pr: Option<AfterPrPullRequest>,
}

#[derive(Debug, Serialize)]
pub struct AfterPrPullRequest {
    pub number: u64,
    pub url: String,
    pub draft: bool,
}

pub const AFTER_PR_EVENT: &str = "after_pr";

/// Run `commands` as `after_pr` hooks from `worktree_root`, feeding each the
/// payload. The pull requests are already published when this runs, so a
/// failure says so and leaves them in place.
pub fn run_after_pr(
    commands: &[String],
    worktree_root: &Path,
    payload: &AfterPrPayload,
) -> Result<()> {
    let json = serde_json::to_vec(payload).context("Failed to serialize after_pr payload")?;
    for command in commands {
        eprintln!("Running {AFTER_PR_EVENT} hook: {command}");
        run_one(command, worktree_root, AFTER_PR_EVENT, &json).map_err(|reason| {
            anyhow!(
                "Pull requests were published, but the {AFTER_PR_EVENT} hook `{command}` {reason}."
            )
        })?;
    }
    Ok(())
}

/// Run one hook command. The error is the reason it failed, phrased to follow
/// the command in a sentence.
fn run_one(command: &str, cwd: &Path, event: &str, stdin: &[u8]) -> Result<(), String> {
    let mut child = shell_command(command)
        .current_dir(cwd)
        .env("KINDRA_HOOK_EVENT", event)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("could not be started ({err})"))?;

    if let Some(mut pipe) = child.stdin.take() {
        // A hook may exit without reading its input; that is not a failure.
        if let Err(err) = pipe.write_all(stdin)
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            let _ = child.wait();
            return Err(format!("could not be given its input ({err})"));
        }
    }

    let status = child
        .wait()
        .map_err(|err| format!("could not be waited on ({err})"))?;
    if status.success() {
        return Ok(());
    }
    Err(match status.code() {
        Some(code) => format!("failed (exit status {code})"),
        None => "was terminated by a signal".to_string(),
    })
}

/// A command running `script` through the platform shell: `sh -c` on Unix,
/// `cmd /C` on Windows.
pub(crate) fn shell_command(script: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", script]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
}
