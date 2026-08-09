use std::{
    env,
    error::Error,
    io,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use chrono::{DateTime, Datelike, Local};
use crossbeam::channel::{Select, Sender, unbounded};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    style::palette::{self, tailwind},
    widgets::*,
};
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_epoch_info::EpochInfo;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;
use surfpool_core::{solana_rpc_client::rpc_client::RpcClient, surfnet::SLOTS_PER_EPOCH};
use surfpool_types::{
    BlockProductionMode, ClockCommand, SanitizedConfig, SimnetCommand, SimnetEvent,
};
use txtx_core::kit::{channel::Receiver, types::frontend::BlockEvent};
use txtx_gql::kit::types::frontend::{LogEvent, LogLevel, TransientLogEventStatus};

use crate::runbook::persist_log;

const HELP_TEXT: &str = "(Esc) quit | (↑) move up | (↓) move down";
const SURFPOOL_LINK: &str = "Need help? https://docs.surfpool.run/tui";

const ITEM_HEIGHT: usize = 1;

/// Theme variants for the TUI
#[derive(Debug, Clone, Copy, PartialEq)]
enum Theme {
    Classic,
    Halloween,
}

impl Theme {
    /// Detect the current theme based on the date
    fn detect() -> Self {
        let now = Local::now();
        if now.month() == 10 && now.day() == 31 {
            Theme::Halloween
        } else {
            Theme::Classic
        }
    }

    /// Get the color palette for this theme
    fn palette(&self) -> &'static tailwind::Palette {
        match self {
            Theme::Classic => &palette::tailwind::CYAN,
            Theme::Halloween => &palette::tailwind::ORANGE,
        }
    }
}

// Terminal detection constants
const MACOS_TERMINAL: &str = "Apple_Terminal";
/// XTerm-based terminals
const XTERM_TERMINAL_PREFIX: &str = "xterm";
/// Indicates terminal supports 256-color palette (8-bit color)
const SUPPORTS_256_COLOR_INDICATOR: &str = "256";
/// Indicates terminal supports 24-bit true color
const SUPPORTS_TRUECOLOR_INDICATOR: &str = "24bit";
/// Legacy VT100 terminal type - basic 16-color support only
const LEGACY_VT100_TERMINAL: &str = "vt100";
/// ANSI terminal type - basic 16-color support only
const LEGACY_ANSI_TERMINAL: &str = "ansi";

/// Terminal detection and color capability analysis
pub struct TerminalChecks {
    term_program: Option<String>,
    term: Option<String>,
    colorterm: Option<String>,
}

impl Default for TerminalChecks {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalChecks {
    pub fn new() -> Self {
        Self {
            term_program: env::var("TERM_PROGRAM").ok(),
            term: env::var("TERM").ok(),
            colorterm: env::var("COLORTERM").ok(),
        }
    }

    /// Detects macOS Terminal.app which has known color mapping issues
    pub fn is_macos_terminal(&self) -> bool {
        self.term_program.as_deref() == Some(MACOS_TERMINAL)
    }

    /// Checks if terminal is XTerm-based
    fn is_xterm_based(&self) -> bool {
        self.term
            .as_deref()
            .map(|term| term.starts_with(XTERM_TERMINAL_PREFIX))
            .unwrap_or(false)
    }

    /// Checks if terminal advertises 256-color support (8-bit color)
    fn supports_256_colors(&self) -> bool {
        self.term
            .as_deref()
            .map(|term| term.contains(SUPPORTS_256_COLOR_INDICATOR))
            .unwrap_or(false)
    }

    /// Checks if COLORTERM environment variable is empty (indicates basic color support)
    fn has_no_colorterm_declaration(&self) -> bool {
        self.colorterm
            .as_deref()
            .map(|colorterm| colorterm.is_empty())
            .unwrap_or(true)
    }

    /// Checks if terminal advertises 24-bit true color support
    fn supports_truecolor(&self) -> bool {
        self.term
            .as_deref()
            .map(|term| term.contains(SUPPORTS_TRUECOLOR_INDICATOR))
            .unwrap_or(false)
    }

    /// Checks for legacy terminal types with basic 16-color support only
    fn is_legacy_terminal(&self) -> bool {
        matches!(
            self.term.as_deref(),
            Some(LEGACY_VT100_TERMINAL) | Some(LEGACY_ANSI_TERMINAL)
        )
    }

