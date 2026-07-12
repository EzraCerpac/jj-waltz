use crate::lifecycle::{self, CreatedWorkspace, CreationPolicy};
use crate::links;
use crate::shell::{self, ShellKind};
use crate::workspace;
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
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
    #[arg(
        long,
        conflicts_with = "keep_bookmark",
        action = ArgAction::SetTrue,
        help = "Delete bookmarks pointing at the workspace without prompting"
    )]
    delete_bookmark: bool,
    #[arg(
        long,
        conflicts_with = "delete_bookmark",
        action = ArgAction::SetTrue,
        help = "Keep bookmarks pointing at the workspace without prompting"
    )]
    keep_bookmark: bool,
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
    let cli = Cli::parse_from(normalized_args());

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

fn normalized_args() -> Vec<OsString> {
    let mut args = std::env::args_os().collect::<Vec<_>>();
    if matches!(args.get(1).and_then(|arg| arg.to_str()), Some("^" | "-")) {
        let target = args[1].clone();
        args[1] = OsString::from("switch");
        args.insert(2, target);
    }
    args
}

fn run_add(cmd: AddCommand) -> Result<()> {
    let policy = CreationPolicy::load(
        cmd.at,
        cmd.bookmark,
        cmd.no_bookmark,
        cmd.no_links,
        cmd.names.len(),
    )?;
    for created in lifecycle::add_workspaces(&cmd.names, &policy)? {
        print_created_workspace(&created, false);
    }

    Ok(())
}

fn run_switch(cmd: SwitchCommand) -> Result<()> {
    if cmd.execute.is_none() && !cmd.execute_args.is_empty() {
        bail!("arguments after -- require --execute")
    }
    let policy = CreationPolicy::load(
        cmd.at,
        cmd.bookmark,
        cmd.no_bookmark,
        cmd.no_links,
        cmd.names.len(),
    )?;
    let outcome = lifecycle::switch_workspaces(&cmd.names, &policy)?;
    for created in &outcome.intermediate {
        if !cmd.print_path {
            print_created_workspace(created, false);
        }
    }
    print_links_report(outcome.links.as_ref(), cmd.print_path);
    let result = outcome.result;

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

fn print_created_workspace(created: &CreatedWorkspace, quiet: bool) {
    print_links_report(created.links.as_ref(), quiet);
    if quiet {
        return;
    }
    println!("Created workspace: {}", created.result.workspace);
    println!("  path: {}", created.result.path.display());
    if let Some(bookmark) = &created.result.bookmark {
        println!("  bookmark: {bookmark}");
    }
}

fn print_links_report(report: Option<&links::LinkApplyReport>, quiet: bool) {
    if quiet {
        return;
    }
    if let Some(report) = report
        && report.has_entries()
    {
        println!(
            "Links: {} created, {} already satisfied, {} missing target",
            report.linked, report.satisfied, report.skipped_missing_target
        );
    }
}

fn run_list() -> Result<()> {
    let inventory = workspace::WorkspaceInventory::load()?;
    for entry in inventory.entries() {
        let marker = inventory.marker(&entry.name);

        let path = entry
            .root
            .as_ref()
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
        remove_one(None, delete_dir, &cmd)?;
        return Ok(());
    }

    for name in &cmd.names {
        remove_one(Some(name), delete_dir, &cmd)
            .with_context(|| format!("failed to remove workspace {name}"))?;
    }
    Ok(())
}

fn remove_one(token: Option<&str>, delete_dir: bool, cmd: &RemoveCommand) -> Result<()> {
    let plan = workspace::plan_remove_workspace(token, delete_dir)?;
    let delete_bookmarks = if plan.bookmarks.is_empty() || cmd.keep_bookmark {
        false
    } else if cmd.delete_bookmark {
        true
    } else {
        prompt_delete_bookmarks(&plan.bookmarks)?
    };
    let result = workspace::execute_remove_workspace(plan, delete_bookmarks)?;
    print_remove_result(&result);
    Ok(())
}

fn prompt_delete_bookmarks(bookmarks: &[String]) -> Result<bool> {
    let label = if bookmarks.len() == 1 {
        format!("bookmark '{}'", bookmarks[0])
    } else {
        format!("bookmarks {}", bookmarks.join(", "))
    };
    let mut stderr = io::stderr().lock();
    write!(stderr, "Delete associated {label}? [y/N] ")
        .context("failed to write bookmark prompt")?;
    stderr.flush().context("failed to flush bookmark prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read bookmark prompt")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_remove_result(result: &workspace::RemovalResult) {
    println!("Forgot workspace: {}", result.workspace);
    if result.deleted_dir {
        println!("Deleted directory: {}", result.path.display());
    }
    for bookmark in &result.deleted_bookmarks {
        println!("Deleted bookmark: {bookmark}");
    }
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

fn run_execute(cwd: &Path, command: &str, args: &[String]) -> Result<()> {
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
