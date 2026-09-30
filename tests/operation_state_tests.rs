//! Characterisation of the journal each operation persists when it pauses on
//! a conflict, and of how paused operations recorded by older Kindra versions
//! are reported, aborted and continued.
//!
//! These tests pin today's behaviour so that reworking the journal cannot
//! change it silently. They describe what Kindra does, not what it should do:
//! where the behaviour is questionable, a comment says so.
//!
//! # Golden journals
//!
//! Each golden case drives a real conflict through the CLI and compares the
//! saved state with `tests/fixtures/operation_state/golden/<case>.json`.
//! Values that change from run to run are replaced by symbolic names first:
//!
//! - `<branch@before>`: the tip of `branch` before the operation started;
//! - `<branch@paused>`: the tip of `branch` when the operation paused, if it
//!   moved;
//! - `<commit: subject>`: any other commit, by its subject (`#2`, `#3`, ...
//!   tell apart distinct commits with the same subject);
//! - `<kin-... set-aside>`: a set-aside stash entry, whose name carries a
//!   process id and a timestamp, and `<kin-... set-aside stash>` its stash
//!   commit;
//! - `<absorb-namespace>`: the per-operation part of absorb's anchor refs.
//!
//! JSON objects are compared with their keys sorted, so the order in which a
//! `HashMap` happened to serialize does not matter; every field, its type and
//! its value do.
//!
//! After an intended change to what an operation saves, regenerate the golden
//! files and review the diff:
//!
//! ```text
//! KIN_UPDATE_GOLDEN=1 cargo test --test operation_state_tests
//! ```
//!
//! # Legacy journals
//!
//! `tests/fixtures/operation_state/legacy/<case>@<version>.json` are journals
//! in the shape a released Kindra wrote, using the same symbolic names. They
//! are frozen: `KIN_UPDATE_GOLDEN` never touches them. A legacy test pauses
//! the same scenario with today's Kindra, replaces the saved journal with the
//! legacy one, and checks `kin status`, `kin abort` and, for operations built
//! only from branch replays, `kin continue`.
//!
//! The field sets by release (each release also reads every earlier shape):
//!
//! - 0.1.0: no `owned_tip_map`, `stash_apply_index`, `carry_stash_ref`,
//!   `preserve_content_on_abort`, `suppress_editor` or `abort_only`;
//! - 0.2.0 and 0.3.0: adds `owned_tip_map`;
//! - 0.2.1 and 0.2.2: adds `stash_apply_index`, `preserve_content_on_abort` and
//!   `suppress_editor` (absorb);
//! - 1.0.0 and 1.1.0: adds `carry_stash_ref`; tree sync first ships;
//! - after 1.1.0: adds `abort_only` and `replay`; restack and absorb save
//!   their own `operation` labels instead of `Move` and `Commit`; records
//!   `stash_ref`, `stash_apply_index` and `carry_stash_ref` as `set_asides`;
//!   and saves the journal inside a `{"version": 1, "journal": {...}}`
//!   envelope. A flat journal (no `version`) is converted when it is loaded.
//!   Version 2 lets an unstaged-only set-aside record `UnstagedDelta`. Later
//!   version 2 journals record every set-aside up front and no longer save
//!   `autostash` (absent reads as `false`), except while an older journal
//!   still asks the rebase loop to set the tree aside. Version 3 drops
//!   `replay`: `rebase_options` says how each rebase runs, a tree sync
//!   records its planned parents in `new_base_map`, and sync finishes in the
//!   shared rebase loop. Version 4 drops `remaining_branches` and
//!   `in_progress_branch`: the journal records its plan as `steps`, among
//!   them the fold and move rebases of commit and absorb, and its progress
//!   as a `cursor`. Version 5 adds `created_branch`, the branch `kin commit
//!   -b --insert` created, which `kin abort` deletes after returning to the
//!   branch it was created from. The `@journal-v2`, `@journal-v3` and
//!   `@journal-v4` fixtures are version 2, 3 and 4 journals, saved by builds
//!   after 1.1.0 that no release shipped.
//!
//! Checkout hydration keeps its own journal, `kindra_checkout_state.json`,
//! with its own version (the `hydration` tests below):
//!
//! - 1.1.0 (the first release with hydration): a flat `branch`, `repository` and `steps`, each step
//!   with a `completed` flag, including branches that already existed;
//! - after 1.1.0: version 1, inside the same envelope, records `CreateBranch`
//!   steps for the missing branches, a `Checkout` step and a `cursor`.

mod common;

use common::{StateFile, git_command, kin_cmd, rebase_state_file, repo_init, run_ok};
use regex::{Captures, Regex};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::LazyLock;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test repositories
// ---------------------------------------------------------------------------

/// A scratch repository whose `main` holds `shared.txt` = `base`.
struct Repo {
    dir: TempDir,
    /// The bare remote, for scenarios that sync with one.
    _remote: Option<TempDir>,
}

impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        repo_init(dir.path());
        let repo = Self { dir, _remote: None };
        repo.commit("shared.txt", "base\n", "base");
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) {
        run_ok("git", args, self.path());
    }

    fn git_stdout(&self, args: &[&str]) -> String {
        let output = git_command(self.path()).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn write(&self, file: &str, content: &str) {
        fs::write(self.path().join(file), content).unwrap();
    }

    fn stage(&self, file: &str, content: &str) {
        self.write(file, content);
        self.git(&["add", file]);
    }

    fn commit(&self, file: &str, content: &str, message: &str) {
        self.stage(file, content);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn branch(&self, name: &str) {
        self.git(&["switch", "-q", "-c", name]);
    }

    fn switch(&self, name: &str) {
        self.git(&["switch", "-q", name]);
    }

    fn rev(&self, rev: &str) -> String {
        self.git_stdout(&["rev-parse", rev]).trim().to_string()
    }

    /// Every local branch and its tip.
    fn tips(&self) -> BTreeMap<String, String> {
        self.git_stdout(&[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ])
        .lines()
        .map(|line| {
            let (name, oid) = line.split_once(' ').unwrap();
            (name.to_string(), oid.to_string())
        })
        .collect()
    }

    fn current_branch(&self) -> String {
        self.git_stdout(&["branch", "--show-current"])
            .trim()
            .to_string()
    }

    fn porcelain(&self) -> String {
        self.git_stdout(&["status", "--porcelain", "--untracked-files=all"])
    }

    fn stash_list(&self) -> String {
        self.git_stdout(&["stash", "list", "--format=%gs"])
    }

    /// Refs Kindra keeps outside `refs/heads`, one per line.
    fn kindra_refs(&self) -> String {
        self.git_stdout(&["for-each-ref", "--format=%(refname)", "refs/kindra/"])
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        git_command(self.path())
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .status()
            .unwrap()
            .success()
    }

    fn rebase_in_progress(&self) -> bool {
        let git_dir = git2::Repository::open(self.path())
            .unwrap()
            .path()
            .to_path_buf();
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
    }

    fn kin(&self, args: &[&str]) -> Output {
        kin_cmd()
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap()
    }

    fn state_path(&self) -> PathBuf {
        rebase_state_file(self.path())
    }

    fn state_json(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.state_path()).unwrap()).unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ---------------------------------------------------------------------------
// Symbolic names for volatile values
// ---------------------------------------------------------------------------

static OID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{40}\b").unwrap());
static SET_ASIDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(kin-[a-z]+(?:-[a-z]+)*)-[0-9]+-[0-9]+$").unwrap());
static ABSORB_NAMESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"refs/kindra/absorb/([0-9]+-[0-9]+)/").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^<>]+>").unwrap());

/// A two-way mapping between volatile values and their symbolic names.
#[derive(Default)]
struct Labels {
    by_value: HashMap<String, String>,
    by_label: BTreeMap<String, String>,
}

impl Labels {
    /// Name `value` with `label`, unless it already has a name. A label that is
    /// taken by another value gets a `#n` suffix.
    fn name(&mut self, value: &str, label: &str) -> String {
        if let Some(existing) = self.by_value.get(value) {
            return existing.clone();
        }
        let mut candidate = format!("<{label}>");
        let mut n = 2;
        while self.by_label.contains_key(&candidate) {
            candidate = format!("<{label} #{n}>");
            n += 1;
        }
        self.by_value.insert(value.to_string(), candidate.clone());
        self.by_label.insert(candidate.clone(), value.to_string());
        candidate
    }

    /// Name every branch tip in `repo` as `<branch@phase>`.
    fn tips(&mut self, repo: &Repo, phase: &str) {
        for (branch, oid) in repo.tips() {
            self.name(&oid, &format!("{branch}@{phase}"));
        }
    }

    fn normalise_str(&mut self, repo: &Repo, s: &str) -> String {
        if let Some(caps) = SET_ASIDE.captures(s) {
            return self.name(s, &format!("{} set-aside", &caps[1]));
        }
        let s = ABSORB_NAMESPACE
            .replace_all(s, |caps: &Captures| {
                let label = self.name(&caps[1], "absorb-namespace");
                format!("refs/kindra/absorb/{label}/")
            })
            .into_owned();
        OID.replace_all(&s, |caps: &Captures| {
            let oid = &caps[0];
            if let Some(label) = self.by_value.get(oid) {
                return label.clone();
            }
            let subject = git_command(repo.path())
                .args(["log", "-1", "--format=%s", oid])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
            // A stash commit's subject is `On <branch>: <message>`.
            let set_aside = subject
                .as_deref()
                .and_then(|subject| subject.split_once(": "))
                .and_then(|(_, message)| SET_ASIDE.captures(message))
                .map(|caps| caps[1].to_string());
            if let Some(set_aside) = set_aside {
                return self.name(oid, &format!("{set_aside} set-aside stash"));
            }
            match subject {
                Some(subject) => self.name(oid, &format!("commit: {subject}")),
                None => self.name(oid, "unknown object"),
            }
        })
        .into_owned()
    }

