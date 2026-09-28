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
//!   process id and a timestamp;
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
//! - after 1.1.0: adds `abort_only`.

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
        let actual = canonical(&self.journal());
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

    /// Replace the saved journal with the legacy fixture `name`. The fixture's
    /// symbolic names resolve to the values in the journal it replaces.
    fn install_legacy(&mut self, name: &str) {
        self.journal();
        let path = fixture_path("legacy", name);
        let fixture: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|err| {
                panic!("{}: {err}", path.display());
            });
        let journal = self.labels.denormalise(&fixture);
        fs::write(
            self.repo.state_path(),
            serde_json::to_string_pretty(&journal).unwrap(),
        )
        .unwrap();
    }

    fn assert_status(&self, expected: &str) {
        let output = self.repo.kin(&["status"]);
        assert!(output.status.success(), "{}", describe(&output));
        assert_eq!(stdout(&output), expected);
    }

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
    let repo = Repo::new();
    repo.branch("feature-a");
    repo.commit("shared.txt", "a1\n", "a1");
    let a1 = repo.rev("HEAD");
    repo.commit("shared.txt", "a2\n", "a2");
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
    // Restack saves a Move journal, so status reports it as a move of the
    // restacked branch onto itself. Slice S1 is expected to change this.
    paused.assert_status(&format!(
        "Move in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
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
        "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_on_ancestor_journal_and_status() {
    let mut paused = paused_commit_on_ancestor();
    paused.assert_golden("commit_on_ancestor");
    paused.assert_status(&format!(
        "Commit in progress: feature-b onto feature-b\nRemaining branches: \n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_fixup_journal_and_status() {
    let mut paused = paused_commit_fixup();
    paused.assert_golden("commit_fixup");
    paused.assert_status(&format!(
        "Commit in progress: feature-a onto feature-a\nRemaining branches: \n{NATIVE_REBASE}"
    ));
}

#[test]
fn commit_insert_journal_and_status() {
    let mut paused = paused_commit_insert();
    paused.assert_golden("commit_insert");
    paused.assert_status(&format!(
        "Commit in progress: inserted onto inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    ));
}

#[test]
fn absorb_journal_and_status() {
    let mut paused = paused_absorb();
    paused.assert_golden("absorb");
    // Absorb saves a Commit journal, so status reports it as a commit. Slice
    // S1 is expected to change this.
    paused.assert_status(&format!(
        "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
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
        "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
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
        &format!(
            "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
        ),
    );
}

#[test]
fn legacy_commit_restack_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_restack,
        "commit_restack@1.1.0",
        &format!(
            "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
        ),
        COMMIT_RESTACK_ABORTED,
    );
}

#[test]
fn legacy_commit_on_ancestor_1_1_0_aborts() {
    assert_legacy_commit_aborts(
        paused_commit_on_ancestor,
        "commit_on_ancestor@1.1.0",
        &format!(
            "Commit in progress: feature-b onto feature-b\nRemaining branches: \n{NATIVE_REBASE}"
        ),
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
        &format!(
            "Commit in progress: feature-a onto feature-a\nRemaining branches: \n{NATIVE_REBASE}"
        ),
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
        &format!(
            "Commit in progress: inserted onto inserted\nRemaining branches: feature-b\n{NATIVE_REBASE}"
        ),
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

fn absorb_status() -> String {
    format!(
        "Commit in progress: feature-a onto feature-a\nRemaining branches: feature-b\n{NATIVE_REBASE}"
    )
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
