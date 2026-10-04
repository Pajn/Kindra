//! Resolve creation inputs before publication. This is a command input file,
//! not repository configuration; paths are relative to the input manifest.
use super::*;
use serde::Deserialize;
use std::path::Path;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    branches: BTreeMap<String, BranchMetadata>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchMetadata {
    title: Option<String>,
    body: Option<String>,
    body_file: Option<PathBuf>,
    draft: Option<bool>,
}

fn read_body(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("Could not read PR body file '{}' as UTF-8", path.display()))
}

pub(super) fn resolve(
    repo: &Repository,
    all: &[StackBranch],
    scoped: &[StackBranch],
    open: &HashMap<String, gh::OpenPr>,
    bases: &HashMap<String, String>,
    options: &PrCreateOptions,
    no_push: bool,
) -> Result<HashMap<String, PrCreateOptions>> {
    let mut entries = BTreeMap::new();
    let mut errors = Vec::new();
    if let Some(path) = &options.metadata_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Could not read PR metadata file '{}'", path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("Invalid PR metadata file '{}'", path.display()))?;
        for (name, entry) in manifest.branches {
            if !all.iter().any(|branch| branch.name == name) {
                errors.push(format!("Unknown stack branch '{name}' in metadata file"));
            } else if !scoped.iter().any(|branch| branch.name == name) {
                errors.push(format!("Branch '{name}' is outside the submission scope"));
            }
            let mut resolved = options.clone();
            resolved.title = entry.title;
            resolved.draft = entry.draft.or(options.draft);
            if entry.body.is_some() && entry.body_file.is_some() {
                errors.push(format!("Branch '{name}' has both body and body_file"));
            }
            if options.body_from_commits && (entry.body.is_some() || entry.body_file.is_some()) {
                errors.push(format!(
                    "Branch '{name}' has an explicit body conflicting with --body-from-commits"
                ));
            }
            resolved.body = entry.body;
            if let Some(body_path) = entry.body_file {
                let body_path = path.parent().unwrap_or(Path::new(".")).join(body_path);
                match read_body(&body_path) {
                    Ok(body) => resolved.body = Some(body),
                    Err(error) => errors.push(format!("Branch '{name}': {error:#}")),
                }
            }
            if resolved
                .title
                .as_ref()
                .is_some_and(|title| title.trim().is_empty())
            {
                errors.push(format!("Branch '{name}' has an empty PR title"));
            }
            entries.insert(name, resolved);
        }
    }

    let mut shared = options.clone();
    if let Some(path) = &options.body_file {
        shared.body = Some(read_body(path)?);
    }
    if shared
        .title
        .as_ref()
        .is_some_and(|title| title.trim().is_empty())
    {
        errors.push("--title must not be empty".to_string());
    }
    if !errors.is_empty() {
        return Err(anyhow!("Invalid PR metadata:\n  {}", errors.join("\n  ")));
    }

    let mut planned = Vec::new();
    for branch in scoped {
        if open.contains_key(&branch.name) {
            continue;
        }
        if no_push
            && repo
                .find_branch(&branch.name, BranchType::Local)?
                .upstream()
                .is_err()
        {
            continue;
        }
        let base = bases
            .get(&branch.name)
            .expect("base map covers every stack branch");
        let commits = get_branch_commits(repo, &branch.name, base)?;
        if !commits.is_empty() {
            planned.push((branch, commits));
        }
    }
    if planned.len() > 1
        && !options.metadata_all
        && (options.title.is_some() || options.body_file.is_some())
    {
        return Err(crate::interaction::input_required(format!(
            "--title/--body-file are ambiguous for {} new PRs. Use --current to publish only the current branch, --metadata-all to reuse inputs, or --metadata-file for per-branch inputs.",
            planned.len()
        )));
    }

    let unattended = !crate::interaction::current().is_interactive()
        && crate::interaction::current().scripted().is_none();
    let mut resolved = HashMap::new();
    for (branch, commits) in planned {
        let mut branch_options = entries
            .remove(&branch.name)
            .unwrap_or_else(|| shared.clone());
        if unattended {
            if branch_options.title.is_none() {
                // Keep the commit list and established missing-input diagnostic,
                // but collect failures across the entire stack before returning.
                match prompt_title(&branch.name, &commits) {
                    Ok(title) => branch_options.title = Some(title),
                    Err(error) => errors.push(error.to_string()),
                }
            }
            if branch_options.body.is_none() {
                let body = if branch_options.body_from_commits {
                    build_body_from_commits(&commits)
                } else {
                    let draft = crate::editor::Draft::new(crate::editor::draft_path(
                        repo.path(),
                        &format!("pr-body-{}", branch.name),
                    ));
                    prompt_body(&branch.name, &commits, &draft)?
                };
                branch_options.body = Some(body);
            }
        }
        resolved.insert(branch.name.clone(), branch_options);
    }
    if !errors.is_empty() {
        return Err(crate::interaction::input_required(format!(
            "PR input required:\n  {}",
            errors.join("\n  ")
        )));
    }
    Ok(resolved)
}