    /// Replace volatile values in `value` with symbolic names, sorting object
    /// keys. Keys and values are visited in sorted order so the names given
    /// to unnamed commits do not depend on serialization order.
    fn normalise(&mut self, repo: &Repo, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.normalise_str(repo, s)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.normalise(repo, item))
                    .collect(),
            ),
            Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                let mut normalised = BTreeMap::new();
                for (key, item) in entries {
                    let key = self.normalise_str(repo, key);
                    normalised.insert(key, self.normalise(repo, item));
                }
                Value::Object(normalised.into_iter().collect())
            }
            other => other.clone(),
        }
    }

    fn denormalise_str(&self, s: &str) -> String {
        LABEL
            .replace_all(s, |caps: &Captures| {
                self.by_label
                    .get(&caps[0])
                    .unwrap_or_else(|| panic!("fixture uses unknown name {}", &caps[0]))
                    .clone()
            })
            .into_owned()
    }

    /// Replace symbolic names in a fixture with this repository's values.
    fn denormalise(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.denormalise_str(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.denormalise(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (self.denormalise_str(key), self.denormalise(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// `value` with object keys sorted at every level, pretty-printed.
fn canonical(value: &Value) -> String {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            Value::Object(map) => {
                let sorted: BTreeMap<_, _> =
                    map.iter().map(|(k, v)| (k.clone(), sort(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_string_pretty(&sort(value)).unwrap() + "\n"
}

fn fixture_path(kind: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/operation_state")
        .join(kind)
        .join(format!("{name}.json"))
}

// ---------------------------------------------------------------------------
// Paused operations
// ---------------------------------------------------------------------------

/// An operation that paused on a conflict.
struct Paused {
    repo: Repo,
    labels: Labels,
    /// The branch checked out when the operation started.
    started_on: String,
    before: BTreeMap<String, String>,
}

impl Paused {
    /// Run `args` in `repo`, which must pause the operation on a conflict.
    fn start(repo: Repo, args: &[&str]) -> Self {
        Self::start_with_editor(repo, args, None)
    }

    /// [`Paused::start`], with `GIT_EDITOR` set to `editor` if given.
    fn start_with_editor(repo: Repo, args: &[&str], editor: Option<&Path>) -> Self {
        let mut labels = Labels::default();
        labels.tips(&repo, "before");
        let before = repo.tips();
        let started_on = repo.current_branch();
        let mut cmd = kin_cmd();
        cmd.args(args).current_dir(repo.path());
        if let Some(editor) = editor {
            cmd.env("GIT_EDITOR", editor);
        }
        let output = cmd.output().unwrap();
        assert!(
            !output.status.success(),
            "kin {args:?} was expected to pause on a conflict\n{}",
            describe(&output)
        );
        assert!(
            repo.state_path().exists(),
            "kin {args:?} saved no journal\n{}",
            describe(&output)
        );
        assert!(
            repo.rebase_in_progress(),
            "kin {args:?} left no rebase to resolve\n{}",
            describe(&output)
        );
        labels.tips(&repo, "paused");
        Self {
            repo,
            labels,
            started_on,
            before,
        }
    }

    fn journal(&mut self) -> Value {
        let raw = self.repo.state_json();
        self.labels.normalise(&self.repo, &raw)
    }

    /// Compare the saved journal with its golden file, or rewrite the golden
    /// file when `KIN_UPDATE_GOLDEN` is set.
    fn assert_golden(&mut self, case: &str) {
        assert_golden(case, &self.journal());
    }

    /// Replace the saved journal with the legacy fixture `name`. The fixture's
    /// symbolic names resolve to the values in the journal it replaces.
    fn install_legacy(&mut self, name: &str) {
        self.journal();
        install_legacy(&self.labels, name, &self.repo.state_path());
    }

    fn assert_status(&self, expected: &str) {
        let output = self.repo.kin(&["status"]);
        assert!(output.status.success(), "{}", describe(&output));
        assert_eq!(stdout(&output), expected);
    }
}

/// Compare a saved journal, already normalised, with its golden file, or
/// rewrite the golden file when `KIN_UPDATE_GOLDEN` is set.
fn assert_golden(case: &str, journal: &Value) {
    let actual = canonical(journal);
    let path = fixture_path("golden", case);
    if std::env::var_os("KIN_UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: {err}. Run with KIN_UPDATE_GOLDEN=1 to create it.",
            path.display()
        )
    });
    let expected = canonical(&serde_json::from_str(&expected).unwrap());
    assert_eq!(
        actual,
        expected,
        "the journal saved for '{case}' differs from {}. If the change is intended, rerun with KIN_UPDATE_GOLDEN=1 and review the diff.",
        path.display()
    );
}

/// Write the legacy fixture `name` to `state_path`, resolving its symbolic
/// names with `labels`.
fn install_legacy(labels: &Labels, name: &str, state_path: &Path) {
    let path = fixture_path("legacy", name);
    let fixture: Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap_or_else(|err| {
            panic!("{}: {err}", path.display());
        });
    let journal = labels.denormalise(&fixture);
    fs::write(state_path, serde_json::to_string_pretty(&journal).unwrap()).unwrap();
}

impl Paused {
    /// Resolve every conflict with `resolved` and `kin continue`, as often as
    /// the operation stops.
    fn continue_to_completion(&self) {
        for _ in 0..8 {
            let unmerged = self
                .repo
                .git_stdout(&["diff", "--name-only", "--diff-filter=U"]);
            for file in unmerged.lines() {
                self.repo.stage(file, "resolved\n");
            }
            let output = self.repo.kin(&["continue"]);
            if output.status.success() {
                assert!(
                    !self.repo.state_path().exists(),
                    "continue succeeded but left the journal\n{}",
                    describe(&output)
                );
                assert!(!self.repo.rebase_in_progress());
                return;
            }
            assert!(
                self.repo.rebase_in_progress(),
                "continue failed without a conflict to resolve\n{}",
                describe(&output)
            );
        }
        panic!("the operation did not complete");
    }

    fn abort(&self) -> Output {
        let output = self.repo.kin(&["abort"]);
        assert!(output.status.success(), "{}", describe(&output));
        output
    }

    /// Branch tips, by symbolic name.
    fn tips(&mut self) -> BTreeMap<String, String> {
        let tips = self.repo.tips();
        tips.into_iter()
            .map(|(branch, oid)| {
                let label = self.labels.normalise_str(&self.repo, &oid);
                (branch, label)
            })
            .collect()
    }

    /// Assert every branch is back at its tip from before the operation, and
    /// the branch it started on is checked out, with nothing left in progress.
    fn assert_restored(&self) {
        assert_eq!(self.repo.tips(), self.before);
        assert_eq!(self.repo.current_branch(), self.started_on);
        assert!(!self.repo.state_path().exists());
        assert!(!self.repo.rebase_in_progress());
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// `main <- feature-a <- feature-b`, where feature-a changes `shared.txt`
/// and feature-b adds `b.txt`. Leaves feature-a checked out.
fn linear_stack() -> Repo {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a\n", "a");
    repo.branch("feature-b");
    repo.commit("b.txt", "b\n", "b");
    repo.switch("feature-a");
    repo
}

/// `kin move --onto other` from feature-a, where `other` changes the same
/// line as feature-a.
fn paused_move() -> Paused {
    let repo = linear_stack();
    repo.switch("main");
    repo.branch("other");
    repo.commit("shared.txt", "other\n", "other");
    repo.switch("feature-a");
    Paused::start(repo, &["move", "--onto", "other"])
}

/// `kin restack` from feature-a after amending it, where feature-b changes
/// the line the amend rewrote.
fn paused_restack() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a1\n", "a");
    repo.branch("feature-b");
    repo.commit("shared.txt", "b\n", "b");
    repo.switch("feature-a");
    repo.stage("shared.txt", "a2\n");
    repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    Paused::start(repo, &["restack"])
}

/// `kin reorder` swapping `main <- feature-a <- feature-b` to
/// `main <- feature-b <- feature-a`; both change the same line.
fn paused_reorder() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a\n", "a");
    repo.branch("feature-b");
    repo.commit("shared.txt", "b\n", "b");
    repo.switch("feature-a");
    let edited = repo.path().join(".git/edited-reorder.txt");
    fs::write(&edited, "branch feature-b parent main\nbranch feature-a\n").unwrap();
    // kin runs the editor through `sh -c` on Unix and `cmd /C` on Windows.
    let (editor, script) = if cfg!(windows) {
        (
            repo.path().join(".git/reorder-editor.cmd"),
            format!(
                "@echo off\r\ncopy /Y \"{}\" \"%~1\" >NUL\r\n",
                edited.display()
            ),
        )
    } else {
        (
            repo.path().join(".git/reorder-editor.sh"),
            format!("#!/bin/sh\ncp \"{}\" \"$1\"\n", edited.display()),
        )
    };
    fs::write(&editor, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    }

    Paused::start_with_editor(repo, &["reorder"], Some(&editor))
}

/// `kin sync` from feature-b of a linear stack after `main` changed the line
/// feature-a changes. There is no remote, so the stack syncs onto local main.
fn paused_sync_linear() -> Paused {
    let repo = linear_stack();
    repo.switch("main");
    repo.commit("shared.txt", "main\n", "main moved");
    repo.switch("feature-b");
    Paused::start(repo, &["sync"])
}

/// [`paused_sync_linear`] with a tracked file edited and an untracked file
/// added, which the sync sets aside itself and records in the journal.
fn paused_sync_linear_dirty() -> Paused {
    let repo = linear_stack();
    repo.switch("main");
    repo.commit("shared.txt", "main\n", "main moved");
    repo.switch("feature-b");
    repo.write("b.txt", "dirty\n");
    repo.write("untracked.txt", "untracked\n");
    Paused::start(repo, &["sync", "--autostash"])
}

/// `kin sync` on `main` when `origin/main` changed the line local `main`
/// changes.
fn paused_sync_upstream() -> Paused {
    let mut repo = Repo::new();
    let remote = TempDir::new().unwrap();
    run_ok(
        "git",
        &["init", "-q", "--bare", "--initial-branch=main"],
        remote.path(),
    );
    repo.git(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);

    let other = TempDir::new().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            "-q",
            remote.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        repo.path(),
    );
    fs::write(other.path().join("shared.txt"), "remote\n").unwrap();
    run_ok("git", &["commit", "-q", "-am", "remote"], other.path());
    run_ok("git", &["push", "-q", "origin", "main"], other.path());

    repo.commit("shared.txt", "local\n", "local");
    repo._remote = Some(remote);
    Paused::start(repo, &["sync"])
}

/// `kin sync` from feature-a of a tree `main <- feature-a <- {feature-b,
/// feature-c}` after `main` changed the line feature-b changes.
fn paused_sync_tree() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("a.txt", "a\n", "a");
    repo.branch("feature-b");
    repo.commit("shared.txt", "b\n", "b");
    repo.switch("feature-a");
    repo.branch("feature-c");
    repo.commit("c.txt", "c\n", "c");
    repo.switch("main");
    repo.commit("shared.txt", "main\n", "main moved");
    repo.switch("feature-a");
    Paused::start(repo, &["sync"])
}

/// `main <- feature-a <- feature-b`, where both change `shared.txt`. Leaves
/// feature-a checked out.
fn conflicting_stack() -> Repo {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a\n", "a");
    repo.branch("feature-b");
    repo.commit("shared.txt", "b\n", "b");
    repo.switch("feature-a");
    repo
}

/// `kin commit` on feature-a, whose restack of feature-b conflicts.
fn paused_commit_restack() -> Paused {
    let repo = conflicting_stack();
    repo.stage("shared.txt", "a2\n");
    Paused::start(repo, &["commit", "-m", "a2"])
}

/// `kin commit --on feature-a` from feature-b: the commit is made on
/// feature-b and moved below it, where it conflicts with feature-b's change.
fn paused_commit_on_ancestor() -> Paused {
    let repo = conflicting_stack();
    repo.switch("feature-b");
    repo.stage("shared.txt", "x\n");
    repo.write("unstaged.txt", "unstaged\n");
    Paused::start(repo, &["commit", "--on", "feature-a", "-m", "x"])
}

/// `kin commit --fixup <a1>` on feature-a, whose later commit a2 changes the
/// line the fixup changes.
fn paused_commit_fixup() -> Paused {
    paused_commit_fixup_with(&[])
}

/// [`paused_commit_fixup`] with `--autostash`. The flag is a no-op for
/// `kin commit`, so the journal is the same; Kindra 1.1 recorded it alongside
/// the unstaged changes it had already set aside.
fn paused_commit_fixup_autostash() -> Paused {
    paused_commit_fixup_with(&["--autostash"])
}

fn paused_commit_fixup_with(extra_args: &[&str]) -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a1\n", "a1");
    let a1 = repo.rev("HEAD");
    repo.commit("shared.txt", "a2\n", "a2");
    repo.stage("shared.txt", "fix\n");
    repo.write("unstaged.txt", "unstaged\n");
    let mut args = vec!["commit", "--fixup", &a1];
    args.extend_from_slice(extra_args);
    Paused::start(repo, &args)
}

/// [`paused_commit_fixup`] with feature-b stacked on feature-a, so the fixup
/// sets the unstaged changes aside itself before folding.
fn paused_commit_fixup_dependents() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a1\n", "a1");
    let a1 = repo.rev("HEAD");
    repo.commit("shared.txt", "a2\n", "a2");
    repo.branch("feature-b");
    repo.commit("b.txt", "b\n", "b");
    repo.switch("feature-a");
    repo.stage("shared.txt", "fix\n");
    repo.write("unstaged.txt", "unstaged\n");
    Paused::start(repo, &["commit", "--fixup", &a1])
}

/// `kin commit -b inserted --insert` on feature-a, whose restack of feature-b
/// onto the new branch conflicts.
fn paused_commit_insert() -> Paused {
    let repo = conflicting_stack();
    repo.stage("shared.txt", "inserted\n");
    Paused::start(
        repo,
        &["commit", "-b", "inserted", "--insert", "-m", "inserted"],
    )
}

/// `kin absorb` on feature-a, folding a staged change into its first commit;
/// the restack of feature-b, which changes the same line, conflicts.
fn paused_absorb() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("code.txt", "line1\nline2\nline3\n", "a: add code");
    repo.commit("extra.txt", "extra\n", "a: add extra");
    repo.branch("feature-b");
    repo.commit("code.txt", "line1 B\nline2\nline3\n", "b: edit line1");
    repo.switch("feature-a");
    repo.stage("code.txt", "line1 A\nline2\nline3\n");
    repo.write("untracked.txt", "untracked\n");
    Paused::start(repo, &["absorb", "--force-author"])
}

/// `kin absorb` on feature-a, where feature-b forks from feature-a's first
/// commit, which no branch names: absorb anchors that fork point under
/// `refs/kindra/absorb/` and records the anchor in `new_base_map`.
fn paused_absorb_unnamed_fork() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("code.txt", "line1\nline2\nline3\n", "a: add code");
    repo.branch("feature-b");
    repo.commit("code.txt", "line1 B\nline2\nline3\n", "b: edit line1");
    repo.switch("feature-a");
    repo.commit("extra.txt", "extra\n", "a: add extra");
    repo.stage("code.txt", "line1 A\nline2\nline3\n");
    Paused::start(repo, &["absorb", "--force-author"])
}

// ---------------------------------------------------------------------------
// Golden journals and status, per operation
// ---------------------------------------------------------------------------

const NATIVE_REBASE: &str = "A native git operation (rebase) is in progress. Finish it with 'git rebase --continue' or 'git rebase --abort'.\n";

#[test]
fn move_journal_and_status() {
    let mut paused = paused_move();
    paused.assert_golden("move");
    paused.assert_status(&format!(
        "Move in progress: feature-a onto other\nRemaining branches: feature-a, feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn restack_journal_and_status() {
    let mut paused = paused_restack();
    paused.assert_golden("restack");
    paused.assert_status(&format!(
        "Restack in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn reorder_journal_and_status() {
    let mut paused = paused_reorder();
    paused.assert_golden("reorder");
    paused.assert_status(&format!(
        "Reorder in progress from feature-a\nRemaining branches: feature-b, feature-a\n{NATIVE_REBASE}"
    ));
}

#[test]
fn sync_linear_journal_and_status() {
    let mut paused = paused_sync_linear();
    paused.assert_golden("sync_linear");
    paused.assert_status(&format!(
        "Sync in progress: feature-b onto main\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn sync_linear_dirty_journal_and_status() {
    let mut paused = paused_sync_linear_dirty();
    paused.assert_golden("sync_linear_dirty");
    paused.assert_status(&format!(
        "Sync in progress: feature-b onto main\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
    paused.continue_to_completion();
    assert_eq!(paused.repo.current_branch(), "feature-b");
    assert_eq!(
        paused.repo.porcelain(),
        " M b.txt\n?? untracked.txt\n",
        "the set-aside changes come back when the sync completes"
    );
    assert_eq!(paused.repo.stash_list(), "");
}

#[test]
fn sync_upstream_journal_and_status() {
    let mut paused = paused_sync_upstream();
    paused.assert_golden("sync_upstream");
    paused.assert_status(&format!(
        "Sync in progress: main onto origin/main\nRemaining branches: main\n{NATIVE_REBASE}"
    ));
}

#[test]
fn sync_tree_journal_and_status() {
    let mut paused = paused_sync_tree();
    paused.assert_golden("sync_tree");
    paused.assert_status(&format!(
        "Sync in progress: feature-a onto main\nRemaining branches: feature-b, feature-c\n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_restack_journal_and_status() {
    let mut paused = paused_commit_restack();
    paused.assert_golden("commit_restack");
    paused.assert_status(&format!(
        "Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_on_ancestor_journal_and_status() {
    let mut paused = paused_commit_on_ancestor();
    paused.assert_golden("commit_on_ancestor");
    paused.assert_status(&format!(
        "Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_fixup_journal_and_status() {
    let mut paused = paused_commit_fixup();
    paused.assert_golden("commit_fixup");
    paused.assert_status(&format!(
        "Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_fixup_autostash_journal_and_status() {
    let mut paused = paused_commit_fixup_autostash();
    paused.assert_golden("commit_fixup_autostash");
    paused.assert_status(&format!(
        "Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_fixup_dependents_journal_and_status() {
    let mut paused = paused_commit_fixup_dependents();
    paused.assert_golden("commit_fixup_dependents");
    paused.assert_status(&format!(
        "Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
    let output = paused.abort();
    assert!(
        stdout(&output).contains("Operation aborted (state cleared)."),
        "{}",
        describe(&output)
    );
    assert_eq!(
        paused.repo.porcelain(),
        "M  shared.txt\n?? unstaged.txt\n",
        "abort gives the fixup back staged and restores the set-aside changes"
    );
    assert_eq!(paused.repo.stash_list(), "");
    paused.assert_restored();
}

#[test]
fn commit_insert_journal_and_status() {
    let mut paused = paused_commit_insert();
    paused.assert_golden("commit_insert");
    paused.assert_status(&format!(
        "Commit in progress on inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

/// Aborting an insert undoes the whole command: the branch it created is
/// deleted, the branch it ran on is checked out, and the commit's change is
/// staged again.
#[test]
fn commit_insert_aborts_to_where_it_started() {
    let paused = paused_commit_insert();
    paused.abort();
    paused.assert_restored();
    assert_eq!(paused.repo.porcelain(), "M  shared.txt\n");
    assert_eq!(paused.repo.stash_list(), "");
}

#[test]
fn absorb_journal_and_status() {
    let mut paused = paused_absorb();
    paused.assert_golden("absorb");
    paused.assert_status(&format!(
        "Absorb in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn absorb_unnamed_fork_journal_and_status() {
    let mut paused = paused_absorb_unnamed_fork();
    paused.assert_golden("absorb_unnamed_fork");
    assert_eq!(
        paused.repo.kindra_refs().lines().count(),
        1,
        "the anchor recorded in new_base_map must exist while paused"
    );
    paused.assert_status(&format!(
        "Absorb in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

/// The journal is stored under the documented file name in the worktree's
/// own Git directory, and nowhere else.
#[test]
fn journal_lives_in_the_worktree_git_directory() {
    let paused = paused_move();
    let git_dir = paused.repo.path().join(".git");
    assert!(StateFile::Rebase.in_git_dir(&git_dir).exists());
    assert!(!StateFile::Run.in_git_dir(&git_dir).exists());
    assert!(!StateFile::Checkout.in_git_dir(&git_dir).exists());
}

/// The journal is saved inside a `{version, journal}` envelope with nothing
/// else at the top level, so Kindra 1.1 and earlier, which require
/// `operation`, `original_branch`, `target_branch`, `remaining_branches` and
/// `in_progress_branch` there, refuse the file instead of misreading it.
#[test]
fn journal_is_saved_in_an_envelope_older_kindra_cannot_parse() {
    let paused = paused_commit_fixup();
    let saved = paused.repo.state_json();
    let mut keys: Vec<_> = saved.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["journal", "version"]);
    assert_eq!(saved["version"], Value::from(5));
    assert!(saved["journal"]["operation"].is_string());
}

/// The `operation` label only names the command that paused: how the paused
/// rebase is resumed and finished comes from what the journal records (its
/// `rebase_options` and `cleanup_merged_branches`). A linear sync whose label
/// says otherwise still finishes as a sync, deleting the merged branches it
/// recorded.
#[test]
fn replay_not_the_label_decides_how_a_paused_sync_finishes() {
    let paused = paused_sync_linear();
    paused.repo.git(&["branch", "stale", "main"]);
    let mut journal = paused.repo.state_json();
    journal["journal"]["operation"] = Value::from("Move");
    journal["journal"]["cleanup_merged_branches"] = serde_json::json!(["stale"]);
    fs::write(
        paused.repo.state_path(),
        serde_json::to_string_pretty(&journal).unwrap(),
    )
    .unwrap();

    paused.continue_to_completion();
    assert!(
        !paused.repo.tips().contains_key("stale"),
        "the sync's cleanup did not run"
    );
    assert_eq!(paused.repo.current_branch(), "feature-b");
    assert!(paused.repo.is_ancestor("main", "feature-a"));
    assert!(paused.repo.is_ancestor("feature-a", "feature-b"));
}

/// A journal saved by a newer Kindra, naming an operation this one does not
/// know, is refused with advice rather than a bare parse error, and is left
/// as it is until `kin abort --clear-state` discards it.
#[test]
fn journal_from_a_newer_kindra_is_refused_with_advice() {
    let paused = paused_move();
    let mut journal = paused.repo.state_json();
    journal["journal"]["operation"] = Value::from("FromTheFuture");
    let saved = serde_json::to_string_pretty(&journal).unwrap();
    fs::write(paused.repo.state_path(), &saved).unwrap();

    for command in ["status", "continue", "abort"] {
        let output = paused.repo.kin(&[command]);
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("newer version of kin"), "{stderr}");
        assert!(stderr.contains("kin abort --clear-state"), "{stderr}");
        assert_eq!(fs::read_to_string(paused.repo.state_path()).unwrap(), saved);
        assert!(paused.repo.rebase_in_progress());
    }

    let cleared = paused.repo.kin(&["abort", "--clear-state"]);
    assert!(cleared.status.success(), "{}", describe(&cleared));
    assert!(!paused.repo.state_path().exists());
}

/// A journal saved in a format newer than this Kindra reads is refused with
/// advice and left as it is until `kin abort --clear-state` discards it, even
/// when every field it has would parse.
#[test]
fn journal_with_a_newer_version_is_refused_with_advice() {
    let paused = paused_commit_fixup();
    let mut journal = paused.repo.state_json();
    assert_eq!(journal["version"], Value::from(5));
    journal["version"] = Value::from(6);
    let saved = serde_json::to_string_pretty(&journal).unwrap();
    fs::write(paused.repo.state_path(), &saved).unwrap();

    for command in ["status", "continue", "abort"] {
        let output = paused.repo.kin(&[command]);
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("newer version of kin"), "{stderr}");
        assert!(stderr.contains("journal version 6"), "{stderr}");
        assert!(stderr.contains("kin abort --clear-state"), "{stderr}");
        assert_eq!(fs::read_to_string(paused.repo.state_path()).unwrap(), saved);
        assert!(paused.repo.rebase_in_progress());
    }

    let set_aside = paused.repo.stash_list();
    let cleared = paused.repo.kin(&["abort", "--clear-state"]);
    assert!(cleared.status.success(), "{}", describe(&cleared));
    assert!(!paused.repo.state_path().exists());
    assert_eq!(paused.repo.stash_list(), set_aside);
}

/// A journal saved in version 1 of the envelope, which holds nothing version 2
/// added, still resumes to completion. Version 1 recorded its progress as
/// version 3 did, so the frozen version 3 journal stands in for it.
#[test]
fn journal_of_an_older_version_still_continues() {
    let mut paused = paused_absorb();
    paused.install_legacy("absorb@journal-v3");
    let mut journal = paused.repo.state_json();
    assert_eq!(journal["version"], Value::from(3));
    journal["version"] = Value::from(1);
    fs::write(
        paused.repo.state_path(),
        serde_json::to_string_pretty(&journal).unwrap(),
    )
    .unwrap();

    paused.continue_to_completion();
    assert_eq!(
        fs::read_to_string(paused.repo.path().join("untracked.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(paused.repo.stash_list(), "");
}

// ---------------------------------------------------------------------------
// Legacy journals
// ---------------------------------------------------------------------------

/// Pause `scenario` and swap its journal for the legacy fixture `fixture`.
fn paused_with_legacy(scenario: fn() -> Paused, fixture: &str) -> Paused {
    let mut paused = scenario();
    paused.install_legacy(fixture);
    paused
}

/// A paused replay operation from an older Kindra reports as it did, and
/// `kin abort` puts every branch back and checks out the branch it started on.
fn assert_legacy_replay_aborts(scenario: fn() -> Paused, fixture: &str, status: &str) {
    let paused = paused_with_legacy(scenario, fixture);
    paused.assert_status(status);
    let output = paused.abort();
    assert!(
        stdout(&output).contains("Operation aborted (state cleared)."),
        "{}",
        describe(&output)
    );
    paused.assert_restored();
    assert_eq!(paused.repo.porcelain(), "");
}

/// A paused replay operation from an older Kindra continues to completion,
/// ending on `ends_on` with each `(ancestor, branch)` pair stacked in order.
fn assert_legacy_replay_continues(
    scenario: fn() -> Paused,
    fixture: &str,
    ends_on: &str,
    stacked: &[(&str, &str)],
) -> Paused {
    let paused = paused_with_legacy(scenario, fixture);
    paused.continue_to_completion();
    assert_eq!(paused.repo.current_branch(), ends_on);
    for (ancestor, branch) in stacked {
        assert!(
            paused.repo.is_ancestor(ancestor, branch),
            "{branch} is not stacked on {ancestor}"
        );
    }
    assert_eq!(paused.repo.porcelain(), "");
    paused
}

/// Kindra 0.1.0 recorded no `owned_tip_map`, so `kin abort` cannot prove the
/// repository is still as the operation left it. It refuses, leaves the
/// journal, the native rebase and every branch as they are, and says how to
/// finish: continue it, or clear Kindra's record and abort the rebase with
/// Git. ADR 0001 promises abort only for journals from 0.2.0 on.
fn assert_legacy_without_owned_tips_refuses_abort(
    scenario: fn() -> Paused,
    fixture: &str,
    status: &str,
) {
    let paused = paused_with_legacy(scenario, fixture);
    paused.assert_status(status);
    let paused_tips = paused.repo.tips();

    let output = paused.repo.kin(&["abort"]);
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("kin continue"), "{stderr}");
    assert!(stderr.contains("kin abort --clear-state"), "{stderr}");
    assert!(paused.repo.state_path().exists());
    assert!(paused.repo.rebase_in_progress());
    assert_eq!(paused.repo.tips(), paused_tips);

    let cleared = paused.repo.kin(&["abort", "--clear-state"]);
    assert!(cleared.status.success(), "{}", describe(&cleared));
    assert!(!paused.repo.state_path().exists());
    assert!(paused.repo.rebase_in_progress());
    assert_eq!(paused.repo.tips(), paused_tips);
}

fn move_status() -> String {
    format!(
        "Move in progress: feature-a onto other\nRemaining branches: feature-a, feature-b\n{NATIVE_REBASE}"
    )
}

/// Kindra 1.1 and earlier saved a restack under the `Move` label, so it still
/// reports as a move of the restacked branch onto itself.
fn restack_status() -> String {
    format!(
        "Move in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    )
}

fn reorder_status() -> String {
    format!(
        "Reorder in progress from feature-a\nRemaining branches: feature-b, feature-a\n{NATIVE_REBASE}"
    )
}

fn sync_linear_status() -> String {
    format!("Sync in progress: feature-b onto main\nRemaining branches: feature-b\n{NATIVE_REBASE}")
}

fn sync_upstream_status() -> String {
    format!("Sync in progress: main onto origin/main\nRemaining branches: main\n{NATIVE_REBASE}")
}

fn sync_tree_status() -> String {
    format!(
        "Sync in progress: feature-a onto main\nRemaining branches: feature-b, feature-c\n{NATIVE_REBASE}"
    )
}

#[test]
fn legacy_move_0_1_0_abort_refuses_and_explains() {
    assert_legacy_without_owned_tips_refuses_abort(paused_move, "move@0.1.0", &move_status());
}

#[test]
fn legacy_move_0_1_0_continues() {
    assert_legacy_replay_continues(
        paused_move,
        "move@0.1.0",
        "feature-a",
        &[("other", "feature-a"), ("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_move_1_1_0_aborts() {
    assert_legacy_replay_aborts(paused_move, "move@1.1.0", &move_status());
}

#[test]
fn legacy_move_1_1_0_continues() {
    assert_legacy_replay_continues(
        paused_move,
        "move@1.1.0",
        "feature-a",
        &[("other", "feature-a"), ("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_restack_0_3_0_aborts() {
    assert_legacy_replay_aborts(paused_restack, "restack@0.3.0", &restack_status());
}

#[test]
fn legacy_restack_0_3_0_continues() {
    assert_legacy_replay_continues(
        paused_restack,
        "restack@0.3.0",
        "feature-a",
        &[("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_restack_1_1_0_aborts() {
    assert_legacy_replay_aborts(paused_restack, "restack@1.1.0", &restack_status());
}

#[test]
fn legacy_restack_1_1_0_continues() {
    assert_legacy_replay_continues(
        paused_restack,
        "restack@1.1.0",
        "feature-a",
        &[("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_reorder_0_2_2_aborts() {
    assert_legacy_replay_aborts(paused_reorder, "reorder@0.2.2", &reorder_status());
}

#[test]
fn legacy_reorder_0_2_2_continues() {
    let paused = assert_legacy_replay_continues(
        paused_reorder,
        "reorder@0.2.2",
        "feature-a",
        &[("main", "feature-b"), ("feature-b", "feature-a")],
    );
    // The reorder puts feature-b directly on main, not just somewhere above it.
    assert_eq!(paused.repo.rev("feature-b^"), paused.repo.rev("main"));
}

#[test]
fn legacy_reorder_1_1_0_aborts() {
    assert_legacy_replay_aborts(paused_reorder, "reorder@1.1.0", &reorder_status());
}

#[test]
fn legacy_reorder_1_1_0_continues() {
    let paused = assert_legacy_replay_continues(
        paused_reorder,
        "reorder@1.1.0",
        "feature-a",
        &[("main", "feature-b"), ("feature-b", "feature-a")],
    );
    // The reorder puts feature-b directly on main, not just somewhere above it.
    assert_eq!(paused.repo.rev("feature-b^"), paused.repo.rev("main"));
}

#[test]
fn legacy_sync_linear_0_1_0_abort_refuses_and_explains() {
    assert_legacy_without_owned_tips_refuses_abort(
        paused_sync_linear,
        "sync_linear@0.1.0",
        &sync_linear_status(),
    );
}

#[test]
fn legacy_sync_linear_0_1_0_continues() {
    assert_legacy_replay_continues(
        paused_sync_linear,
        "sync_linear@0.1.0",
        "feature-b",
        &[("main", "feature-a"), ("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_sync_linear_1_1_0_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_linear,
        "sync_linear@1.1.0",
        &sync_linear_status(),
    );
}

#[test]
fn legacy_sync_linear_1_1_0_continues() {
    assert_legacy_replay_continues(
        paused_sync_linear,
        "sync_linear@1.1.0",
        "feature-b",
        &[("main", "feature-a"), ("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_sync_upstream_0_3_0_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_upstream,
        "sync_upstream@0.3.0",
        &sync_upstream_status(),
    );
}

#[test]
fn legacy_sync_upstream_0_3_0_continues() {
    assert_legacy_replay_continues(
        paused_sync_upstream,
        "sync_upstream@0.3.0",
        "main",
        &[("origin/main", "main")],
    );
}

#[test]
fn legacy_sync_upstream_1_1_0_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_upstream,
        "sync_upstream@1.1.0",
        &sync_upstream_status(),
    );
}

#[test]
fn legacy_sync_upstream_1_1_0_continues() {
    assert_legacy_replay_continues(
        paused_sync_upstream,
        "sync_upstream@1.1.0",
        "main",
        &[("origin/main", "main")],
    );
}

#[test]
fn legacy_sync_tree_1_1_0_aborts() {
    assert_legacy_replay_aborts(paused_sync_tree, "sync_tree@1.1.0", &sync_tree_status());
}

#[test]
fn legacy_sync_tree_1_1_0_continues() {
    assert_legacy_replay_continues(
        paused_sync_tree,
        "sync_tree@1.1.0",
        "feature-a",
        &[
            ("main", "feature-a"),
            ("feature-a", "feature-b"),
            ("feature-a", "feature-c"),
        ],
    );
}

// Version 2 journals, saved before sync finished in the shared rebase loop,
// record `replay` instead of `rebase_options`.

#[test]
fn legacy_sync_linear_journal_v2_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_linear,
        "sync_linear@journal-v2",
        &sync_linear_status(),
    );
}

#[test]
fn legacy_sync_linear_journal_v2_continues() {
    assert_legacy_replay_continues(
        paused_sync_linear,
        "sync_linear@journal-v2",
        "feature-b",
        &[("main", "feature-a"), ("feature-a", "feature-b")],
    );
}

/// A version 2 linear sync ran its one rebase only when it started: once
/// that rebase is aborted with Git, `kin continue` refuses rather than start
/// it again, and leaves the journal for `kin abort`.
#[test]
fn legacy_sync_linear_journal_v2_refuses_to_restart_an_abandoned_rebase() {
    let paused = paused_with_legacy(paused_sync_linear, "sync_linear@journal-v2");
    paused.repo.git(&["rebase", "--abort"]);
    let tips = paused.repo.tips();

    let output = paused.repo.kin(&["continue"]);
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Sync did not complete: 'feature-b' is not rebased onto 'main'"),
        "{stderr}"
    );
    assert!(paused.repo.state_path().exists());
    assert!(!paused.repo.rebase_in_progress());
    assert_eq!(paused.repo.tips(), tips);

    paused.abort();
    paused.assert_restored();
}

/// A version 2 linear sync keeps the working-tree changes it set aside, and
/// gives them back when it completes or is aborted.
#[test]
fn legacy_sync_linear_dirty_journal_v2_restores_its_set_aside() {
    for abort in [false, true] {
        let paused = paused_with_legacy(paused_sync_linear_dirty, "sync_linear_dirty@journal-v2");
        if abort {
            paused.abort();
            paused.assert_restored();
        } else {
            paused.continue_to_completion();
            assert_eq!(paused.repo.current_branch(), "feature-b");
            assert!(paused.repo.is_ancestor("main", "feature-a"));
            assert!(paused.repo.is_ancestor("feature-a", "feature-b"));
        }
        assert_eq!(
            paused.repo.porcelain(),
            " M b.txt\n?? untracked.txt\n",
            "abort: {abort}"
        );
        assert_eq!(paused.repo.stash_list(), "");
    }
}

#[test]
fn legacy_sync_upstream_journal_v2_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_upstream,
        "sync_upstream@journal-v2",
        &sync_upstream_status(),
    );
}

#[test]
fn legacy_sync_upstream_journal_v2_continues() {
    assert_legacy_replay_continues(
        paused_sync_upstream,
        "sync_upstream@journal-v2",
        "main",
        &[("origin/main", "main")],
    );
}

#[test]
fn legacy_sync_tree_journal_v2_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_tree,
        "sync_tree@journal-v2",
        &sync_tree_status(),
    );
}

/// The tree sync's original branch was already rebased when it paused; the
/// rest land on it, each directly on its planned parent.
#[test]
fn legacy_sync_tree_journal_v2_continues() {
    let paused = assert_legacy_replay_continues(
        paused_sync_tree,
        "sync_tree@journal-v2",
        "feature-a",
        &[
            ("main", "feature-a"),
            ("feature-a", "feature-b"),
            ("feature-a", "feature-c"),
        ],
    );
    assert_eq!(paused.repo.rev("feature-a^"), paused.repo.rev("main"));
    assert_eq!(paused.repo.rev("feature-b^"), paused.repo.rev("feature-a"));
    assert_eq!(paused.repo.rev("feature-c^"), paused.repo.rev("feature-a"));
}

/// What `kin abort` leaves behind for a paused commit or absorb, besides an
/// empty stash list and no journal: branch tips by symbolic name, the branch
/// checked out, and `git status --porcelain`.
struct AbortOutcome<'a> {
    tips: &'a [(&'a str, &'a str)],
    ends_on: &'a str,
    porcelain: &'a str,
}

fn assert_legacy_commit_aborts(
    scenario: fn() -> Paused,
    fixture: &str,
    status: &str,
    expected: AbortOutcome,
) {
    let mut paused = paused_with_legacy(scenario, fixture);
    paused.assert_status(status);
    let output = paused.abort();
    assert!(
        stdout(&output).contains("Operation aborted (state cleared)."),
        "{}",
        describe(&output)
    );
    let expected_tips: BTreeMap<String, String> = expected
        .tips
        .iter()
        .map(|(branch, label)| (branch.to_string(), label.to_string()))
        .collect();
    assert_eq!(paused.tips(), expected_tips);
    assert_eq!(paused.repo.current_branch(), expected.ends_on);
    assert_eq!(paused.repo.porcelain(), expected.porcelain);
    assert_eq!(paused.repo.stash_list(), "");
    assert_eq!(
        paused.repo.kindra_refs(),
        "",
        "absorb anchors were left behind"
    );
    assert!(!paused.repo.state_path().exists());
    assert!(!paused.repo.rebase_in_progress());
}

/// Abort keeps the commit on feature-a, which the journal records as its
/// checkpoint, and undoes only the restack of feature-b.
const COMMIT_RESTACK_ABORTED: AbortOutcome = AbortOutcome {
    tips: &[
        ("feature-a", "<feature-a@paused>"),
        ("feature-b", "<feature-b@before>"),
        ("main", "<main@before>"),
    ],
    ends_on: "feature-a",
    porcelain: "",
};

#[test]
fn legacy_commit_restack_0_1_0_abort_refuses_and_explains() {
    assert_legacy_without_owned_tips_refuses_abort(
        paused_commit_restack,
        "commit_restack@0.1.0",
        &format!("Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
    );
}

#[test]
fn legacy_commit_restack_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_restack,
        "commit_restack@1.1.0",
        &format!("Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        COMMIT_RESTACK_ABORTED,
    );
}

/// Kindra 1.1 recorded the branch rewritten in place, not the ancestor the
/// commit moves onto, so status names feature-b.
#[test]
fn legacy_commit_on_ancestor_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_on_ancestor,
        "commit_on_ancestor@1.1.0",
        &format!("Commit in progress on feature-b\nRemaining branches: \n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-b",
            porcelain: "M  shared.txt\n?? unstaged.txt\n",
        },
    );
}

#[test]
fn legacy_commit_fixup_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_fixup,
        "commit_fixup@1.1.0",
        &format!("Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  shared.txt\n?? unstaged.txt\n",
        },
    );
}

/// Kindra 1.1 saved `autostash` alongside the unstaged changes a fixup had
/// already set aside; it still means the same and the set-aside comes back.
#[test]
fn legacy_commit_fixup_autostash_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_fixup_autostash,
        "commit_fixup_autostash@1.1.0",
        &format!("Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  shared.txt\n?? unstaged.txt\n",
        },
    );
}

#[test]
fn legacy_commit_insert_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_insert,
        "commit_insert@1.1.0",
        &format!("Commit in progress on inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("inserted", "<inserted@paused>"),
                ("main", "<main@before>"),
            ],
            ends_on: "inserted",
            porcelain: "",
        },
    );
}

/// Kindra 1.1 and earlier saved an absorb under the `Commit` label, so it still
/// reports as a commit.
fn absorb_status() -> String {
    format!("Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}")
}

#[test]
fn legacy_absorb_0_2_2_aborts() {
    assert_legacy_commit_aborts(
        paused_absorb,
        "absorb@0.2.2",
        &absorb_status(),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  code.txt\n?? untracked.txt\n",
        },
    );
}

#[test]
fn legacy_absorb_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_absorb,
        "absorb@1.1.0",
        &absorb_status(),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  code.txt\n?? untracked.txt\n",
        },
    );
}

#[test]
fn legacy_absorb_unnamed_fork_1_1_0_aborts_and_deletes_its_anchors() {
    assert_legacy_commit_aborts(
        paused_absorb_unnamed_fork,
        "absorb_unnamed_fork@1.1.0",
        &absorb_status(),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  code.txt\n",
        },
    );
}

// Version 3 journals, saved before the journal recorded steps, record the
// branches left to replay and the branch in progress. Loading turns them into
// steps and a cursor; every one of them still continues and aborts.

fn restack_v3_status() -> String {
    format!("Restack in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}")
}

#[test]
fn legacy_move_journal_v3_aborts() {
    assert_legacy_replay_aborts(paused_move, "move@journal-v3", &move_status());
}

#[test]
fn legacy_move_journal_v3_continues() {
    assert_legacy_replay_continues(
        paused_move,
        "move@journal-v3",
        "feature-a",
        &[("other", "feature-a"), ("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_restack_journal_v3_aborts() {
    assert_legacy_replay_aborts(paused_restack, "restack@journal-v3", &restack_v3_status());
}

#[test]
fn legacy_restack_journal_v3_continues() {
    assert_legacy_replay_continues(
        paused_restack,
        "restack@journal-v3",
        "feature-a",
        &[("feature-a", "feature-b")],
    );
}

#[test]
fn legacy_reorder_journal_v3_aborts() {
    assert_legacy_replay_aborts(paused_reorder, "reorder@journal-v3", &reorder_status());
}

#[test]
fn legacy_reorder_journal_v3_continues() {
    let paused = assert_legacy_replay_continues(
        paused_reorder,
        "reorder@journal-v3",
        "feature-a",
        &[("main", "feature-b"), ("feature-b", "feature-a")],
    );
    assert_eq!(paused.repo.rev("feature-b^"), paused.repo.rev("main"));
}

#[test]
fn legacy_sync_linear_journal_v3_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_linear,
        "sync_linear@journal-v3",
        &sync_linear_status(),
    );
}

#[test]
fn legacy_sync_linear_journal_v3_continues() {
    assert_legacy_replay_continues(
        paused_sync_linear,
        "sync_linear@journal-v3",
        "feature-b",
        &[("main", "feature-a"), ("feature-a", "feature-b")],
    );
}

/// A version 3 linear sync, like a version 2 one, ran its one rebase only
/// when it started.
#[test]
fn legacy_sync_linear_journal_v3_refuses_to_restart_an_abandoned_rebase() {
    let paused = paused_with_legacy(paused_sync_linear, "sync_linear@journal-v3");
    paused.repo.git(&["rebase", "--abort"]);
    let tips = paused.repo.tips();

    let output = paused.repo.kin(&["continue"]);
    assert!(!output.status.success(), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Sync did not complete: 'feature-b' is not rebased onto 'main'"),
        "{stderr}"
    );
    assert!(paused.repo.state_path().exists());
    assert_eq!(paused.repo.tips(), tips);

    paused.abort();
    paused.assert_restored();
}

#[test]
fn legacy_sync_linear_dirty_journal_v3_restores_its_set_aside() {
    for abort in [false, true] {
        let paused = paused_with_legacy(paused_sync_linear_dirty, "sync_linear_dirty@journal-v3");
        if abort {
            paused.abort();
            paused.assert_restored();
        } else {
            paused.continue_to_completion();
            assert_eq!(paused.repo.current_branch(), "feature-b");
            assert!(paused.repo.is_ancestor("main", "feature-a"));
            assert!(paused.repo.is_ancestor("feature-a", "feature-b"));
        }
        assert_eq!(
            paused.repo.porcelain(),
            " M b.txt\n?? untracked.txt\n",
            "abort: {abort}"
        );
        assert_eq!(paused.repo.stash_list(), "");
    }
}

#[test]
fn legacy_sync_upstream_journal_v3_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_upstream,
        "sync_upstream@journal-v3",
        &sync_upstream_status(),
    );
}

#[test]
fn legacy_sync_upstream_journal_v3_continues() {
    assert_legacy_replay_continues(
        paused_sync_upstream,
        "sync_upstream@journal-v3",
        "main",
        &[("origin/main", "main")],
    );
}

#[test]
fn legacy_sync_tree_journal_v3_aborts() {
    assert_legacy_replay_aborts(
        paused_sync_tree,
        "sync_tree@journal-v3",
        &sync_tree_status(),
    );
}

#[test]
fn legacy_sync_tree_journal_v3_continues() {
    let paused = assert_legacy_replay_continues(
        paused_sync_tree,
        "sync_tree@journal-v3",
        "feature-a",
        &[
            ("main", "feature-a"),
            ("feature-a", "feature-b"),
            ("feature-a", "feature-c"),
        ],
    );
    assert_eq!(paused.repo.rev("feature-a^"), paused.repo.rev("main"));
    assert_eq!(paused.repo.rev("feature-b^"), paused.repo.rev("feature-a"));
    assert_eq!(paused.repo.rev("feature-c^"), paused.repo.rev("feature-a"));
}

#[test]
fn legacy_commit_restack_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_restack,
        "commit_restack@journal-v3",
        &format!("Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        COMMIT_RESTACK_ABORTED,
    );
}

/// What `kin continue` leaves behind for a paused commit or absorb, besides
/// no journal, no rebase, no absorb anchors and an empty stash list: the
/// branch checked out, each `(ancestor, branch)` pair stacked in order,
/// `git status --porcelain`, and no `fixup!` or `squash!` commit left
/// between `main` and `folded`, if given.
struct ContinueOutcome<'a> {
    ends_on: &'a str,
    stacked: &'a [(&'a str, &'a str)],
    porcelain: &'a str,
    folded: Option<&'a str>,
}

fn assert_legacy_commit_continues(
    scenario: fn() -> Paused,
    fixture: &str,
    expected: ContinueOutcome,
) {
    let paused = paused_with_legacy(scenario, fixture);
    paused.continue_to_completion();
    assert_eq!(paused.repo.current_branch(), expected.ends_on);
    for (ancestor, branch) in expected.stacked {
        assert!(
            paused.repo.is_ancestor(ancestor, branch),
            "{branch} is not stacked on {ancestor}"
        );
    }
    assert_eq!(paused.repo.porcelain(), expected.porcelain);
    if let Some(branch) = expected.folded {
        let subjects = paused
            .repo
            .git_stdout(&["log", "--format=%s", &format!("main..{branch}")]);
        assert!(
            !subjects.contains("fixup!") && !subjects.contains("squash!"),
            "{subjects}"
        );
    }
    assert_eq!(paused.repo.stash_list(), "");
    let refs = paused.repo.kindra_refs();
    assert!(!refs.contains("refs/kindra/absorb/"), "{refs}");
}

#[test]
fn legacy_commit_restack_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_restack,
        "commit_restack@journal-v3",
        ContinueOutcome {
            ends_on: "feature-a",
            stacked: &[("feature-a", "feature-b")],
            porcelain: "",
            folded: None,
        },
    );
}

/// Version 3 recorded the ancestor the commit moves onto as the target.
#[test]
fn legacy_commit_on_ancestor_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_on_ancestor,
        "commit_on_ancestor@journal-v3",
        &format!("Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-b",
            porcelain: "M  shared.txt\n?? unstaged.txt\n",
        },
    );
}

#[test]
fn legacy_commit_on_ancestor_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_on_ancestor,
        "commit_on_ancestor@journal-v3",
        ContinueOutcome {
            ends_on: "feature-b",
            stacked: &[("main", "feature-a"), ("feature-a", "feature-b")],
            porcelain: "?? unstaged.txt\n",
            folded: None,
        },
    );
}

const COMMIT_FIXUP_ABORTED: AbortOutcome = AbortOutcome {
    tips: &[
        ("feature-a", "<feature-a@before>"),
        ("main", "<main@before>"),
    ],
    ends_on: "feature-a",
    porcelain: "M  shared.txt\n?? unstaged.txt\n",
};

const COMMIT_FIXUP_CONTINUED: ContinueOutcome = ContinueOutcome {
    ends_on: "feature-a",
    stacked: &[("main", "feature-a")],
    porcelain: "?? unstaged.txt\n",
    folded: Some("feature-a"),
};

#[test]
fn legacy_commit_fixup_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_fixup,
        "commit_fixup@journal-v3",
        &format!("Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"),
        COMMIT_FIXUP_ABORTED,
    );
}

#[test]
fn legacy_commit_fixup_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_fixup,
        "commit_fixup@journal-v3",
        COMMIT_FIXUP_CONTINUED,
    );
}

#[test]
fn legacy_commit_fixup_autostash_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_fixup_autostash,
        "commit_fixup_autostash@journal-v3",
        &format!("Commit in progress on feature-a\nRemaining branches: \n{NATIVE_REBASE}"),
        COMMIT_FIXUP_ABORTED,
    );
}

#[test]
fn legacy_commit_fixup_autostash_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_fixup_autostash,
        "commit_fixup_autostash@journal-v3",
        COMMIT_FIXUP_CONTINUED,
    );
}

#[test]
fn legacy_commit_fixup_dependents_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_fixup_dependents,
        "commit_fixup_dependents@journal-v3",
        &format!("Commit in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  shared.txt\n?? unstaged.txt\n",
        },
    );
}

#[test]
fn legacy_commit_fixup_dependents_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_fixup_dependents,
        "commit_fixup_dependents@journal-v3",
        ContinueOutcome {
            ends_on: "feature-a",
            stacked: &[("main", "feature-a"), ("feature-a", "feature-b")],
            porcelain: "?? unstaged.txt\n",
            folded: Some("feature-a"),
        },
    );
}

