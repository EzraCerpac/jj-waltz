use super::preferences::{load_delete_bookmarks, save_delete_bookmarks};
use super::removal::{self, BatchOutcome, BatchPlan, OutcomeState};
use super::status::{self, Integration, ManagerRow, ManagerSnapshot};
use crate::doctor::DoctorEngine;
use crate::lifecycle::{self, CreationPolicy};
use crate::snapshot::WorkingCopyStatus;
use anyhow::{Context, Result, anyhow};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear as ClearTerminal, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use std::collections::HashSet;
use std::io::{self, Stderr, Write};
use std::panic;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(50);

// Semantic foregrounds only: the terminal keeps its own background and palette.
mod palette {
    use ratatui::style::{Color, Modifier, Style};

    pub fn focus() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
    pub fn bookmark() -> Style {
        Style::default().fg(Color::Magenta)
    }
    pub fn good() -> Style {
        Style::default().fg(Color::Green)
    }
    pub fn caution() -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn danger() -> Style {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
    pub fn secondary() -> Style {
        // ANSI bright black can be nearly invisible in user palettes.
        Style::default()
    }
    pub fn heading() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

/// Opens the current repository's workspace manager.
///
/// The terminal UI writes only to stderr so stdout remains available to shell
/// integration. A successful selection returns the workspace name; switching is
/// deliberately left to the caller after the terminal has been restored.
pub fn run(trunk: &str) -> Result<Option<String>> {
    let mut terminal = TerminalSession::start()?;
    let (task_tx, task_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    let worker = Worker::start(task_rx, reply_tx);
    let (delete_bookmarks, preference_warning) = match load_delete_bookmarks() {
        Ok(value) => (value, None),
        Err(error) => (
            false,
            Some(format!(
                "could not read UI preferences; bookmarks will be kept: {error:#}"
            )),
        ),
    };
    let mut app = App::new(trunk.to_owned(), delete_bookmarks);
    app.notice = preference_warning;
    app.request_capture(&task_tx, Vec::new())?;

    let result = event_loop(&mut terminal.terminal, &mut app, &task_tx, &reply_rx);
    let mutating = app.busy.as_ref().is_some_and(Busy::is_mutating);
    drop(task_tx);
    // Restore the user's terminal before any error-path wait for an already
    // confirmed repository mutation.
    drop(terminal);
    if mutating {
        // A mutation has already crossed its confirmation boundary. Wait for it
        // rather than leaving a repository-changing job behind after the UI exits.
        worker.join();
    }
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stderr>>,
    app: &mut App,
    task_tx: &Sender<WorkerTask>,
    reply_rx: &Receiver<WorkerReply>,
) -> Result<Option<String>> {
    let mut dirty = true;
    loop {
        dirty |= drain_replies(app, task_tx, reply_rx)?;
        if dirty {
            terminal.draw(|frame| app.render(frame))?;
            dirty = false;
        }
        if let Some(done) = app.done.take() {
            return Ok(done);
        }
        if !event::poll(TICK)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let action = app.handle_key(key);
                apply_action(app, terminal, task_tx, action)?;
                dirty = true;
            }
            Event::Mouse(mouse) if app.mouse_capture => dirty |= app.handle_mouse(mouse),
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}

fn drain_replies(
    app: &mut App,
    task_tx: &Sender<WorkerTask>,
    reply_rx: &Receiver<WorkerReply>,
) -> Result<bool> {
    let mut changed = false;
    loop {
        match reply_rx.try_recv() {
            Ok(reply) => {
                app.apply_reply(reply, task_tx)?;
                changed = true;
            }
            Err(TryRecvError::Empty) => return Ok(changed),
            Err(TryRecvError::Disconnected) => {
                if app.busy.take().is_some() {
                    app.notice = Some("background worker stopped".to_owned());
                    changed = true;
                }
                return Ok(changed);
            }
        }
    }
}

fn apply_action(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stderr>>,
    task_tx: &Sender<WorkerTask>,
    action: Action,
) -> Result<()> {
    if app.busy.is_some() && (action.requires_worker() || matches!(action, Action::Choose(_))) {
        app.notice = Some("another repository check is still running".to_owned());
        return Ok(());
    }
    match action {
        Action::None => {}
        Action::ToggleMouse => {
            if app.mouse_capture {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            }
            app.mouse_capture = !app.mouse_capture;
        }
        Action::Quit => {
            if app.busy.as_ref().is_some_and(Busy::is_mutating) {
                app.notice =
                    Some("finish the confirmed repository change before leaving".to_owned());
            } else {
                app.done = Some(None);
            }
        }
        Action::Choose(name) => app.done = Some(Some(name)),
        Action::Capture(refresh) => app.request_capture(task_tx, refresh)?,
        Action::Create { name, base } => {
            let generation = app.next_generation();
            app.busy = Some(Busy::Create(generation));
            app.screen = Screen::Main;
            task_tx
                .send(WorkerTask::Create {
                    generation,
                    name,
                    base,
                })
                .map_err(|_| anyhow!("background worker stopped"))?;
        }
        Action::Health => {
            let generation = app.next_generation();
            app.busy = Some(Busy::Health(generation));
            task_tx
                .send(WorkerTask::Health {
                    generation,
                    trunk: app.trunk.clone(),
                })
                .map_err(|_| anyhow!("background worker stopped"))?;
        }
        Action::Prepare { names, prune } => {
            let generation = app.next_generation();
            app.busy = Some(Busy::Prepare(generation));
            task_tx
                .send(WorkerTask::Prepare {
                    generation,
                    trunk: app.trunk.clone(),
                    names,
                    prune,
                })
                .map_err(|_| anyhow!("background worker stopped"))?;
        }
        Action::Execute {
            plan,
            delete_bookmarks,
            include_risky,
            acknowledge_ignored,
        } => {
            let generation = app.next_generation();
            app.busy = Some(Busy::Remove(generation));
            app.delete_bookmarks = delete_bookmarks;
            app.screen = Screen::Main;
            task_tx
                .send(WorkerTask::Execute {
                    generation,
                    plan,
                    delete_bookmarks,
                    include_risky,
                    acknowledge_ignored,
                })
                .map_err(|_| anyhow!("background worker stopped"))?;
        }
        Action::Copy(value) => {
            let sequence = format!("\x1b]52;c;{}\x07", base64(value.as_bytes()));
            terminal.backend_mut().write_all(sequence.as_bytes())?;
            terminal.backend_mut().flush()?;
            app.notice = Some(format!("Clipboard request sent; path: {value}"));
        }
    }
    Ok(())
}

type PanicHook = Arc<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stderr>>,
    previous_hook: Option<PanicHook>,
}

impl TerminalSession {
    fn start() -> Result<Self> {
        enable_raw_mode().context("could not enable terminal raw mode")?;
        let mut restore_on_error = RestoreOnDrop(true);
        let mut stderr = io::stderr();
        if let Err(error) = execute!(
            stderr,
            EnterAlternateScreen,
            ClearTerminal(ClearType::All),
            MoveTo(0, 0),
            EnableMouseCapture
        ) {
            let _ = disable_raw_mode();
            return Err(error).context("could not enter the alternate screen");
        }
        let backend = CrosstermBackend::new(stderr);
        let terminal = Terminal::new(backend)?;

        let previous: PanicHook = panic::take_hook().into();
        let hook = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            hook(info);
        }));
        restore_on_error.0 = false;
        Ok(Self {
            terminal,
            previous_hook: Some(previous),
        })
    }
}

struct RestoreOnDrop(bool);

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        if self.0 {
            restore_terminal();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
        if !thread::panicking()
            && let Some(previous) = self.previous_hook.take()
        {
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stderr(), DisableMouseCapture, LeaveAlternateScreen);
}

