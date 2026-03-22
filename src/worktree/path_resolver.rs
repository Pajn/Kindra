use crate::worktree::WorktreeRole;
use anyhow::{Result, anyhow};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeTarget {
    Role(WorktreeRole),
    TempBranch(String),
}

pub fn parse_target(target: &str) -> WorktreeTarget {
    if let Some(branch) = target.strip_prefix("branch:") {
        return WorktreeTarget::TempBranch(branch.to_string());
    }

    match target {
        "main" => WorktreeTarget::Role(WorktreeRole::Main),
        "review" => WorktreeTarget::Role(WorktreeRole::Review),
        _ => WorktreeTarget::TempBranch(target.to_string()),
    }
}

pub fn sanitize_branch_for_path(branch: &str) -> String {
    let mut sanitized = String::with_capacity(branch.len());
    let mut last_dash = false;

    for ch in branch.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-');
        let next = if keep { ch } else { '-' };
        if next == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        sanitized.push(next);
    }

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "branch".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let mut normalized = PathBuf::new();
    let mut has_root = false;
    let mut normal_depth = 0usize;

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth > 0 {
                    normalized.pop();
                    normal_depth -= 1;
                } else if has_root {
                    continue;
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                has_root = true;
                normal_depth = 0;
                normalized.push(component.as_os_str());
            }
            Component::Normal(_) => {
                normalized.push(component.as_os_str());
                normal_depth += 1;
            }
        }
    }

    normalized
}

pub fn expand_path_template(template: &Path, branch: &str) -> Result<PathBuf> {
    let template = template.to_string_lossy();
    if !template.contains("{branch}") {
        return Err(anyhow!(
            "Temp worktree path template '{}' must include '{{branch}}'.",
            template
        ));
    }

    Ok(normalize_path(
        template.replace("{branch}", &sanitize_branch_for_path(branch)),
    ))
}

pub fn temp_template_root(template: &Path) -> Result<PathBuf> {
    let template = template.to_string_lossy();
    let (prefix, _) = template.split_once("{branch}").ok_or_else(|| {
        anyhow!(
            "Temp worktree path template '{}' must include '{{branch}}'.",
            template
        )
    })?;
    Ok(normalize_path(prefix))
}

#[cfg(test)]
mod tests {
    use super::{
        expand_path_template, normalize_path, parse_target, sanitize_branch_for_path,
        temp_template_root,
    };
    use crate::worktree::WorktreeRole;
    use std::path::Path;

    #[test]
    fn sanitizes_branch_names() {
        assert_eq!(sanitize_branch_for_path("feature/auth"), "feature-auth");
        assert_eq!(
            sanitize_branch_for_path("fix/bug_123.test"),
            "fix-bug_123.test"
        );
        assert_eq!(sanitize_branch_for_path("///"), "branch");
    }

    #[test]
    fn expands_templates() {
        let path = expand_path_template(
            Path::new(".git/kindra-worktrees/temp/{branch}"),
            "feature/a",
        )
        .unwrap();
        assert_eq!(
            path,
            Path::new(".git/kindra-worktrees/temp/feature-a").to_path_buf()
        );
    }

    #[test]
    fn parses_targets() {
        assert_eq!(
            parse_target("main"),
            super::WorktreeTarget::Role(WorktreeRole::Main)
        );
        assert_eq!(
            parse_target("feature/a"),
            super::WorktreeTarget::TempBranch("feature/a".to_string())
        );
        assert_eq!(
            parse_target("branch:main"),
            super::WorktreeTarget::TempBranch("main".to_string())
        );
    }

    #[test]
    fn extracts_temp_root() {
        let root =
            temp_template_root(Path::new(".git/kindra-worktrees/temp/{branch}/suffix")).unwrap();
        assert_eq!(root, Path::new(".git/kindra-worktrees/temp").to_path_buf());
    }

    #[test]
    fn preserves_leading_parent_components() {
        assert_eq!(
            normalize_path("../a/../../b"),
            Path::new("../../b").to_path_buf()
        );
    }
}
