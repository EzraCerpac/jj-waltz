use anyhow::{Context, Result};
use clap::Command;
use clap_complete::{Generator, Shell, generate};
use std::io::Write;

#[derive(Clone, Copy, Debug)]
pub enum ShellKind {
    Bash,
    Elvish,
    Fish,
    Powershell,
    Zsh,
}

pub fn write_completions(
    shell: ShellKind,
    command: &mut Command,
    out: &mut dyn Write,
) -> Result<()> {
    match shell {
        ShellKind::Bash => generate_to(Shell::Bash, command, out),
        ShellKind::Elvish => generate_to(Shell::Elvish, command, out),
        ShellKind::Fish => generate_to(Shell::Fish, command, out),
        ShellKind::Powershell => generate_to(Shell::PowerShell, command, out),
        ShellKind::Zsh => generate_to(Shell::Zsh, command, out),
    }
}

pub fn init_script(shell: ShellKind) -> Result<String> {
    let script = match shell {
        ShellKind::Fish => fish_init(),
        ShellKind::Zsh => posix_init("zsh"),
        ShellKind::Bash => posix_init("bash"),
        ShellKind::Elvish => elvish_init(),
        ShellKind::Powershell => powershell_init(),
    };
    Ok(script)
}

fn generate_to<G: Generator>(
    generator: G,
    command: &mut Command,
    out: &mut dyn Write,
) -> Result<()> {
    generate(generator, command, "jw", out);
    out.flush()
        .context("failed to flush generated completions")?;
    Ok(())
}

fn fish_init() -> String {
    r#"function jw --description 'Jujutsu workspace switching'
    if test (count $argv) -eq 0
        command jw
        return $status
    end

    switch $argv[1]
        case switch s
            if contains -- -x $argv; or contains -- --execute $argv; or contains -- -h $argv; or contains -- --help $argv
                command jw $argv
                return $status
            end

            set -l target (command jw $argv --print-path)
            or return $status

            cd $target
        case '*'
            command jw $argv
    end
end

command jw completions fish | source
"#
        .to_owned()
}

fn posix_init(shell_name: &str) -> String {
    format!(
        r#"jw() {{
    case "$1" in
        switch|s)
            case " $* " in
                *" -x "*|*" --execute "*|*" -h "*|*" --help "*)
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

if command -v compdef >/dev/null 2>&1; then
    eval "$(command jw completions {shell_name})"
elif command -v complete >/dev/null 2>&1; then
    source <(command jw completions bash)
fi
"#
    )
}

fn elvish_init() -> String {
    r#"fn jw {|@args|
    if (== (count $args) 0) {
        e:command jw
    } elif (or (== $args[0] switch) (== $args[0] s)) {
        var joined = (str:join ' ' $args)
        if (or (has-value $args -x) (has-value $args --execute) (has-value $args -h) (has-value $args --help)) {
            e:command jw $@args
        } else {
            var target = (e:command jw $@args --print-path)
            cd $target
        }
    } else {
        e:command jw $@args
    }
}

eval (command jw completions elvish)
"#
        .to_owned()
}

fn powershell_init() -> String {
    r#"function jw {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Args)

    if ($Args.Length -eq 0) {
        & jw
        return
    }

    if (($Args[0] -eq 'switch' -or $Args[0] -eq 's') -and -not ($Args -contains '-x' -or $Args -contains '--execute' -or $Args -contains '-h' -or $Args -contains '--help')) {
        $target = & jw @Args --print-path
        if ($LASTEXITCODE -ne 0) { return }
        Set-Location $target
    } else {
        & jw @Args
    }
}

Invoke-Expression (& jw completions powershell)
"#
        .to_owned()
}