struct Worker {
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(tasks: Receiver<WorkerTask>, replies: Sender<WorkerReply>) -> Self {
        let handle = thread::spawn(move || {
            while let Ok(task) = tasks.recv() {
                let reply = match task {
                    WorkerTask::Capture {
                        generation,
                        trunk,
                        refresh,
                    } => WorkerReply::Capture {
                        generation,
                        result: status::capture(&trunk, &refresh).map_err(format_error),
                    },
                    WorkerTask::Create {
                        generation,
                        name,
                        base,
                    } => {
                        let result = CreationPolicy::load(Some(base), None, false, false, 1)
                            .and_then(|policy| lifecycle::add_workspaces(&[name], &policy))
                            .map(|_| ())
                            .map_err(format_error);
                        WorkerReply::Create { generation, result }
                    }
                    WorkerTask::Health { generation, trunk } => WorkerReply::Health {
                        generation,
                        result: DoctorEngine::current(trunk)
                            .map(|engine| engine.run().render_plain())
                            .map_err(format_error),
                    },
                    WorkerTask::Prepare {
                        generation,
                        trunk,
                        names,
                        prune,
                    } => WorkerReply::Prepare {
                        generation,
                        result: removal::prepare(&trunk, &names, prune).map_err(format_error),
                    },
                    WorkerTask::Execute {
                        generation,
                        plan,
                        delete_bookmarks,
                        include_risky,
                        acknowledge_ignored,
                    } => {
                        let (outcomes, preference_warning) = execute_with_preference(
                            delete_bookmarks,
                            save_delete_bookmarks,
                            || {
                                removal::execute(
                                    plan,
                                    delete_bookmarks,
                                    include_risky,
                                    acknowledge_ignored,
                                )
                            },
                        );
                        WorkerReply::Execute {
                            generation,
                            result: Ok(outcomes),
                            preference_warning,
                        }
                    }
                };
                if replies.send(reply).is_err() {
                    break;
                }
            }
        });
        Self {
            handle: Some(handle),
        }
    }

    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches read-only work. Confirmed mutations are
        // joined explicitly by run().
        let _ = self.handle.take();
    }
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn execute_with_preference<T, F, E>(
    delete_bookmarks: bool,
    save: F,
    execute: E,
) -> (T, Option<String>)
where
    F: FnOnce(bool) -> Result<()>,
    E: FnOnce() -> T,
{
    let warning = save(delete_bookmarks)
        .err()
        .map(|error| format!("Bookmark choice was not saved for next time: {error:#}"));
    let outcomes = execute();
    (outcomes, warning)
}

enum WorkerTask {
    Capture {
        generation: u64,
        trunk: String,
        refresh: Vec<String>,
    },
    Create {
        generation: u64,
        name: String,
        base: String,
    },
    Health {
        generation: u64,
        trunk: String,
    },
    Prepare {
        generation: u64,
        trunk: String,
        names: Vec<String>,
        prune: bool,
    },
    Execute {
        generation: u64,
        plan: BatchPlan,
        delete_bookmarks: bool,
        include_risky: bool,
        acknowledge_ignored: bool,
    },
}

enum WorkerReply {
    Capture {
        generation: u64,
        result: std::result::Result<ManagerSnapshot, String>,
    },
    Create {
        generation: u64,
        result: std::result::Result<(), String>,
    },
    Health {
        generation: u64,
        result: std::result::Result<String, String>,
    },
    Prepare {
        generation: u64,
        result: std::result::Result<BatchPlan, String>,
    },
    Execute {
        generation: u64,
        result: std::result::Result<Vec<BatchOutcome>, String>,
        preference_warning: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Capture { generation: u64, refreshing: bool },
    Create(u64),
    Health(u64),
    Prepare(u64),
    Remove(u64),
}

impl Busy {
    fn generation(self) -> u64 {
        match self {
            Self::Capture { generation, .. } => generation,
            Self::Create(value)
            | Self::Health(value)
            | Self::Prepare(value)
            | Self::Remove(value) => value,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Capture { .. } => "checking workspaces",
            Self::Create(_) => "creating workspace",
            Self::Health(_) => "running health report",
            Self::Prepare(_) => "building removal preview",
            Self::Remove(_) => "applying confirmed removal",
        }
    }

    fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::Capture {
                refreshing: true,
                ..
            } | Self::Create(_)
                | Self::Prepare(_)
                | Self::Remove(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Integrated,
    Unfinished,
    Problems,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Integrated,
            Self::Integrated => Self::Unfinished,
            Self::Unfinished => Self::Problems,
            Self::Problems => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Integrated => "integrated",
            Self::Unfinished => "unfinished",
            Self::Problems => "problems",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateField {
    Name,
    Base,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseChoice {
    Trunk,
    Workspace,
    Custom,
}

impl BaseChoice {
    fn next(self) -> Self {
        match self {
            Self::Trunk => Self::Workspace,
            Self::Workspace => Self::Custom,
            Self::Custom => Self::Trunk,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Trunk => Self::Custom,
            Self::Workspace => Self::Trunk,
            Self::Custom => Self::Workspace,
        }
    }
}

struct CreateForm {
    name: String,
    base_choice: BaseChoice,
    base: String,
    workspace_base: Option<String>,
    field: CreateField,
}

impl CreateForm {
    fn new(trunk: &str, workspace_base: Option<String>) -> Self {
        Self {
            name: String::new(),
            base_choice: BaseChoice::Trunk,
            base: trunk.to_owned(),
            workspace_base,
            field: CreateField::Name,
        }
    }

    fn selected_base(&self, trunk: &str) -> String {
        match self.base_choice {
            BaseChoice::Trunk => trunk.to_owned(),
            BaseChoice::Workspace => self
                .workspace_base
                .clone()
                .unwrap_or_else(|| trunk.to_owned()),
            BaseChoice::Custom => self.base.trim().to_owned(),
        }
    }
}

struct Preview {
    plan: BatchPlan,
    options: PreviewOptions,
    scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewOptions {
    delete_bookmarks: bool,
    include_risky: bool,
    unrecorded_files: UnrecordedChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnrecordedChoice {
    Undecided,
    Delete,
    Skip,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PreviewCounts {
    remove: usize,
    skip: usize,
    blocked: usize,
    undecided: usize,
}

impl Preview {
    fn counts(&self) -> PreviewCounts {
        preview_counts(&self.plan.rows, self.options)
    }

    fn ready(&self) -> bool {
        let counts = self.counts();
        counts.remove > 0 && counts.undecided == 0
    }
}

fn preview_counts(rows: &[removal::RemovalRow], options: PreviewOptions) -> PreviewCounts {
    let mut counts = PreviewCounts::default();
    for row in rows {
        if row.blocked.is_some() {
            counts.blocked += 1;
        } else if row.risky && !options.include_risky {
            counts.skip += 1;
        } else if !row.ignored.is_empty() {
            match options.unrecorded_files {
                UnrecordedChoice::Undecided => counts.undecided += 1,
                UnrecordedChoice::Delete => counts.remove += 1,
                UnrecordedChoice::Skip => counts.skip += 1,
            }
        } else {
            counts.remove += 1;
        }
    }
    counts
}

impl PreviewOptions {
    fn new(delete_bookmarks: bool) -> Self {
        Self {
            delete_bookmarks,
            include_risky: false,
            unrecorded_files: UnrecordedChoice::Undecided,
        }
    }
}

enum Screen {
    Main,
    Help,
    Search,
    Create(CreateForm),
    Preview(Preview),
    Report {
        title: String,
        body: String,
        scroll: u16,
    },
}

enum Action {
    None,
    ToggleMouse,
    Quit,
    Choose(String),
    Capture(Vec<String>),
    Create {
        name: String,
        base: String,
    },
    Health,
    Prepare {
        names: Vec<String>,
        prune: bool,
    },
    Execute {
        plan: BatchPlan,
        delete_bookmarks: bool,
        include_risky: bool,
        acknowledge_ignored: bool,
    },
    Copy(String),
}

impl Action {
    fn requires_worker(&self) -> bool {
        matches!(
            self,
            Self::Capture(_)
                | Self::Create { .. }
                | Self::Health
                | Self::Prepare { .. }
                | Self::Execute { .. }
        )
    }
}

struct App {
    trunk: String,
    snapshot: Option<ManagerSnapshot>,
    cursor: usize,
    selected: HashSet<String>,
    table_state: TableState,
    table_area: Rect,
    query: String,
    filter: Filter,
    detail_overlay: bool,
    screen: Screen,
    notice: Option<String>,
    busy: Option<Busy>,
    generation: u64,
    last_capture: u64,
    delete_bookmarks: bool,
    mouse_capture: bool,
    done: Option<Option<String>>,
}

impl App {
    fn new(trunk: String, delete_bookmarks: bool) -> Self {
        Self {
            trunk,
            snapshot: None,
            cursor: 0,
            selected: HashSet::new(),
            table_state: TableState::default(),
            table_area: Rect::default(),
            query: String::new(),
            filter: Filter::All,
            detail_overlay: false,
            screen: Screen::Main,
            notice: None,
            busy: None,
            generation: 0,
            last_capture: 0,
            delete_bookmarks,
            mouse_capture: true,
            done: None,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn request_capture(&mut self, tx: &Sender<WorkerTask>, refresh: Vec<String>) -> Result<()> {
        let generation = self.next_generation();
        self.busy = Some(Busy::Capture {
            generation,
            refreshing: !refresh.is_empty(),
        });
        tx.send(WorkerTask::Capture {
            generation,
            trunk: self.trunk.clone(),
            refresh,
        })
        .map_err(|_| anyhow!("background worker stopped"))
    }

    fn apply_reply(&mut self, reply: WorkerReply, tx: &Sender<WorkerTask>) -> Result<()> {
        let generation = match &reply {
            WorkerReply::Capture { generation, .. }
            | WorkerReply::Create { generation, .. }
            | WorkerReply::Health { generation, .. }
            | WorkerReply::Prepare { generation, .. }
            | WorkerReply::Execute { generation, .. } => *generation,
        };
        if generation < self.generation {
            return Ok(());
        }
        if self
            .busy
            .is_some_and(|busy| busy.generation() != generation)
        {
            return Ok(());
        }
        self.busy = None;
        match reply {
            WorkerReply::Capture { generation, result } => match result {
                Ok(snapshot) if generation >= self.last_capture => {
                    self.last_capture = generation;
                    let names = snapshot
                        .rows
                        .iter()
                        .map(|row| &row.workspace.name)
                        .collect::<HashSet<_>>();
                    self.selected.retain(|name| names.contains(name));
                    self.snapshot = Some(snapshot);
                    self.clamp_cursor();
                }
                Ok(_) => {}
                Err(error) => self.open_error("Workspace check failed", error),
            },
            WorkerReply::Create { result, .. } => match result {
                Ok(()) => {
                    self.notice = Some("workspace created".to_owned());
                    self.request_capture(tx, Vec::new())?;
                }
                Err(error) => self.open_error("Create failed", error),
            },
            WorkerReply::Health { result, .. } => match result {
                Ok(body) => {
                    self.screen = Screen::Report {
                        title: "Repository health".to_owned(),
                        body,
                        scroll: 0,
                    }
                }
                Err(error) => self.open_error("Health report failed", error),
            },
            WorkerReply::Prepare { result, .. } => match result {
                Ok(plan) if plan.rows.is_empty() => {
                    self.notice = Some("nothing matched the removal request".to_owned())
                }
                Ok(plan) => {
                    self.screen = Screen::Preview(Preview {
                        plan,
                        options: PreviewOptions::new(self.delete_bookmarks),
                        scroll: 0,
                    });
                }
                Err(error) => self.open_error("Removal preview failed", error),
            },
            WorkerReply::Execute {
                result,
                preference_warning,
                ..
            } => {
                let outcomes = match result {
                    Ok(outcomes) => outcomes,
                    Err(error) => {
                        self.open_error("Removal failed", error);
                        return Ok(());
                    }
                };
                let mut lines = Vec::new();
                for outcome in outcomes {
                    let label = match outcome.state {
                        OutcomeState::Completed => "DONE",
                        OutcomeState::Failed => "FAILED",
                        OutcomeState::Partial => "PARTIAL",
                        OutcomeState::Skipped => "SKIPPED",
                    };
                    lines.push(format!("{label} {}: {}", outcome.name, outcome.message));
                    if outcome.success() || outcome.state == OutcomeState::Partial {
                        self.selected.remove(&outcome.name);
                    } else {
                        self.selected.insert(outcome.name);
                    }
                }
                self.screen = Screen::Report {
                    title: "Removal results".to_owned(),
                    body: lines.join("\n"),
                    scroll: 0,
                };
                if let Some(warning) = preference_warning {
                    self.notice = Some(warning.clone());
                    if let Screen::Report { body, .. } = &mut self.screen {
                        body.push_str("\n\nPREFERENCE WARNING: ");
                        body.push_str(&warning);
                    }
                }
                self.request_capture(tx, Vec::new())?;
            }
        }
        Ok(())
    }

    fn open_error(&mut self, title: &str, error: String) {
        self.screen = Screen::Report {
            title: title.to_owned(),
            body: error,
            scroll: 0,
        };
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if key.code == KeyCode::Char('m')
            && !matches!(self.screen, Screen::Search | Screen::Create(_))
        {
            return Action::ToggleMouse;
        }
        match &mut self.screen {
            Screen::Main => self.handle_main_key(key),
            Screen::Help => {
                self.screen = Screen::Main;
                if key.code == KeyCode::Char('q') {
                    Action::Quit
                } else {
                    Action::None
                }
            }
            Screen::Search => {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => self.screen = Screen::Main,
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.cursor = 0;
                    }
                    KeyCode::Char(character) => {
                        self.query.push(character);
                        self.cursor = 0;
                    }
                    _ => {}
                }
                self.clamp_cursor();
                Action::None
            }
            Screen::Create(form) => match key.code {
                KeyCode::Esc => {
                    self.screen = Screen::Main;
                    Action::None
                }
                KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                    form.field = match form.field {
                        CreateField::Name => CreateField::Base,
                        CreateField::Base => CreateField::Name,
                    };
                    Action::None
                }
                KeyCode::Left if form.field == CreateField::Base => {
                    form.base_choice = form.base_choice.previous();
                    if form.base_choice == BaseChoice::Workspace && form.workspace_base.is_none() {
                        form.base_choice = BaseChoice::Trunk;
                    }
                    Action::None
                }
                KeyCode::Right | KeyCode::Char(' ') if form.field == CreateField::Base => {
                    form.base_choice = form.base_choice.next();
                    if form.base_choice == BaseChoice::Workspace && form.workspace_base.is_none() {
                        form.base_choice = BaseChoice::Custom;
                    }
                    Action::None
                }
                KeyCode::Backspace => {
                    match form.field {
                        CreateField::Name => {
                            form.name.pop();
                        }
                        CreateField::Base if form.base_choice == BaseChoice::Custom => {
                            form.base.pop();
                        }
                        CreateField::Base => {}
                    }
                    Action::None
                }
                KeyCode::Enter if !form.name.trim().is_empty() => Action::Create {
                    name: form.name.trim().to_owned(),
                    base: form.selected_base(&self.trunk),
                },
                KeyCode::Char(character) => {
                    match form.field {
                        CreateField::Name => form.name.push(character),
                        CreateField::Base if form.base_choice == BaseChoice::Custom => {
                            form.base.push(character)
                        }
                        CreateField::Base => {}
                    }
                    Action::None
                }
                _ => Action::None,
            },
            Screen::Preview(preview) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.screen = Screen::Main;
                    Action::None
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    preview.scroll = preview.scroll.saturating_add(1);
                    Action::None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    preview.scroll = preview.scroll.saturating_sub(1);
                    Action::None
                }
                KeyCode::Char('b') => {
                    preview.options.delete_bookmarks = !preview.options.delete_bookmarks;
                    Action::None
                }
                KeyCode::Char('r') => {
                    preview.options.include_risky = !preview.options.include_risky;
                    Action::None
                }
                KeyCode::Char('i') => {
                    preview.options.unrecorded_files = UnrecordedChoice::Delete;
                    Action::None
                }
                KeyCode::Char('s') => {
                    preview.options.unrecorded_files = UnrecordedChoice::Skip;
                    Action::None
                }
                KeyCode::Enter if preview.ready() => Action::Execute {
                    plan: preview.plan.clone(),
                    delete_bookmarks: preview.options.delete_bookmarks,
                    include_risky: preview.options.include_risky,
                    acknowledge_ignored: preview.options.unrecorded_files
                        == UnrecordedChoice::Delete,
                },
                KeyCode::Enter => {
                    let counts = preview.counts();
                    self.notice = Some(if counts.undecided > 0 {
                        "Choose [i] delete listed files or [s] skip those workspaces first"
                            .to_owned()
                    } else {
                        "No eligible workspace to remove".to_owned()
                    });
                    Action::None
                }
                _ => Action::None,
            },
            Screen::Report { scroll, .. } => {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(1)
                    }
                    KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(1)
                    }
                    _ => self.screen = Screen::Main,
                }
                Action::None
            }
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('?') => {
                self.screen = Screen::Help;
                Action::None
            }
            KeyCode::Char('/') => {
                self.screen = Screen::Search;
                Action::None
            }
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.cursor = 0;
                self.clamp_cursor();
                Action::None
            }
            KeyCode::Char('1') => self.set_filter(Filter::All),
            KeyCode::Char('2') => self.set_filter(Filter::Integrated),
            KeyCode::Char('3') => self.set_filter(Filter::Unfinished),
            KeyCode::Char('4') => self.set_filter(Filter::Problems),
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor(1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor(-1);
                Action::None
            }
            KeyCode::Char(' ') => {
                if let Some(name) = self.current_name()
                    && !self.selected.remove(&name)
                {
                    self.selected.insert(name);
                }
                Action::None
            }
            KeyCode::Char('a') => {
                for name in self.visible_names() {
                    self.selected.insert(name);
                }
                Action::None
            }
            KeyCode::Char('c') => {
                self.selected.clear();
                Action::None
            }
            KeyCode::Enter => self.current_name().map_or(Action::None, Action::Choose),
            KeyCode::Char('n') => {
                let workspace_base = self
                    .current_row()
                    .map(|row| row.workspace.commit_id.clone());
                self.screen = Screen::Create(CreateForm::new(&self.trunk, workspace_base));
                Action::None
            }
            KeyCode::Char('i') => self
                .current_name()
                .map_or(Action::None, |name| Action::Capture(vec![name])),
            KeyCode::Char('h') => Action::Health,
            KeyCode::Char('y') => self
                .current_row()
                .and_then(|row| row.workspace.path.as_ref())
                .map(|path| Action::Copy(path.display().to_string()))
                .unwrap_or(Action::None),
            KeyCode::Char('p') => Action::Prepare {
                names: Vec::new(),
                prune: true,
            },
            KeyCode::Char('d') => {
                let names: Vec<String> = if self.selected.is_empty() {
                    self.current_name().into_iter().collect()
                } else {
                    self.selected.iter().cloned().collect()
                };
                if names.is_empty() {
                    Action::None
                } else {
                    Action::Prepare {
                        names,
                        prune: false,
                    }
                }
            }
            KeyCode::Tab => {
                self.detail_overlay = !self.detail_overlay;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn set_filter(&mut self, filter: Filter) -> Action {
        self.filter = filter;
        self.cursor = 0;
        self.clamp_cursor();
        Action::None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(self.screen, Screen::Main) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                let old = self.cursor;
                self.move_cursor(1);
                self.cursor != old
            }
            MouseEventKind::ScrollUp => {
                let old = self.cursor;
                self.move_cursor(-1);
                self.cursor != old
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.table_area.contains((mouse.column, mouse.row).into()) =>
            {
                // Border, header, and the header's bottom margin are not rows.
                let first_row = self.table_area.y.saturating_add(3);
                if mouse.row < first_row {
                    return false;
                }
                let visible_row = mouse.row - first_row;
                let index = self
                    .table_state
                    .offset()
                    .saturating_add(visible_row as usize);
                if index < self.visible_indices().len() {
                    let old = self.cursor;
                    self.cursor = index;
                    // The first cell begins just inside the border and highlight
                    // symbol. Clicking its checkbox mirrors Space.
                    let mut changed = self.cursor != old;
                    if mouse.column <= self.table_area.x.saturating_add(7)
                        && let Some(name) = self.current_name()
                    {
                        if !self.selected.remove(&name) {
                            self.selected.insert(name);
                        }
                        changed = true;
                    }
                    return changed;
                }
                false
            }
            _ => false,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let query = self.query.to_lowercase();
        snapshot
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row_matches(row, self.filter, &query))
            .map(|(index, _)| index)
            .collect()
    }

    fn visible_names(&self) -> Vec<String> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        self.visible_indices()
            .into_iter()
            .map(|index| snapshot.rows[index].workspace.name.clone())
            .collect()
    }

