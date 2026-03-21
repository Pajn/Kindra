use criterion::{Criterion, criterion_group, criterion_main};
use git2::{Oid, Repository, Signature, build::CheckoutBuilder};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

const START_BRANCH: &str = "feature-b";

#[derive(Clone, Copy)]
struct Scenario {
    id: &'static str,
    main_commits: u32,
    noise_branches: u32,
}

const SCENARIOS: [Scenario; 2] = [
    Scenario {
        id: "5000_main_10000_noise",
        main_commits: 5_000,
        noise_branches: 10_000,
    },
    Scenario {
        id: "50000_main_1000_noise",
        main_commits: 50_000,
        noise_branches: 1_000,
    },
];

#[derive(Clone, Copy)]
struct CommandCase {
    id: &'static str,
    args: &'static [&'static str],
}

const COMMANDS: [CommandCase; 3] = [
    CommandCase {
        id: "checkout_top",
        args: &["checkout", "top"],
    },
    CommandCase {
        id: "co_up",
        args: &["co", "up"],
    },
    CommandCase {
        id: "co_down",
        args: &["co", "down"],
    },
];

struct BenchRepo {
    _dir: TempDir,
    path: PathBuf,
}

impl BenchRepo {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn next_signature(timestamp: &mut i64) -> Signature<'static> {
    let sig = Signature::new("bench", "bench@test.com", &git2::Time::new(*timestamp, 0))
        .expect("failed to create signature");
    *timestamp += 1;
    sig
}

fn append_empty_commits(repo: &Repository, refname: &str, n: u32, timestamp: &mut i64) -> Oid {
    let mut parent_oid = repo.refname_to_id(refname).ok();
    let mut last = parent_oid.unwrap_or_else(Oid::zero);

    for i in 0..n {
        let sig = next_signature(timestamp);
        let tree_id = if let Some(parent) = parent_oid {
            repo.find_commit(parent)
                .expect("failed to load parent commit")
                .tree_id()
        } else {
            repo.treebuilder(None)
                .expect("failed to create treebuilder")
                .write()
                .expect("failed to write empty tree")
        };
        let tree = repo.find_tree(tree_id).expect("failed to load commit tree");
        let parent_commits = parent_oid
            .map(|parent| {
                vec![
                    repo.find_commit(parent)
                        .expect("failed to load parent commit"),
                ]
            })
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit<'_>> = parent_commits.iter().collect();

        last = repo
            .commit(
                Some(refname),
                &sig,
                &sig,
                &format!("commit {i}"),
                &tree,
                &parent_refs,
            )
            .expect("failed to create commit");
        parent_oid = Some(last);
    }

    last
}

fn branch_with_empty_commits(
    repo: &Repository,
    branch_name: &str,
    base_oid: Oid,
    extra_commits: u32,
    timestamp: &mut i64,
) -> Oid {
    let refname = format!("refs/heads/{branch_name}");

    if extra_commits == 0 {
        repo.reference(
            &refname,
            base_oid,
            true,
            "benchmark: create branch at existing commit",
        )
        .expect("failed to create branch reference");
        return base_oid;
    }

    let base = repo
        .find_commit(base_oid)
        .expect("failed to load base commit");
    let base_tree = repo
        .find_tree(base.tree_id())
        .expect("failed to load base tree");
    let sig = next_signature(timestamp);

    let mut tip = repo
        .commit(
            Some(&refname),
            &sig,
            &sig,
            "branch c1",
            &base_tree,
            &[&base],
        )
        .expect("failed to create first branch commit");

    for i in 1..extra_commits {
        let sig = next_signature(timestamp);
        let parent = repo.find_commit(tip).expect("failed to load parent commit");
        let tree = repo
            .find_tree(parent.tree_id())
            .expect("failed to load parent tree");
        tip = repo
            .commit(
                Some(&refname),
                &sig,
                &sig,
                &format!("branch c{}", i + 1),
                &tree,
                &[&parent],
            )
            .expect("failed to create branch commit");
    }

    tip
}

fn create_noise_branches(repo: &Repository, oid: Oid, count: u32) {
    for i in 0..count {
        let refname = format!("refs/heads/noise-{i}");
        repo.reference(&refname, oid, true, "benchmark: noise branch")
            .expect("failed to create noise branch");
    }
}

fn checkout_branch(repo: &Repository, branch_name: &str) {
    repo.set_head(&format!("refs/heads/{branch_name}"))
        .expect("failed to set HEAD");
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))
        .expect("failed to checkout branch");
}

fn setup_repo(scenario: Scenario) -> BenchRepo {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let repo = Repository::init(dir.path()).expect("failed to init repository");
    let mut timestamp = 1_700_000_000;

    let main_tip = append_empty_commits(
        &repo,
        "refs/heads/main",
        scenario.main_commits,
        &mut timestamp,
    );
    create_noise_branches(&repo, main_tip, scenario.noise_branches);

    let feature_a = branch_with_empty_commits(&repo, "feature-a", main_tip, 1, &mut timestamp);
    let feature_b = branch_with_empty_commits(&repo, "feature-b", feature_a, 1, &mut timestamp);
    let _feature_c = branch_with_empty_commits(&repo, "feature-c", feature_b, 1, &mut timestamp);

    checkout_branch(&repo, START_BRANCH);

    BenchRepo {
        path: dir.path().to_path_buf(),
        _dir: dir,
    }
}

fn reset_to_start_branch(repo_path: &Path) {
    let repo = Repository::open(repo_path).expect("failed to open benchmark repository");
    checkout_branch(&repo, START_BRANCH);
}

fn run_checkout_command(gits_bin: &Path, repo_path: &Path, args: &[&str]) {
    let output = Command::new(gits_bin)
        .args(args)
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to execute kin checkout command");

    assert!(
        output.status.success(),
        "kin {:?} failed\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stderr),
    );
}

fn bench_checkout_navigation(c: &mut Criterion) {
    let gits_bin = assert_cmd::cargo::cargo_bin!("kin");

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);
        for command in COMMANDS {
            reset_to_start_branch(repo.path());
            run_checkout_command(gits_bin, repo.path(), command.args);
            let bench_name = format!("{}/{}", command.id, scenario.id);
            c.bench_function(&bench_name, |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        reset_to_start_branch(repo.path());
                        let start = std::time::Instant::now();
                        run_checkout_command(gits_bin, repo.path(), command.args);
                        total += start.elapsed();
                    }
                    total
                })
            });
        }
    }
}

criterion_group!(benches, bench_checkout_navigation);
criterion_main!(benches);
