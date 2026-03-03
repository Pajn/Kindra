use anyhow::{Context, Result};

/// Configure runtime settings to improve performance and avoid resource exhaustion.
///
/// # Safety
///
/// This function must be called only once at startup before any other threads are spawned,
/// as it modifies global process state (libgit2 options and file descriptor limits).
pub(crate) unsafe fn configure_runtime_tuning() -> Result<()> {
    // Increase the file descriptor limit on systems that support it.
    // This helps prevent "Too many open files" errors in large repositories.
    #[cfg(unix)]
    {
        use rustix::process::{Resource, getrlimit, setrlimit};
        let limit = getrlimit(Resource::Nofile);
        let mut new_limit = limit;
        new_limit.current = new_limit.maximum;
        if let Err(e) = setrlimit(Resource::Nofile, new_limit) {
            eprintln!(
                "Warning: Failed to increase file descriptor limit (setrlimit Resource::Nofile): {}",
                e
            );
        }
    }

    // Set a limit on the number of open file descriptors libgit2 will use for packfiles.
    // This helps prevent "Too many open files" errors on systems with low limits (like macOS).
    // SAFETY: set_mwindow_file_limit is safe to call at startup before other git2 operations.
    unsafe {
        git2::opts::set_mwindow_file_limit(128).context(
            "Failed to set git2 mwindow file limit (git2::opts::set_mwindow_file_limit(128))",
        )?;
    }

    Ok(())
}