    /// XTerm terminals without 256-color support and no COLORTERM declaration
    fn is_limited_xterm_terminal(&self) -> bool {
        self.is_xterm_based() && !self.supports_256_colors() && self.has_no_colorterm_declaration()
    }

    /// Basic terminals without modern color support capabilities
    fn is_basic_color_terminal(&self) -> bool {
        self.has_no_colorterm_declaration()
            && !self.supports_256_colors()
            && !self.supports_truecolor()
    }

    /// Comprehensive check for terminals that need color compatibility mode
    /// Returns true if the terminal has limited color support or known color mapping issues
    pub fn is_limited_terminal(&self) -> bool {
        // macOS Terminal.app has known color mapping issues with modern palettes
        if self.is_macos_terminal() {
            return true;
        }

        // XTerm terminals without proper color support
        if self.is_limited_xterm_terminal() {
            return true;
        }

        // Legacy terminal types with basic 16-color support only
        if self.is_legacy_terminal() {
            return true;
        }

        // Terminals without modern color capability declarations
        if self.is_basic_color_terminal() {
            return true;
        }

        false
    }
}

struct ColorTheme {
    background: Color,
    content_background: Color,
    accent: Color,
    white: Color,
    dark_gray: Color,
    light_gray: Color,
    error: Color,
    warning: Color,
    info: Color,
    success: Color,
}

impl ColorTheme {
    fn new(color: &tailwind::Palette) -> Self {
        let terminal_checks = TerminalChecks::new();
        if terminal_checks.is_limited_terminal() {
            Self::new_safe_colors(color)
        } else {
            Self::new_full_colors(color)
        }
    }

    fn new_full_colors(color: &tailwind::Palette) -> Self {
        Self {
            background: tailwind::ZINC.c900,
            content_background: tailwind::ZINC.c950,
            accent: color.c400,
            white: tailwind::SLATE.c100,
            dark_gray: tailwind::ZINC.c800,
            light_gray: tailwind::ZINC.c400,
            error: tailwind::RED.c400,
            warning: tailwind::YELLOW.c500,
            info: tailwind::BLUE.c500,
            success: tailwind::GREEN.c500,
        }
    }

    fn new_safe_colors(_color: &tailwind::Palette) -> Self {
        Self {
            background: Color::Reset,
            content_background: Color::Black,
            accent: Color::Cyan,
            white: Color::White,
            dark_gray: Color::DarkGray,
            light_gray: Color::Gray,
            error: Color::Red,
            warning: Color::Yellow,
            info: Color::Blue,
            success: Color::Green,
        }
    }
}

enum EventType {
    Debug,
    Info,
    Success,
    Failure,
    Warning,
}

struct App {
    state: TableState,
    scroll_state: ScrollbarState,
    colors: ColorTheme,
    simnet_events_rx: Receiver<SimnetEvent>,
    simnet_commands_tx: Sender<SimnetCommand>,
    clock: Clock,
    epoch_info: EpochInfo,
    successful_transactions: u32,
    events: Vec<(EventType, DateTime<Local>, String)>,
    transactions: Vec<(bool, DateTime<Local>, String)>, // (success, time, signature)
    include_debug_logs: bool,
    runbook_progress_rx: Vec<Receiver<BlockEvent>>,
    status_bar_message: Option<String>,
    displayed_url: DisplayedUrl,
    breaker: Option<Keypair>,
    paused: bool,
    blink_state: bool,
    last_blink: Instant,
    selected_tab: usize,
}

impl App {
    fn new(
        simnet_events_rx: Receiver<SimnetEvent>,
        simnet_commands_tx: Sender<SimnetCommand>,
        include_debug_logs: bool,
        runbook_progress_rx: Vec<Receiver<BlockEvent>>,
        displayed_url: DisplayedUrl,
        breaker: Option<Keypair>,
        initial_transactions: u64,
    ) -> App {
        let theme = Theme::detect();
        let palette = theme.palette();

        let mut events = vec![];
        let (rpc_url, ws_url, datasource) = match &displayed_url {
            DisplayedUrl::Datasource(config) | DisplayedUrl::Studio(config) => (
                config.rpc_url.clone(),
                config.ws_url.clone(),
                config.rpc_datasource_url.clone(),
            ),
        };
        events.push((
            EventType::Success,
            Local::now(),
            format!("Surfnet up and running, emulating local Solana validator (RPC: {rpc_url}, WS: {ws_url})"),
        ));
        events.push((
            EventType::Info,
            Local::now(),
            match &datasource {
                Some(url) => {
                    format!("Connecting surfnet to datasource {url}")
                }
                None => "No datasource configured, working in offline mode".to_string(),
            },
        ));

        App {
            state: TableState::default().with_offset(0),
            scroll_state: ScrollbarState::new(0),
            colors: ColorTheme::new(palette),
            simnet_events_rx,
            simnet_commands_tx,
            clock: Clock::default(),
            epoch_info: EpochInfo {
                epoch: 0,
                slot_index: 0,
                slots_in_epoch: SLOTS_PER_EPOCH,
                absolute_slot: 0,
                block_height: 0,
                transaction_count: None,
            },
            successful_transactions: initial_transactions as u32,
            events,
            transactions: vec![],
            include_debug_logs,
            runbook_progress_rx,
            status_bar_message: None,
            displayed_url,
            breaker,
            paused: false,
            blink_state: false,
            last_blink: Instant::now(),
            selected_tab: 0,
        }
    }