#[test]
fn legacy_commit_insert_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_insert,
        "commit_insert@journal-v3",
        &format!("Commit in progress on inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("inserted", "<inserted@paused>"),
                ("main", "<main@before>"),
            ],
            ends_on: "inserted",
            porcelain: "",
        },
    );
}

#[test]
fn legacy_commit_insert_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_commit_insert,
        "commit_insert@journal-v3",
        ContinueOutcome {
            ends_on: "inserted",
            stacked: &[("feature-a", "inserted"), ("inserted", "feature-b")],
            porcelain: "",
            folded: None,
        },
    );
}

fn absorb_v3_status() -> String {
    format!("Absorb in progress on feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}")
}

#[test]
fn legacy_absorb_journal_v3_aborts() {
    assert_legacy_commit_aborts(
        paused_absorb,
        "absorb@journal-v3",
        &absorb_v3_status(),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  code.txt\n?? untracked.txt\n",
        },
    );
}

#[test]
fn legacy_absorb_journal_v3_continues() {
    assert_legacy_commit_continues(
        paused_absorb,
        "absorb@journal-v3",
        ContinueOutcome {
            ends_on: "feature-a",
            stacked: &[("main", "feature-a"), ("feature-a", "feature-b")],
            porcelain: "?? untracked.txt\n",
            folded: Some("feature-a"),
        },
    );
}

