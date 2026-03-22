mod common;

use common::{kin_cmd, managed_worktree_path, run_ok};
use std::fs;

fn setup_repo() -> tempfile::TempDir {
    let dir = common::setup_repo();
    run_ok("git", &["checkout", "main"], dir.path());
    dir
}

#[test]
fn worktree_list_prints_managed_worktrees() {
    let dir = setup_repo();

    let out = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "kin wt main failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let out = kin_cmd()
        .args(["wt", "review", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "kin wt review feature-a failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let out = kin_cmd()
        .args(["wt", "temp", "feature-b"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "kin wt temp feature-b failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let output = kin_cmd()
        .args(["wt", "list"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ROLE"));
    assert!(stdout.contains("BRANCH"));
    assert!(stdout.contains("STATE"));
    assert!(stdout.contains("PATH"));

    let rows = stdout
        .lines()
        .skip(1)
        .map(|line| {
            let cols = line.split_whitespace().collect::<Vec<_>>();
            assert!(cols.len() >= 4, "unexpected row format: {line}");
            (
                cols[0].to_string(),
                cols[1].to_string(),
                cols[2].to_string(),
                cols[3..].join(" "),
            )
        })
        .collect::<Vec<_>>();
    let main_path = fs::canonicalize(managed_worktree_path(dir.path(), "main"))
        .unwrap()
        .display()
        .to_string();
    let review_path = fs::canonicalize(managed_worktree_path(dir.path(), "review"))
        .unwrap()
        .display()
        .to_string();
    let temp_path = fs::canonicalize(managed_worktree_path(dir.path(), "temp/feature-b"))
        .unwrap()
        .display()
        .to_string();
    assert!(rows.iter().any(|cols| {
        cols.0 == "main" && cols.1 == "main" && cols.2 == "clean" && cols.3 == main_path
    }));
    assert!(rows.iter().any(|cols| {
        cols.0 == "review" && cols.1 == "feature-a" && cols.2 == "clean" && cols.3 == review_path
    }));
    assert!(rows.iter().any(|cols| {
        cols.0 == "temp" && cols.1 == "feature-b" && cols.2 == "clean" && cols.3 == temp_path
    }));
}