    pub fn slot(&self) -> usize {
        self.clock.slot.try_into().unwrap()
    }

    pub fn epoch_progress(&self) -> u16 {
        let absolute = self.slot() as u64;
        let progress = absolute.rem_euclid(self.epoch_info.slots_in_epoch);
        ((progress as f64 / self.epoch_info.slots_in_epoch as f64) * 100.0) as u16
    }

    pub fn next(&mut self) {
        self.state.select_next();
        self.scroll_state.next();
        *self.state.offset_mut() = ITEM_HEIGHT;
    }

    pub fn tail(&mut self) {
        self.state.select_last();
    }

    pub fn previous(&mut self) {
        self.state.select_previous();
        self.scroll_state.prev();
        let current_offset = self.state.offset();
        let new_offset = if current_offset == 0 {
            0
        } else {
            current_offset - ITEM_HEIGHT
        };
        *self.state.offset_mut() = new_offset;
    }

    pub fn update_blink_state(&mut self) {
        if self.paused {
            let now = Instant::now();
            if now.duration_since(self.last_blink).as_millis() >= 500 {
                self.blink_state = !self.blink_state;
                self.last_blink = now;
            }
        } else {
            self.blink_state = false;
        }
    }

    // pub fn set_colors(&mut self) {
    //     self.colors = ColorTheme::new(&tailwind::EMERALD)
    // }
}

pub enum DisplayedUrl {
    Studio(SanitizedConfig),
    Datasource(SanitizedConfig),
}

pub fn start_app(
    simnet_events_rx: Receiver<SimnetEvent>,
    simnet_commands_tx: Sender<SimnetCommand>,
    include_debug_logs: bool,
    runbook_progress_rx: Vec<Receiver<BlockEvent>>,
    displayed_url: DisplayedUrl,
    breaker: Option<Keypair>,
    initial_transactions: u64,
) -> Result<(), Box<dyn Error>> {
    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // create app and run it
    let app = App::new(
        simnet_events_rx,
        simnet_commands_tx,
        include_debug_logs,
        runbook_progress_rx,
        displayed_url,
        breaker,
        initial_transactions,
    );
    let res = run_app(&mut terminal, app);

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{err:?}");
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, mut app: App) -> io::Result<()> {
    let (tx, rx) = unbounded();
    let rpc_api_url = match app.displayed_url {
        DisplayedUrl::Datasource(ref config) => config.rpc_url.clone(),
        DisplayedUrl::Studio(ref config) => config.rpc_url.clone(),
    };
    let _ = hiro_system_kit::thread_named("break solana").spawn(move || {
        while let Ok((message, keypair)) = rx.recv() {
            let client =
                RpcClient::new_with_commitment(&rpc_api_url, CommitmentConfig::processed());
            let _ = client.get_latest_blockhash().and_then(|blockhash| {
                let transaction = Transaction::new(&[keypair], message, blockhash);
                client.send_transaction(&transaction)
            });
        }
    });

    let ctrlc_simnet_cmd_tx_clone = app.simnet_commands_tx.clone();

    let should_exit = Arc::new(AtomicBool::new(false));
    let should_exit_read = Arc::clone(&should_exit);
    let should_exit_write = Arc::clone(&should_exit);

    ctrlc::set_handler(move || {
        // Send terminate command to allow graceful shutdown (Drop to run)
        let _ = ctrlc_simnet_cmd_tx_clone.send(SimnetCommand::Terminate(None));
        should_exit_write.store(true, std::sync::atomic::Ordering::Release);
    })
    .expect("Error setting Ctrl-C handler");

    let mut runbooks_completed = false;
    loop {
        if should_exit_read.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }

        let mut selector = Select::new();
        let mut handles = vec![];
        let mut new_events = vec![];

        {
            selector.recv(&app.simnet_events_rx);
            if !runbooks_completed {
                for rx in app.runbook_progress_rx.iter() {
                    handles.push(selector.recv(rx));
                }
            }

            loop {
                let Ok(oper) = selector.try_select() else {
                    break;
                };
                match oper.index() {
                    0 => match oper.recv(&app.simnet_events_rx) {
                        Ok(event) => match &event {
                            // The startup sequence matters to other listeners
                            // (the headless watchdog, the SDK's event stream),
                            // not to the TUI.
                            SimnetEvent::StartupStatusChanged(_) => {}
                            SimnetEvent::AccountUpdate(dt, _account) => {
                                new_events.push((
                                    EventType::Success,
                                    *dt,
                                    event.account_update_msg(),
                                ));
                            }
                            SimnetEvent::PluginLoaded(_) => {
                                new_events.push((
                                    EventType::Success,
                                    Local::now(),
                                    event.plugin_loaded_msg(),
                                ));
                            }
                            SimnetEvent::EpochInfoUpdate(epoch_info) => {
                                app.epoch_info = epoch_info.clone();
                                new_events.push((
                                    EventType::Success,
                                    Local::now(),
                                    event.epoch_info_update_msg(),
                                ));
                            }
                            SimnetEvent::SystemClockUpdated(clock) => {
                                app.clock = clock.clone();
                            }
                            SimnetEvent::ClockUpdate(ClockCommand::PauseWithConfirmation(_)) => {
                                app.paused = true;
                            }
                            SimnetEvent::ClockUpdate(ClockCommand::Resume) => {
                                app.paused = false;
                            }
                            SimnetEvent::ClockUpdate(ClockCommand::Toggle) => {
                                app.paused = !app.paused;
                            }
                            SimnetEvent::ClockUpdate(_) => {}
                            SimnetEvent::ErrorLog(dt, log) => {
                                new_events.push((EventType::Failure, *dt, log.clone()));
                            }
                            SimnetEvent::InfoLog(dt, log) => {
                                new_events.push((EventType::Info, *dt, log.clone()));
                            }
                            SimnetEvent::DebugLog(dt, log) => {
                                if app.include_debug_logs {
                                    new_events.push((EventType::Debug, *dt, log.clone()));
                                }
                            }
                            SimnetEvent::WarnLog(dt, log) => {
                                new_events.push((EventType::Warning, *dt, log.clone()));
                            }
                            SimnetEvent::TransactionReceived(_dt, _transaction) => {}
                            SimnetEvent::TransactionProcessed(dt, meta, err) => {
                                let success = err.is_none();
                                let sig = meta.signature.to_string();
                                app.transactions.push((success, *dt, sig));

                                if let Some(err) = err {
                                    new_events.push((
                                        EventType::Failure,
                                        *dt,
                                        format!("Failed processing tx {}: {}", meta.signature, err),
                                    ));
                                } else {
                                    if runbooks_completed {
                                        new_events.push((
                                            EventType::Success,
                                            *dt,
                                            format!("Processed tx {}", meta.signature),
                                        ));
                                        if app.include_debug_logs {
                                            for log in meta.logs.iter() {
                                                new_events.push((
                                                    EventType::Debug,
                                                    *dt,
                                                    log.clone(),
                                                ));
                                            }
                                        }
                                    }
                                    app.successful_transactions += 1;
                                }
                            }
                            SimnetEvent::Aborted(_error) => {
                                break;
                            }
                            SimnetEvent::CoreStarted(_) => {}
                            SimnetEvent::Connected(_) => {}
                            SimnetEvent::Shutdown => {
                                return Ok(());
                            }
                            SimnetEvent::TaggedProfile {
                                result,
                                tag,
                                timestamp,
                            } => {
                                let msg = format!(
                                    "Profiled [{}]: {} CUs",
                                    tag, result.transaction_profile.compute_units_consumed
                                );
                                new_events.push((EventType::Info, *timestamp, msg));
                            }
                            SimnetEvent::RunbookStarted(runbook_id) => {
                                runbooks_completed = false;
                                new_events.push((
                                    EventType::Success,
                                    Local::now(),
                                    format!("Runbook '{}' execution started", runbook_id),
                                ));
                                let _ = app
                                    .simnet_commands_tx
                                    .send(SimnetCommand::StartRunbookExecution(runbook_id.clone()));
                            }
                            SimnetEvent::RunbookCompleted(runbook_id, errors) => {
                                runbooks_completed = true;
                                new_events.push((
                                    EventType::Success,
                                    Local::now(),
                                    format!("Runbook '{}' execution completed", runbook_id),
                                ));
                                let _ = app.simnet_commands_tx.send(
                                    SimnetCommand::CompleteRunbookExecution(
                                        runbook_id.clone(),
                                        errors.clone(),
                                    ),
                                );
                                app.status_bar_message = None;
                            }
                        },
                        Err(_) => break,
                    },
                    i => match oper.recv(&app.runbook_progress_rx[i - 1]) {
                        Ok(event) => match event {
                            BlockEvent::LogEvent(event) => {
                                let summary = event.summary();
                                let message = event.message();
                                let level = event.level();
                                let ns = event.namespace();
                                let msg = format!("{} {}", summary, message);

                                match &event {
                                    LogEvent::Static(event) => {
                                        persist_log(
                                            &message,
                                            &summary,
                                            ns,
                                            &level,
                                            &LogLevel::Info,
                                            false,
                                        );
                                        match event.level {
                                            LogLevel::Trace => {}
                                            LogLevel::Debug => {
                                                new_events.push((
                                                    EventType::Debug,
                                                    Local::now(),
                                                    msg,
                                                ));
                                            }
                                            LogLevel::Info => {
                                                new_events.push((
                                                    EventType::Info,
                                                    Local::now(),
                                                    msg,
                                                ));
                                            }
                                            LogLevel::Warn => {
                                                new_events.push((
                                                    EventType::Warning,
                                                    Local::now(),
                                                    msg,
                                                ));
                                            }
                                            LogLevel::Error => {
                                                new_events.push((
                                                    EventType::Failure,
                                                    Local::now(),
                                                    msg,
                                                ));
                                            }
                                        }
                                    }
                                    LogEvent::Transient(event) => match event.status {
                                        TransientLogEventStatus::Pending(_) => {
                                            app.status_bar_message = Some(msg);
                                        }
                                        TransientLogEventStatus::Success(_) => {
                                            app.status_bar_message = None;
                                            new_events.push((EventType::Info, Local::now(), msg));
                                            persist_log(
                                                &message,
                                                &summary,
                                                ns,
                                                &level,
                                                &LogLevel::Info,
                                                false,
                                            );
                                        }
                                        TransientLogEventStatus::Failure(_) => {
                                            app.status_bar_message = None;
                                            new_events.push((
                                                EventType::Failure,
                                                Local::now(),
                                                msg,
                                            ));
                                            persist_log(
                                                &message,
                                                &summary,
                                                ns,
                                                &level,
                                                &LogLevel::Info,
                                                false,
                                            );
                                        }
                                    },
                                }
                            }
                            _ => break,
                        },
                        Err(_) => {
                            runbooks_completed = true;
                            break;
                        }
                    },
                }
            }
        }

        for event in new_events {
            app.events.push(event);
            app.tail();
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key_event) = event::read()? {
                if key_event.kind == KeyEventKind::Press {
                    use KeyCode::*;
                    if key_event.modifiers == KeyModifiers::CONTROL && key_event.code == Char('c') {
                        // Send terminate command to allow graceful shutdown (Drop to run)
                        let _ = app.simnet_commands_tx.send(SimnetCommand::Terminate(None));
                        return Ok(());
                    }
                    match key_event.code {
                        Char('q') | Esc => {
                            // Send terminate command to allow graceful shutdown (Drop to run)
                            let _ = app.simnet_commands_tx.send(SimnetCommand::Terminate(None));
                            return Ok(());
                        }
                        Down => app.next(),
                        Up => app.previous(),
                        Left => app.selected_tab = 0,
                        Right => app.selected_tab = 1,
                        Char('f') | Char('j') => {
                            // Break Solana
                            let sender = app.breaker.as_ref().unwrap();
                            let instruction = system_instruction::transfer(
                                &sender.pubkey(),
                                &Pubkey::new_unique(),
                                100,
                            );
                            let message = Message::new(&[instruction], Some(&sender.pubkey()));
                            let _ = tx.send((message, sender.insecure_clone()));
                        }
                        Char(' ') => {
                            app.paused = !app.paused;
                            let _ = app
                                .simnet_commands_tx
                                .send(SimnetCommand::CommandClock(None, ClockCommand::Toggle));
                        }
                        Tab => {
                            let _ = app
                                .simnet_commands_tx
                                .send(SimnetCommand::SlotForward(None));
                        }
                        Char('t') => {
                            let _ = app.simnet_commands_tx.send(
                                SimnetCommand::UpdateBlockProductionMode(
                                    BlockProductionMode::Transaction,
                                ),
                            );
                        }
                        Char('c') => {
                            let _ = app.simnet_commands_tx.send(
                                SimnetCommand::UpdateBlockProductionMode(
                                    BlockProductionMode::Clock,
                                ),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }

        app.update_blink_state();
        terminal.draw(|f| ui(f, &mut app))?;
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let rects = Layout::vertical([
        Constraint::Length(3), // Header with border (2 content + 1 bottom border)
        Constraint::Length(1), // Gap
        Constraint::Min(5),    // Events
        Constraint::Length(2), // Footer
    ])
    .split(f.area());

    // Background
    let bg_style = Style::new().bg(app.colors.background);
    f.render_widget(Block::default().style(bg_style), f.area());

    render_header(f, app, rects[0]);
    render_events(f, app, rects[2]);
    render_scrollbar(f, app, rects[2]);
    render_footer(f, app, rects[3]);
}

fn title_block(title: &str, alignment: Alignment) -> Block<'_> {
    let title = Line::from(title).alignment(alignment);
    Block::new().borders(Borders::NONE).title(title)
}

fn render_header(f: &mut Frame, app: &mut App, area: Rect) {
    // Layout: [slot/epoch box with gray border] [gap] [studio box with cyan border]
    let url = match &app.displayed_url {
        DisplayedUrl::Datasource(config) => config.rpc_url.clone(),
        DisplayedUrl::Studio(config) => config.studio_url.clone(),
    };
    let studio_width = (url.len() + 13) as u16; // "Studio │ " + url + borders + padding

    let columns = Layout::horizontal([
        Constraint::Min(30),              // Slot/epoch box
        Constraint::Length(1),            // Gap
        Constraint::Length(studio_width), // Studio box
    ])
    .split(area);

    // Left box: slot/epoch with light gray border
    let slot_box = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.colors.light_gray))
        .border_type(BorderType::Rounded);
    f.render_widget(slot_box, columns[0]);
    let inner_left = columns[0].inner(Margin::new(1, 0));
    // Vertical center: skip line 0, render on line 1
    let centered_left = Rect::new(inner_left.x, inner_left.y + 1, inner_left.width, 1);
    render_epoch(f, app, centered_left);

    // Right box: Studio with cyan border
    let studio_box = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.colors.accent))
        .border_type(BorderType::Rounded);
    f.render_widget(studio_box, columns[2]);
    let inner_right = columns[2].inner(Margin::new(1, 0));
    // Vertical center: skip line 0, render on line 1
    let centered_right = Rect::new(inner_right.x, inner_right.y + 1, inner_right.width, 1);
    render_stats(f, app, centered_right);
}

