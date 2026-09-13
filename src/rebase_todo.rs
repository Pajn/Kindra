//! Rewriting of interactive-rebase todo lists.
//!
//! `kin commit --on <ancestor branch>` moves a freshly created commit down onto
//! an ancestor branch without ever switching branches: it commits at HEAD and
//! then replays the range `<target>..HEAD` with the new commit reordered to the
//! bottom, claiming the target's ref for it. Git generates the todo list (and
//! with it the `update-ref` lines for every other branch in the range); this
//! module only moves one line and adds one, so git's own bookkeeping is left
//! untouched.
//!
//! Kindra hands itself to git as `GIT_SEQUENCE_EDITOR` (see
//! [`sequence_editor_command`]) and the hidden `rebase-todo` subcommand applies
//! [`move_commit_to_front`] to the file git passes in.

use anyhow::{Context, Result, anyhow};
use std::path::Path;

/// Rewrite `todo` so the `pick` of `commit` runs first and `claim_ref` is moved
/// to the commit it produces.
///
/// Only the picked line moves; every other line — including the `update-ref`
/// lines git derived for the other branches in the range, and the trailing
/// comment block — keeps its relative order.
pub fn move_commit_to_front(todo: &str, commit: &str, claim_ref: &str) -> Result<String> {
    let mut picked: Option<&str> = None;
    let mut rest: Vec<&str> = Vec::new();

    for line in todo.lines() {
        if picked.is_none() && line_picks_commit(line, commit) {
            picked = Some(line);
            continue;
        }
        rest.push(line);
    }

    let picked = picked.ok_or_else(|| {
        anyhow!(
            "Commit '{}' is not in the rebase todo list; refusing to rewrite it.",
            commit
        )
    })?;

    // A second pick of the same commit would make "move the new commit" mean two
    // different things, so refuse rather than guess which one to move.
    if rest.iter().any(|line| line_picks_commit(line, commit)) {
        return Err(anyhow!(
            "Commit '{}' appears more than once in the rebase todo list; refusing to rewrite it.",
            commit
        ));
    }

    let mut out = String::with_capacity(todo.len() + claim_ref.len() + 16);
    out.push_str(picked);
    out.push('\n');
    out.push_str("update-ref ");
    out.push_str(claim_ref);
    out.push('\n');
    for line in rest {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Whether `line` is a `pick` of `commit`. Git writes abbreviated object ids in
/// the todo it generates, so the comparison accepts either side as a prefix of
/// the other (with enough characters to be an object id at all) rather than
/// requiring equal lengths.
fn line_picks_commit(line: &str, commit: &str) -> bool {
    let mut fields = line.split_whitespace();
    let Some(command) = fields.next() else {
        return false;
    };
    if command != "pick" && command != "p" {
        return false;
    }
    let Some(id) = fields.next() else {
        return false;
    };
    if id.len() < 4 || commit.len() < 4 {
        return false;
    }
    commit.starts_with(id) || id.starts_with(commit)
}

/// Apply [`move_commit_to_front`] to the todo file at `path`, in place.
pub fn rewrite_todo_file(path: &Path, commit: &str, claim_ref: &str) -> Result<()> {
    let todo = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read rebase todo list '{}'.", path.display()))?;
    let rewritten = move_commit_to_front(&todo, commit, claim_ref)?;
    crate::state_io::write_atomic(path, &rewritten)
        .with_context(|| format!("Failed to rewrite rebase todo list '{}'.", path.display()))
}

/// The `GIT_SEQUENCE_EDITOR` command line that rewrites the todo for moving
/// `commit` onto `claim_ref`.
///
/// Git runs the value through a shell and appends the todo path, so the
/// executable and every argument are single-quoted here.
pub fn sequence_editor_command(commit: &str, claim_ref: &str) -> Result<String> {
    let exe = std::env::current_exe()
        .context("Could not locate the running 'kin' executable to rewrite the rebase todo.")?;
    Ok(format!(
        "{} rebase-todo --commit {} --claim-ref {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(commit),
        shell_quote(claim_ref)
    ))
}

/// Single-quote `value` for a POSIX shell (git uses its own shell on Windows),
/// closing and reopening the quotes around any embedded single quote.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub fn absorb_sequence_editor_command() -> Result<String> {
    let exe =
        std::env::current_exe().context("Could not locate kin for the absorb sequence editor")?;
    Ok(format!(
        "{} absorb-todo",
        shell_quote(&exe.to_string_lossy())
    ))
}

/// Capture each fork point after its complete autosquash group. Git persists
/// update-ref instructions across conflicts and applies them when the fold ends.
/// No temporary branch is exposed to stack discovery or other worktrees.
pub fn rewrite_absorb_todo_file(todo: impl AsRef<Path>) -> Result<()> {
    let repo = crate::open_repo()?;
    let state = crate::rebase_utils::load_state(&repo)?;
    let prefix = crate::rebase_utils::ABSORB_FORK_REF_PREFIX;
    let mut anchors = std::collections::BTreeMap::new();
    for reference in state.new_base_map.values() {
        if let Some(suffix) = reference.strip_prefix(prefix) {
            let oid = suffix
                .rsplit('/')
                .next()
                .ok_or_else(|| anyhow!("Invalid absorb anchor"))?;
            anchors.insert(git2::Oid::from_str(oid)?, reference.clone());
        }
    }
    let input = std::fs::read_to_string(todo.as_ref())?;
    let mut output = String::new();
    let mut pending = Vec::new();
    for line in input.lines() {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        let is_fold = matches!(command, "fixup" | "f" | "squash" | "s");
        let is_commit = is_fold || matches!(command, "pick" | "p" | "reword" | "r" | "edit" | "e");
        if is_commit && !is_fold {
            for reference in pending.drain(..) {
                output.push_str(&format!("update-ref {reference}\n"));
            }
        }
        output.push_str(line);
        output.push('\n');
        if is_commit {
            let oid = words
                .find(|word| !word.starts_with('-'))
                .ok_or_else(|| anyhow!("Missing commit in rebase todo"))?;
            let oid = repo.revparse_single(oid)?.peel_to_commit()?.id();
            if let Some(reference) = anchors.remove(&oid) {
                pending.push(reference);
            }
        }
    }
    for reference in pending {
        output.push_str(&format!("update-ref {reference}\n"));
    }
    if !anchors.is_empty() {
        return Err(anyhow!(
            "The absorb rebase todo omitted a required fork point"
        ));
    }
    std::fs::write(todo, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODO: &str = "\
pick bf260cd # mid
update-ref refs/heads/mid

pick c0e59bd # upper
pick 5d53470 # wip commit

# Rebase fde4403..5d53470 onto fde4403 (4 commands)
#
# Commands:
# p, pick <commit> = use commit
";

    #[test]
    fn moves_pick_to_front_and_claims_ref() {
        let rewritten = move_commit_to_front(
            TODO,
            "5d534705d534705d534705d534705d534705d534",
            "refs/heads/lower",
        )
        .unwrap();
        let lines: Vec<&str> = rewritten.lines().collect();
        assert_eq!(lines[0], "pick 5d53470 # wip commit");
        assert_eq!(lines[1], "update-ref refs/heads/lower");
        assert_eq!(lines[2], "pick bf260cd # mid");
        assert_eq!(lines[3], "update-ref refs/heads/mid");
        // Everything else keeps its order, comment block included.
        assert!(rewritten.contains("pick c0e59bd # upper"));
        assert!(rewritten.ends_with("# p, pick <commit> = use commit\n"));
        assert_eq!(
            rewritten.matches("update-ref").count(),
            2,
            "the rewrite must not duplicate git's own update-ref lines"
        );
    }

    #[test]
    fn keeps_a_single_pick_todo_intact() {
        let rewritten = move_commit_to_front(
            "pick abc1234 # only\n",
            "abc1234abc1234abc1234abc1234abc1234abc12",
            "refs/heads/lower",
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "pick abc1234 # only\nupdate-ref refs/heads/lower\n"
        );
    }

    #[test]
    fn rejects_a_todo_without_the_commit() {
        let err = move_commit_to_front(
            "pick bf260cd # mid\n",
            "5d534705d534705d534705d534705d534705d534",
            "refs/heads/lower",
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not in the rebase todo list"));
    }

    #[test]
    fn rejects_a_todo_listing_the_commit_twice() {
        let err = move_commit_to_front(
            "pick 5d53470 # wip\npick 5d53470 # wip again\n",
            "5d534705d534705d534705d534705d534705d534",
            "refs/heads/lower",
        )
        .unwrap_err();
        assert!(err.to_string().contains("more than once"));
    }

    #[test]
    fn ignores_commands_that_are_not_picks() {
        let err = move_commit_to_front(
            "drop 5d53470 # wip\n",
            "5d534705d534705d534705d534705d534705d534",
            "refs/heads/lower",
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not in the rebase todo list"));
    }

    #[test]
    fn quotes_values_for_the_shell() {
        assert_eq!(shell_quote("/tmp/kin dir/kin"), "'/tmp/kin dir/kin'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
