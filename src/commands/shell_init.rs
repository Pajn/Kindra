use anyhow::{Result, anyhow};
use clap::{Args, ValueEnum};

#[derive(Args, Clone, Debug)]
pub struct ShellInitArgs {
    /// Shell to emit integration for
    #[arg(value_enum)]
    pub shell: Shell,

    /// Do not include shell completions (e.g. if you install them separately)
    #[arg(long)]
    pub no_completions: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    fn as_str(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }
}

/// Print a shell snippet that enables Kindra's shell integration. Today that is
/// completions plus the `kin wt cd` wrapper; it lives at the top level (`kin
/// shell-init`) so more integration can be folded into the same snippet later
/// without users having to change how they load it. Completions are included by
/// default so a single `eval` sets everything up; `--no-completions` opts out.
pub fn shell_init(args: &ShellInitArgs) -> Result<()> {
    if !args.no_completions {
        print!("{}", completions_snippet(args.shell)?);
        println!();
    }
    print!("{}", script(args.shell));
    Ok(())
}

/// Generate the dynamic-completion registration snippet by re-invoking this
/// binary with `COMPLETE=<shell>`, the same mechanism as `kin completions`.
fn completions_snippet(shell: Shell) -> Result<String> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .env("COMPLETE", shell.as_str())
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to generate {} completions: {}",
            shell.as_str(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A shell wrapper that makes `kin wt cd <target>` change the calling shell's
/// directory. It shadows `kin` with a function that special-cases `wt cd`
/// (capturing the path the binary prints and running `cd`) and forwards every
/// other invocation untouched to the real binary via `command kin`.
fn script(shell: Shell) -> String {
    match shell {
        // bash and zsh share POSIX function syntax and `local`.
        Shell::Bash | Shell::Zsh => "\
# kin shell integration.
#   bash/zsh: eval \"$(kin shell-init zsh)\"
kin() {
  if [ \"$1\" = \"wt\" ] && [ \"$2\" = \"cd\" ]; then
    shift 2
    local __kin_target
    __kin_target=\"$(command kin wt cd \"$@\")\" || return $?
    if [ -n \"$__kin_target\" ]; then
      cd \"$__kin_target\" || return $?
    fi
  else
    command kin \"$@\"
  fi
}
"
        .to_string(),
        Shell::Fish => "\
# kin shell integration.
#   fish: kin shell-init fish | source
function kin
    if test (count $argv) -ge 2; and test \"$argv[1]\" = wt; and test \"$argv[2]\" = cd
        set -l __kin_target (command kin wt cd $argv[3..-1])
        or return $status
        if test -n \"$__kin_target\"
            cd $__kin_target
        end
    else
        command kin $argv
    end
end
"
        .to_string(),
    }
}
