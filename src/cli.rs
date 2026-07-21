use crate::config::Config;
use crate::doctor::DoctorEngine;
use crate::jj::JjClient;
use crate::lifecycle::{self, AdoptionRequest, CreatedWorkspace, CreationPolicy};
use crate::links;
use crate::observe::{ObservationEngine, RefreshMode, resolve_workspace_token};
use crate::shell::{self, Shell};
use crate::snapshot::{SnapshotEnvelope, WorkingCopyStatus};
use crate::workspace;
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{ArgValueCompleter, CompletionCandidate};
use serde::Serialize;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as FmtWrite;
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
    #[command(
        visible_aliases = shell::SWITCH_ALIASES,
        about = "Switch to or create a workspace"
    )]
    Switch(SwitchCommand),
    #[command(aliases = ["l", "ls"], about = "List known workspaces")]
    List(ListCommand),
    #[command(about = "Explain one workspace's semantic state")]
    Status(StatusCommand),
    #[command(about = "Diagnose repository and workspace configuration")]
    Doctor(DoctorCommand),
    #[command(about = "Record an existing workspace as managed")]
    Adopt(AdoptCommand),
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
    #[arg(
        value_name = "NAME",
        num_args = 1..,
        required = true,
        add = ArgValueCompleter::new(complete_workspaces)
    )]
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
    #[arg(
        value_name = "NAME",
        num_args = 1..,
        required = true,
        add = ArgValueCompleter::new(complete_workspaces)
    )]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ListRefresh {
    None,
    Current,
    All,
}

impl From<ListRefresh> for RefreshMode {
    fn from(value: ListRefresh) -> Self {
        match value {
            ListRefresh::None => Self::None,
            ListRefresh::Current => Self::Current,
            ListRefresh::All => Self::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StatusRefresh {
    None,
    Current,
}

impl From<StatusRefresh> for RefreshMode {
    fn from(value: StatusRefresh) -> Self {
        match value {
            StatusRefresh::None => Self::None,
            StatusRefresh::Current => Self::Current,
        }
    }
}

#[derive(Debug, Args)]
struct ListCommand {
    #[arg(long, value_enum, default_value_t)]
    format: OutputFormat,
    #[arg(long, value_enum)]
    refresh: Option<ListRefresh>,
}

#[derive(Debug, Args)]
struct StatusCommand {
    #[arg(
        value_name = "WORKSPACE",
        default_value = "@",
        add = ArgValueCompleter::new(complete_workspaces)
    )]
    workspace: String,
    #[arg(long, value_enum, default_value_t)]
    format: OutputFormat,
    #[arg(long, value_enum, default_value = "current")]
    refresh: StatusRefresh,
}

#[derive(Debug, Args)]
struct DoctorCommand {
    #[arg(long, value_enum, default_value_t)]
    format: OutputFormat,
}

#[derive(Debug, Args)]
struct AdoptCommand {
    #[arg(
        value_name = "NAME",
        add = ArgValueCompleter::new(complete_workspaces)
    )]
    name: String,
    #[arg(long, value_name = "REVSET", required = true)]
    base: String,
    #[arg(
        long,
        value_name = "BOOKMARK",
        help = "Record a bookmark association without creating or moving the bookmark"
    )]
    bookmark: Option<String>,
}

#[derive(Debug, Args)]
struct PathCommand {
    #[arg(
        value_name = "NAME",
        add = ArgValueCompleter::new(complete_workspaces)
    )]
    name: String,
}

#[derive(Debug, Args)]
struct RemoveCommand {
    #[arg(
        value_name = "NAME",
        add = ArgValueCompleter::new(complete_workspaces)
    )]
    names: Vec<String>,
    #[arg(long, action = ArgAction::SetTrue, help = "Forget the workspace but keep its directory")]
    keep_dir: bool,
    #[arg(
        long,
        conflicts_with = "keep_bookmark",
        action = ArgAction::SetTrue,
        help = "Delete associated bookmarks without prompting"
    )]
    delete_bookmark: bool,
    #[arg(
        long,
        conflicts_with = "delete_bookmark",
        action = ArgAction::SetTrue,
        help = "Keep associated bookmarks without prompting"
    )]
    keep_bookmark: bool,
}

#[derive(Debug, Args)]
struct CompletionCommand {
    #[arg(value_enum)]
    shell: Shell,
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
}

#[derive(Debug, Args)]
struct ShellInitCommand {
    #[arg(value_enum)]
    shell: Shell,
}