#[test]
fn legacy_absorb_unnamed_fork_journal_v3_aborts_and_deletes_its_anchors() {
    assert_legacy_commit_aborts(
        paused_absorb_unnamed_fork,
        "absorb_unnamed_fork@journal-v3",
        &absorb_v3_status(),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("main", "<main@before>"),
            ],
            ends_on: "feature-a",
            porcelain: "M  code.txt\n",
        },
    );
}

#[test]
fn legacy_absorb_unnamed_fork_journal_v3_continues_and_deletes_its_anchors() {
    assert_legacy_commit_continues(
        paused_absorb_unnamed_fork,
        "absorb_unnamed_fork@journal-v3",
        ContinueOutcome {
            ends_on: "feature-a",
            stacked: &[("main", "feature-a"), ("main", "feature-b")],
            porcelain: "",
            folded: Some("feature-a"),
        },
    );
}

/// Version 5 only adds `created_branch`, which only an insert records, and an
/// insert's rollback that goes with it. Every other version 4 journal is
/// today's but for its version, so the tests of today's journals cover it.
#[test]
fn legacy_journal_v4_differs_from_today_only_for_an_insert() {
    for case in [
        "absorb",
        "absorb_unnamed_fork",
        "commit_fixup",
        "commit_fixup_autostash",
        "commit_fixup_dependents",
        "commit_on_ancestor",
        "commit_restack",
        "move",
        "reorder",
        "restack",
        "sync_linear",
        "sync_linear_dirty",
        "sync_tree",
        "sync_upstream",
    ] {
        let read = |kind: &str, name: &str| -> Value {
            let path = fixture_path(kind, name);
            serde_json::from_str(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
        };
        let mut legacy = read("legacy", &format!("{case}@journal-v4"));
        assert_eq!(legacy["version"], Value::from(4), "{case}");
        legacy["version"] = Value::from(5);
        assert_eq!(
            canonical(&legacy),
            canonical(&read("golden", case)),
            "{case}"
        );
    }
}

/// A version 4 insert recorded neither the branch it was created from nor
/// that it created the new one, so `kin abort` undoes only the restack, as it
/// did then: the new branch keeps the commit and is checked out.
#[test]
fn legacy_commit_insert_journal_v4_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_insert,
        "commit_insert@journal-v4",
        &format!("Commit in progress on inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"),
        AbortOutcome {
            tips: &[
                ("feature-a", "<feature-a@before>"),
                ("feature-b", "<feature-b@before>"),
                ("inserted", "<inserted@paused>"),
                ("main", "<main@before>"),
            ],
            ends_on: "inserted",
            porcelain: "",
        },
    );
}

