//! Herdr UI adapter for jj-waltz.
//!
//! Workspace behavior deliberately stays in the bundled `jw` process. This binary
//! only reads Herdr context, renders prompts, and connects both CLIs.
//!
//! Modal rendering is adapted from Nathan Flurry's MIT-licensed
//! `herdr-plugin-jj-workspace`; see `LICENSE-THIRD-PARTY`.

mod removal;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use serde::{Deserialize, Serialize};

use removal::{CloseTarget, RemovalEffects, RemovalPlan, execute_removal};

type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
struct AppError {
    message: String,
    wait_for_enter: bool,
}

impl AppError {
    fn action(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            wait_for_enter: false,
        }
    }

    fn pane(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            wait_for_enter: true,
        }
    }
}

impl From<io::Error> for AppError {
    fn from(error: io::Error) -> Self {
        Self::pane(format!("terminal error: {error}"))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.message);
            if error.wait_for_enter {
                eprint!("\npress enter to close...");
                let _ = io::stderr().flush();
                let mut line = String::new();
                let _ = io::stdin().read_line(&mut line);
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("open") => cmd_open(args.next().as_deref()),
        Some("create") => cmd_create(),
        Some("remove") => cmd_remove(),
        _ => Err(AppError::action(
            "usage: herdr-jj-waltz <open <workspace|tab|remove> | create | remove>",
        )),
    }
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct InvocationContext {
    workspace_id: Option<String>,
    workspace_cwd: Option<String>,
    tab_id: Option<String>,
    focused_pane_cwd: Option<String>,
}

fn invocation_context() -> Result<InvocationContext> {
    let raw = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .map_err(|_| AppError::action("HERDR_PLUGIN_CONTEXT_JSON is missing"))?;
    serde_json::from_str(&raw)
        .map_err(|error| AppError::action(format!("invalid Herdr plugin context: {error}")))
}

fn source_cwd(context: &InvocationContext) -> Option<PathBuf> {
    context
        .focused_pane_cwd
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            context
                .workspace_cwd
                .as_deref()
                .filter(|value| !value.is_empty())
        })
        .map(PathBuf::from)
}

fn cmd_open(kind: Option<&str>) -> Result<()> {
    let context = invocation_context()?;
    let source = source_cwd(&context)
        .ok_or_else(|| AppError::action("focused pane and workspace have no cwd"))?;
    let (entrypoint, mode) = match kind {
        Some("workspace") => ("create", Some("workspace")),
        Some("tab") => ("create", Some("tab")),
        Some("remove") => ("remove", None),
        _ => {
            return Err(AppError::action(
                "open mode must be workspace, tab, or remove",
            ));
        }
    };

    let mut command = Command::new(herdr_bin());
    command.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        &plugin_id(),
        "--entrypoint",
        entrypoint,
    ]);
    push_env(&mut command, "JW_HERDR_SOURCE_CWD", source.as_os_str());
    if let Some(mode) = mode {
        push_env(&mut command, "JW_HERDR_OPEN_MODE", mode);
    }
    if let Some(value) = context.workspace_id.as_deref() {
        push_env(&mut command, "JW_HERDR_WORKSPACE_ID", value);
    }
    if let Some(value) = context.tab_id.as_deref() {
        push_env(&mut command, "JW_HERDR_TAB_ID", value);
    }
    command.arg("--focus");

    checked_status(command, "open Herdr plugin pane").map_err(AppError::action)
}

fn push_env(command: &mut Command, key: &str, value: impl Into<OsString>) {
    let mut assignment = OsString::from(key);
    assignment.push("=");
    assignment.push(value.into());
    command.arg("--env").arg(assignment);
}

