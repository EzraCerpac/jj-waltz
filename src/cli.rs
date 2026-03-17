use crate::links;
use crate::shell::{self, ShellKind};
use crate::workspace::{self, SwitchOptions};
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Parser)]
#[command(
    name = "jw",
    version,
    about = "Jujutsu workspace switching",
    long_about = None,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(alias = "s", about = "Switch to or create a workspace")]
    Switch(SwitchCommand),
    #[command(alias = "l", about = "List known workspaces")]
    List,
    #[command(about = "Print a workspace path")]
    Path(PathCommand),
    #[command(alias = "rm", about = "Forget a workspace")]
    Remove(RemoveCommand),
    #[command(about = "Forget missing workspaces")]
    Prune,
    #[command(about = "Print the current workspace root")]
    Root,
    #[command(about = "Print the current workspace name")]
    Current,
    #[command(about = "Shell integration helpers")]
    Shell(ShellCommand),
    #[command(about = "Manage workspace links")]
    Links(LinksCommand),
    #[command(about = "Generate shell completions")]
    Completions(CompletionCommand),
}

#[derive(Debug, Args)]
struct SwitchCommand {
    #[arg(value_name = "NAME")]
    name: String,
    #[arg(
        long,
        value_name = "REVSET",
        help = "Create a new workspace at a revset"
    )]
    at: Option<String>,
    #[arg(
        short,
        long,
        value_name = "BOOKMARK",
        help = "Create a bookmark in a new workspace"
    )]
    bookmark: Option<String>,
    #[arg(
        short = 'x',
        long,
        value_name = "COMMAND",
        help = "Run a command after switching"
    )]
    execute: Option<String>,
    #[arg(long, hide = true, action = ArgAction::SetTrue)]
    print_path: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Skip applying workspace links")]
    no_links: bool,
    #[arg(trailing_var_arg = true)]
    execute_args: Vec<String>,
}

#[derive(Debug, Args)]
struct LinksCommand {
    #[command(subcommand)]
    command: LinksSubcommand,
}

#[derive(Debug, Subcommand)]
enum LinksSubcommand {
    #[command(about = "Apply configured links to the current workspace")]
    Apply,
}

#[derive(Debug, Args)]
struct PathCommand {
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Debug, Args)]
struct RemoveCommand {
    #[arg(value_name = "NAME")]
    name: Option<String>,
    #[arg(long, action = ArgAction::SetTrue, help = "Forget the workspace but keep its directory")]
    keep_dir: bool,
}

#[derive(Debug, Args)]
struct CompletionCommand {
    #[arg(value_enum)]
    shell: ShellArg,
}

#[derive(Debug, Args)]
struct ShellCommand {
    #[command(subcommand)]
    command: ShellSubcommand,
}

#[derive(Debug, Subcommand)]
enum ShellSubcommand {
    Init(ShellInitCommand),
    Completions(CompletionCommand),
    #[command(hide = true)]
    CompleteWorkspaces,
}

#[derive(Debug, Args)]
struct ShellInitCommand {
    #[arg(value_enum)]
    shell: ShellArg,
}

#[derive(Clone, Debug, ValueEnum)]
enum ShellArg {
    Bash,
    Elvish,
    Fish,
    Powershell,
    Zsh,
}

