mod common;

use common::{read_worktree_metadata, run_ok, write_repo_config};
use std::fs;
use std::process::{Child, Command, ExitStatus};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn setup_repo() -> tempfile::TempDir {
    let dir = common::setup_repo();
    run_ok("git", &["checkout", "main"], dir.path());
    dir
}

fn shell_quote(value: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn barrier_hook(barrier_dir: &std::path::Path) -> String {
    let barrier_dir = barrier_dir.display().to_string();
    if cfg!(windows) {
        format!(
            "powershell -NoProfile -Command \"$dir={dir}; New-Item -ItemType Directory -Force -Path $dir | Out-Null; $self=Join-Path $dir $env:KINDRA_WORKTREE_ROLE; New-Item -ItemType File -Force -Path $self | Out-Null; while (-not ((Test-Path (Join-Path $dir 'main')) -and (Test-Path (Join-Path $dir 'review')))) {{ Start-Sleep -Milliseconds 50 }}\"",
            dir = shell_quote(&barrier_dir),
        )
    } else {
        format!(
            "mkdir -p {dir}; touch {dir}/\"$KINDRA_WORKTREE_ROLE\"; while [ ! -f {dir}/main ] || [ ! -f {dir}/review ]; do sleep 0.05; done",
            dir = shell_quote(&barrier_dir),
        )
    }
}

fn kin_process(repo_root: &std::path::Path, args: &[&str]) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kin"));
    command
        .args(args)
        .current_dir(repo_root)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    command.spawn().unwrap()
}

fn wait_for_children(
    children: &mut [(&str, &mut Child)],
    timeout: Duration,
) -> Vec<(&'static str, ExitStatus)> {
    let deadline = Instant::now() + timeout;
    let mut statuses = Vec::with_capacity(children.len());
    let mut completed = vec![false; children.len()];

    while completed.iter().any(|done| !done) && Instant::now() < deadline {
        for (index, (name, child)) in children.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            if let Some(status) = child.try_wait().unwrap() {
                let stable_name: &'static str = match *name {
                    "main" => "main",
                    "review" => "review",
                    _ => unreachable!("unexpected child name"),
                };
                statuses.push((stable_name, status));
                completed[index] = true;
            }
        }
        if completed.iter().any(|done| !done) {
            sleep(Duration::from_millis(25));
        }
    }

    if completed.iter().any(|done| !done) {
        let mut pending = Vec::new();
        for (index, (name, child)) in children.iter_mut().enumerate() {
            if completed[index] {
                continue;
            }
            let _ = child.kill();
            let status = child.wait().unwrap();
            pending.push(format!("{name}={status}"));
        }
        panic!(
            "timed out waiting for concurrent worktree commands to exit: {}",
            pending.join(", ")
        );
    }

    statuses
}

#[test]
fn concurrent_worktree_creates_preserve_both_metadata_records() {
    let dir = setup_repo();
    let barrier_dir = dir.path().join("hook-barrier");
    fs::create_dir_all(&barrier_dir).unwrap();
    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.hooks]\non_create = [{}]\n",
            serde_json::to_string(&barrier_hook(&barrier_dir)).unwrap()
        ),
    );

    let mut main = kin_process(dir.path(), &["wt", "main"]);
    let mut review = kin_process(dir.path(), &["wt", "review", "feature-a"]);

    let statuses = wait_for_children(
        &mut [("main", &mut main), ("review", &mut review)],
        Duration::from_secs(10),
    );
    let main_status = statuses
        .iter()
        .find_map(|(name, status)| (*name == "main").then_some(*status))
        .unwrap();
    let review_status = statuses
        .iter()
        .find_map(|(name, status)| (*name == "review").then_some(*status))
        .unwrap();
    assert!(main_status.success(), "kin wt main failed");
    assert!(review_status.success(), "kin wt review failed");

    let metadata = read_worktree_metadata(dir.path());
    let records = metadata["worktrees"].as_array().unwrap();
    assert!(
        records
            .iter()
            .any(|record| record["role"] == "main" && record["branch"] == "main")
    );
    assert!(
        records
            .iter()
            .any(|record| record["role"] == "review" && record["branch"] == "feature-a")
    );
    fs::remove_dir_all(&barrier_dir).unwrap();
}