fn cmd_create() -> Result<()> {
    let source = required_path_env("JW_HERDR_SOURCE_CWD")?;
    let mode =
        env::var("JW_HERDR_OPEN_MODE").map_err(|_| AppError::pane("missing JW_HERDR_OPEN_MODE"))?;
    if !matches!(mode.as_str(), "workspace" | "tab") {
        return Err(AppError::pane("open mode must be workspace or tab"));
    }

    let repo = PathBuf::from(jw_output(&["root"], &source).map_err(AppError::pane)?);
    let Some(name) = run_create_modal(generated_name(seed()))? else {
        return Ok(());
    };

    jw_output(&["add", &name], &repo).map_err(AppError::pane)?;
    let path = PathBuf::from(jw_output(&["path", &name], &repo).map_err(AppError::pane)?);

    let mut open = Command::new(herdr_bin());
    if mode == "workspace" {
        open.args(["workspace", "create", "--cwd"])
            .arg(&path)
            .args(["--label", &name, "--focus"]);
    } else {
        let workspace_id = env::var("JW_HERDR_WORKSPACE_ID")
            .map_err(|_| AppError::pane("missing source Herdr workspace id"))?;
        open.args(["tab", "create", "--workspace", &workspace_id, "--cwd"])
            .arg(&path)
            .args(["--label", &name, "--focus"]);
    }

    let response = checked_output(open, "open created workspace in Herdr").map_err(|error| {
        AppError::pane(format!(
            "{error}\n\nJJ workspace remains at {}\nrecover with: jw remove {name}",
            path.display()
        ))
    })?;
    let (kind, id) = created_container(&mode, &response).map_err(AppError::pane)?;
    write_container_record(kind, &id, &canonical(&path).map_err(AppError::pane)?)
        .map_err(AppError::pane)
}

fn cmd_remove() -> Result<()> {
    let source = required_path_env("JW_HERDR_SOURCE_CWD")?;
    let root = canonical(Path::new(
        &jw_output(&["root"], &source).map_err(AppError::pane)?,
    ))
    .map_err(AppError::pane)?;
    let default_root = canonical(Path::new(
        &jw_output(&["path", "^"], &root).map_err(AppError::pane)?,
    ))
    .map_err(AppError::pane)?;
    if is_default_workspace(&root, &default_root) {
        return Err(AppError::pane(
            "refusing to remove the default JJ workspace",
        ));
    }

    let name = jw_output(&["current"], &root).map_err(AppError::pane)?;
    let (target, marker) = close_target(
        &root,
        env::var("JW_HERDR_WORKSPACE_ID").ok(),
        env::var("JW_HERDR_TAB_ID").ok(),
    )
    .map_err(AppError::pane)?;

    if !run_remove_modal(&name, &root, target.label())? {
        return Ok(());
    }

    let mut effects = CommandRemovalEffects;
    execute_removal(
        RemovalPlan {
            name: &name,
            default_root: &default_root,
            target: &target,
            marker: marker.as_deref(),
        },
        &mut effects,
    )
    .map_err(|error| AppError::pane(error.to_string()))
}

struct CommandRemovalEffects;

impl RemovalEffects for CommandRemovalEffects {
    fn remove_workspace(
        &mut self,
        name: &str,
        default_root: &Path,
    ) -> std::result::Result<(), String> {
        jw_output(&["remove", name], default_root).map(|_| ())
    }

    fn close_container(&mut self, target: &CloseTarget) -> std::result::Result<(), String> {
        let mut close = Command::new(herdr_bin());
        match target {
            CloseTarget::Workspace(id) => {
                close.args(["workspace", "close", id]);
            }
            CloseTarget::Tab(id) => {
                close.args(["tab", "close", id]);
            }
        }
        checked_status(close, "close removed checkout in Herdr")
    }

    fn clear_marker(&mut self, marker: &Path) -> std::result::Result<(), String> {
        fs::remove_file(marker)
            .map_err(|error| format!("cannot remove {}: {error}", marker.display()))
    }
}

fn required_path_env(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| AppError::pane(format!("missing {name}")))
}

fn jw_output(args: &[&str], cwd: &Path) -> std::result::Result<String, String> {
    let mut command = Command::new(jw_bin()?);
    command.current_dir(cwd).args(args);
    checked_output(command, &format!("jw {}", args.join(" ")))
}