impl From<ShellArg> for ShellKind {
    fn from(value: ShellArg) -> Self {
        match value {
            ShellArg::Bash => ShellKind::Bash,
            ShellArg::Elvish => ShellKind::Elvish,
            ShellArg::Fish => ShellKind::Fish,
            ShellArg::Powershell => ShellKind::Powershell,
            ShellArg::Zsh => ShellKind::Zsh,
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Switch(cmd) => run_switch(cmd),
        Commands::List => run_list(),
        Commands::Path(cmd) => run_path(cmd),
        Commands::Remove(cmd) => run_remove(cmd),
        Commands::Prune => run_prune(),
        Commands::Root => print_line(workspace::workspace_root_current()?.display()),
        Commands::Current => print_line(workspace::current_workspace_name()?),
        Commands::Shell(cmd) => run_shell(cmd),
        Commands::Links(cmd) => run_links(cmd),
        Commands::Completions(cmd) => run_completions(cmd.shell.into()),
    }
}

fn run_switch(cmd: SwitchCommand) -> Result<()> {
    if cmd.execute.is_none() && !cmd.execute_args.is_empty() {
        bail!("arguments after -- require --execute")
    }

    let result = workspace::switch_workspace(
        &cmd.name,
        &SwitchOptions {
            at_revset: cmd.at,
            bookmark: cmd.bookmark.clone(),
            preserve_subdir: true,
        },
    )?;

    if !cmd.no_links {
        let links_report = links::apply_workspace_links(&result.path)?;
        if !cmd.print_path && links_report.has_entries() {
            println!(
                "Links: {} created, {} already satisfied, {} missing target",
                links_report.linked, links_report.satisfied, links_report.skipped_missing_target
            );
        }
    }

    if cmd.print_path {
        let path = match result.relative_subdir {
            Some(relative) => {
                let candidate = result.path.join(relative);
                if candidate.is_dir() {
                    candidate
                } else {
                    result.path.clone()
                }
            }
            None => result.path.clone(),
        };
        return print_line(path.display());
    }

    if let Some(command) = cmd.execute {
        return run_execute(&result.path, &command, &cmd.execute_args);
    }

    if result.created {
        println!("Created workspace: {}", result.workspace);
    } else {
        println!("Switched workspace: {}", result.workspace);
    }
    println!("  path: {}", result.path.display());
    if let Some(bookmark) = result.bookmark {
        println!("  bookmark: {bookmark}");
    }
    Ok(())
}

fn run_list() -> Result<()> {
    let entries = workspace::workspace_entries()?;
    let current = workspace::current_workspace_name().ok();
    let previous = workspace::previous_workspace_name().ok();
    let default = workspace::default_workspace_name().ok();

    for entry in entries {
        let marker = if current.as_deref() == Some(entry.name.as_str()) {
            '@'
        } else if previous.as_deref() == Some(entry.name.as_str()) {
            '-'
        } else if default.as_deref() == Some(entry.name.as_str()) {
            '^'
        } else {
            ' '
        };

        let path = entry
            .root
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(missing)".to_owned());
        println!("{marker} {}\t{path}", entry.name);
    }

    Ok(())
}

fn run_path(cmd: PathCommand) -> Result<()> {
    print_line(workspace::path_for_workspace(&cmd.name)?.display())
}

fn run_remove(cmd: RemoveCommand) -> Result<()> {
    let delete_dir = !cmd.keep_dir;
    let (name, path) = workspace::remove_workspace(cmd.name.as_deref(), delete_dir)?;
    println!("Forgot workspace: {name}");
    if delete_dir {
        println!("Deleted directory: {}", path.display());
    }
    Ok(())
}

fn run_prune() -> Result<()> {
    let removed = workspace::prune_missing_workspaces()?;
    for name in &removed {
        println!("Forgetting missing workspace: {name}");
    }
    println!("Pruned {} workspace(s)", removed.len());
    Ok(())
}

fn run_shell(cmd: ShellCommand) -> Result<()> {
    match cmd.command {
        ShellSubcommand::Init(cmd) => print_line(shell::init_script(cmd.shell.into())?),
        ShellSubcommand::Completions(cmd) => run_completions(cmd.shell.into()),
        ShellSubcommand::CompleteWorkspaces => run_complete_workspaces(),
    }
}

fn run_completions(shell: ShellKind) -> Result<()> {
    let mut command = Cli::command();
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    shell::write_completions(shell, &mut command, &mut handle)?;
    Ok(())
}

fn run_links(cmd: LinksCommand) -> Result<()> {
    match cmd.command {
        LinksSubcommand::Apply => {
            let root = workspace::workspace_root_current()?;
            let report = links::apply_workspace_links(&root)?;
            println!(
                "Links: {} created, {} already satisfied, {} missing target",
                report.linked, report.satisfied, report.skipped_missing_target
            );
            Ok(())
        }
    }
}

fn run_execute(cwd: &PathBuf, command: &str, args: &[String]) -> Result<()> {
    let status = if cfg!(windows) {
        let mut full = String::from(command);
        for arg in args {
            full.push(' ');
            full.push_str(&shlex::try_quote(arg).unwrap_or_else(|_| arg.into()));
        }
        Command::new("cmd")
            .arg("/C")
            .arg(full)
            .current_dir(cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to execute command")?
    } else {
        let mut full = String::from(command);
        for arg in args {
            full.push(' ');
            full.push_str(&shlex::try_quote(arg).unwrap_or_else(|_| arg.into()));
        }
        Command::new("sh")
            .arg("-lc")
            .arg(full)
            .current_dir(cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to execute command")?
    };

    if status.success() {
        Ok(())
    } else {
        bail!("execute command exited with {status}")
    }
}

fn print_line(value: impl std::fmt::Display) -> Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{value}").context("failed to write stdout")?;
    Ok(())
}

fn run_complete_workspaces() -> Result<()> {
    let mut stdout = io::stdout().lock();
    for (candidate, description) in workspace::completion_workspace_candidates()? {
        writeln!(stdout, "{candidate}\t{description}").context("failed to write stdout")?;
    }
    Ok(())
}