fn render_epoch(f: &mut Frame, app: &mut App, area: Rect) {
    // The compact bar row: [progress] SLOT xxx    EPOCH xxx ●
    let slot_str = format!("{}", app.clock.slot);
    let epoch_str = format!("{}", app.epoch_info.epoch);

    let bar_row = Layout::horizontal([
        Constraint::Length(1),                          // Left bracket
        Constraint::Min(5),                             // Progress bars (min 5 slots)
        Constraint::Length(1),                          // Right bracket
        Constraint::Length(2),                          // Space
        Constraint::Length(5),                          // "SLOT "
        Constraint::Length(slot_str.len() as u16),      // Slot number
        Constraint::Length(4),                          // Space
        Constraint::Length(7),                          // "EPOCH  "
        Constraint::Length(epoch_str.len() as u16 + 1), // Epoch number + space
        Constraint::Length(2),                          // Space + circle
    ])
    .split(area);

    // Left bracket
    let bracket = Paragraph::new("▕").style(Style::default().fg(app.colors.dark_gray));
    f.render_widget(bracket, bar_row[0]);

    // Progress bars
    render_slot_bar(f, app, bar_row[1]);

    // Right bracket
    let bracket = Paragraph::new("▏").style(Style::default().fg(app.colors.dark_gray));
    f.render_widget(bracket, bar_row[2]);

    // SLOT label
    let slot_label = Paragraph::new("SLOT ").style(Style::default().fg(app.colors.light_gray));
    f.render_widget(slot_label, bar_row[4]);

    // Slot number
    let slot_num = Paragraph::new(slot_str).style(Style::default().fg(app.colors.white));
    f.render_widget(slot_num, bar_row[5]);

    // EPOCH label
    let epoch_label = Paragraph::new("EPOCH  ").style(Style::default().fg(app.colors.light_gray));
    f.render_widget(epoch_label, bar_row[7]);

    // Epoch number + space
    let epoch_num =
        Paragraph::new(format!("{} ", epoch_str)).style(Style::default().fg(app.colors.white));
    f.render_widget(epoch_num, bar_row[8]);

    // Circular progress indicator based on epoch progress
    let epoch_progress = app.epoch_progress();
    let circle_char = match epoch_progress {
        0..=12 => "○",  // Empty
        13..=37 => "◔", // Quarter
        38..=62 => "◑", // Half
        63..=87 => "◕", // Three quarters
        _ => "●",       // Full
    };
    let circle = Paragraph::new(circle_char).style(Style::default().fg(app.colors.accent));
    f.render_widget(circle, bar_row[9]);
}

