use anyhow::{Context, Result};

/// Silence the panic message emitted when writing to stdout/stderr fails because
/// a downstream reader closed the pipe (e.g. `kin sync | head`).
///
/// We keep SIGPIPE ignored so such a closed pipe unwinds (running Drop guards)
/// rather than killing the process, but the default panic hook would still print
/// a noisy `thread 'main' panicked ... failed printing to stdout` message. The
/// `"failed printing to stdout"` / `"failed printing to stderr"` prefixes are
/// stable string literals in the standard library's print machinery (not
/// locale-dependent), so we key on them to stay quiet for the pipe-closed case
/// while letting every other panic report as usual. Unwinding still runs, so the
/// Drop-based cleanup is unaffected.
pub(crate) fn install_quiet_output_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied());
        if payload.is_some_and(is_output_write_failure) {
            return;
        }
        default_hook(info);
    }));
}

/// Whether a panic message is the standard library's "couldn't write to
/// stdout/stderr" report (which fires on a closed downstream pipe). The print
/// macros panic with exactly `failed printing to stdout: {err}` /
/// `failed printing to stderr: {err}` (see library/std/src/io/stdio.rs), so match
/// those two forms specifically rather than a broader shared prefix — an
/// unrelated panic must never be misclassified as an output-write failure.
fn is_output_write_failure(message: &str) -> bool {
    message.starts_with("failed printing to stdout")
        || message.starts_with("failed printing to stderr")
}

/// Configure runtime settings to improve performance and avoid resource exhaustion.
///
/// # Safety
///
/// This function must be called only once at startup before any other threads are spawned,
/// as it modifies global process state (libgit2 options and file descriptor limits).
pub(crate) unsafe fn configure_runtime_tuning() -> Result<()> {
    // NOTE: we deliberately leave SIGPIPE at the Rust runtime's default
    // (SIG_IGN). Resetting it to SIG_DFL would make a downstream reader closing
    // the pipe (e.g. `kin sync | head`) kill the process outright via the
    // signal, skipping every destructor — including the Drop guards that finalize
    // the oplog snapshot and restore the terminal. With SIG_IGN the closed pipe
    // instead surfaces as an EPIPE write error that panics and *unwinds*, so
    // those guards still run. `install_quiet_output_panic_hook` suppresses the
    // resulting message so the common `| head` case still exits quietly.

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

#[cfg(test)]
mod tests {
    use super::is_output_write_failure;

    #[test]
    fn classifies_stdlib_output_write_failures() {
        // The exact prefix std uses when a print!/println! write fails; the
        // OS-error suffix is locale-dependent, so only the prefix is matched.
        assert!(is_output_write_failure(
            "failed printing to stdout: Broken pipe (os error 32)"
        ));
        assert!(is_output_write_failure(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
    }

    #[test]
    fn leaves_unrelated_panics_alone() {
        assert!(!is_output_write_failure("index out of bounds"));
        assert!(!is_output_write_failure(
            "called `Option::unwrap()` on a `None` value"
        ));
        // Only the exact stdout/stderr forms count: a message that merely shares
        // the "failed printing to std" prefix must not be swallowed.
        assert!(!is_output_write_failure("failed printing to studio"));
        assert!(!is_output_write_failure("failed printing to std"));
    }
}