    fn current_row(&self) -> Option<&ManagerRow> {
        let snapshot = self.snapshot.as_ref()?;
        let index = *self.visible_indices().get(self.cursor)?;
        snapshot.rows.get(index)
    }

    fn current_name(&self) -> Option<String> {
        self.current_row().map(|row| row.workspace.name.clone())
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.visible_indices().len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = if delta.is_negative() {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            (self.cursor + delta as usize).min(count - 1)
        };
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self
            .cursor
            .min(self.visible_indices().len().saturating_sub(1));
    }

    fn hidden_selection_count(&self) -> usize {
        let visible = self.visible_names().into_iter().collect::<HashSet<_>>();
        self.selected
            .iter()
            .filter(|name| !visible.contains(*name))
            .count()
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Block::default(), area);
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(area);
        self.render_header(frame, vertical[0]);
        self.render_body(frame, vertical[1]);
        self.render_footer(frame, vertical[2]);

        match &self.screen {
            Screen::Main => {
                if self.detail_overlay && area.width < 100 {
                    self.render_detail_popup(frame, area);
                }
            }
            Screen::Help => render_help(frame, area),
            Screen::Search => render_search(frame, area, &self.query),
            Screen::Create(form) => render_create(frame, area, form, &self.trunk),
            Screen::Preview(preview) => render_preview(frame, area, preview),
            Screen::Report {
                title,
                body,
                scroll,
            } => render_report(frame, area, title, body, *scroll),
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let hidden = self.hidden_selection_count();
        let mut spans = vec![
            Span::styled(" jj-waltz ", palette::heading()),
            Span::styled(
                format!(
                    "{} workspaces",
                    self.snapshot.as_ref().map_or(0, |s| s.rows.len())
                ),
                palette::secondary(),
            ),
            Span::raw("  "),
            Span::styled(format!("filter:{}", self.filter.label()), palette::focus()),
        ];
        if !self.query.is_empty() {
            spans.push(Span::styled(
                format!("  search:{}", self.query),
                palette::focus(),
            ));
        }
        if !self.selected.is_empty() {
            spans.push(Span::styled(
                format!("  selected:{} (+{} hidden)", self.selected.len(), hidden),
                palette::caution(),
            ));
        }
        let repository_warnings = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.snapshot.warnings.len());
        if repository_warnings != 0 {
            spans.push(Span::styled(
                format!("  repository warnings:{repository_warnings}"),
                palette::caution(),
            ));
        }
        let trunk = self.snapshot.as_ref().map_or_else(
            || format!("trunk: {}", self.trunk),
            |snapshot| {
                let trunk = &snapshot.snapshot.repository.trunk;
                format!(
                    "trunk: {} · {} · {}",
                    trunk.revset,
                    short_id(&trunk.commit_id),
                    trunk.description
                )
            },
        );
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(palette::secondary());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(spans),
                Line::from(vec![
                    Span::styled(" ", palette::secondary()),
                    Span::styled(trunk, palette::focus()),
                ]),
            ])
            .block(block),
            area,
        );
    }

    fn render_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.snapshot.is_none() {
            frame.render_widget(
                Paragraph::new("Checking the current repository…")
                    .alignment(Alignment::Center)
                    .style(palette::secondary())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(palette::secondary())
                            .title(" Workspaces ")
                            .title_style(palette::heading()),
                    ),
                area,
            );
            return;
        }
        if area.width >= 100 {
            let horizontal = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                .split(area);
            self.render_table(frame, horizontal[0]);
            self.render_detail(frame, horizontal[1]);
        } else {
            self.render_table(frame, area);
        }
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.table_area = area;
        let indices = self.visible_indices();
        let rows = self.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            indices
                .iter()
                .enumerate()
                .map(|(visible_index, index)| {
                    let row = &snapshot.rows[*index];
                    let workspace = &row.workspace;
                    let selected = if self.selected.contains(&workspace.name) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    let role = if workspace.role.current {
                        "@"
                    } else if workspace.role.previous {
                        "-"
                    } else if workspace.role.default {
                        "^"
                    } else {
                        " "
                    };
                    let bookmark = workspace.associated_bookmark.as_deref().unwrap_or("—");
                    let warned = !row.warnings.is_empty() || !workspace.hazards.is_empty();
                    let name_style = if visible_index == self.cursor {
                        palette::focus()
                    } else {
                        Style::default()
                    };
                    Row::new(vec![
                        Cell::from(format!("{selected} {role} {}", workspace.name))
                            .style(name_style),
                        Cell::from(bookmark.to_owned()).style(palette::bookmark()),
                        Cell::from(integration_short(&row.bookmark_integration))
                            .style(integration_style(&row.bookmark_integration)),
                        Cell::from(integration_short(&row.work_integration))
                            .style(integration_style(&row.work_integration)),
                        Cell::from(format!(
                            "{} {}",
                            working_copy_label(workspace.working_copy),
                            if warned { "!" } else { "" }
                        ))
                        .style(if warned {
                            palette::caution()
                        } else {
                            working_copy_style(workspace.working_copy)
                        }),
                    ])
                })
                .collect()
        });
        let header = Row::new([
            "workspace",
            "bookmark",
            "bookmark state",
            "work state",
            "checkout",
        ])
        .style(palette::heading())
        .bottom_margin(1);
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(28),
                Constraint::Percentage(25),
                Constraint::Percentage(15),
                Constraint::Percentage(15),
                Constraint::Percentage(17),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::secondary())
                .title(" Workspaces ")
                .title_style(palette::heading()),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
        .highlight_symbol("› ");
        self.table_state
            .select((!indices.is_empty()).then_some(self.cursor));
        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn render_detail(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = self
            .current_row()
            .map(detail_text)
            .unwrap_or_else(|| Text::from("No matching workspace"));
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(palette::secondary())
                    .title(" Details ")
                    .title_style(palette::heading()),
            ),
            area,
        );
    }

    fn render_detail_popup(&self, frame: &mut Frame<'_>, area: Rect) {
        let popup = centered(area, 90, 75);
        frame.render_widget(Clear, popup);
        self.render_detail(frame, popup);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let status = if let Some(busy) = self.busy {
            Span::styled(format!("{}…", busy.label()), palette::caution())
        } else if let Some(notice) = &self.notice {
            Span::styled(notice.clone(), palette::caution())
        } else {
            Span::styled(
                format!(
                    "{} visible · {} selected",
                    self.visible_indices().len(),
                    self.selected.len()
                ),
                palette::secondary(),
            )
        };
        let mouse_mode = if self.mouse_capture {
            "mouse: controls"
        } else {
            "mouse: text selection"
        };
        let bindings: &[(&str, &str)] = if area.width < 100 {
            &[
                ("↑↓", "move"),
                ("Space", "mark"),
                ("Enter", "switch"),
                ("/", "search"),
                ("d", "remove"),
                ("n", "new"),
                ("?", "help"),
            ]
        } else {
            &[
                ("↑↓/jk", "move"),
                ("Space", "mark"),
                ("Enter", "switch"),
                ("/", "search"),
                ("f", "filter"),
                ("n", "new"),
                ("p", "prune missing"),
                ("d", "remove"),
                ("?", "help"),
            ]
        };
        let shortcuts = bindings
            .iter()
            .flat_map(|(key, label)| {
                [
                    Span::styled(
                        *key,
                        if *key == "d" {
                            palette::danger()
                        } else {
                            palette::focus()
                        },
                    ),
                    Span::raw(format!(" {label}  ")),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(shortcuts),
                Line::from(vec![
                    Span::styled(format!(" [m] {mouse_mode} · "), palette::focus()),
                    status,
                ]),
            ])
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(palette::secondary()),
            ),
            area,
        );
    }
}