fn render_stats(f: &mut Frame, app: &mut App, area: Rect) {
    let url = match &app.displayed_url {
        DisplayedUrl::Datasource(config) => config.rpc_url.clone(),
        DisplayedUrl::Studio(config) => config.studio_url.clone(),
    };

    // Content: "Studio │ <url>"
    let content = Line::from(vec![
        Span::styled("Studio ", Style::default().fg(app.colors.white)),
        Span::styled("│ ", Style::default().fg(app.colors.light_gray)),
        Span::styled(url, Style::default().fg(app.colors.accent)),
    ]);

    let paragraph = Paragraph::new(content).alignment(Alignment::Center);

    f.render_widget(paragraph, area);
}

fn render_slot_bar(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 {
        return;
    }

    let total_bars = area.width as usize;
    let cursor = app.slot() % (total_bars + 1);

    let mut spans = Vec::new();

    for i in 0..total_bars {
        let is_filled = i < cursor;
        let style = if is_filled {
            if app.paused && app.blink_state {
                Style::default().fg(app.colors.dark_gray)
            } else {
                Style::default().fg(app.colors.accent)
            }
        } else {
            Style::default().fg(app.colors.dark_gray)
        };

        spans.push(Span::styled("▌", style));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

fn render_events(f: &mut Frame, app: &mut App, area: Rect) {
    let rects = Layout::vertical([
        Constraint::Length(1), // Tabs
        Constraint::Min(1),    // Content (with separator)
    ])
    .split(area);

    // Render tabs
    let tx_label = format!("Transactions ({})", app.transactions.len());
    let tabs = Tabs::new(vec!["Logs", &tx_label])
        .select(app.selected_tab)
        .style(Style::default().fg(app.colors.light_gray))
        .highlight_style(Style::default().fg(app.colors.accent))
        .divider("│")
        .padding(" ", " ");
    f.render_widget(tabs, rects[0]);

    // Fill content area with darker background
    let bg_fill = Block::default().style(Style::default().bg(app.colors.content_background));
    f.render_widget(bg_fill, rects[1]);

    // Separator line at top of dark area using overline character (thinner)
    let separator_line = Line::from("‾".repeat(rects[1].width as usize)).style(
        Style::default()
            .fg(app.colors.light_gray)
            .bg(app.colors.content_background),
    );
    let separator_area = Rect::new(rects[1].x, rects[1].y, rects[1].width, 1);
    f.render_widget(Paragraph::new(separator_line), separator_area);

    // Content area below separator
    let content_area = Rect::new(
        rects[1].x,
        rects[1].y + 1,
        rects[1].width,
        rects[1].height.saturating_sub(1),
    );

    // Render content based on selected tab
    if app.selected_tab == 1 {
        // Transactions tab
        render_transactions(f, app, content_area);
        return;
    }

    // Estimate available width for the log column
    let log_col_width = content_area.width.saturating_sub(1 + 12 + 2); // event + timestamp + padding

    let mut rows = Vec::new();
    for (event_type, dt, log) in &app.events {
        let color = match event_type {
            EventType::Failure => app.colors.error,
            EventType::Info => app.colors.info,
            EventType::Success => app.colors.success,
            EventType::Warning => app.colors.warning,
            EventType::Debug => app.colors.light_gray,
        };

        // Smart word wrapping
        let mut current_line = String::new();
        let mut first = true;
        for word in log.split_whitespace() {
            if current_line.len() + word.len() + 1 > log_col_width as usize
                && !current_line.is_empty()
            {
                // Push the current line
                let row = if first {
                    vec![
                        Cell::new("⏐").style(color),
                        Cell::new(dt.format("%H:%M:%S.%3f").to_string())
                            .style(app.colors.light_gray),
                        Cell::new(current_line.clone()),
                    ]
                } else {
                    vec![
                        Cell::new(" "),
                        Cell::new(" "),
                        Cell::new(current_line.clone()),
                    ]
                };
                rows.push(
                    Row::new(row)
                        .style(Style::new().fg(app.colors.white))
                        .height(1),
                );
                current_line.clear();
                first = false;
            }
            if !current_line.is_empty() {
                current_line.push(' ');
            }
            current_line.push_str(word);
        }
        // Push any remaining text
        if !current_line.is_empty() {
            let row = if first {
                vec![
                    Cell::new("⏐").style(color),
                    Cell::new(dt.format("%H:%M:%S.%3f").to_string()).style(app.colors.light_gray),
                    Cell::new(current_line.clone()),
                ]
            } else {
                vec![
                    Cell::new(" "),
                    Cell::new(" "),
                    Cell::new(current_line.clone()),
                ]
            };
            rows.push(
                Row::new(row)
                    .style(Style::new().fg(app.colors.white))
                    .height(1),
            );
        }
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(12),
            Constraint::Min(1),
        ],
    )
    .style(
        Style::new()
            .fg(app.colors.white)
            .bg(app.colors.content_background),
    )
    .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, content_area, &mut app.state);
}