#[test]
fn legacy_commit_insert_journal_v4_continues() {
    assert_legacy_commit_continues(
        paused_commit_insert,
        "commit_insert@journal-v4",
        ContinueOutcome {
            ends_on: "inserted",
            stacked: &[("feature-a", "inserted"), ("inserted", "feature-b")],
            porcelain: "",
            folded: None,
        },
    );
}

// ---------------------------------------------------------------------------
// Steps and the cursor
// ---------------------------------------------------------------------------

/// Stage every conflicted file as `resolved`.
fn resolve_conflicts(repo: &Repo) {
    let unmerged = repo.git_stdout(&["diff", "--name-only", "--diff-filter=U"]);
    assert!(!unmerged.is_empty(), "nothing to resolve");
    for file in unmerged.lines() {
        repo.stage(file, "resolved\n");
    }
}

/// Finish the stopped rebase with Git alone, resolving each conflict with
/// `resolved`, as a user who never runs `kin continue` for it would.
fn finish_rebase_with_git(repo: &Repo) {
    for _ in 0..8 {
        resolve_conflicts(repo);
        let output = git_command(repo.path())
            .args(["rebase", "--continue"])
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap();
        if !repo.rebase_in_progress() {
            assert!(output.status.success(), "{}", describe(&output));
            return;
        }
    }
    panic!("the rebase did not finish");
}

