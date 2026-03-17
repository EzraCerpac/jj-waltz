use anyhow::{Context, Result};
use clap::Command;
use clap_complete::{Generator, Shell, generate};
use std::io::Write;

const FISH_COMPLETIONS: &str = r#"function __jw_workspace_candidates
    command jw shell complete-workspaces 2>/dev/null
end

function __jw_subcommands
    printf '%s\t%s\n' switch 'Switch to or create a workspace'
    printf '%s\t%s\n' s 'Alias for switch'
    printf '%s\t%s\n' list 'List known workspaces'
    printf '%s\t%s\n' l 'Alias for list'
    printf '%s\t%s\n' path 'Print a workspace path'
    printf '%s\t%s\n' remove 'Forget a workspace'
    printf '%s\t%s\n' rm 'Alias for remove'
    printf '%s\t%s\n' prune 'Forget missing workspaces'
    printf '%s\t%s\n' root 'Print the current workspace root'
    printf '%s\t%s\n' current 'Print the current workspace name'
    printf '%s\t%s\n' shell 'Shell integration helpers'
    printf '%s\t%s\n' completions 'Generate shell completions'
    printf '%s\t%s\n' help 'Show command help'
end

function __jw_needs_subcommand
    set -l cmd (commandline -opc)
    test (count $cmd) -le 1
end

function __jw_using_subcommand
    set -l cmd (commandline -opc)
    test (count $cmd) -ge 2
    and contains -- $cmd[2] $argv
end

complete -e -c jw
complete -c jw -n __jw_needs_subcommand -f -a '(__jw_subcommands)'
complete -c jw -n __jw_needs_subcommand -s h -l help -d 'Print help'
complete -c jw -n __jw_needs_subcommand -s V -l version -d 'Print version'

complete -c jw -n '__jw_using_subcommand switch s' -f -a '(__jw_workspace_candidates)'
complete -c jw -n '__jw_using_subcommand switch s' -l at -r -d 'Create a new workspace at a revset'
complete -c jw -n '__jw_using_subcommand switch s' -s b -l bookmark -r -d 'Create a bookmark in a new workspace'
complete -c jw -n '__jw_using_subcommand switch s' -s x -l execute -r -d 'Run a command after switching'
complete -c jw -n '__jw_using_subcommand switch s' -s h -l help -d 'Print help'

complete -c jw -n '__jw_using_subcommand path' -f -a '(__jw_workspace_candidates)'
complete -c jw -n '__jw_using_subcommand path' -s h -l help -d 'Print help'

complete -c jw -n '__jw_using_subcommand remove rm' -f -a '(__jw_workspace_candidates)'
complete -c jw -n '__jw_using_subcommand remove rm' -l keep-dir -d 'Forget the workspace but keep its directory'
complete -c jw -n '__jw_using_subcommand remove rm' -s h -l help -d 'Print help'

complete -c jw -n '__jw_using_subcommand list l prune root current completions' -s h -l help -d 'Print help'

complete -c jw -n '__jw_using_subcommand completions' -f -a 'bash\tBash shell fish\tFish shell zsh\tZsh shell elvish\tElvish shell powershell\tPowerShell shell'

complete -c jw -n '__jw_using_subcommand shell' -f -a 'init\tEmit shell integration code completions\tGenerate shell completions help\tShow shell command help'
complete -c jw -n '__jw_using_subcommand shell; and __fish_seen_subcommand_from init completions' -f -a 'bash\tBash shell fish\tFish shell zsh\tZsh shell elvish\tElvish shell powershell\tPowerShell shell'
complete -c jw -n '__jw_using_subcommand shell' -s h -l help -d 'Print help'
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef jw

_jw_workspace_candidates() {
  local -a entries
  entries=("${(@f)$(command jw shell complete-workspaces 2>/dev/null)}")
  _describe -t workspaces 'workspace' entries
}

_jw_shell_names() {
  local -a shells
  shells=(
    'bash:Bash shell'
    'fish:Fish shell'
    'zsh:Zsh shell'
    'elvish:Elvish shell'
    'powershell:PowerShell shell'
  )
  _describe -t shells 'shell' shells
}

