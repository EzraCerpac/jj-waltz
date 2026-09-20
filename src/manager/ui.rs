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
use ratatui::style::{Color, Modifier, Style};
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
    loop {
        drain_replies(app, task_tx, reply_rx)?;
        terminal.draw(|frame| app.render(frame))?;
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
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => {}
        }
    }
}

fn drain_replies(
    app: &mut App,
    task_tx: &Sender<WorkerTask>,
    reply_rx: &Receiver<WorkerReply>,
) -> Result<()> {
    loop {
        match reply_rx.try_recv() {
            Ok(reply) => app.apply_reply(reply, task_tx)?,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                app.busy = None;
                app.notice = Some("background worker stopped".to_owned());
                return Ok(());
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
                        let result = save_delete_bookmarks(delete_bookmarks)
                            .context("could not save the bookmark removal choice")
                            .map(|()| {
                                removal::execute(
                                    plan,
                                    delete_bookmarks,
                                    include_risky,
                                    acknowledge_ignored,
                                )
                            })
                            .map_err(format_error);
                        WorkerReply::Execute { generation, result }
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
    acknowledge_ignored: bool,
}

impl PreviewOptions {
    fn new(delete_bookmarks: bool) -> Self {
        Self {
            delete_bookmarks,
            include_risky: false,
            acknowledge_ignored: false,
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
            WorkerReply::Execute { result, .. } => {
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
                    if outcome.success() {
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
                    preview.options.acknowledge_ignored = !preview.options.acknowledge_ignored;
                    Action::None
                }
                KeyCode::Enter => Action::Execute {
                    plan: preview.plan.clone(),
                    delete_bookmarks: preview.options.delete_bookmarks,
                    include_risky: preview.options.include_risky,
                    acknowledge_ignored: preview.options.acknowledge_ignored,
                },
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

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.screen, Screen::Main) {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(1),
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::Down(MouseButton::Left)
                if self.table_area.contains((mouse.column, mouse.row).into()) =>
            {
                // Border, header, and the header's bottom margin are not rows.
                let first_row = self.table_area.y.saturating_add(3);
                if mouse.row < first_row {
                    return;
                }
                let visible_row = mouse.row - first_row;
                let index = self
                    .table_state
                    .offset()
                    .saturating_add(visible_row as usize);
                if index < self.visible_indices().len() {
                    self.cursor = index;
                    // The first cell begins just inside the border and highlight
                    // symbol. Clicking its checkbox mirrors Space.
                    if mouse.column <= self.table_area.x.saturating_add(7)
                        && let Some(name) = self.current_name()
                        && !self.selected.remove(&name)
                    {
                        self.selected.insert(name);
                    }
                }
            }
            _ => {}
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
            Span::styled(" jj-waltz ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "{} workspaces",
                self.snapshot.as_ref().map_or(0, |s| s.rows.len())
            )),
            Span::raw("  "),
            Span::styled(
                format!("filter:{}", self.filter.label()),
                Style::default().fg(Color::Cyan),
            ),
        ];
        if !self.query.is_empty() {
            spans.push(Span::raw(format!("  search:{}", self.query)));
        }
        if !self.selected.is_empty() {
            spans.push(Span::styled(
                format!("  selected:{} (+{} hidden)", self.selected.len(), hidden),
                Style::default().fg(Color::Yellow),
            ));
        }
        let repository_warnings = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.snapshot.warnings.len());
        if repository_warnings != 0 {
            spans.push(Span::styled(
                format!("  repository warnings:{repository_warnings}"),
                Style::default().fg(Color::Yellow),
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
        let block = Block::default().borders(Borders::BOTTOM);
        frame.render_widget(
            Paragraph::new(vec![Line::from(spans), Line::from(trunk)]).block(block),
            area,
        );
    }

    fn render_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if self.snapshot.is_none() {
            frame.render_widget(
                Paragraph::new("Checking the current repository…")
                    .alignment(Alignment::Center)
                    .block(Block::default().borders(Borders::ALL).title(" Workspaces ")),
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
                .map(|index| {
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
                    let warning = if row.warnings.is_empty() && workspace.hazards.is_empty() {
                        ""
                    } else {
                        "!"
                    };
                    Row::new(vec![
                        Cell::from(format!("{selected} {role} {}", workspace.name)),
                        Cell::from(bookmark.to_owned()),
                        Cell::from(integration_short(&row.bookmark_integration)),
                        Cell::from(integration_short(&row.work_integration)),
                        Cell::from(format!(
                            "{} {warning}",
                            working_copy_label(workspace.working_copy)
                        )),
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
        .style(Style::default().add_modifier(Modifier::BOLD))
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
        .block(Block::default().borders(Borders::ALL).title(" Workspaces "))
        .row_highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
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
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" Details ")),
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
            Line::from(Span::styled(
                format!("{}…", busy.label()),
                Style::default().fg(Color::Yellow),
            ))
        } else if let Some(notice) = &self.notice {
            Line::from(notice.clone())
        } else {
            Line::from(format!(
                "{} visible · {} selected",
                self.visible_indices().len(),
                self.selected.len()
            ))
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("↑↓/jk move  Space mark  Enter switch  / search  f filter  n new  d remove  ? help"),
                status,
            ])
            .block(Block::default().borders(Borders::TOP)),
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

fn detail_text(row: &ManagerRow) -> Text<'static> {
    let workspace = &row.workspace;
    let mut lines = vec![
        Line::from(Span::styled(
            workspace.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "path: {}",
            workspace
                .path
                .as_ref()
                .map_or_else(|| "missing".to_owned(), |path| path.display().to_string())
        )),
        Line::from(format!(
            "bookmark: {} ({})",
            workspace.associated_bookmark.as_deref().unwrap_or("none"),
            integration_detail(&row.bookmark_integration)
        )),
        Line::from(format!(
            "work: {}",
            integration_detail(&row.work_integration)
        )),
        Line::from(format!(
            "checkout: {}",
            working_copy_label(workspace.working_copy)
        )),
        Line::from(format!("change: {}", workspace.change_id)),
        Line::from(format!("commit: {}", short_id(&workspace.commit_id))),
        Line::from(""),
        Line::from(workspace.description.clone()),
    ];
    if !row.warnings.is_empty() || !workspace.hazards.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Warnings",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.extend(
            row.warnings
                .iter()
                .map(|warning| Line::from(format!("• {warning}"))),
        );
        lines.extend(
            workspace
                .hazards
                .iter()
                .map(|hazard| Line::from(format!("• {}", hazard.message))),
        );
    }
    Text::from(lines)
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 78, 82);
    frame.render_widget(Clear, popup);
    let lines = [
        "↑/↓ or j/k   move",
        "Space         mark/unmark workspace",
        "a / c         mark all visible / clear marks",
        "Enter         return highlighted workspace",
        "/             search (Enter accepts, Esc closes)",
        "f or 1–4      cycle / select all, integrated, unfinished, problems",
        "Tab           show details on a narrow terminal",
        "n             create workspace",
        "i             refresh highlighted checkout details",
        "h             repository health report",
        "y             copy workspace path with OSC 52",
        "p / d         prune preview / remove preview",
        "q             cancel",
        "",
        "@ current   - previous   ^ default   ! warning",
        "",
        "Any destructive action opens a complete preview first.",
    ];
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Help — any key closes "),
            ),
        popup,
    );
}

fn render_search(frame: &mut Frame<'_>, area: Rect, query: &str) {
    let popup = centered_fixed(area, 70, 5);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(query).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search — Enter accepts "),
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
            Span::raw("workspace  "),
            Span::styled(&form.name, name_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("start at   "),
            Span::styled(choices, base_style),
        ]),
        Line::from(vec![
            Span::raw("revset    "),
            Span::styled(base, base_style),
        ]),
        Line::from(""),
        Line::from("Tab changes field · ←/→ changes base · Enter creates · Esc cancels"),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New workspace "),
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
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(5),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "PERMANENT: confirmed rows are forgotten and their directories may be deleted.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        chunks[0],
    );
    let mut lines = Vec::new();
    for row in &preview.plan.rows {
        lines.push(Line::from(Span::styled(
            row.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if let Some(path) = &row.path {
            lines.push(Line::from(format!("  path: {}", path.display())));
        }
        if let Some(bookmark) = &row.bookmark {
            lines.push(Line::from(format!("  bookmark: {bookmark}")));
        }
        if let Some(blocked) = &row.blocked {
            lines.push(Line::from(Span::styled(
                format!("  BLOCKED: {blocked}"),
                Style::default().fg(Color::Red),
            )));
        }
        if row.risky {
            lines.push(Line::from(Span::styled(
                "  RISKY: skipped unless risky rows are included",
                Style::default().fg(Color::Yellow),
            )));
        }
        lines.extend(
            row.warnings
                .iter()
                .map(|value| Line::from(format!("  ! {value}"))),
        );
        lines.extend(
            row.ignored
                .iter()
                .map(|value| Line::from(format!("  ignored: {value}"))),
        );
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((preview.scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    let bookmark = if preview.options.delete_bookmarks {
        "delete"
    } else {
        "keep"
    };
    let options = format!(
        "[b] bookmark: {bookmark}   [r] include risky: {}   [i] acknowledge ignored: {}\nEnter confirms permanently · Esc cancels · ↑↓ scroll",
        checked(preview.options.include_risky),
        checked(preview.options.acknowledge_ignored)
    );
    frame.render_widget(Paragraph::new(options), chunks[2]);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title(format!(
            " Removal preview — {} target{} ",
            preview.plan.rows.len(),
            if preview.plan.rows.len() == 1 {
                ""
            } else {
                "s"
            }
        )),
        popup,
    );
}

fn render_report(frame: &mut Frame<'_>, area: Rect, title: &str, body: &str, scroll: u16) {
    let popup = centered(area, 90, 88);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} — ↑↓ scroll, any other key closes ")),
            ),
        popup,
    );
}

fn field_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
        assert!(!options.acknowledge_ignored);
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
            },
            &tx,
        )
        .unwrap();
        assert!(app.selected.contains("blocked"));
        assert!(matches!(app.screen, Screen::Report { .. }));
        assert!(matches!(rx.try_recv(), Ok(WorkerTask::Capture { .. })));
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
}
