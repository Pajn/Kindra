mod common;

use common::{kin_cmd, setup_repo};
use predicates::prelude::*;

#[test]
fn completions_command_emits_dynamic_zsh_registration() {
    let mut cmd = kin_cmd();
    cmd.arg("completions")
        .arg("zsh")
        .assert()
        .success()
        .stdout(predicate::str::contains("COMPLETE=\"zsh\""))
        .stdout(predicate::str::contains("kin -- \"${words[@]}\""));
}

#[test]
fn commit_on_completes_local_branch_names() {
    let dir = setup_repo();

    let mut cmd = kin_cmd();
    cmd.env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "3")
        .arg("--")
        .arg("kin")
        .arg("commit")
        .arg("--on")
        .arg("")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a"))
        .stdout(predicate::str::contains("feature-b"));
}

#[test]
fn commit_fixup_completes_stack_commit_shas() {
    let dir = setup_repo();

    // The tip commit (feature-b) is a valid fixup target, so its abbreviated SHA
    // must be suggested for `--fixup`.
    let head_short: String = git2::Repository::open(dir.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string()
        .chars()
        .take(12)
        .collect();

    let mut cmd = kin_cmd();
    cmd.env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "3")
        .arg("--")
        .arg("kin")
        .arg("commit")
        .arg("--fixup")
        .arg("")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(head_short));
}

#[test]
fn move_onto_completes_local_branch_names() {
    let dir = setup_repo();

    let mut cmd = kin_cmd();
    cmd.env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "3")
        .arg("--")
        .arg("kin")
        .arg("move")
        .arg("--onto")
        .arg("")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-a"))
        .stdout(predicate::str::contains("feature-b"));
}