fn row_matches(row: &ManagerRow, filter: Filter, query: &str) -> bool {
    let workspace = &row.workspace;
    let searchable = format!(
        "{} {} {} {} {} {} {}",
        workspace.name,
        workspace.associated_bookmark.as_deref().unwrap_or_default(),
        workspace.description,
        row.bookmark_integration.label(),
        row.work_integration.label(),
        row.warnings.join(" "),
        workspace
            .hazards
            .iter()
            .map(|hazard| hazard.message.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_lowercase();
    if !query.is_empty() && !searchable.contains(query) {
        return false;
    }
    match filter {
        Filter::All => true,
        Filter::Integrated => row.bookmark_integration == Integration::InTrunk,
        Filter::Unfinished => {
            row.work_integration == Integration::OutsideTrunk
                || matches!(
                    workspace.working_copy,
                    WorkingCopyStatus::Modified { .. } | WorkingCopyStatus::Conflicted { .. }
                )
        }
        Filter::Problems => !row.warnings.is_empty() || !workspace.hazards.is_empty(),
    }
}

fn working_copy_label(status: WorkingCopyStatus) -> String {
    match status {
        WorkingCopyStatus::Empty => "clean".to_owned(),
        WorkingCopyStatus::Modified { files, .. } => format!("{files} changed"),
        WorkingCopyStatus::Conflicted { conflicts } => format!("{conflicts} conflicts"),
        WorkingCopyStatus::Stale => "stale".to_owned(),
        WorkingCopyStatus::Unknown => "unchecked".to_owned(),
    }
}

fn integration_short(integration: &Integration) -> &'static str {
    match integration {
        Integration::InTrunk => "in trunk",
        Integration::OutsideTrunk => "outside",
        Integration::Missing => "missing",
        Integration::Conflicted => "conflict",
        Integration::Unassociated => "none",
        Integration::Unknown => "unknown",
    }
}

fn integration_detail(integration: &Integration) -> &'static str {
    match integration {
        Integration::InTrunk => "integrated in trunk",
        Integration::OutsideTrunk => "outside trunk",
        Integration::Missing => "recorded target is missing",
        Integration::Conflicted => "has conflicting targets",
        Integration::Unassociated => "no managed bookmark",
        Integration::Unknown => "could not be checked",
    }
}

fn integration_style(integration: &Integration) -> Style {
    match integration {
        Integration::InTrunk => palette::good(),
        Integration::OutsideTrunk | Integration::Missing => palette::caution(),
        Integration::Conflicted => palette::danger(),
        Integration::Unassociated | Integration::Unknown => palette::secondary(),
    }
}

fn working_copy_style(status: WorkingCopyStatus) -> Style {
    match status {
        WorkingCopyStatus::Empty => palette::good(),
        WorkingCopyStatus::Modified { .. } | WorkingCopyStatus::Stale => palette::caution(),
        WorkingCopyStatus::Conflicted { .. } => palette::danger(),
        WorkingCopyStatus::Unknown => palette::secondary(),
    }
}

