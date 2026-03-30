use criterion::{Criterion, criterion_group, criterion_main};
use git2::{Oid, Repository, Signature, build::CheckoutBuilder};
use kindra::stack::{
    collect_merged_local_branches, find_sync_boundary, get_stack_branches_from_merge_base,
    get_stack_tips,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

const TARGET_BRANCH: &str = "feature-c";

#[derive(Clone, Copy)]
struct Scenario {
    id: &'static str,
    main_commits: u32,
    noise_branches: u32,
}

const SCENARIOS: [Scenario; 2] = [
    Scenario {
        id: "45000_main_100_noise",
        main_commits: 45_000,
        noise_branches: 100,
    },
    Scenario {
        id: "45000_main_500_noise",
        main_commits: 45_000,
        noise_branches: 500,
    },
];

#[derive(Clone, Copy)]
struct RepresentativeScenario {
    id: &'static str,
    main_commits: u32,
    target_history_commits: u32,
    stack_commits: u32,
    files_per_commit: u32,
    target_files_per_commit: u32,
}

const REPRESENTATIVE_SCENARIOS: [RepresentativeScenario; 1] = [RepresentativeScenario {
    id: "5000_main_250_target_churn_68_branch_paths",
    main_commits: 5_000,
    target_history_commits: 250,
    stack_commits: 17,
    files_per_commit: 4,
    target_files_per_commit: 2,
}];

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
        let parents = parent_oid
            .map(|parent| {
                vec![
                    repo.find_commit(parent)
                        .expect("failed to load parent commit"),
                ]
            })
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();

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

fn commit_with_parent(
    repo: &Repository,
    refname: &str,
    message: &str,
    parent_oid: Oid,
    timestamp: &mut i64,
) -> Oid {
    let sig = next_signature(timestamp);
    let parent = repo
        .find_commit(parent_oid)
        .expect("failed to load parent commit");
    let parent_tree = repo
        .find_tree(parent.tree_id())
        .expect("failed to load parent tree");
    repo.commit(Some(refname), &sig, &sig, message, &parent_tree, &[&parent])
        .expect("failed to create commit")
}

fn commit_with_file_updates(
    repo: &Repository,
    refname: &str,
    message: &str,
    parent_oid: Oid,
    updates: &[(String, String)],
    timestamp: &mut i64,
) -> Oid {
    let sig = next_signature(timestamp);
    let parent = repo
        .find_commit(parent_oid)
        .expect("failed to load parent commit");
    let parent_tree = repo
        .find_tree(parent.tree_id())
        .expect("failed to load parent tree");
    let workdir = repo
        .workdir()
        .expect("benchmark repo should have a working tree");
    let mut index = repo.index().expect("failed to open repository index");
    index
        .read_tree(&parent_tree)
        .expect("failed to seed benchmark index from parent tree");

    for (path, content) in updates {
        let path_buf = workdir.join(path);
        if let Some(parent_dir) = path_buf.parent() {
            fs::create_dir_all(parent_dir).expect("failed to create benchmark parent directory");
        }
        fs::write(&path_buf, content).expect("failed to write benchmark file");
        index
            .add_path(Path::new(path))
            .expect("failed to add benchmark path to index");
    }
    index.write().expect("failed to write benchmark index");

    let tree_id = index.write_tree().expect("failed to write benchmark tree");
    let tree = repo
        .find_tree(tree_id)
        .expect("failed to load benchmark tree");

    repo.commit(Some(refname), &sig, &sig, message, &tree, &[&parent])
        .expect("failed to create benchmark commit")
}

fn branch_with_commits(
    repo: &Repository,
    branch_name: &str,
    base_oid: Oid,
    num_commits: u32,
    timestamp: &mut i64,
) -> Oid {
    let refname = format!("refs/heads/{branch_name}");
    let mut tip = base_oid;

    for i in 0..num_commits {
        tip = commit_with_parent(
            repo,
            &refname,
            &format!("{branch_name} commit {i}"),
            tip,
            timestamp,
        );
    }

    tip
}

fn create_noise_branches_at_ancestors(
    repo: &Repository,
    prefix: &str,
    start_oid: Oid,
    count: u32,
    commits_per_branch: u32,
    timestamp: &mut i64,
) {
    let mut current = start_oid;
    for i in 0..count {
        let ancestor = repo
            .find_commit(current)
            .expect("failed to load commit for noise branch");
        branch_with_commits(
            repo,
            &format!("{prefix}-{i}"),
            ancestor.id(),
            commits_per_branch,
            timestamp,
        );

        if ancestor.parent_count() == 0 {
            break;
        }
        current = ancestor.parent_id(0).expect("failed to load parent id");
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

    // Create main history with 45k commits
    let main_tip = append_empty_commits(
        &repo,
        "refs/heads/main",
        scenario.main_commits,
        &mut timestamp,
    );

    // Create the stack branching off early (around commit 5000)
    // Stack: feature-a -> feature-b -> feature-c (3 commits each)
    let stack_base_oid = {
        let mut walk = repo.revwalk().expect("failed to create revwalk");
        walk.push(main_tip).expect("failed to push main tip");
        for _ in 0..5000 {
            let oid = walk.next().expect("failed to walk").expect("invalid oid");
            if walk.next().is_none() {
                break;
            }
            walk.reset().expect("failed to reset walk");
            walk.push(oid).expect("failed to push oid");
        }
        walk.next()
            .expect("failed to get stack base")
            .expect("invalid oid")
    };

    let _fa_tip = branch_with_commits(&repo, "feature-a", stack_base_oid, 3, &mut timestamp);
    let fb_tip = branch_with_commits(&repo, "feature-b", _fa_tip, 3, &mut timestamp);
    let _fc_tip = branch_with_commits(&repo, "feature-c", fb_tip, 3, &mut timestamp);

    // Create noise branches diverging from various points on main
    // Some will be above the stack's merge base, some below
    create_noise_branches_at_ancestors(
        &repo,
        "noise",
        main_tip,
        scenario.noise_branches,
        2, // 2 commits per noise branch
        &mut timestamp,
    );

    // Checkout the top of the stack
    checkout_branch(&repo, TARGET_BRANCH);

    BenchRepo {
        path: dir.path().to_path_buf(),
        _dir: dir,
    }
}

fn setup_representative_repo(scenario: RepresentativeScenario) -> BenchRepo {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let repo = Repository::init(dir.path()).expect("failed to init repository");
    let mut timestamp = 1_700_000_000;

    let main_tip = append_empty_commits(
        &repo,
        "refs/heads/main",
        scenario.main_commits,
        &mut timestamp,
    );

    let mut target_branch_tip = main_tip;
    let mut branch_paths = Vec::new();
    for commit_idx in 0..scenario.stack_commits {
        let updates = (0..scenario.files_per_commit)
            .map(|file_idx| {
                let path = format!("bench/feature-{:02}-{:02}.txt", commit_idx, file_idx);
                branch_paths.push(path.clone());
                (
                    path,
                    format!("feature commit {commit_idx} file {file_idx}\n"),
                )
            })
            .collect::<Vec<_>>();

        target_branch_tip = commit_with_file_updates(
            &repo,
            "refs/heads/feature-c",
            &format!("feature commit {commit_idx}"),
            target_branch_tip,
            &updates,
            &mut timestamp,
        );
    }

    let mut main_tip = main_tip;
    for commit_idx in 0..scenario.target_history_commits {
        let updates = (0..scenario.target_files_per_commit)
            .map(|offset| {
                let path_index = ((commit_idx * scenario.target_files_per_commit + offset)
                    as usize)
                    % branch_paths.len();
                (
                    branch_paths[path_index].clone(),
                    format!("main churn {commit_idx} file {offset}\n"),
                )
            })
            .collect::<Vec<_>>();

        main_tip = commit_with_file_updates(
            &repo,
            "refs/heads/main",
            &format!("main churn {commit_idx}"),
            main_tip,
            &updates,
            &mut timestamp,
        );
    }

    checkout_branch(&repo, TARGET_BRANCH);

    BenchRepo {
        path: dir.path().to_path_buf(),
        _dir: dir,
    }
}

fn run_sync_command(gits_bin: &std::path::Path, repo_path: &std::path::Path) {
    let output = Command::new(gits_bin)
        .args(["sync", "--no-delete"])
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to execute kin sync command");

    // Note: sync may fail due to rebase conflicts in this artificial scenario,
    // but we're measuring the discovery/preparation time, not completion
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore expected rebase conflicts, but warn on actual errors
        if !stderr.contains("rebase") && !stderr.contains("Resolve") {
            eprintln!("kin sync failed unexpectedly: {stderr}");
        }
    }
}

/// Benchmark the full sync command (excluding actual rebase time which varies)
fn bench_sync_full(c: &mut Criterion) {
    let gits_bin = assert_cmd::cargo::cargo_bin!("kin");
    let mut group = c.benchmark_group("sync_full");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(10);

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);
        let repo_path = repo.path().to_path_buf();

        group.bench_function(scenario.id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    // Re-open repo to reset any state
                    let repo = Repository::open(&repo_path).expect("failed to open repo");
                    checkout_branch(&repo, TARGET_BRANCH);

                    let start = std::time::Instant::now();
                    run_sync_command(gits_bin, &repo_path);
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark stack discovery only (get_stack_branches_from_merge_base)
fn bench_sync_stack_discovery(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_stack_discovery");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(10);

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);

        group.bench_function(scenario.id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let repo = Repository::open(repo.path()).expect("failed to open repo");
                    checkout_branch(&repo, TARGET_BRANCH);

                    let head = repo.head().expect("failed to get HEAD");
                    let head_id = head
                        .peel_to_commit()
                        .expect("failed to peel to commit")
                        .id();
                    let upstream_id = repo
                        .revparse_single("main")
                        .expect("failed to resolve main")
                        .id();
                    let merge_base = repo
                        .merge_base(head_id, upstream_id)
                        .expect("failed to find merge base");

                    let start = std::time::Instant::now();
                    let _ = get_stack_branches_from_merge_base(
                        &repo,
                        merge_base,
                        head_id,
                        upstream_id,
                        "main",
                    )
                    .expect("failed to get stack branches");
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark find_sync_boundary
fn bench_sync_find_boundary(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_find_boundary");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(10);

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);

        group.bench_function(scenario.id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let repo = Repository::open(repo.path()).expect("failed to open repo");
                    checkout_branch(&repo, TARGET_BRANCH);

                    let head = repo.head().expect("failed to get HEAD");
                    let head_id = head
                        .peel_to_commit()
                        .expect("failed to peel to commit")
                        .id();
                    let upstream_id = repo
                        .revparse_single("main")
                        .expect("failed to resolve main")
                        .id();
                    let merge_base = repo
                        .merge_base(head_id, upstream_id)
                        .expect("failed to find merge base");

                    let stack_branches = get_stack_branches_from_merge_base(
                        &repo,
                        merge_base,
                        head_id,
                        upstream_id,
                        "main",
                    )
                    .expect("failed to get stack branches");

                    let mut tips =
                        get_stack_tips(&repo, &stack_branches).expect("failed to get stack tips");
                    tips.sort();
                    let top_branch = tips
                        .last()
                        .cloned()
                        .unwrap_or_else(|| TARGET_BRANCH.to_string());

                    let start = std::time::Instant::now();
                    let _ = find_sync_boundary(&repo, &top_branch, "main", &stack_branches)
                        .expect("failed to find sync boundary");
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark the content-heavy prefix scan that previously dominated `kin sync`
/// before the rebase even started.
fn bench_sync_find_boundary_representative(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_find_boundary_representative");
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(10);

    for scenario in REPRESENTATIVE_SCENARIOS {
        let repo = setup_representative_repo(scenario);

        group.bench_function(scenario.id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let repo = Repository::open(repo.path()).expect("failed to open repo");
                    checkout_branch(&repo, TARGET_BRANCH);

                    let head = repo.head().expect("failed to get HEAD");
                    let head_id = head
                        .peel_to_commit()
                        .expect("failed to peel to commit")
                        .id();
                    let upstream_id = repo
                        .revparse_single("main")
                        .expect("failed to resolve main")
                        .id();
                    let merge_base = repo
                        .merge_base(head_id, upstream_id)
                        .expect("failed to find merge base");

                    let stack_branches = get_stack_branches_from_merge_base(
                        &repo,
                        merge_base,
                        head_id,
                        upstream_id,
                        "main",
                    )
                    .expect("failed to get stack branches");

                    let mut tips =
                        get_stack_tips(&repo, &stack_branches).expect("failed to get stack tips");
                    tips.sort();
                    let top_branch = tips
                        .last()
                        .cloned()
                        .unwrap_or_else(|| TARGET_BRANCH.to_string());

                    let start = std::time::Instant::now();
                    let _ = find_sync_boundary(&repo, &top_branch, "main", &stack_branches)
                        .expect("failed to find sync boundary");
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Benchmark collect_merged_local_branches (called when syncing on main)
/// Note: This requires a remote to be set up, so we use main as target
fn bench_sync_collect_merged(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_collect_merged");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(10);

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);

        group.bench_function(scenario.id, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let repo = Repository::open(repo.path()).expect("failed to open repo");
                    checkout_branch(&repo, TARGET_BRANCH);

                    // Use main as target (simulates syncing on main)
                    let start = std::time::Instant::now();
                    let _ = collect_merged_local_branches(&repo, "main", &["main"])
                        .expect("failed to collect merged branches");
                    total += start.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_sync_full,
    bench_sync_stack_discovery,
    bench_sync_find_boundary,
    bench_sync_find_boundary_representative,
    bench_sync_collect_merged
);
criterion_main!(benches);
