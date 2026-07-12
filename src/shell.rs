use anyhow::{Context, Result, bail};
use clap::Command;
use clap_complete::CompleteEnv;
use clap_complete::env::Shells;
use std::io::Write;

pub use clap_complete::Shell;

const COMPLETION_ENV: &str = "_JW_COMPLETE";
const SWITCH_COMMAND: &str = "switch";
pub const SWITCH_SHORTHANDS: [&str; 2] = ["^", "-"];
pub const SWITCH_ALIASES: [&str; 3] = ["s", SWITCH_SHORTHANDS[0], SWITCH_SHORTHANDS[1]];
const PASSTHROUGH_ARGS: [&str; 4] = ["-x", "--execute", "-h", "--help"];

pub fn complete_if_requested<F>(factory: F)
where
    F: Fn() -> Command,
{
    CompleteEnv::with_factory(factory)
        .var(COMPLETION_ENV)
        .complete();
}

pub fn write_completions(shell: Shell, out: &mut dyn Write) -> Result<()> {
    let shell_name = shell.to_string();
    let shells = Shells::builtins();
    let adapter = shells
        .completer(&shell_name)
        .with_context(|| format!("no completion adapter for {shell_name}"))?;

    adapter
        .write_registration(COMPLETION_ENV, "jw", "jw", "jw", out)
        .with_context(|| format!("failed to write {shell_name} completions"))?;
    out.flush()
        .context("failed to flush generated completions")?;
    Ok(())
}

pub fn init_script(shell: Shell) -> Result<String> {
    match shell {
        Shell::Bash => Ok(posix_init("bash")),
        Shell::Elvish => Ok(elvish_init()),
        Shell::Fish => Ok(fish_init()),
        Shell::PowerShell => Ok(powershell_init()),
        Shell::Zsh => Ok(posix_init("zsh")),
        _ => bail!("no shell integration adapter for {shell}"),
    }
}

fn switch_commands() -> impl Iterator<Item = &'static str> {
    std::iter::once(SWITCH_COMMAND).chain(SWITCH_ALIASES)
}

fn quoted_switch_commands(separator: &str) -> String {
    switch_commands()
        .map(|command| format!("'{command}'"))
        .collect::<Vec<_>>()
        .join(separator)
}

fn quoted_passthrough_args(separator: &str) -> String {
    PASSTHROUGH_ARGS
        .iter()
        .map(|arg| format!("'{arg}'"))
        .collect::<Vec<_>>()
        .join(separator)
}

fn fish_init() -> String {
    let switch_commands = quoted_switch_commands(" ");
    let passthrough_args = quoted_passthrough_args(" ");

    format!(
        r#"function jw --description 'Jujutsu workspace switching'
    if test (count $argv) -eq 0
        command jw
        return $status
    end

    set -l switch_commands {switch_commands}
    set -l passthrough_args {passthrough_args}

    if contains -- $argv[1] $switch_commands
        for arg in $argv
            if contains -- $arg $passthrough_args
                command jw $argv
                return $status
            end
        end

        set -l target (command jw $argv --print-path)
        or return $status

        cd $target
        return $status
    end

    command jw $argv
end

command jw shell completions fish | source
"#
    )
}

fn posix_init(shell_name: &str) -> String {
    let switch_commands = quoted_switch_commands("|");
    let passthrough_patterns = PASSTHROUGH_ARGS
        .iter()
        .map(|arg| format!(r#"*" {arg} "*"#))
        .collect::<Vec<_>>()
        .join("|");

    format!(
        r#"jw() {{
    case "$1" in
        {switch_commands})
            case " $* " in
                {passthrough_patterns})
                    command jw "$@"
                    return $?
                    ;;
            esac

            local target
            target="$(command jw "$@" --print-path)" || return $?
            cd "$target" || return $?
            ;;
        *)
            command jw "$@"
            ;;
    esac
}}

eval "$(command jw shell completions {shell_name})"
"#
    )
}

fn elvish_init() -> String {
    let switch_commands = quoted_switch_commands(" ");
    let passthrough_condition = PASSTHROUGH_ARGS
        .iter()
        .map(|arg| format!("(has-value $args '{arg}')"))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r#"fn jw {{|@args|
    if (== (count $args) 0) {{
        e:command jw
    }} elif (has-value [{switch_commands}] $args[0]) {{
        if (or {passthrough_condition}) {{
            e:command jw $@args
        }} else {{
            var target = (e:command jw $@args --print-path)
            cd $target
        }}
    }} else {{
        e:command jw $@args
    }}
}}

eval (command jw shell completions elvish | slurp)
"#
    )
}

fn powershell_init() -> String {
    let switch_commands = quoted_switch_commands(", ");
    let passthrough_args = quoted_passthrough_args(", ");

    format!(
        r#"$script:__jwExecutable = (Get-Command jw -CommandType Application).Path

function jw {{
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Args)

    if ($Args.Length -eq 0) {{
        & $script:__jwExecutable
        return
    }}

    $switchCommands = @({switch_commands})
    $passthroughArgs = @({passthrough_args})
    $shouldPassthrough = $false
    foreach ($arg in $Args) {{
        if ($passthroughArgs -contains $arg) {{
            $shouldPassthrough = $true
            break
        }}
    }}

    if (($switchCommands -contains $Args[0]) -and -not $shouldPassthrough) {{
        $target = & $script:__jwExecutable @Args --print-path
        if ($LASTEXITCODE -ne 0) {{ return }}
        Set-Location $target
    }} else {{
        & $script:__jwExecutable @Args
    }}
}}

Invoke-Expression (& $script:__jwExecutable shell completions powershell | Out-String)
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn all_shell_init_adapters_share_switch_policy() {
        for shell in Shell::value_variants() {
            let script = init_script(*shell).expect("generate init script");
            for command in switch_commands() {
                assert!(
                    script.contains(&format!("'{command}'")),
                    "{shell} init omits switch command {command}"
                );
            }
            for arg in PASSTHROUGH_ARGS {
                assert!(
                    script.contains(arg),
                    "{shell} init omits passthrough argument {arg}"
                );
            }
        }
    }

    #[test]
    fn all_shell_completion_adapters_delegate_to_clap() {
        for shell in Shell::value_variants() {
            let mut script = Vec::new();
            write_completions(*shell, &mut script).expect("generate completions");
            let script = String::from_utf8(script).expect("completion script is UTF-8");
            assert!(
                script.contains(COMPLETION_ENV),
                "{shell} completion does not call Clap completion engine"
            );
        }
    }
}