fn jw_bin() -> std::result::Result<PathBuf, String> {
    let root = env::var_os("HERDR_PLUGIN_ROOT")
        .map(PathBuf::from)
        .ok_or_else(|| "HERDR_PLUGIN_ROOT is missing".to_owned())?;
    let path = root
        .join("target")
        .join("jj-waltz")
        .join("release")
        .join("jw");
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("bundled jw not found at {}", path.display()))
    }
}

fn herdr_bin() -> OsString {
    env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"))
}

fn plugin_id() -> String {
    env::var("HERDR_PLUGIN_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ezracerpac.jj-waltz".to_owned())
}

fn checked_output(mut command: Command, label: &str) -> std::result::Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{label} failed to start: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(command_error(label, &output))
    }
}

fn checked_status(mut command: Command, label: &str) -> std::result::Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("{label} failed to start: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(label, &output))
    }
}

fn command_error(label: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or("no command output");
    format!(
        "{label} failed (exit {}): {detail}",
        output.status.code().unwrap_or(-1)
    )
}

fn canonical(path: &Path) -> std::result::Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| format!("cannot resolve {}: {error}", path.display()))
}

fn is_default_workspace(root: &Path, default_root: &Path) -> bool {
    root == default_root
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ContainerKind {
    Workspace,
    Tab,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ContainerRecord {
    kind: ContainerKind,
    id: String,
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct HerdrCreateResponse {
    result: HerdrCreateResult,
}

#[derive(Debug, Deserialize)]
struct HerdrCreateResult {
    workspace: Option<HerdrWorkspace>,
    tab: Option<HerdrTab>,
}

#[derive(Debug, Deserialize)]
struct HerdrWorkspace {
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct HerdrTab {
    tab_id: String,
}

fn created_container(
    mode: &str,
    response: &str,
) -> std::result::Result<(ContainerKind, String), String> {
    let response: HerdrCreateResponse = serde_json::from_str(response)
        .map_err(|error| format!("invalid Herdr create response: {error}"))?;
    match mode {
        "workspace" => response
            .result
            .workspace
            .map(|workspace| (ContainerKind::Workspace, workspace.workspace_id))
            .ok_or_else(|| "Herdr create response has no workspace".to_owned()),
        "tab" => response
            .result
            .tab
            .map(|tab| (ContainerKind::Tab, tab.tab_id))
            .ok_or_else(|| "Herdr create response has no tab".to_owned()),
        _ => Err(format!("unknown container mode: {mode}")),
    }
}

fn state_dir() -> std::result::Result<PathBuf, String> {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HERDR_PLUGIN_STATE_DIR is missing".to_owned())
}

fn marker_path(kind: ContainerKind, id: &str) -> std::result::Result<PathBuf, String> {
    let kind = match kind {
        ContainerKind::Workspace => "workspace",
        ContainerKind::Tab => "tab",
    };
    let id = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    Ok(state_dir()?
        .join("containers")
        .join(format!("{kind}-{id}.json")))
}

fn write_container_record(
    kind: ContainerKind,
    id: &str,
    root: &Path,
) -> std::result::Result<(), String> {
    let path = marker_path(kind, id)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let record = ContainerRecord {
        kind,
        id: id.to_owned(),
        root: root.to_path_buf(),
    };
    let contents = serde_json::to_vec(&record)
        .map_err(|error| format!("cannot encode Herdr container marker: {error}"))?;
    fs::write(&path, contents).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn read_container_record(kind: ContainerKind, id: &str) -> Option<(ContainerRecord, PathBuf)> {
    let path = marker_path(kind, id).ok()?;
    let contents = fs::read(&path).ok()?;
    let record = serde_json::from_slice(&contents).ok()?;
    Some((record, path))
}

fn close_target(
    root: &Path,
    workspace_id: Option<String>,
    tab_id: Option<String>,
) -> std::result::Result<(CloseTarget, Option<PathBuf>), String> {
    if let Some(id) = tab_id.as_deref()
        && let Some((record, marker)) = read_container_record(ContainerKind::Tab, id)
        && record.kind == ContainerKind::Tab
        && record.id == id
        && record.root == root
    {
        return Ok((CloseTarget::Tab(id.to_owned()), Some(marker)));
    }
    if let Some(id) = workspace_id.as_deref()
        && let Some((record, marker)) = read_container_record(ContainerKind::Workspace, id)
        && record.kind == ContainerKind::Workspace
        && record.id == id
        && record.root == root
    {
        return Ok((CloseTarget::Workspace(id.to_owned()), Some(marker)));
    }

    match (workspace_id, tab_id) {
        (Some(workspace_id), Some(tab_id)) if tab_id != format!("{workspace_id}:t1") => {
            Ok((CloseTarget::Tab(tab_id), None))
        }
        (Some(workspace_id), _) => Ok((CloseTarget::Workspace(workspace_id), None)),
        (None, Some(tab_id)) => Ok((CloseTarget::Tab(tab_id), None)),
        (None, None) => Err("missing Herdr workspace and tab ids".to_owned()),
    }
}

// Modal rendering adapted from NathanFlurry/herdr-plugin-jj-workspace.

struct Palette {
    accent: Color,
    panel_bg: Color,
    surface0: Color,
    surface_dim: Color,
    overlay0: Color,
    text: Color,
    subtext0: Color,
    red: Color,
}

fn palette() -> Palette {
    Palette {
        accent: Color::Rgb(137, 180, 250),
        panel_bg: Color::Rgb(24, 24, 37),
        surface0: Color::Rgb(49, 50, 68),
        surface_dim: Color::Rgb(30, 30, 46),
        overlay0: Color::Rgb(108, 112, 134),
        text: Color::Rgb(205, 214, 244),
        subtext0: Color::Rgb(166, 173, 200),
        red: Color::Rgb(243, 139, 168),
    }
}

fn run_create_modal(initial: String) -> io::Result<Option<String>> {
    let mut name = initial;
    let mut replace_on_type = true;
    let mut error = None;

    with_terminal(|terminal| {
        loop {
            terminal.draw(|frame| draw_create_modal(frame, &name, error.as_deref()))?;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match handle_create_key(
                        &mut name,
                        &mut replace_on_type,
                        key.code,
                        key.modifiers,
                    ) {
                        CreateAction::Continue => {
                            error = None;
                        }
                        CreateAction::Cancel => break Ok(None),
                        CreateAction::Submit(candidate) if candidate.is_empty() => {
                            error = Some("workspace name cannot be empty".to_owned());
                        }
                        CreateAction::Submit(candidate) => break Ok(Some(candidate)),
                    }
                }
                _ => {}
            }
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum CreateAction {
    Continue,
    Cancel,
    Submit(String),
}

fn handle_create_key(
    name: &mut String,
    replace_on_type: &mut bool,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> CreateAction {
    match code {
        KeyCode::Esc => CreateAction::Cancel,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => CreateAction::Cancel,
        KeyCode::Enter => CreateAction::Submit(name.trim().to_owned()),
        KeyCode::Backspace => {
            if *replace_on_type {
                name.clear();
                *replace_on_type = false;
            } else {
                name.pop();
            }
            CreateAction::Continue
        }
        KeyCode::Char(character)
            if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if *replace_on_type {
                name.clear();
                *replace_on_type = false;
            }
            name.push(character);
            CreateAction::Continue
        }
        _ => CreateAction::Continue,
    }
}

fn run_remove_modal(name: &str, path: &Path, target: &str) -> io::Result<bool> {
    with_terminal(|terminal| {
        loop {
            terminal.draw(|frame| draw_remove_modal(frame, name, path, target))?;
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(confirmed) = confirmation_action(key.code, key.modifiers) {
                        break Ok(confirmed);
                    }
                }
                _ => {}
            }
        }
    })
}

fn confirmation_action(code: KeyCode, modifiers: KeyModifiers) -> Option<bool> {
    match code {
        KeyCode::Enter => Some(true),
        KeyCode::Esc => Some(false),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Some(false),
        _ => None,
    }
}

fn with_terminal<T>(
    run: impl FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<T>,
) -> io::Result<T> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(error) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error);
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            return Err(error);
        }
    };

    let outcome = run(&mut terminal);
    let restore = restore_terminal(&mut terminal);
    match (outcome, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let raw_mode = disable_raw_mode();
    let alternate_screen = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    raw_mode.and(alternate_screen)
}

fn draw_create_modal(frame: &mut Frame, name: &str, error: Option<&str>) {
    let palette = palette();
    let area = frame.area();
    dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 68, 10, &palette) else {
        return;
    };
    if inner.height < 7 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<7>(inner);

    render_modal_header(frame, rows[0], "new jw workspace", &palette);
    frame.render_widget(
        Paragraph::new(" workspace").style(Style::default().fg(palette.overlay0)),
        rows[1],
    );
    frame.render_widget(Clear, rows[2]);
    frame.render_widget(
        Paragraph::new(format!(" {name}█"))
            .style(Style::default().fg(palette.text).bg(palette.surface0)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" path, links, and bookmark resolved by jw")
            .style(Style::default().fg(palette.subtext0)),
        rows[3],
    );
    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(palette.red)),
            rows[4],
        );
    }
    render_buttons(
        frame,
        inner,
        ("↵", "create and open"),
        ("esc", "cancel"),
        &palette,
    );
}