/// `main <- feature-a <- {feature-b, feature-c}`, where feature-a adds
/// `a.txt`, feature-b changes `shared.txt` and feature-c adds `c.txt`.
fn forked_stack() -> Repo {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("a.txt", "a\n", "a");
    repo.branch("feature-b");
    repo.commit("shared.txt", "b\n", "b");
    repo.switch("feature-a");
    repo.branch("feature-c");
    repo.commit("c.txt", "c\n", "c");
    repo.switch("feature-a");
    repo
}

/// `kin move --onto other` of [`forked_stack`] from feature-a, where `other`
/// changes the line feature-b changes: the replay of feature-a completes, the
/// second one, of feature-b, stops, and feature-c's is still to come.
fn paused_move_at_a_middle_step() -> Paused {
    let repo = forked_stack();
    repo.switch("main");
    repo.branch("other");
    repo.commit("shared.txt", "other\n", "other");
    repo.switch("feature-a");
    Paused::start(repo, &["move", "--onto", "other"])
}

/// `kin restack` from feature-a after amending it, in `main <- feature-a <-
/// feature-b <- feature-c <- feature-d`, where feature-c changes the line the
/// amend rewrote: the replay of feature-b completes, the second one, of
/// feature-c, stops, and feature-d's is still to come.
fn paused_restack_at_a_middle_step() -> Paused {
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a1\n", "a");
    repo.branch("feature-b");
    repo.commit("b.txt", "b\n", "b");
    repo.branch("feature-c");
    repo.commit("shared.txt", "c\n", "c");
    repo.branch("feature-d");
    repo.commit("d.txt", "d\n", "d");
    repo.switch("feature-a");
    repo.stage("shared.txt", "a2\n");
    repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    Paused::start(repo, &["restack"])
}