fn render_transactions(f: &mut Frame, app: &mut App, area: Rect) {
    // Get studio URL if available
    let studio_url = match &app.displayed_url {
        DisplayedUrl::Studio(config) => Some(config.studio_url.clone()),
        DisplayedUrl::Datasource(_) => None,
    };

    let mut rows = Vec::new();
    for (success, dt, sig) in app.transactions.iter() {
        let color = if *success {
            app.colors.success
        } else {
            app.colors.error
        };

        // Build the URL or just show signature
        let url_or_sig = match &studio_url {
            Some(base_url) => format!("{}?t={}", base_url, sig),
            None => sig.clone(),
        };

        let row = vec![
            Cell::new("⏐").style(color),
            Cell::new(dt.format("%H:%M:%S.%3f").to_string()).style(app.colors.light_gray),
            Cell::new(url_or_sig).style(Style::default().fg(app.colors.white)),
        ];
        rows.push(
            Row::new(row)
                .style(Style::new().fg(app.colors.white))
                .height(1),
        );
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(12),
            Constraint::Min(1),
        ],
    )
    .style(
        Style::new()
            .fg(app.colors.white)
            .bg(app.colors.content_background),
    )
    .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, area, &mut app.state);
}

fn render_scrollbar(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_stateful_widget(
        Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        }),
        &mut app.scroll_state,
    );
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let rects = Layout::horizontal([
        Constraint::Min(30),    // Help
        Constraint::Length(50), // https://txtx.run
    ])
    .split(area);

    let status = match app.status_bar_message {
        Some(ref message) => title_block(message.as_str(), Alignment::Left)
            .style(Style::new().fg(app.colors.light_gray)),
        None => {
            title_block(HELP_TEXT, Alignment::Left).style(Style::new().fg(app.colors.light_gray))
        }
    };
    f.render_widget(status, rects[0]);

    let link =
        title_block(SURFPOOL_LINK, Alignment::Right).style(Style::new().fg(app.colors.white));
    f.render_widget(link, rects[1]);
}
