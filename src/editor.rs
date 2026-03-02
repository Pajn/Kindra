use anyhow::{Context, Result, anyhow};
use std::env;
use std::path::Path;
use std::process::Command;

pub fn launch_editor(path: &Path) -> Result<()> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut editor_parts = editor.split_whitespace();
    let editor_program = editor_parts
        .next()
        .ok_or_else(|| anyhow!("EDITOR is empty"))?;
    let editor_args: Vec<&str> = editor_parts.collect();

    let status = Command::new(editor_program)
        .args(&editor_args)
        .arg(path)
        .status()
        .with_context(|| format!("Failed to launch editor '{}'", editor))?;

    if !status.success() {
        return Err(anyhow!("Editor exited with non-zero status"));
    }

    Ok(())
}
