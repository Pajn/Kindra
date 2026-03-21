use criterion::{Criterion, criterion_group, criterion_main};
use git2::{Oid, Repository, Signature, build::CheckoutBuilder};
use kindra::commands::DEFAULT_RESTACK_HISTORY_LIMIT;
use kindra::stack::{build_floating_target_context, find_floating_base};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const TARGET_BRANCH: &str = "main";

#[derive(Clone, Copy)]
struct Scenario {
    id: &'static str,
    main_commits: u32,
    stale_branches: u32,
    noise_branches: u32,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario {
        id: "45000_main_short_stack",
        main_commits: 45_000,
        stale_branches: 0,
        noise_branches: 0,
    },
    Scenario {
        id: "45000_main_30_local",
        main_commits: 45_000,
        stale_branches: 10,
        noise_branches: 18,
    },
    Scenario {
        id: "45000_main_200_local",
        main_commits: 45_000,
        stale_branches: 50,
        noise_branches: 148,
    },
];

struct BenchRepo {
    _dir: TempDir,
    path: PathBuf,
    target_branch: String,
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
    content_tag: Option<&str>,
    timestamp: &mut i64,
) -> Oid {
    let sig = next_signature(timestamp);
    let parent = repo
        .find_commit(parent_oid)
        .expect("failed to load parent commit");
    let parent_tree = repo
        .find_tree(parent.tree_id())
        .expect("failed to load parent tree");
    let tree = if let Some(content_tag) = content_tag {
        let mut index = repo.index().expect("failed to open repository index");
        index
            .read_tree(&parent_tree)
            .expect("failed to seed index from parent tree");

        let filename = format!("bench-{content_tag}.txt");
        fs::write(
            repo.workdir()
                .expect("benchmark repo should have a working tree")
                .join(&filename),
            format!("content {content_tag}"),
        )
        .expect("failed to write benchmark content");
        index
            .add_path(Path::new(&filename))
            .expect("failed to add benchmark content to index");
        index.write().expect("failed to write benchmark index");

        let tree_id = index.write_tree().expect("failed to write benchmark tree");
        repo.find_tree(tree_id)
            .expect("failed to load benchmark tree")
    } else {
        parent_tree
    };

    repo.commit(Some(refname), &sig, &sig, message, &tree, &[&parent])
        .expect("failed to create commit")
}

fn create_branches_at_ancestors(
    repo: &Repository,
    prefix: &str,
    start_oid: Oid,
    count: u32,
    timestamp: &mut i64,
) {
    let mut current = start_oid;
    for i in 0..count {
        let ancestor = repo
            .find_commit(current)
            .expect("failed to load commit for ancestor branch");
        let refname = format!("refs/heads/{prefix}-{i}");
        let sig = next_signature(timestamp);
        let ancestor_tree = repo
            .find_tree(ancestor.tree_id())
            .expect("failed to load ancestor tree");
        let branch_tip = repo
            .commit(
                Some(&refname),
                &sig,
                &sig,
                &format!("{prefix} branch {i}"),
                &ancestor_tree,
                &[&ancestor],
            )
            .expect("failed to create branch tip commit");

        repo.reference(
            &refname,
            branch_tip,
            true,
            "benchmark: create branch reference",
        )
        .expect("failed to update branch reference");

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

fn discover_floating_children(repo_path: &Path, current_branch: &str) -> usize {
    let repo = Repository::open(repo_path).expect("failed to open benchmark repository");
    let head_commit = repo
        .revparse_single(current_branch)
        .expect("failed to resolve benchmark target branch")
        .peel_to_commit()
        .expect("failed to peel benchmark target branch to commit");
    let mut patch_id_cache = HashMap::new();
    let target = build_floating_target_context(
        &repo,
        &head_commit,
        current_branch,
        DEFAULT_RESTACK_HISTORY_LIMIT,
        &mut patch_id_cache,
    )
    .expect("failed to build floating target context");
    let branches = repo
        .branches(Some(git2::BranchType::Local))
        .expect("failed to enumerate local branches");

    let mut found = 0;
    for branch_res in branches {
        let (branch, _) = branch_res.expect("failed to read branch");
        let name = match branch.name() {
            Ok(Some(name)) => name.to_string(),
            _ => continue,
        };
        if name == current_branch {
            continue;
        }

        let Some(tip) = branch.get().target() else {
            continue;
        };

        if find_floating_base(
            &repo,
            tip,
            &target,
            DEFAULT_RESTACK_HISTORY_LIMIT,
            &mut patch_id_cache,
        )
        .expect("failed to search floating base")
        .is_some()
        {
            found += 1;
        }
    }

    found
}

fn setup_repo(scenario: Scenario) -> BenchRepo {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let repo = Repository::init(dir.path()).expect("failed to init repository");
    let mut timestamp = 1_700_000_000;

    let root_oid = append_empty_commits(
        &repo,
        "refs/heads/main",
        scenario.main_commits,
        &mut timestamp,
    );

    let old_a = commit_with_parent(
        &repo,
        "refs/heads/main",
        "commit A",
        root_oid,
        Some("commit-a"),
        &mut timestamp,
    );
    let _old_b = commit_with_parent(
        &repo,
        "refs/heads/main",
        "commit B",
        old_a,
        Some("commit-b"),
        &mut timestamp,
    );

    let _old_feat = commit_with_parent(
        &repo,
        "refs/heads/feature-child",
        "child commit",
        old_a,
        Some("feature-child"),
        &mut timestamp,
    );

    create_branches_at_ancestors(
        &repo,
        "stale-main",
        root_oid,
        scenario.stale_branches,
        &mut timestamp,
    );

    let noise_base = commit_with_parent(
        &repo,
        "refs/heads/noise-root",
        "noise root",
        root_oid,
        Some("noise-root"),
        &mut timestamp,
    );
    create_branches_at_ancestors(
        &repo,
        "noise",
        noise_base,
        scenario.noise_branches,
        &mut timestamp,
    );

    let rewritten_a = commit_with_parent(
        &repo,
        "refs/heads/rewritten-main",
        "commit A",
        root_oid,
        Some("commit-a"),
        &mut timestamp,
    );
    let rewritten_b = commit_with_parent(
        &repo,
        "refs/heads/rewritten-main",
        "commit B",
        rewritten_a,
        Some("commit-b"),
        &mut timestamp,
    );
    repo.reference(
        "refs/heads/main",
        rewritten_b,
        true,
        "benchmark: replace rewritten main tip",
    )
    .expect("failed to update rewritten main reference");

    checkout_branch(&repo, TARGET_BRANCH);

    BenchRepo {
        path: dir.path().to_path_buf(),
        _dir: dir,
        target_branch: TARGET_BRANCH.to_string(),
    }
}

fn bench_restack_discovery(c: &mut Criterion) {
    let mut group = c.benchmark_group("restack_discovery");
    group.measurement_time(Duration::from_secs(12));
    group.sample_size(10);

    for scenario in SCENARIOS {
        let repo = setup_repo(scenario);

        let baseline_found = discover_floating_children(repo.path(), &repo.target_branch);
        assert_eq!(
            baseline_found, 1,
            "benchmark fixture should contain exactly one floating child"
        );

        group.bench_function(scenario.id, |b| {
            b.iter(|| {
                let found = discover_floating_children(repo.path(), &repo.target_branch);
                criterion::black_box(found);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_restack_discovery);
criterion_main!(benches);