fn detail_text(row: &ManagerRow) -> Text<'static> {
    let workspace = &row.workspace;
    let mut lines = vec![
        Line::from(Span::styled(workspace.name.clone(), palette::heading())),
        Line::from(vec![
            Span::styled("path: ", palette::secondary()),
            Span::raw(
                workspace
                    .path
                    .as_ref()
                    .map_or_else(|| "missing".to_owned(), |path| path.display().to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("bookmark: ", palette::secondary()),
            Span::styled(
                workspace
                    .associated_bookmark
                    .clone()
                    .unwrap_or_else(|| "none".to_owned()),
                palette::bookmark(),
            ),
            Span::raw(" ("),
            Span::styled(
                integration_detail(&row.bookmark_integration),
                integration_style(&row.bookmark_integration),
            ),
            Span::raw(")"),
        ]),
        Line::from(vec![
            Span::styled("work: ", palette::secondary()),
            Span::styled(
                integration_detail(&row.work_integration),
                integration_style(&row.work_integration),
            ),
        ]),
        Line::from(vec![
            Span::styled("checkout: ", palette::secondary()),
            Span::styled(
                working_copy_label(workspace.working_copy),
                working_copy_style(workspace.working_copy),
            ),
        ]),
        Line::from(vec![
            Span::styled("change: ", palette::secondary()),
            Span::raw(workspace.change_id.clone()),
        ]),
        Line::from(vec![
            Span::styled("commit: ", palette::secondary()),
            Span::raw(short_id(&workspace.commit_id).to_owned()),
        ]),
        Line::from(""),
        Line::from(workspace.description.clone()),
    ];
    if !row.warnings.is_empty() || !workspace.hazards.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Warnings",
            palette::caution().add_modifier(Modifier::BOLD),
        )));
        lines.extend(
            row.warnings.iter().map(|warning| {
                Line::from(Span::styled(format!("• {warning}"), palette::caution()))
            }),
        );
        lines.extend(workspace.hazards.iter().map(|hazard| {
            Line::from(Span::styled(
                format!("• {}", hazard.message),
                palette::caution(),
            ))
        }));
    }
    Text::from(lines)
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 78, 82);
    frame.render_widget(Clear, popup);
    let bindings = [
        ("↑/↓ or j/k", "move"),
        ("Space", "mark/unmark workspace"),
        ("a / c", "mark all visible / clear marks"),
        ("Enter", "switch to highlighted workspace"),
        ("/", "search (Enter accepts, Esc closes)"),
        (
            "f or 1–4",
            "filter all / integrated / unfinished / problems",
        ),
        ("Tab", "show details on a narrow terminal"),
        ("n", "create workspace"),
        ("i", "refresh highlighted checkout details"),
        ("h", "repository health report"),
        ("y", "copy workspace path with OSC 52"),
        (
            "p",
            "prune missing workspaces: forget registrations with missing directories",
        ),
        (
            "d",
            "remove: forget registrations and delete existing directories",
        ),
        ("m", "toggle mouse controls / native text selection"),
        ("q", "cancel"),
    ];
    let mut lines = bindings
        .into_iter()
        .map(|(key, action)| {
            Line::from(vec![
                Span::styled(format!("{key:<14}"), palette::focus()),
                Span::raw(action),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "@ current   - previous   ^ default   ! warning",
        palette::secondary(),
    )));
    lines.push(Line::from(Span::styled(
        "Removing a directory is permanent. Review every row before Enter.",
        palette::caution(),
    )));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::secondary())
                .title(" Help — any key closes ")
                .title_style(palette::heading()),
        ),
        popup,
    );
}

fn render_search(frame: &mut Frame<'_>, area: Rect, query: &str) {
    let popup = centered_fixed(area, 70, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(query).style(palette::focus()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::focus())
                .title(" Search — Enter accepts ")
                .title_style(palette::heading()),
        ),
        popup,
    );
    let x = popup.x.saturating_add(1 + query.chars().count() as u16);
    frame.set_cursor_position((x.min(popup.right().saturating_sub(2)), popup.y + 1));
}

fn render_create(frame: &mut Frame<'_>, area: Rect, form: &CreateForm, trunk: &str) {
    let popup = centered_fixed(area, 76, 11);
    frame.render_widget(Clear, popup);
    let name_style = field_style(form.field == CreateField::Name);
    let base_style = field_style(form.field == CreateField::Base);
    let base = form.selected_base(trunk);
    let choices = format!(
        "{} trunk   {} workspace   {} custom",
        choice_marker(form.base_choice == BaseChoice::Trunk),
        choice_marker(form.base_choice == BaseChoice::Workspace),
        choice_marker(form.base_choice == BaseChoice::Custom),
    );
    let lines = vec![
        Line::from(vec![
            Span::styled("workspace  ", palette::secondary()),
            Span::styled(&form.name, name_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("start at   ", palette::secondary()),
            Span::styled(choices, base_style),
        ]),
        Line::from(vec![
            Span::styled("revset    ", palette::secondary()),
            Span::styled(base, base_style),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Tab changes field · ←/→ changes base · Enter creates · Esc cancels",
            palette::focus(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::secondary())
                .title(" New workspace ")
                .title_style(palette::heading()),
        ),
        popup,
    );
}

fn render_preview(frame: &mut Frame<'_>, area: Rect, preview: &Preview) {
    let popup = centered(area, 94, 90);
    frame.render_widget(Clear, popup);
    let inner = popup.inner(Margin::new(1, 1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(7),
        ])
        .split(inner);
    let counts = preview.counts();
    let warning = if preview.plan.prune {
        "Prune missing workspaces: forget registrations whose directories are already gone."
    } else {
        "PERMANENT: confirmed directories are deleted along with their registrations."
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                warning,
                if preview.plan.prune {
                    palette::caution()
                } else {
                    palette::danger()
                },
            )),
            Line::from(vec![
                Span::styled(format!("remove: {}  ", counts.remove), palette::good()),
                Span::styled(format!("skip: {}  ", counts.skip), palette::caution()),
                Span::styled(format!("blocked: {}  ", counts.blocked), palette::danger()),
                Span::styled(
                    format!("needs choice: {}", counts.undecided),
                    palette::caution(),
                ),
            ]),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    let mut lines = Vec::new();
    for row in &preview.plan.rows {
        let disposition = if let Some(reason) = &row.blocked {
            (format!("BLOCKED: {reason}"), palette::danger())
        } else if row.risky && !preview.options.include_risky {
            (
                "SKIP: risky work; press [r] to include".to_owned(),
                palette::caution(),
            )
        } else if !row.ignored.is_empty() {
            match preview.options.unrecorded_files {
                UnrecordedChoice::Undecided => (
                    "NEEDS CHOICE: files not recorded by JJ".to_owned(),
                    palette::caution(),
                ),
                UnrecordedChoice::Delete => (
                    if preview.plan.prune {
                        "PRUNE: forget registration"
                    } else {
                        "REMOVE: delete directory and listed files"
                    }
                    .to_owned(),
                    palette::danger(),
                ),
                UnrecordedChoice::Skip => (
                    "SKIP: keep directory with files not recorded by JJ".to_owned(),
                    palette::caution(),
                ),
            }
        } else {
            (
                if preview.plan.prune {
                    "PRUNE: forget registration"
                } else {
                    "REMOVE: delete directory"
                }
                .to_owned(),
                palette::good(),
            )
        };
        lines.push(Line::from(Span::styled(
            row.name.clone(),
            palette::heading(),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {}", disposition.0),
            disposition.1,
        )));
        if let Some(path) = &row.path {
            lines.push(Line::from(vec![
                Span::styled("  path: ", palette::secondary()),
                Span::raw(path.display().to_string()),
            ]));
        }
        if let Some(bookmark) = &row.bookmark {
            lines.push(Line::from(vec![
                Span::styled("  local bookmark: ", palette::secondary()),
                Span::styled(bookmark.clone(), palette::bookmark()),
            ]));
        }
        if row.risky {
            lines.push(Line::from(Span::styled(
                "  RISKY: work outside trunk, conflict, unknown, or uncertain association",
                palette::caution(),
            )));
        }
        lines.extend(
            row.warnings
                .iter()
                .map(|value| Line::from(Span::styled(format!("  ! {value}"), palette::caution()))),
        );
        lines.extend(row.ignored.iter().map(|value| {
            Line::from(vec![
                Span::styled("  not recorded by JJ: ", palette::caution()),
                Span::raw(value.clone()),
            ])
        }));
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((preview.scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    let bookmark = if preview.options.delete_bookmarks {
        "DELETE"
    } else {
        "KEEP"
    };
    let choice = match preview.options.unrecorded_files {
        UnrecordedChoice::Undecided => "choose before confirmation",
        UnrecordedChoice::Delete => "DELETE listed files",
        UnrecordedChoice::Skip => "SKIP those workspaces",
    };
    let confirm = if preview.ready() {
        "Enter confirms permanently · Esc cancels · ↑↓ scroll"
    } else if counts.undecided > 0 {
        "Enter disabled: choose how to handle listed files first · Esc cancels"
    } else {
        "Enter disabled: no eligible workspace · Esc cancels"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("[b]", palette::focus()),
                Span::raw(" local bookmarks: "),
                Span::styled(
                    bookmark,
                    if preview.options.delete_bookmarks {
                        palette::danger()
                    } else {
                        palette::good()
                    },
                ),
                Span::raw("  "),
                Span::styled("[r]", palette::focus()),
                Span::styled(
                    format!(" include risky: {}", checked(preview.options.include_risky)),
                    if preview.options.include_risky {
                        palette::danger()
                    } else {
                        palette::secondary()
                    },
                ),
            ]),
            Line::from(Span::styled(
                "Files not recorded by JJ may include caches, build output, or private files:",
                palette::caution(),
            )),
            Line::from(vec![
                Span::styled("[i]", palette::danger()),
                Span::raw(" delete listed files   "),
                Span::styled("[s]", palette::focus()),
                Span::raw(" skip those workspaces"),
            ]),
            Line::from(vec![
                Span::styled("choice: ", palette::secondary()),
                Span::styled(
                    choice,
                    if preview.options.unrecorded_files == UnrecordedChoice::Delete {
                        palette::danger()
                    } else {
                        palette::caution()
                    },
                ),
            ]),
            Line::from(Span::styled(
                confirm,
                if preview.ready() {
                    palette::focus()
                } else {
                    palette::caution()
                },
            )),
        ])
        .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(palette::secondary())
            .title(format!(
                " {} — {} target{} ",
                if preview.plan.prune {
                    "Prune missing workspaces"
                } else {
                    "Removal preview"
                },
                preview.plan.rows.len(),
                if preview.plan.rows.len() == 1 {
                    ""
                } else {
                    "s"
                }
            ))
            .title_style(if preview.plan.prune {
                palette::heading()
            } else {
                palette::danger()
            }),
        popup,
    );
}