pub fn run() -> Result<()> {
    shell::complete_if_requested(Cli::command);
    let cli = Cli::parse_from(normalized_args());

    match cli.command {
        Commands::Add(cmd) => run_add(cmd),
        Commands::Switch(cmd) => run_switch(cmd),
        Commands::List(cmd) => run_list(cmd),
        Commands::Status(cmd) => run_status(cmd),
        Commands::Doctor(cmd) => run_doctor(cmd),
        Commands::Adopt(cmd) => run_adopt(cmd),
        Commands::Path(cmd) => run_path(cmd),
        Commands::Remove(cmd) => run_remove(cmd),
        Commands::Prune => run_prune(),
        Commands::Root => print_line(workspace::workspace_root_current()?.display()),
        Commands::Current => print_line(workspace::current_workspace_name()?),
        Commands::Shell(cmd) => run_shell(cmd),
        Commands::Links(cmd) => run_links(cmd),
        Commands::Completions(cmd) => run_completions(cmd.shell),
    }
}

fn normalized_args() -> Vec<OsString> {
    let mut args = std::env::args_os().collect::<Vec<_>>();
    if args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| shell::SWITCH_SHORTHANDS.contains(&arg))
    {
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

fn run_list(cmd: ListCommand) -> Result<()> {
    if cmd.format == OutputFormat::Plain {
        if cmd.refresh.is_some() {
            bail!("--refresh requires --format=json for `jw list`");
        }
        return run_list_plain();
    }

    let config = Config::load()?;
    validate_trunk_revset(&config.trunk.revset)?;
    let envelope = ObservationEngine::new(JjClient::current()?, config.trunk.revset)?
        .capture_list(cmd.refresh.unwrap_or(ListRefresh::Current).into())?;
    write_json(&envelope)
}

// Compatibility contract: plain list remains the pre-snapshot implementation. In particular, it
// does not load config, metadata, or trunk and therefore keeps its exact historical output.
fn run_list_plain() -> Result<()> {
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

fn run_status(cmd: StatusCommand) -> Result<()> {
    let config = Config::load()?;
    validate_trunk_revset(&config.trunk.revset)?;
    let envelope = ObservationEngine::new(JjClient::current()?, config.trunk.revset)?
        .capture_status(&cmd.workspace, cmd.refresh.into())?;
    match cmd.format {
        OutputFormat::Json => write_json(&envelope),
        OutputFormat::Plain => write_text(&render_status_plain(&envelope)),
    }
}

fn run_doctor(cmd: DoctorCommand) -> Result<()> {
    let config = Config::load()?;
    let report = DoctorEngine::current(config.trunk.revset)?.run();
    match cmd.format {
        OutputFormat::Json => write_json(&report)?,
        OutputFormat::Plain => write_text(&report.render_plain())?,
    }
    if report.has_errors() {
        bail!("doctor found repository errors")
    }
    Ok(())
}

fn run_adopt(cmd: AdoptCommand) -> Result<()> {
    let current_client = JjClient::current()?;
    let resolved = resolve_workspace_token(&current_client, &cmd.name)?;
    let path = resolved
        .path
        .filter(|path| path.is_dir())
        .with_context(|| format!("workspace path is missing or unusable: {}", resolved.name))?;

    let result = lifecycle::adopt_workspace(&AdoptionRequest {
        workspace_name: resolved.name.clone(),
        workspace_root: path.clone(),
        base_revset: cmd.base,
        bookmark: cmd.bookmark,
    })?;

    let bookmark = result
        .metadata
        .associated_bookmark
        .as_deref()
        .unwrap_or("(none)");
    let output = format!(
        "Adopted workspace: {}\n  path: {}\n  creation base: {}\n  frozen operation: {}\n  current revision: {} ({})\n  bookmark: {}\n  stack analysis: deferred to milestone 1\n",
        result.metadata.workspace_name,
        path.display(),
        result.metadata.creation_base_commit_id,
        result.metadata.creation_operation_id,
        result.current_revision.commit_id,
        result.current_revision.change_id,
        bookmark,
    );
    write_text(&output)
}

fn validate_trunk_revset(revset: &str) -> Result<()> {
    if revset.trim().is_empty() {
        bail!("configured trunk revset is blank; set `[trunk].revset` to an exact-one revset")
    }
    Ok(())
}

fn render_status_plain(envelope: &SnapshotEnvelope) -> String {
    let workspace = envelope
        .workspaces
        .first()
        .expect("status envelope contains one workspace");
    let mut output = String::new();
    writeln!(output, "workspace: {}", workspace.name).expect("write string");
    writeln!(output, "operation: {}", envelope.repository.operation_id).expect("write string");
    writeln!(
        output,
        "path: {}",
        workspace
            .path
            .as_deref()
            .map_or_else(|| "(missing)".into(), |path| path.display().to_string())
    )
    .expect("write string");
    writeln!(
        output,
        "roles: current={} previous={} default={}",
        workspace.role.current, workspace.role.previous, workspace.role.default
    )
    .expect("write string");
    writeln!(output, "management: {:?}", workspace.management).expect("write string");
    writeln!(
        output,
        "working copy: {}{}",
        describe_working_copy(workspace.working_copy),
        if workspace.working_copy_refreshed {
            " (refreshed)"
        } else {
            " (not refreshed)"
        }
    )
    .expect("write string");
    writeln!(
        output,
        "revision: {} ({}) {}",
        workspace.commit_id, workspace.change_id, workspace.description
    )
    .expect("write string");
    writeln!(
        output,
        "trunk: {} ({})",
        envelope.repository.trunk.commit_id, envelope.repository.trunk.revset
    )
    .expect("write string");
    if workspace.hazards.is_empty() {
        output.push_str("hazards: none\n");
    } else {
        output.push_str("hazards:\n");
        for hazard in &workspace.hazards {
            writeln!(output, "  {:?}: {}", hazard.id, hazard.message).expect("write string");
        }
    }
    output
}

fn describe_working_copy(status: WorkingCopyStatus) -> String {
    match status {
        WorkingCopyStatus::Empty => "empty".to_owned(),
        WorkingCopyStatus::Modified {
            files,
            added,
            removed,
        } => format!("modified ({files} files, +{added}/-{removed})"),
        WorkingCopyStatus::Conflicted { conflicts } => {
            format!("conflicted ({conflicts} files)")
        }
        WorkingCopyStatus::Stale => "stale".to_owned(),
        WorkingCopyStatus::Unknown => "unknown".to_owned(),
    }
}

fn write_json(value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).context("failed to serialize JSON output")?;
    bytes.push(b'\n');
    write_bytes(&bytes)
}

fn write_text(value: &str) -> Result<()> {
    write_bytes(value.as_bytes())
}

fn write_bytes(bytes: &[u8]) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(bytes).context("failed to write stdout")?;
    Ok(())
}

