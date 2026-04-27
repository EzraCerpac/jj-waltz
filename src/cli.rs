use crate::config::{self, Config};
use crate::links;
use crate::shell::{self, ShellKind};
use crate::workspace::{self, AddOptions, SwitchOptions};
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
    #[command(about = "Create one or more workspaces")]
    Add(AddCommand),
    #[command(alias = "s", about = "Switch to or create a workspace")]
    Switch(SwitchCommand),
    #[command(aliases = ["l", "ls"], about = "List known workspaces")]
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
struct AddCommand {
    #[arg(value_name = "NAME", num_args = 1.., required = true)]
    names: Vec<String>,
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
        long,
        action = ArgAction::SetTrue,
        help = "Do not create a bookmark for a new workspace"
    )]
    no_bookmark: bool,
    #[arg(long, action = ArgAction::SetTrue, help = "Skip applying workspace links")]
    no_links: bool,
}

#[derive(Debug, Args)]
struct SwitchCommand {
    #[arg(value_name = "NAME", num_args = 1.., required = true)]
    names: Vec<String>,
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
        long,
        action = ArgAction::SetTrue,
        help = "Do not create a bookmark for a new workspace"
    )]
    no_bookmark: bool,
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
    #[arg(last = true)]
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
    names: Vec<String>,
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
        Commands::Add(cmd) => run_add(cmd),
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

fn run_add(cmd: AddCommand) -> Result<()> {
    if cmd.bookmark.is_some() && cmd.no_bookmark {
        bail!("--bookmark and --no-bookmark cannot be used together")
    }
    if cmd.bookmark.is_some() && cmd.names.len() > 1 {
        bail!("--bookmark can only be used with a single workspace")
    }

    for name in &cmd.names {
        let bookmark = effective_bookmark(name, cmd.bookmark.as_deref(), cmd.no_bookmark)?;
        let result = workspace::add_workspace(
            name,
            &AddOptions {
                at_revset: cmd.at.clone(),
                bookmark,
            },
        )
        .with_context(|| format!("failed to add workspace {name}"))?;

        if !cmd.no_links {
            apply_links_for_path(&result.path, false)?;
        }

        println!("Created workspace: {}", result.workspace);
        println!("  path: {}", result.path.display());
        if let Some(bookmark) = result.bookmark {
            println!("  bookmark: {bookmark}");
        }
    }

    Ok(())
}

fn run_switch(cmd: SwitchCommand) -> Result<()> {
    if cmd.execute.is_none() && !cmd.execute_args.is_empty() {
        bail!("arguments after -- require --execute")
    }
    if cmd.bookmark.is_some() && cmd.no_bookmark {
        bail!("--bookmark and --no-bookmark cannot be used together")
    }
    if cmd.bookmark.is_some() && cmd.names.len() > 1 {
        bail!("--bookmark can only be used with a single workspace")
    }

    let (final_name, intermediate_names) = cmd
        .names
        .split_last()
        .expect("clap requires at least one workspace name");

    for name in intermediate_names {
        if workspace::workspace_exists(&workspace::resolve_workspace_token(name)?)? {
            continue;
        }
        let bookmark = effective_bookmark(name, None, cmd.no_bookmark)?;
        let result = workspace::add_workspace(
            name,
            &AddOptions {
                at_revset: cmd.at.clone(),
                bookmark,
            },
        )
        .with_context(|| format!("failed to add workspace {name}"))?;
        if !cmd.no_links {
            apply_links_for_path(&result.path, false)?;
        }
        if !cmd.print_path {
            println!("Created workspace: {}", result.workspace);
            println!("  path: {}", result.path.display());
            if let Some(bookmark) = result.bookmark {
                println!("  bookmark: {bookmark}");
            }
        }
    }

    let bookmark = effective_bookmark(final_name, cmd.bookmark.as_deref(), cmd.no_bookmark)?;

    let result = workspace::switch_workspace(
        final_name,
        &SwitchOptions {
            at_revset: cmd.at,
            bookmark,
            preserve_subdir: true,
        },
    )?;

    if !cmd.no_links {
        apply_links_for_path(&result.path, cmd.print_path)?;
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

fn effective_bookmark(
    workspace_name: &str,
    explicit_bookmark: Option<&str>,
    no_bookmark: bool,
) -> Result<Option<String>> {
    if no_bookmark {
        return Ok(None);
    }
    if let Some(bookmark) = explicit_bookmark {
        return Ok(Some(bookmark.to_owned()));
    }

    let config = Config::load()?;
    if !config.workspace.create_bookmark {
        return Ok(None);
    }

    let workspace = workspace::resolve_workspace_token(workspace_name)?;
    Ok(Some(config::bookmark_from_template(
        &config.workspace.bookmark_template,
        &workspace,
    )))
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
    if cmd.names.is_empty() {
        let (name, path) = workspace::remove_workspace(None, delete_dir)?;
        print_remove_result(&name, &path, delete_dir);
        return Ok(());
    }

    for name in &cmd.names {
        let (removed_name, path) = workspace::remove_workspace(Some(name), delete_dir)
            .with_context(|| format!("failed to remove workspace {name}"))?;
        print_remove_result(&removed_name, &path, delete_dir);
    }
    Ok(())
}

fn print_remove_result(name: &str, path: &PathBuf, delete_dir: bool) {
    println!("Forgot workspace: {name}");
    if delete_dir {
        println!("Deleted directory: {}", path.display());
    }
}

fn apply_links_for_path(path: &PathBuf, quiet: bool) -> Result<()> {
    let config_root = workspace::default_workspace_root().unwrap_or_else(|_| path.clone());
    let links_report = links::apply_workspace_links_with_config_root(&config_root, path)?;
    if !quiet && links_report.has_entries() {
        println!(
            "Links: {} created, {} already satisfied, {} missing target",
            links_report.linked, links_report.satisfied, links_report.skipped_missing_target
        );
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
