use crate::worktree::WorktreeRole;
use crate::worktree::config::{HookListConfig, WorktreeConfig};
use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    Create,
    Checkout,
    Remove,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "on_create",
            Self::Checkout => "on_checkout",
            Self::Remove => "on_remove",
        }
    }
}

pub fn hooks_for_role(
    config: &WorktreeConfig,
    role: WorktreeRole,
    event: HookEvent,
) -> Vec<String> {
    let global = hooks_from_list(&config.hooks, event);
    let role_hooks = match role {
        WorktreeRole::Main => hooks_from_list(&config.main.hooks, event),
        WorktreeRole::Review => hooks_from_list(&config.review.hooks, event),
        WorktreeRole::Temp => hooks_from_list(&config.temp.hooks, event),
    };

    global.into_iter().chain(role_hooks).collect()
}

pub fn run_hooks(
    config: &WorktreeConfig,
    role: WorktreeRole,
    event: HookEvent,
    worktree_path: &Path,
    branch: &str,
) -> Result<()> {
    for hook in hooks_for_role(config, role, event) {
        let output = shell_command(&hook)
            .current_dir(worktree_path)
            .env("KINDRA_WORKTREE_ROLE", role.as_str())
            .env("KINDRA_WORKTREE_BRANCH", branch)
            .env("KINDRA_WORKTREE_PATH", worktree_path)
            .output()?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let exit_code = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "<no exit code>".to_string());
            return Err(anyhow!(
                "Worktree {} hook failed for role '{}' at '{}': {}\nexit code: {}\nstdout: {}\nstderr: {}",
                event.as_str(),
                role,
                worktree_path.display(),
                hook,
                exit_code,
                if stdout.is_empty() {
                    "<empty>"
                } else {
                    &stdout
                },
                if stderr.is_empty() {
                    "<empty>"
                } else {
                    &stderr
                },
            ));
        }
    }

    Ok(())
}

fn hooks_from_list(list: &HookListConfig, event: HookEvent) -> Vec<String> {
    match event {
        HookEvent::Create => list.on_create.clone(),
        HookEvent::Checkout => list.on_checkout.clone(),
        HookEvent::Remove => list.on_remove.clone(),
    }
}

fn shell_command(script: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", script]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::{HookEvent, hooks_for_role, run_hooks};
    use crate::worktree::WorktreeRole;
    use crate::worktree::config::{
        HookListConfig, MainWorktreeConfig, ReviewWorktreeConfig, TempWorktreeConfig,
        WorktreeConfig,
    };
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn echo_file_command(path: &std::path::Path, text: &str) -> String {
        let rendered = path.display().to_string().replace('\\', "/");
        if cfg!(windows) {
            format!("echo {text}>{rendered}")
        } else {
            format!("printf '{text}' > '{rendered}'")
        }
    }

    fn sample_config(hook: String) -> WorktreeConfig {
        WorktreeConfig {
            root: PathBuf::from(".git/kindra-worktrees"),
            trunk: "main".to_string(),
            hooks: HookListConfig {
                on_create: vec![hook],
                on_checkout: Vec::new(),
                on_remove: Vec::new(),
            },
            main: MainWorktreeConfig {
                enabled: true,
                branch: "main".to_string(),
                path: PathBuf::from(".git/kindra-worktrees/main"),
                allow_branch_switch: false,
                hooks: HookListConfig::default(),
            },
            review: ReviewWorktreeConfig {
                enabled: true,
                path: PathBuf::from(".git/kindra-worktrees/review"),
                reuse: true,
                clean_before_switch: true,
                hooks: HookListConfig::default(),
            },
            temp: TempWorktreeConfig {
                enabled: true,
                path_template: PathBuf::from(".git/kindra-worktrees/temp/{branch}"),
                delete_merged: true,
                hooks: HookListConfig::default(),
            },
        }
    }

    #[test]
    fn combines_global_and_role_hooks() {
        let mut config = sample_config("first".to_string());
        config.review.hooks.on_create.push("second".to_string());

        let hooks = hooks_for_role(&config, WorktreeRole::Review, HookEvent::Create);
        assert_eq!(hooks, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn runs_hooks_in_worktree_dir() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker.txt");
        let config = sample_config(echo_file_command(&marker, "ok"));

        run_hooks(
            &config,
            WorktreeRole::Main,
            HookEvent::Create,
            dir.path(),
            "main",
        )
        .unwrap();

        assert_eq!(fs::read_to_string(marker).unwrap(), "ok");
    }

    #[test]
    fn hook_failures_include_command_output() {
        let dir = TempDir::new().unwrap();
        let hook = if cfg!(windows) {
            "echo hook failed 1>&2 && exit /b 1".to_string()
        } else {
            "echo hook failed >&2; exit 1".to_string()
        };
        let config = sample_config(hook);

        let err = run_hooks(
            &config,
            WorktreeRole::Main,
            HookEvent::Create,
            dir.path(),
            "main",
        )
        .unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("hook failed"));
        assert!(rendered.contains("stderr: hook failed"));
    }
}