fn draw_remove_modal(frame: &mut Frame, name: &str, path: &Path, target: &str) {
    let palette = palette();
    let area = frame.area();
    dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 72, 11, &palette) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<7>(inner);

    render_modal_header(frame, rows[0], "remove jw workspace", &palette);
    frame.render_widget(
        Paragraph::new(format!(" workspace  {name}")).style(Style::default().fg(palette.text)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(format!(" checkout   {}", path.display()))
            .style(Style::default().fg(palette.subtext0)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(format!(" closes     {target}"))
            .style(Style::default().fg(palette.subtext0)),
        rows[4],
    );
    frame.render_widget(
        Paragraph::new(" checkout directory will be deleted").style(
            Style::default()
                .fg(palette.red)
                .add_modifier(Modifier::BOLD),
        ),
        rows[5],
    );
    render_buttons(frame, inner, ("↵", "remove"), ("esc", "cancel"), &palette);
}

fn dim_background(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buffer[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::DIM));
        }
    }
}

fn render_modal_shell(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    height: u16,
    palette: &Palette,
) -> Option<Rect> {
    let popup = centered_popup_rect(area, width, height)?;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.accent))
        .border_set(symbols::border::PLAIN)
        .style(Style::default().bg(palette.panel_bg));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    Some(inner)
}