/// An operation paused on the replay of `stopped`, after the replay of `done`
/// and before the replay of `later`. Once it completes, each of them sits
/// directly on the branch named with it.
struct MiddleStep {
    scenario: fn() -> Paused,
    ends_on: &'static str,
    done: (&'static str, &'static str),
    stopped: (&'static str, &'static str),
    later: (&'static str, &'static str),
}

const MOVE_AT_A_MIDDLE_STEP: MiddleStep = MiddleStep {
    scenario: paused_move_at_a_middle_step,
    ends_on: "feature-a",
    done: ("feature-a", "other"),
    stopped: ("feature-b", "feature-a"),
    later: ("feature-c", "feature-a"),
};

const RESTACK_AT_A_MIDDLE_STEP: MiddleStep = MiddleStep {
    scenario: paused_restack_at_a_middle_step,
    ends_on: "feature-a",
    done: ("feature-b", "feature-a"),
    stopped: ("feature-c", "feature-b"),
    later: ("feature-d", "feature-c"),
};

/// [`paused_sync_tree`]: feature-a is replayed onto main, feature-b stops,
/// feature-c follows.
const SYNC_TREE_AT_A_MIDDLE_STEP: MiddleStep = MiddleStep {
    scenario: paused_sync_tree,
    ends_on: "feature-a",
    done: ("feature-a", "main"),
    stopped: ("feature-b", "feature-a"),
    later: ("feature-c", "feature-a"),
};