fn run_path(cmd: PathCommand) -> Result<()> {
    print_line(workspace::path_for_workspace(&cmd.name)?.display())
}

fn run_remove(cmd: RemoveCommand) -> Result<()> {
    let delete_dir = !cmd.keep_dir;
    let inventory = workspace::WorkspaceInventory::load()?;
    let plans = if cmd.names.is_empty() {
        vec![workspace::plan_remove_workspace(
            &inventory, None, delete_dir,
        )?]
    } else {
        cmd.names
            .iter()
            .map(|name| {
                workspace::plan_remove_workspace(&inventory, Some(name), delete_dir)
                    .with_context(|| format!("failed to remove workspace {name}"))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut planned = HashSet::new();
    for plan in &plans {
        if !planned.insert(plan.workspace.as_str()) {
            bail!("workspace listed more than once: {}", plan.workspace)
        }
    }

    let mut choices = Vec::with_capacity(plans.len());
    for plan in plans {
        let delete_bookmarks = choose_bookmark_removal(&plan, &cmd)?;
        choices.push((plan, delete_bookmarks));
    }
    for (plan, delete_bookmarks) in choices {
        let result = workspace::execute_remove_workspace(plan, delete_bookmarks)?;
        print_remove_result(&result);
    }
    Ok(())
}

fn choose_bookmark_removal(plan: &workspace::RemovalPlan, cmd: &RemoveCommand) -> Result<bool> {
    Ok(if plan.bookmarks.is_empty() || cmd.keep_bookmark {
        false
    } else if cmd.delete_bookmark {
        true
    } else {
        prompt_delete_bookmarks(&plan.bookmarks)?
    })
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
        ShellSubcommand::Init(cmd) => print_line(shell::init_script(cmd.shell)?),
        ShellSubcommand::Completions(cmd) => run_completions(cmd.shell),
    }
}

fn run_completions(shell: Shell) -> Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    shell::write_completions(shell, &mut handle)?;
    Ok(())
}

fn run_links(cmd: LinksCommand) -> Result<()> {
    match cmd.command {
        LinksSubcommand::Apply => {
            let inventory = workspace::WorkspaceInventory::load()?;
            let config_root = inventory
                .root(inventory.default_name()?)
                .context("failed to locate default workspace link configuration")?;
            let report = links::apply_workspace_links(&config_root, inventory.current_root())?;
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

fn complete_workspaces(current: &OsStr) -> Vec<CompletionCandidate> {
    let current = current.to_string_lossy();
    let candidates = match workspace::completion_workspace_candidates() {
        Ok(candidates) => candidates,
        Err(error) => {
            eprintln!("jw: failed to complete workspaces: {error:#}");
            return Vec::new();
        }
    };

    candidates
        .into_iter()
        .filter(|(candidate, _)| candidate.starts_with(current.as_ref()))
        .map(|(candidate, description)| {
            CompletionCandidate::new(candidate).help(Some(description.into()))
        })
        .collect()
}