fn centered_popup_rect(area: Rect, width: u16, height: u16) -> Option<Rect> {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    if width < 4 || height < 4 {
        return None;
    }
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    ))
}

fn render_modal_header(frame: &mut Frame, area: Rect, title: &str, palette: &Palette) {
    let line = Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_buttons(
    frame: &mut Frame,
    inner: Rect,
    primary: (&str, &str),
    secondary: (&str, &str),
    palette: &Palette,
) {
    let primary_text = button_text(primary.0, primary.1);
    let secondary_text = button_text(secondary.0, secondary.1);
    let gap = 2;
    let total = primary_text.chars().count() as u16 + secondary_text.chars().count() as u16 + gap;
    let mut x = inner.x + inner.width.saturating_sub(total) / 2;
    let y = inner.y + inner.height.saturating_sub(1);
    let primary_rect = Rect::new(x, y, primary_text.chars().count() as u16, 1);
    x += primary_rect.width + gap;
    let secondary_rect = Rect::new(x, y, secondary_text.chars().count() as u16, 1);

    frame.render_widget(
        Paragraph::new(primary_text)
            .style(
                Style::default()
                    .fg(panel_contrast_fg(palette))
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        primary_rect,
    );
    frame.render_widget(
        Paragraph::new(secondary_text)
            .style(
                Style::default()
                    .fg(palette.text)
                    .bg(palette.surface0)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        secondary_rect,
    );
}

fn button_text(hint: &str, label: &str) -> String {
    format!(" {hint} {label} ")
}

fn panel_contrast_fg(palette: &Palette) -> Color {
    match palette.panel_bg {
        Color::Reset => palette.surface_dim,
        color => color,
    }
}

const ADJECTIVES: [&str; 8] = [
    "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
];
const NOUNS: [&str; 8] = [
    "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
];

fn generated_name(seed: u64) -> String {
    let adjective = ADJECTIVES[(seed as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((seed / ADJECTIVES.len() as u64) as usize) % NOUNS.len()];
    format!("{adjective}-{noun}-{:04x}", seed & 0xffff)
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_prefers_focused_pane_cwd() {
        let context: InvocationContext = serde_json::from_str(
            r#"{
                "workspace_id":"w1",
                "workspace_cwd":"/repo",
                "tab_id":"w1:t2",
                "focused_pane_cwd":"/repo.feature"
            }"#,
        )
        .unwrap();

        assert_eq!(source_cwd(&context), Some(PathBuf::from("/repo.feature")));
    }

    #[test]
    fn context_falls_back_to_workspace_cwd() {
        let context = InvocationContext {
            workspace_cwd: Some("/repo".to_owned()),
            ..InvocationContext::default()
        };

        assert_eq!(source_cwd(&context), Some(PathBuf::from("/repo")));
    }

    #[test]
    fn escape_cancels_create_and_remove() {
        let mut name = "generated".to_owned();
        let mut replace = true;

        assert_eq!(
            handle_create_key(&mut name, &mut replace, KeyCode::Esc, KeyModifiers::NONE),
            CreateAction::Cancel
        );
        assert_eq!(
            confirmation_action(KeyCode::Esc, KeyModifiers::NONE),
            Some(false)
        );
    }

    #[test]
    fn first_character_replaces_generated_name() {
        let mut name = "generated".to_owned();
        let mut replace = true;

        assert_eq!(
            handle_create_key(
                &mut name,
                &mut replace,
                KeyCode::Char('f'),
                KeyModifiers::NONE
            ),
            CreateAction::Continue
        );
        assert_eq!(name, "f");
        assert!(!replace);
    }

    #[test]
    fn removal_refuses_default_workspace() {
        assert!(is_default_workspace(Path::new("/repo"), Path::new("/repo")));
        assert!(!is_default_workspace(
            Path::new("/repo.feature"),
            Path::new("/repo")
        ));
    }

    #[test]
    fn close_target_uses_workspace_for_matching_root() {
        assert_eq!(
            close_target(
                Path::new("/repo.feature"),
                Some("w2".to_owned()),
                Some("w2:t1".to_owned()),
            )
            .unwrap(),
            (CloseTarget::Workspace("w2".to_owned()), None)
        );
    }

    #[test]
    fn close_target_uses_tab_for_mixed_checkout_workspace() {
        assert_eq!(
            close_target(
                Path::new("/repo.feature"),
                Some("w1".to_owned()),
                Some("w1:t2".to_owned()),
            )
            .unwrap(),
            (CloseTarget::Tab("w1:t2".to_owned()), None)
        );
    }

    #[test]
    fn parses_created_container_ids() {
        let workspace =
            r#"{"result":{"workspace":{"workspace_id":"w3"},"tab":{"tab_id":"w3:t1"}}}"#;
        let tab = r#"{"result":{"tab":{"tab_id":"w2:t2"}}}"#;

        assert_eq!(
            created_container("workspace", workspace).unwrap(),
            (ContainerKind::Workspace, "w3".to_owned())
        );
        assert_eq!(
            created_container("tab", tab).unwrap(),
            (ContainerKind::Tab, "w2:t2".to_owned())
        );
    }
}