impl MiddleStep {
    /// Pause the operation and check the journal: the replays of `done`,
    /// `stopped` and `later` in that order, then completion, with the cursor
    /// on the started replay of `stopped`.
    fn pause(&self) -> Paused {
        let paused = (self.scenario)();
        let journal = &paused.repo.state_json()["journal"];
        let replays: Vec<_> = journal["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["kind"] == "Replay")
            .map(|step| step["branch"].as_str().unwrap().to_string())
            .collect();
        let at = replays.iter().position(|b| b == self.stopped.0).unwrap();
        assert_eq!(
            replays[at - 1..=at + 1],
            [self.done.0, self.stopped.0, self.later.0],
            "{journal}"
        );
        assert_eq!(journal["cursor"]["started"], Value::Bool(true), "{journal}");
        assert_eq!(
            journal["steps"][journal["cursor"]["step"].as_u64().unwrap() as usize]["branch"],
            Value::from(self.stopped.0),
            "{journal}"
        );
        assert!(paused.repo.is_ancestor(self.done.1, self.done.0));
        paused
    }

    /// Check the operation completed with every branch in place, `done`
    /// still at `done_tip`.
    fn assert_completed(&self, paused: &Paused, done_tip: &str) {
        assert!(!paused.repo.state_path().exists());
        assert!(!paused.repo.rebase_in_progress());
        assert_eq!(paused.repo.current_branch(), self.ends_on);
        assert_eq!(
            paused.repo.rev(self.done.0),
            done_tip,
            "a done step ran again"
        );
        for (branch, parent) in [self.done, self.stopped, self.later] {
            assert_eq!(
                paused.repo.rev(&format!("{branch}^")),
                paused.repo.rev(parent),
                "{branch} is not directly on {parent}"
            );
        }
        assert_eq!(paused.repo.porcelain(), "");
        assert_eq!(paused.repo.stash_list(), "");
    }

    /// Assert `kin continue` succeeded without replaying `done` again, and
    /// with `later` replayed.
    fn assert_continued(&self, output: &Output) {
        assert!(output.status.success(), "{}", describe(output));
        let out = stdout(output);
        assert!(
            !out.contains(&format!("Rebasing {}...", self.done.0)),
            "{out}"
        );
        assert!(
            out.contains(&format!("Rebasing {}...", self.later.0)),
            "{out}"
        );
    }
}

/// Continuing a paused operation finishes the stopped step, runs the steps
/// after it, and leaves the ones before it alone.
fn assert_continues_from_a_middle_step(step: MiddleStep) {
    let paused = step.pause();
    let done_tip = paused.repo.rev(step.done.0);
    resolve_conflicts(&paused.repo);
    let output = paused.repo.kin(&["continue"]);
    step.assert_continued(&output);
    assert!(
        !stdout(&output).contains(&format!("Rebasing {}...", step.stopped.0)),
        "the stopped replay was started again\n{}",
        describe(&output)
    );
    step.assert_completed(&paused, &done_tip);
}

/// Aborting it puts back every branch, including those the steps before the
/// stopped one rewrote.
fn assert_aborts_from_a_middle_step(step: MiddleStep) {
    let paused = step.pause();
    let output = paused.abort();
    assert!(
        stdout(&output).contains("Operation aborted (state cleared)."),
        "{}",
        describe(&output)
    );
    paused.assert_restored();
    assert_eq!(paused.repo.porcelain(), "");
}

/// Finishing the stopped rebase with Git first leaves `kin continue` to take
/// that step as done, from the graph, and to run the rest.
fn assert_continues_after_git_rebase_continue(step: MiddleStep) {
    let paused = step.pause();
    let done_tip = paused.repo.rev(step.done.0);
    finish_rebase_with_git(&paused.repo);
    assert!(!paused.repo.rebase_in_progress());
    let stopped_tip = paused.repo.rev(step.stopped.0);

    let output = paused.repo.kin(&["continue"]);
    step.assert_continued(&output);
    assert!(
        stdout(&output).contains(&format!("Branch {} already rebased.", step.stopped.0)),
        "{}",
        describe(&output)
    );
    assert_eq!(paused.repo.rev(step.stopped.0), stopped_tip);
    step.assert_completed(&paused, &done_tip);
}

/// Giving the stopped rebase up with Git leaves its step started: `kin
/// continue` replays that branch again, which stops on the same conflict,
/// and the operation then completes as usual.
fn assert_continues_after_git_rebase_abort(step: MiddleStep) {
    let paused = step.pause();
    let done_tip = paused.repo.rev(step.done.0);
    let cursor = paused.repo.state_json()["journal"]["cursor"].clone();
    paused.repo.git(&["rebase", "--abort"]);

    let output = paused.repo.kin(&["continue"]);
    assert!(!output.status.success(), "{}", describe(&output));
    let out = stdout(&output);
    assert!(
        out.contains(&format!("Rebasing {}...", step.stopped.0)),
        "{out}"
    );
    assert!(
        !out.contains(&format!("Rebasing {}...", step.done.0)),
        "{out}"
    );
    assert!(paused.repo.rebase_in_progress());
    assert_eq!(paused.repo.state_json()["journal"]["cursor"], cursor);

    paused.continue_to_completion();
    step.assert_completed(&paused, &done_tip);
}

/// Aborting after giving the stopped rebase up with Git still puts every
/// branch back.
fn assert_aborts_after_git_rebase_abort(step: MiddleStep) {
    let paused = step.pause();
    paused.repo.git(&["rebase", "--abort"]);
    paused.abort();
    paused.assert_restored();
    assert_eq!(paused.repo.porcelain(), "");
}

#[test]
fn move_paused_at_a_middle_step_continues_from_it() {
    assert_continues_from_a_middle_step(MOVE_AT_A_MIDDLE_STEP);
}

#[test]
fn move_paused_at_a_middle_step_aborts() {
    assert_aborts_from_a_middle_step(MOVE_AT_A_MIDDLE_STEP);
}

#[test]
fn move_paused_at_a_middle_step_continues_after_git_rebase_continue() {
    assert_continues_after_git_rebase_continue(MOVE_AT_A_MIDDLE_STEP);
}

#[test]
fn move_paused_at_a_middle_step_continues_after_git_rebase_abort() {
    assert_continues_after_git_rebase_abort(MOVE_AT_A_MIDDLE_STEP);
}

#[test]
fn move_paused_at_a_middle_step_aborts_after_git_rebase_abort() {
    assert_aborts_after_git_rebase_abort(MOVE_AT_A_MIDDLE_STEP);
}

#[test]
fn restack_paused_at_a_middle_step_continues_from_it() {
    assert_continues_from_a_middle_step(RESTACK_AT_A_MIDDLE_STEP);
}

#[test]
fn restack_paused_at_a_middle_step_aborts() {
    assert_aborts_from_a_middle_step(RESTACK_AT_A_MIDDLE_STEP);
}

#[test]
fn restack_paused_at_a_middle_step_continues_after_git_rebase_continue() {
    assert_continues_after_git_rebase_continue(RESTACK_AT_A_MIDDLE_STEP);
}

#[test]
fn restack_paused_at_a_middle_step_continues_after_git_rebase_abort() {
    assert_continues_after_git_rebase_abort(RESTACK_AT_A_MIDDLE_STEP);
}

#[test]
fn restack_paused_at_a_middle_step_aborts_after_git_rebase_abort() {
    assert_aborts_after_git_rebase_abort(RESTACK_AT_A_MIDDLE_STEP);
}

#[test]
fn sync_tree_paused_at_a_middle_step_continues_from_it() {
    assert_continues_from_a_middle_step(SYNC_TREE_AT_A_MIDDLE_STEP);
}

#[test]
fn sync_tree_paused_at_a_middle_step_aborts() {
    assert_aborts_from_a_middle_step(SYNC_TREE_AT_A_MIDDLE_STEP);
}

#[test]
fn sync_tree_paused_at_a_middle_step_continues_after_git_rebase_continue() {
    assert_continues_after_git_rebase_continue(SYNC_TREE_AT_A_MIDDLE_STEP);
}

#[test]
fn sync_tree_paused_at_a_middle_step_continues_after_git_rebase_abort() {
    assert_continues_after_git_rebase_abort(SYNC_TREE_AT_A_MIDDLE_STEP);
}

#[test]
fn sync_tree_paused_at_a_middle_step_aborts_after_git_rebase_abort() {
    assert_aborts_after_git_rebase_abort(SYNC_TREE_AT_A_MIDDLE_STEP);
}

/// A fold is a step of its own, ahead of the replays of the dependents. Once
/// the user finishes it with Git, `kin continue` takes it as done and runs
/// the replay after it.
#[test]
fn commit_fold_finished_with_git_rebase_continue_then_continues() {
    let paused = paused_commit_fixup_dependents();
    let journal = &paused.repo.state_json()["journal"];
    assert_eq!(journal["steps"][0]["kind"], "Autosquash", "{journal}");
    assert_eq!(journal["steps"][1]["kind"], "Replay", "{journal}");
    assert_eq!(
        journal["cursor"],
        serde_json::json!({"step": 0, "started": true})
    );

    finish_rebase_with_git(&paused.repo);
    let folded_tip = paused.repo.rev("feature-a");

    let output = paused.repo.kin(&["continue"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        stdout(&output).contains("Rebasing feature-b..."),
        "{}",
        describe(&output)
    );
    assert_eq!(paused.repo.rev("feature-a"), folded_tip);
    assert_eq!(paused.repo.rev("feature-b^"), folded_tip);
    let subjects = paused
        .repo
        .git_stdout(&["log", "--format=%s", "main..feature-b"]);
    assert!(!subjects.contains("fixup!"), "{subjects}");
    assert_eq!(paused.repo.current_branch(), "feature-a");
    assert_eq!(paused.repo.porcelain(), "?? unstaged.txt\n");
    assert_eq!(paused.repo.stash_list(), "");
    assert!(!paused.repo.state_path().exists());
}

/// A fold given up with Git is never started again: as before steps were
/// recorded, `kin continue` replays the dependents onto the unfolded branch,
/// with the fixup commit still in it, and restores what was set aside;
/// `kin abort` puts everything back instead.
#[test]
fn commit_fold_given_up_with_git_rebase_abort_continues_or_aborts() {
    for abort in [false, true] {
        let paused = paused_commit_fixup_dependents();
        paused.repo.git(&["rebase", "--abort"]);
        if abort {
            paused.abort();
            paused.assert_restored();
            assert_eq!(paused.repo.porcelain(), "M  shared.txt\n?? unstaged.txt\n");
        } else {
            let unfolded_tip = paused.repo.rev("feature-a");
            let output = paused.repo.kin(&["continue"]);
            assert!(output.status.success(), "{}", describe(&output));
            assert_eq!(paused.repo.rev("feature-a"), unfolded_tip);
            assert_eq!(paused.repo.rev("feature-b^"), unfolded_tip);
            let subjects = paused
                .repo
                .git_stdout(&["log", "--format=%s", "main..feature-a"]);
            assert!(subjects.contains("fixup! a1"), "{subjects}");
            assert_eq!(paused.repo.current_branch(), "feature-a");
            assert_eq!(paused.repo.porcelain(), "?? unstaged.txt\n");
            assert!(!paused.repo.state_path().exists());
        }
        assert_eq!(paused.repo.stash_list(), "", "abort: {abort}");
    }
}

// ---------------------------------------------------------------------------
// Checkout hydration
// ---------------------------------------------------------------------------

/// Named checkout keeps its own journal, `kindra_checkout_state.json`: the
/// branches it creates, the checkout that finishes it and a cursor, inside
/// its own `{"version": N, "journal": {...}}` envelope. Kindra 1.1 saved it
/// flat, with a `completed` flag on every branch (`checkout@1.1.0`).
#[cfg(unix)]
mod hydration {
    use super::*;

    const STATUS: &str =
        "Checkout hydration in progress. Run 'kin continue' to resume or 'kin abort' to stop.\n";

    /// A named checkout of feature-c that stopped because feature-c could not
    /// be created.
    struct PausedHydration {
        repo: Repo,
        labels: Labels,
    }

    impl PausedHydration {
        /// `main <- feature-a <- feature-b <- feature-c`, one PR each, pushed
        /// to a remote that identifies as `github.com/test/project`. Only
        /// main and feature-a are local, and feature-c's ref is locked, so
        /// `kin co feature-c` skips feature-a, creates feature-b and stops.
        /// The commits it plans are named `<branch@planned>`.
        fn start() -> Self {
            let mut repo = Repo::new();
            for branch in ["feature-a", "feature-b", "feature-c"] {
                repo.branch(branch);
                repo.commit(&format!("{branch}.txt"), "x\n", branch);
            }
            let remote = TempDir::new().unwrap();
            run_ok("git", &["init", "-q", "--bare"], remote.path());
            let url = "https://github.com/test/project.git";
            repo.git(&[
                "config",
                &format!("url.{}.insteadOf", remote.path().display()),
                url,
            ]);
            repo.git(&["remote", "add", "origin", url]);
            repo.git(&[
                "push",
                "-q",
                "origin",
                "main",
                "feature-a",
                "feature-b",
                "feature-c",
            ]);
            repo._remote = Some(remote);
            let mut labels = Labels::default();
            labels.tips(&repo, "planned");
            repo.switch("main");
            repo.git(&["branch", "-q", "-D", "feature-b", "feature-c"]);

            // gh is mocked from the Git directory, not the working tree.
            let git_dir = git2::Repository::open(repo.path())
                .unwrap()
                .path()
                .to_path_buf();
            let bin = git_dir.join("mock-bin");
            fs::create_dir(&bin).unwrap();
            let gh = bin.join("gh");
            fs::write(
                &gh,
                r#"#!/bin/sh
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then printf '{"url":"https://github.com/test/project"}'; exit 0; fi
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s' '[{"number":1,"headRefName":"feature-a","baseRefName":"main"},{"number":2,"headRefName":"feature-b","baseRefName":"feature-a"},{"number":3,"headRefName":"feature-c","baseRefName":"feature-b"}]'
  exit 0
fi
exit 1
"#,
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
            let mut path = vec![bin];
            path.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));

            let lock = git_dir.join("refs/heads/feature-c.lock");
            fs::write(&lock, "locked").unwrap();
            let output = kin_cmd()
                .args(["co", "feature-c"])
                .current_dir(repo.path())
                .env("PATH", std::env::join_paths(path).unwrap())
                .output()
                .unwrap();
            assert!(!output.status.success(), "{}", describe(&output));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("Checkout hydration stopped"),
                "{}",
                describe(&output)
            );
            fs::remove_file(lock).unwrap();
            let paused = Self { repo, labels };
            assert!(paused.state_path().exists());
            paused
        }

        fn state_path(&self) -> PathBuf {
            common::state_file(self.repo.path(), StateFile::Checkout)
        }

        fn journal(&mut self) -> Value {
            let raw: Value =
                serde_json::from_str(&fs::read_to_string(self.state_path()).unwrap()).unwrap();
            self.labels.normalise(&self.repo, &raw)
        }

        fn install_legacy(&mut self, name: &str) {
            self.journal();
            install_legacy(&self.labels, name, &self.state_path());
        }

        fn assert_status(&self) {
            let output = self.repo.kin(&["status"]);
            assert!(output.status.success(), "{}", describe(&output));
            assert_eq!(stdout(&output), STATUS);
        }

        /// `kin continue` creates what is left at the planned commits and
        /// checks out feature-c. The remote has moved on since, and feature-b,
        /// already created, has been moved by the user: neither changes what
        /// continue does.
        fn assert_continues(&mut self) {
            self.repo
                .git(&["push", "-q", "-f", "origin", "main:feature-c"]);
            self.repo.git(&["fetch", "-q", "origin"]);
            self.repo.git(&["branch", "-f", "feature-b", "feature-a"]);
            let output = self.repo.kin(&["continue"]);
            assert!(output.status.success(), "{}", describe(&output));
            assert_eq!(self.repo.current_branch(), "feature-c");
            assert_eq!(
                self.labels
                    .normalise_str(&self.repo, &self.repo.rev("feature-c")),
                "<feature-c@planned>"
            );
            assert_eq!(
                self.repo.rev("feature-c@{upstream}"),
                self.repo.rev("origin/feature-c")
            );
            assert_eq!(self.repo.rev("feature-b"), self.repo.rev("feature-a"));
            assert!(!self.state_path().exists());
        }

        /// `kin abort` forgets the hydration and keeps feature-b, which it
        /// created.
        fn assert_aborts(&mut self) {
            let output = self.repo.kin(&["abort"]);
            assert!(output.status.success(), "{}", describe(&output));
            assert!(
                stdout(&output).contains("Already-created branches were retained"),
                "{}",
                describe(&output)
            );
            assert_eq!(self.repo.current_branch(), "main");
            let tips = self
                .repo
                .tips()
                .into_iter()
                .map(|(branch, oid)| (branch, self.labels.normalise_str(&self.repo, &oid)))
                .collect::<Vec<_>>();
            assert_eq!(
                tips,
                [
                    ("feature-a".to_string(), "<feature-a@planned>".to_string()),
                    ("feature-b".to_string(), "<feature-b@planned>".to_string()),
                    ("main".to_string(), "<main@planned>".to_string()),
                ]
            );
            assert!(!self.state_path().exists());
        }
    }

    #[test]
    fn golden_checkout() {
        let mut paused = PausedHydration::start();
        assert_golden("checkout", &paused.journal());
    }

    #[test]
    fn checkout_continues() {
        let mut paused = PausedHydration::start();
        paused.assert_status();
        paused.assert_continues();
    }

    #[test]
    fn checkout_aborts() {
        PausedHydration::start().assert_aborts();
    }

    #[test]
    fn legacy_checkout_1_1_0_continues() {
        let mut paused = PausedHydration::start();
        paused.install_legacy("checkout@1.1.0");
        paused.assert_status();
        paused.assert_continues();
    }

    #[test]
    fn legacy_checkout_1_1_0_aborts() {
        let mut paused = PausedHydration::start();
        paused.install_legacy("checkout@1.1.0");
        paused.assert_status();
        paused.assert_aborts();
    }

    /// Kindra 1.1 could stop after creating feature-b but before recording
    /// it. Continue accepts the branch it finds at the planned commit.
    #[test]
    fn legacy_checkout_uncheckpointed_1_1_0_continues() {
        let mut paused = PausedHydration::start();
        paused.install_legacy("checkout_uncheckpointed@1.1.0");
        let output = paused.repo.kin(&["continue"]);
        assert!(output.status.success(), "{}", describe(&output));
        assert_eq!(paused.repo.current_branch(), "feature-c");
        assert_eq!(
            paused
                .labels
                .normalise_str(&paused.repo, &paused.repo.rev("feature-b")),
            "<feature-b@planned>"
        );
        assert!(!paused.state_path().exists());
    }

    /// A journal from a newer Kindra is refused by continue, with advice, and
    /// left as it is; abort, which never reads it, still stops hydration.
    #[test]
    fn a_newer_checkout_journal_is_refused_but_aborts() {
        let paused = PausedHydration::start();
        let newer = r#"{"version":2,"journal":{}}"#;
        fs::write(paused.state_path(), newer).unwrap();
        let output = paused.repo.kin(&["continue"]);
        assert!(!output.status.success(), "{}", describe(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("newer version of kin"), "{stderr}");
        assert_eq!(fs::read_to_string(paused.state_path()).unwrap(), newer);
        let output = paused.repo.kin(&["abort"]);
        assert!(output.status.success(), "{}", describe(&output));
        assert!(!paused.state_path().exists());
    }
}
