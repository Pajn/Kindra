//! Integration tests for topological ordering of stack branches.

mod common;
use common::{make_commit, repo_init};
use git2::Repository;
use kindra::stack::{StackBranch, sort_branches_topologically};
use tempfile::tempdir;

fn tip(repo: &Repository, name: &str) -> StackBranch {
    StackBranch {
        name: name.to_string(),
        id: repo
            .revparse_single(name)
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id(),
    }
}

/// Ancestors must be ordered before the branches that descend from them, even
/// when the tips fan out from different points of the shared history.
///
/// The shape is the one `git2` documents for `merge_base_many`, which returns a
/// base for a hypothetical merge of the later commits rather than a common
/// ancestor of all of them:
///
/// ```text
///        o---o---o---o---c-feature
///       /
///      /   o---o---o---b-feature
///     /   /
/// ---2---1---o---o---o---a-feature
///     ^
///     z-base
/// ```
///
/// Here that base is `1`, which `z-base` and `c-feature` do not descend from.
/// Deriving the ancestry boundary from it drops `z-base` out of the walk, and
/// the sort then emits it after its own descendants.
#[test]
fn ancestor_branch_sorts_before_descendants_that_fork_below_the_pairwise_base() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let root = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);

    // `2` in the diagram: the only commit every tip descends from.
    let c2 = {
        let parent = repo.find_commit(root).unwrap();
        make_commit(&repo, "refs/heads/main", "c2.txt", "c2", "c2", &[&parent])
    };
    // `1` in the diagram: shared by a-feature and b-feature, but not c-feature.
    let c1 = {
        let parent = repo.find_commit(c2).unwrap();
        make_commit(&repo, "refs/heads/main", "c1.txt", "c1", "c1", &[&parent])
    };

    {
        let parent = repo.find_commit(c1).unwrap();
        make_commit(&repo, "refs/heads/a-feature", "a.txt", "a", "a", &[&parent]);
    }
    {
        let parent = repo.find_commit(c1).unwrap();
        make_commit(&repo, "refs/heads/b-feature", "b.txt", "b", "b", &[&parent]);
    }
    {
        let parent = repo.find_commit(c2).unwrap();
        make_commit(&repo, "refs/heads/c-feature", "c.txt", "c", "c", &[&parent]);
    }
    // A branch left behind at the older shared commit. Every other tip descends
    // from it, so it has to sort first. Its name sorts last, so a run that lost
    // the ancestry edges falls back to name order and puts it last instead.
    repo.reference("refs/heads/z-base", c2, true, "z-base")
        .unwrap();

    let mut branches = vec![
        tip(&repo, "a-feature"),
        tip(&repo, "b-feature"),
        tip(&repo, "c-feature"),
        tip(&repo, "z-base"),
    ];
    sort_branches_topologically(&repo, &mut branches).unwrap();

    let order: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(
        order[0], "z-base",
        "z-base is an ancestor of every other tip and must sort first, got {order:?}"
    );
}