fn render_report(frame: &mut Frame<'_>, area: Rect, title: &str, body: &str, scroll: u16) {
    let popup = centered(area, 90, 88);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(
            body.lines().map(report_line).collect::<Vec<_>>(),
        ))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::secondary())
                .title(format!(" {title} — ↑↓ scroll, any other key closes "))
                .title_style(palette::heading()),
        ),
        popup,
    );
}

fn report_line(line: &str) -> Line<'static> {
    let prefix = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    let style = match prefix {
        "DONE" | "PASS" | "OK" => palette::good(),
        "FAILED" | "FAIL" | "BLOCKED" | "ERROR" => palette::danger(),
        "SKIPPED" | "SKIP" | "PARTIAL" | "WARN" | "WARNING" | "PREFERENCE" => palette::caution(),
        _ => Style::default(),
    };
    Line::from(Span::styled(line.to_owned(), style))
}

fn field_style(active: bool) -> Style {
    if active {
        palette::focus().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
    }
}

fn checked(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn choice_marker(selected: bool) -> &'static str {
    if selected { "●" } else { "○" }
}

fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn short_id(value: &str) -> &str {
    value.get(..value.len().min(12)).unwrap_or(value)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn row(name: &str, work: Integration) -> ManagerRow {
        ManagerRow {
            workspace: crate::snapshot::WorkspaceSnapshot {
                name: name.to_owned(),
                path: Some(std::path::PathBuf::from(format!("/repo/{name}"))),
                role: crate::snapshot::WorkspaceRole::default(),
                management: crate::snapshot::ManagementState::Managed,
                working_copy: WorkingCopyStatus::Empty,
                working_copy_refreshed: false,
                change_id: format!("{name}-change"),
                commit_id: format!("{name}-commit"),
                description: format!("work on {name}"),
                associated_bookmark: Some(format!("wip/{name}")),
                created_at_unix_ms: None,
                creation_operation_id: None,
                creation_base_commit_id: None,
                intended_remote: None,
                hazards: Vec::new(),
            },
            bookmark_integration: Integration::OutsideTrunk,
            work_integration: work,
            warnings: Vec::new(),
        }
    }

    fn snapshot(rows: Vec<ManagerRow>) -> ManagerSnapshot {
        let workspaces = rows.iter().map(|row| row.workspace.clone()).collect();
        ManagerSnapshot {
            snapshot: crate::snapshot::SnapshotEnvelope::new(
                crate::snapshot::SnapshotCommand::List,
                crate::snapshot::RepositorySnapshot {
                    captured_at_unix_ms: 1,
                    repository_id: "repo".to_owned(),
                    operation_id: "op".to_owned(),
                    trunk: crate::snapshot::ResolvedTrunk {
                        revset: "trunk()".to_owned(),
                        change_id: "trunk-change".to_owned(),
                        commit_id: "trunk-commit".to_owned(),
                        description: "trunk".to_owned(),
                    },
                },
                workspaces,
                Vec::new(),
            ),
            rows,
        }
    }

    #[test]
    fn filter_cycles_through_each_view() {
        let mut filter = Filter::All;
        filter = filter.next();
        assert_eq!(filter, Filter::Integrated);
        filter = filter.next();
        assert_eq!(filter, Filter::Unfinished);
        filter = filter.next();
        assert_eq!(filter, Filter::Problems);
        assert_eq!(filter.next(), Filter::All);
    }

    #[test]
    fn main_events_open_and_close_help() {
        let mut app = App::new("trunk()".to_owned(), false);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(app.screen, Screen::Help));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.screen, Screen::Main));
    }

    #[test]
    fn preview_options_start_conservative() {
        let options = PreviewOptions::new(true);
        assert!(options.delete_bookmarks);
        assert!(!options.include_risky);
        assert_eq!(options.unrecorded_files, UnrecordedChoice::Undecided);
    }

    #[test]
    fn preview_counts_track_blocked_risky_and_unrecorded_choices() {
        let make_row = |name: &str| removal::RemovalRow {
            name: name.to_owned(),
            path: None,
            bookmark: None,
            warnings: Vec::new(),
            ignored: Vec::new(),
            blocked: None,
            risky: false,
        };
        let mut rows = vec![
            make_row("safe"),
            make_row("risky"),
            make_row("unrecorded"),
            make_row("blocked"),
        ];
        rows[1].risky = true;
        rows[2].ignored = vec!["private/data".to_owned()];
        rows[3].blocked = Some("current workspace".to_owned());
        let mut options = PreviewOptions::new(false);
        assert_eq!(
            preview_counts(&rows, options),
            PreviewCounts {
                remove: 1,
                skip: 1,
                blocked: 1,
                undecided: 1
            }
        );
        options.unrecorded_files = UnrecordedChoice::Skip;
        assert_eq!(
            preview_counts(&rows, options),
            PreviewCounts {
                remove: 1,
                skip: 2,
                blocked: 1,
                undecided: 0
            }
        );
        options.unrecorded_files = UnrecordedChoice::Delete;
        options.include_risky = true;
        assert_eq!(
            preview_counts(&rows, options),
            PreviewCounts {
                remove: 3,
                skip: 0,
                blocked: 1,
                undecided: 0
            }
        );
        rows.remove(0);
        options.unrecorded_files = UnrecordedChoice::Skip;
        options.include_risky = false;
        assert_eq!(preview_counts(&rows, options).remove, 0);
    }

    #[test]
    fn mouse_toggle_is_available_on_nonediting_screens() {
        let mut app = App::new("trunk()".to_owned(), false);
        let m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(matches!(app.handle_key(m), Action::ToggleMouse));
        app.screen = Screen::Help;
        assert!(matches!(app.handle_key(m), Action::ToggleMouse));
        assert!(matches!(app.screen, Screen::Help));
        app.screen = Screen::Report {
            title: "Report".to_owned(),
            body: String::new(),
            scroll: 0,
        };
        assert!(matches!(app.handle_key(m), Action::ToggleMouse));
        assert!(matches!(app.screen, Screen::Report { .. }));
        app.screen = Screen::Search;
        assert!(matches!(app.handle_key(m), Action::None));
        assert_eq!(app.query, "m");
        app.screen = Screen::Create(CreateForm::new("trunk()", None));
        assert!(matches!(app.handle_key(m), Action::None));
        assert!(matches!(&app.screen, Screen::Create(form) if form.name == "m"));
    }

    #[test]
    fn preference_write_failure_does_not_stop_confirmed_work() {
        let executed = std::cell::Cell::new(false);
        let (result, warning) = execute_with_preference(
            true,
            |_| Err(anyhow!("state directory is unwritable")),
            || {
                executed.set(true);
                "completed"
            },
        );
        assert!(executed.get());
        assert_eq!(result, "completed");
        assert!(warning.unwrap().contains("state directory is unwritable"));
    }

    #[test]
    fn selection_survives_search_and_reports_hidden_count() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.snapshot = Some(snapshot(vec![
            row("alpha", Integration::OutsideTrunk),
            row("beta", Integration::InTrunk),
        ]));
        app.selected.extend(["alpha".to_owned(), "beta".to_owned()]);
        app.query = "alpha".to_owned();
        assert_eq!(app.visible_names(), vec!["alpha"]);
        assert_eq!(app.hidden_selection_count(), 1);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(!app.selected.contains("alpha"));
        assert!(app.selected.contains("beta"));
    }

    #[test]
    fn refresh_drops_removed_selections_but_keeps_filtered_rows() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.selected
            .extend(["alpha", "beta", "removed"].map(str::to_owned));
        app.query = "alpha".to_owned();
        let (tx, _rx) = mpsc::channel();
        app.apply_reply(
            WorkerReply::Capture {
                generation: 1,
                result: Ok(snapshot(vec![
                    row("alpha", Integration::OutsideTrunk),
                    row("beta", Integration::InTrunk),
                ])),
            },
            &tx,
        )
        .unwrap();
        assert_eq!(
            app.selected,
            HashSet::from(["alpha".to_owned(), "beta".to_owned()])
        );
        assert_eq!(app.hidden_selection_count(), 1);
    }

    #[test]
    fn select_all_marks_only_visible_rows() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.snapshot = Some(snapshot(vec![
            row("alpha", Integration::OutsideTrunk),
            row("beta", Integration::InTrunk),
        ]));
        app.query = "alpha".to_owned();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.selected, HashSet::from(["alpha".to_owned()]));
    }

    #[test]
    fn integrated_filter_uses_bookmark_even_when_work_is_unfinished() {
        let mut integrated = row("integrated", Integration::OutsideTrunk);
        integrated.bookmark_integration = Integration::InTrunk;
        let mut app = App::new("trunk()".to_owned(), false);
        app.snapshot = Some(snapshot(vec![
            integrated,
            row("outside", Integration::OutsideTrunk),
        ]));
        app.filter = Filter::Integrated;
        assert_eq!(app.visible_names(), vec!["integrated"]);
    }

    #[test]
    fn stale_worker_generation_is_ignored() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.busy = Some(Busy::Capture {
            generation: 2,
            refreshing: false,
        });
        let (tx, _rx) = mpsc::channel();
        app.apply_reply(
            WorkerReply::Capture {
                generation: 1,
                result: Ok(snapshot(vec![row("stale", Integration::InTrunk)])),
            },
            &tx,
        )
        .unwrap();
        assert!(app.snapshot.is_none());
        assert_eq!(
            app.busy,
            Some(Busy::Capture {
                generation: 2,
                refreshing: false,
            })
        );
    }

    #[test]
    fn implicit_single_removal_failure_becomes_selected() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.busy = Some(Busy::Remove(1));
        let (tx, rx) = mpsc::channel();
        app.apply_reply(
            WorkerReply::Execute {
                generation: 1,
                result: Ok(vec![BatchOutcome {
                    name: "blocked".to_owned(),
                    state: OutcomeState::Failed,
                    message: "still has work".to_owned(),
                }]),
                preference_warning: None,
            },
            &tx,
        )
        .unwrap();
        assert!(app.selected.contains("blocked"));
        assert!(matches!(app.screen, Screen::Report { .. }));
        assert!(matches!(rx.try_recv(), Ok(WorkerTask::Capture { .. })));
    }

    #[test]
    fn removal_results_keep_only_failed_and_skipped_names_selected() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.selected.extend(
            ["failed", "skipped", "partial", "completed"]
                .into_iter()
                .map(str::to_owned),
        );
        app.busy = Some(Busy::Remove(1));
        let (tx, _rx) = mpsc::channel();
        app.apply_reply(
            WorkerReply::Execute {
                generation: 1,
                result: Ok(vec![
                    BatchOutcome {
                        name: "failed".to_owned(),
                        state: OutcomeState::Failed,
                        message: "still has work".to_owned(),
                    },
                    BatchOutcome {
                        name: "skipped".to_owned(),
                        state: OutcomeState::Skipped,
                        message: "not eligible for removal".to_owned(),
                    },
                    BatchOutcome {
                        name: "partial".to_owned(),
                        state: OutcomeState::Partial,
                        message: "workspace was forgotten; later cleanup failed".to_owned(),
                    },
                    BatchOutcome {
                        name: "completed".to_owned(),
                        state: OutcomeState::Completed,
                        message: "removed".to_owned(),
                    },
                ]),
                preference_warning: None,
            },
            &tx,
        )
        .unwrap();

        assert_eq!(
            app.selected,
            HashSet::from(["failed".to_owned(), "skipped".to_owned()])
        );
    }

    #[test]
    fn create_form_accepts_name_and_custom_base() {
        let mut app = App::new("trunk()".to_owned(), false);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        for character in "topic".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        if let Screen::Create(form) = &mut app.screen {
            form.base.clear();
        }
        for character in "main@origin".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match action {
            Action::Create { name, base } => {
                assert_eq!(name, "topic");
                assert_eq!(base, "main@origin");
            }
            _ => panic!("expected create action"),
        }
    }

    #[test]
    fn confirmed_mutation_is_not_detachable() {
        assert!(Busy::Create(1).is_mutating());
        assert!(Busy::Prepare(2).is_mutating());
        assert!(
            Busy::Capture {
                generation: 3,
                refreshing: true,
            }
            .is_mutating()
        );
        assert!(
            !Busy::Capture {
                generation: 4,
                refreshing: false,
            }
            .is_mutating()
        );
    }

    #[test]
    fn base64_handles_osc52_payloads() {
        assert_eq!(base64(b"path"), "cGF0aA==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn loading_view_renders_with_test_backend() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("trunk()".to_owned(), false);
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Checking the current repository"));
    }

    #[test]
    fn populated_view_renders_table_and_details() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("trunk()".to_owned(), false);
        app.snapshot = Some(snapshot(vec![row("feature-09", Integration::OutsideTrunk)]));
        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("feature-09"));
        assert!(rendered.contains("work on feature-09"));
    }

    #[test]
    fn highlighted_row_keeps_semantic_foregrounds_and_default_background() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new("trunk()".to_owned(), false);
        let mut feature = row("feature-09", Integration::OutsideTrunk);
        feature.bookmark_integration = Integration::InTrunk;
        app.snapshot = Some(snapshot(vec![feature]));
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let cell_at = |needle: &str| {
            let chars = needle.chars().collect::<Vec<_>>();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width.saturating_sub(chars.len() as u16) {
                    if chars.iter().enumerate().all(|(index, character)| {
                        buffer[(x + index as u16, y)].symbol() == character.to_string()
                    }) {
                        return &buffer[(x, y)];
                    }
                }
            }
            panic!("missing {needle}");
        };
        let integrated = cell_at("in trunk");
        assert_eq!(integrated.fg, ratatui::style::Color::Green);
        assert_eq!(integrated.bg, ratatui::style::Color::Reset);
        assert_eq!(cell_at("wip/feature-09").fg, ratatui::style::Color::Magenta);
    }
}