_jw() {
  local context state line
  typeset -A opt_args

  _arguments -C \
    '(-h --help)'{-h,--help}'[Print help]' \
    '(-V --version)'{-V,--version}'[Print version]' \
    '1:command:->command' \
    '*::arg:->args'

  case $state in
    command)
      local -a commands
      commands=(
        'switch:Switch to or create a workspace'
        's:Alias for switch'
        'list:List known workspaces'
        'l:Alias for list'
        'path:Print a workspace path'
        'remove:Forget a workspace'
        'rm:Alias for remove'
        'prune:Forget missing workspaces'
        'root:Print the current workspace root'
        'current:Print the current workspace name'
        'shell:Shell integration helpers'
        'completions:Generate shell completions'
        'help:Show command help'
      )
      _describe -t commands 'jw command' commands
      ;;
    args)
      case $words[2] in
        switch|s)
          _arguments \
            '--at[Create a new workspace at a revset]:revset:' \
            '(-b --bookmark)'{-b,--bookmark}'[Create a bookmark in a new workspace]:bookmark:' \
            '(-x --execute)'{-x,--execute}'[Run a command after switching]:command:_command_names' \
            '(-h --help)'{-h,--help}'[Print help]' \
            '1:workspace:_jw_workspace_candidates' \
            '*::args:_files'
          ;;
        path)
          _arguments \
            '(-h --help)'{-h,--help}'[Print help]' \
            '1:workspace:_jw_workspace_candidates'
          ;;
        remove|rm)
          _arguments \
            '--keep-dir[Forget the workspace but keep its directory]' \
            '(-h --help)'{-h,--help}'[Print help]' \
            '1:workspace:_jw_workspace_candidates'
          ;;
        list|l|prune|root|current)
          _arguments '(-h --help)'{-h,--help}'[Print help]'
          ;;
        completions)
          _arguments \
            '(-h --help)'{-h,--help}'[Print help]' \
            '1:shell:_jw_shell_names'
          ;;
        shell)
          if (( CURRENT == 3 )); then
            local -a shell_commands
            shell_commands=(
              'init:Emit shell integration code'
              'completions:Generate shell completions'
              'help:Show shell command help'
            )
            _describe -t shell-commands 'shell command' shell_commands
          elif [[ $words[3] == init || $words[3] == completions ]]; then
            _arguments '1:shell:_jw_shell_names'
          else
            _arguments '(-h --help)'{-h,--help}'[Print help]'
          fi
          ;;
        help)
          local -a help_commands
          help_commands=(
            'switch:Switch to or create a workspace'
            's:Alias for switch'
            'list:List known workspaces'
            'l:Alias for list'
            'path:Print a workspace path'
            'remove:Forget a workspace'
            'rm:Alias for remove'
            'prune:Forget missing workspaces'
            'root:Print the current workspace root'
            'current:Print the current workspace name'
            'shell:Shell integration helpers'
            'completions:Generate shell completions'
            'help:Show command help'
          )
          _describe -t commands 'jw command' help_commands
          ;;
      esac
      ;;
  esac
}

compdef _jw jw
"#;

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
        ShellKind::Fish => {
            out.write_all(FISH_COMPLETIONS.as_bytes())
                .context("failed to write fish completions")?;
            out.flush().context("failed to flush fish completions")?;
            Ok(())
        }
        ShellKind::Powershell => generate_to(Shell::PowerShell, command, out),
        ShellKind::Zsh => {
            out.write_all(ZSH_COMPLETIONS.as_bytes())
                .context("failed to write zsh completions")?;
            out.flush().context("failed to flush zsh completions")?;
            Ok(())
        }
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

command jw shell completions fish | source
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
    eval "$(command jw shell completions {shell_name})"
elif command -v complete >/dev/null 2>&1; then
    source <(command jw shell completions bash)
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

eval (command jw shell completions elvish)
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

Invoke-Expression (& jw shell completions powershell)
"#
        .to_owned()
}
