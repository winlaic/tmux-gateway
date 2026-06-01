use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use serde::Deserialize;
use toml::Value;

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 3;
const DEFAULT_SCAN_CONCURRENCY: usize = 32;
const DEFAULT_MOUSE_SCROLL_LINES: usize = 5;
const DEFAULT_AUTO_REFRESH_SECS: u64 = 15;
const DEFAULT_EXPAND_LEVEL: ExpandLevel = ExpandLevel::Server;
const DEFAULT_STATUS: &str =
    "Enter attach | right-click menu | a/x add/kill | r reload | /? n/N | ^u/^d | gg/G | q";
const DEFAULT_SERVER_LINE_TEXT: &str = "[Server] {server_name}";
const DEFAULT_SESSION_LINE_TEXT: &str = "[Session] {session_name}";
const DEFAULT_WINDOW_LINE_TEXT: &str = "[Window] {is_active}{window_index}: {window_name}";
const DEFAULT_PANE_LINE_TEXT: &str =
    "[Pane] {is_active}{pane_index} {pane_id} {process_elapsed_time} {pane_commandline}";
const STATUS_TTL: Duration = Duration::from_secs(3);
const LOG_PATH: &str = "tmux-gateway.log";

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[arg(
        short,
        long,
        help = "Config file path; defaults to ~/.config/tmux-gateway/config.toml"
    )]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print a server/session/window/pane tree.
    List,

    /// Attach to tmux on a configured host.
    Attach {
        host: String,

        #[arg(short, long)]
        session: Option<String>,

        #[arg(short, long)]
        window: Option<String>,

        #[arg(short, long)]
        pane: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    hosts: Option<Value>,
    connect_timeout_secs: Option<u64>,
    scan_concurrency: Option<usize>,
    mouse_scroll_lines: Option<usize>,
    auto_refresh_secs: Option<u64>,
    default_expand_level: Option<String>,
    server_line_text: Option<String>,
    session_line_text: Option<String>,
    window_line_text: Option<String>,
    pane_line_text: Option<String>,
}

#[derive(Clone, Debug)]
struct Config {
    hosts: Vec<String>,
    connect_timeout_secs: u64,
    scan_concurrency: usize,
    mouse_scroll_lines: usize,
    auto_refresh_secs: u64,
    default_expand_level: ExpandLevel,
    line_formats: LineFormats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpandLevel {
    Server,
    Session,
    Window,
    Pane,
}

#[derive(Clone, Debug)]
struct LineFormats {
    server: String,
    session: String,
    window: String,
    pane: String,
}

#[derive(Clone, Debug)]
struct PaneInfo {
    session_name: String,
    session_id: String,
    window_index: String,
    window_id: String,
    window_name: String,
    pane_index: String,
    pane_id: String,
    pane_pid: u32,
    pane_current_command: String,
    pane_commandline: String,
    pane_title: String,
    active_window: bool,
    active_pane: bool,
    busy_duration_secs: Option<u64>,
}

#[derive(Clone, Debug)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    elapsed_secs: u64,
    command: String,
    commandline: String,
}

#[derive(Clone, Debug)]
struct HostTree {
    host: String,
    panes: Vec<PaneInfo>,
    error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum NodeId {
    Host(String),
    Session {
        host: String,
        session: String,
    },
    Window {
        host: String,
        session: String,
        window: String,
    },
    Pane {
        host: String,
        session: String,
        window: String,
        pane: String,
    },
}

#[derive(Clone, Debug)]
struct VisibleRow {
    id: NodeId,
    depth: usize,
    label: String,
    detail: String,
    search_text: String,
    selectable: bool,
    busy_duration_secs: Option<u64>,
}

struct App {
    config: Config,
    trees: Vec<HostTree>,
    expanded: BTreeSet<NodeId>,
    rows: Vec<VisibleRow>,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    screen_area: Rect,
    tree_area: Rect,
    status: String,
    status_expires_at: Option<Instant>,
    search: String,
    search_direction: SearchDirection,
    last_search_direction: SearchDirection,
    mode: Mode,
    pending_g: bool,
    attach_request: Option<AttachTarget>,
}

struct AutoRefresh {
    config: Config,
    interval: Option<Duration>,
    next_refresh: Instant,
    receiver: Option<Receiver<Vec<HostTree>>>,
}

impl AutoRefresh {
    fn new(config: Config) -> Self {
        let interval =
            (config.auto_refresh_secs > 0).then(|| Duration::from_secs(config.auto_refresh_secs));
        Self {
            config,
            interval,
            next_refresh: Instant::now() + interval.unwrap_or_default(),
            receiver: None,
        }
    }

    fn poll(&mut self) -> Option<Vec<HostTree>> {
        let Some(interval) = self.interval else {
            return None;
        };

        if let Some(receiver) = self.receiver.take() {
            match receiver.try_recv() {
                Ok(trees) => {
                    self.next_refresh = Instant::now() + interval;
                    return Some(trees);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.next_refresh = Instant::now() + interval;
                    return None;
                }
            }
        }

        if Instant::now() >= self.next_refresh {
            let (sender, receiver) = mpsc::channel();
            let config = self.config.clone();
            thread::spawn(move || {
                let _ = sender.send(collect_hosts(&config));
            });
            self.receiver = Some(receiver);
            self.next_refresh = Instant::now() + interval;
        }

        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchDirection {
    Down,
    Up,
}

impl SearchDirection {
    fn reversed(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Down => "/",
            Self::Up => "?",
        }
    }
}

#[derive(Clone, Debug)]
enum Mode {
    Normal,
    Search,
    ContextMenu(ContextMenuState),
    Prompt(PromptState),
    SplitChoice(SplitChoiceState),
    Confirm(ConfirmState),
}

#[derive(Clone, Debug)]
struct ContextMenuState {
    items: Vec<ContextMenuItem>,
    selected: usize,
    area: Rect,
}

#[derive(Clone, Debug)]
struct ContextMenuItem {
    label: String,
    action: ContextAction,
    shortcut: Option<char>,
}

impl ContextMenuItem {
    fn display_label(&self) -> String {
        match self.shortcut {
            Some(shortcut) => format!("{}({shortcut})", self.label),
            None => self.label.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContextAction {
    Attach,
    Kill,
    NewSession,
    NewWindow,
    NewPane,
    Rename,
}

#[derive(Clone, Debug)]
struct PromptState {
    title: String,
    value: String,
    kind: PromptKind,
}

#[derive(Clone, Debug)]
enum PromptKind {
    CreateSession {
        host: String,
    },
    CreateWindow {
        host: String,
        target: String,
        after_window: Option<String>,
    },
    RenameSession {
        host: String,
        target: String,
    },
    RenameWindow {
        host: String,
        target: String,
    },
    RenamePane {
        host: String,
        pane: String,
    },
}

#[derive(Clone, Debug)]
struct SplitChoiceState {
    host: String,
    pane: String,
    selected: SplitChoice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitChoice {
    Vertical,
    Horizontal,
}

#[derive(Clone, Debug)]
struct ConfirmState {
    title: String,
    detail: String,
    action: ConfirmAction,
    selected_yes: bool,
}

#[derive(Clone, Debug)]
enum ConfirmAction {
    KillSession { host: String, target: String },
    KillWindow { host: String, target: String },
    KillPane { host: String, pane: String },
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let config = load_config(&config_path)?;

    match cli.command {
        Some(Commands::List) => {
            let trees = collect_hosts(&config);
            print_tree(&trees, &config.line_formats);
        }
        Some(Commands::Attach {
            host,
            session,
            window,
            pane,
        }) => {
            ensure_configured_host(&config, &host)?;
            attach_host(
                &host,
                session.as_deref(),
                window.as_deref(),
                pane.as_deref(),
                config.connect_timeout_secs,
            )?;
        }
        None => {
            run_tui(config)?;
        }
    }

    Ok(())
}

fn default_config_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home)
            .join(".config")
            .join("tmux-gateway")
            .join("config.toml"),
        None => PathBuf::from("config.toml"),
    }
}

fn load_config(path: &PathBuf) -> Result<Config> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }
        fs::write(path, "")
            .with_context(|| format!("failed to create empty config {}", path.display()))?;
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let raw: RawConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    let config = normalize_config(raw)?;

    Ok(config)
}

fn normalize_config(raw: RawConfig) -> Result<Config> {
    let hosts_value = raw
        .hosts
        .unwrap_or_else(|| Value::String("all".to_string()));
    let hosts = match hosts_value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(host) => Ok(host),
                other => bail!("hosts array must contain only strings, got {other:?}"),
            })
            .collect::<Result<Vec<_>>>()?,
        Value::String(value) if value == "all" => load_ssh_config_hosts()?,
        Value::String(value) => bail!("unsupported hosts string {value:?}; expected \"all\""),
        other => bail!("hosts must be an array of strings or the string \"all\", got {other:?}"),
    };

    let connect_timeout_secs = raw
        .connect_timeout_secs
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);
    let scan_concurrency = raw
        .scan_concurrency
        .unwrap_or(DEFAULT_SCAN_CONCURRENCY)
        .max(1);
    let mouse_scroll_lines = raw
        .mouse_scroll_lines
        .unwrap_or(DEFAULT_MOUSE_SCROLL_LINES)
        .max(1);
    let auto_refresh_secs = raw.auto_refresh_secs.unwrap_or(DEFAULT_AUTO_REFRESH_SECS);
    let default_expand_level = parse_expand_level(raw.default_expand_level.as_deref())?;

    Ok(Config {
        hosts: dedup_hosts(hosts),
        connect_timeout_secs,
        scan_concurrency,
        mouse_scroll_lines,
        auto_refresh_secs,
        default_expand_level,
        line_formats: LineFormats {
            server: raw
                .server_line_text
                .unwrap_or_else(|| DEFAULT_SERVER_LINE_TEXT.to_string()),
            session: raw
                .session_line_text
                .unwrap_or_else(|| DEFAULT_SESSION_LINE_TEXT.to_string()),
            window: raw
                .window_line_text
                .unwrap_or_else(|| DEFAULT_WINDOW_LINE_TEXT.to_string()),
            pane: raw
                .pane_line_text
                .unwrap_or_else(|| DEFAULT_PANE_LINE_TEXT.to_string()),
        },
    })
}

fn parse_expand_level(value: Option<&str>) -> Result<ExpandLevel> {
    let Some(value) = value else {
        return Ok(DEFAULT_EXPAND_LEVEL);
    };

    match value.trim().to_lowercase().as_str() {
        "server" => Ok(ExpandLevel::Server),
        "session" => Ok(ExpandLevel::Session),
        "window" => Ok(ExpandLevel::Window),
        "pane" => Ok(ExpandLevel::Pane),
        other => bail!(
            "unsupported default_expand_level {other:?}; expected server, session, window, or pane"
        ),
    }
}

fn load_ssh_config_hosts() -> Result<Vec<String>> {
    let home = std::env::var("HOME").context("HOME is not set; cannot read ~/.ssh/config")?;
    let path = PathBuf::from(home).join(".ssh").join("config");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read ssh config {}", path.display()))?;
    Ok(parse_ssh_config_hosts(&content))
}

fn parse_ssh_config_hosts(content: &str) -> Vec<String> {
    let mut hosts = Vec::new();

    for line in content.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }

        for pattern in parts {
            if is_concrete_ssh_host(pattern) {
                hosts.push(pattern.to_string());
            }
        }
    }

    dedup_hosts(hosts)
}

fn is_concrete_ssh_host(pattern: &str) -> bool {
    !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.contains('[')
        && !pattern.contains(']')
}

fn dedup_hosts(hosts: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();

    for host in hosts {
        if seen.insert(host.clone()) {
            deduped.push(host);
        }
    }

    deduped
}

fn ensure_configured_host(config: &Config, host: &str) -> Result<()> {
    if config.hosts.iter().any(|item| item == host) {
        return Ok(());
    }
    bail!("host {host:?} is not listed in config");
}

fn run_tui(config: Config) -> Result<()> {
    let mut app = App::new(config);
    let mut auto_refresh = AutoRefresh::new(app.config.clone());

    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    loop {
        app.apply_auto_refresh(&mut auto_refresh);
        terminal.draw(|frame| draw_app(frame, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if app.handle_key(key)? {
                    break;
                }
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => {}
        }

        if let Some(target) = app.take_attach_request() {
            terminal.clear()?;
            disable_raw_mode()?;
            execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;

            let result = attach_host(
                &target.host,
                Some(&target.session),
                Some(&target.window),
                Some(&target.pane),
                app.config.connect_timeout_secs,
            );

            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

            app.refresh();
            app.set_temp_status(match result {
                Ok(()) => "detached; refreshed tree".to_string(),
                Err(err) => format!("attach failed: {err}"),
            });
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

impl App {
    fn new(config: Config) -> Self {
        let mut app = Self {
            config,
            trees: Vec::new(),
            expanded: BTreeSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 1,
            screen_area: Rect::default(),
            tree_area: Rect::default(),
            status: "loading remote tmux trees".to_string(),
            status_expires_at: Some(Instant::now() + STATUS_TTL),
            search: String::new(),
            search_direction: SearchDirection::Down,
            last_search_direction: SearchDirection::Down,
            mode: Mode::Normal,
            pending_g: false,
            attach_request: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.trees = collect_hosts(&self.config);
        self.apply_trees_after_refresh();
        self.set_status(DEFAULT_STATUS);
        self.apply_search_from_current();
    }

    fn apply_auto_refresh(&mut self, auto_refresh: &mut AutoRefresh) {
        if let Some(trees) = auto_refresh.poll() {
            self.trees = trees;
            self.apply_trees_after_refresh();
            self.apply_search_from_current();
            self.set_temp_status("auto refreshed");
        }
    }

    fn apply_trees_after_refresh(&mut self) {
        self.expand_initial();
        self.rebuild_rows();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.clamp_scroll();
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_expires_at = None;
    }

    fn set_temp_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_expires_at = Some(Instant::now() + STATUS_TTL);
    }

    fn current_status(&mut self) -> String {
        if self
            .status_expires_at
            .is_some_and(|expires_at| Instant::now() >= expires_at)
        {
            self.set_status(DEFAULT_STATUS);
        }
        self.status.clone()
    }

    fn expand_initial(&mut self) {
        if !self.expanded.is_empty() {
            return;
        }

        if self.config.default_expand_level == ExpandLevel::Server {
            return;
        }

        for tree in &self.trees {
            let host_id = NodeId::Host(tree.host.clone());
            self.expanded.insert(host_id);

            if self.config.default_expand_level == ExpandLevel::Session {
                continue;
            }

            for session_name in group_tree(tree).keys() {
                self.expanded.insert(NodeId::Session {
                    host: tree.host.clone(),
                    session: session_name.clone(),
                });
            }

            if self.config.default_expand_level == ExpandLevel::Window {
                continue;
            }

            for (session_name, windows) in group_tree(tree) {
                for window_index in windows.keys() {
                    self.expanded.insert(NodeId::Window {
                        host: tree.host.clone(),
                        session: session_name.clone(),
                        window: window_index.clone(),
                    });
                }
            }
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows = build_rows(&self.trees, &self.expanded, &self.config.line_formats);
    }

    fn select_next(&mut self) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1).min(self.rows.len() - 1);
        self.keep_selected_visible_down();
    }

    fn select_previous(&mut self) {
        self.pending_g = false;
        self.selected = self.selected.saturating_sub(1);
        self.keep_selected_visible_up();
    }

    fn expand_selected(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if matches!(row.id, NodeId::Pane { .. }) {
            return;
        }
        self.expanded.insert(row.id.clone());
        self.rebuild_rows();
        self.clamp_scroll();
    }

    fn collapse_selected(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        if self.expanded.remove(&row.id) {
            self.rebuild_rows();
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            self.clamp_scroll();
            return;
        }

        if let Some(parent) = parent_id(&row.id) {
            if let Some(index) = self.rows.iter().position(|item| item.id == parent) {
                self.selected = index;
                self.keep_selected_visible_up();
            }
        }
    }

    fn toggle_selected(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if matches!(row.id, NodeId::Pane { .. }) {
            return;
        }

        if self.expanded.contains(&row.id) {
            self.expanded.remove(&row.id);
        } else {
            self.expanded.insert(row.id.clone());
        }
        self.rebuild_rows();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.clamp_scroll();
    }

    fn select_first(&mut self) {
        self.pending_g = false;
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn select_last(&mut self) {
        self.pending_g = false;
        self.selected = self.rows.len().saturating_sub(1);
        self.scroll_offset = self.selected;
    }

    fn page_down(&mut self, amount: usize) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }

        let amount = amount.max(1);
        self.selected = (self.selected + amount).min(self.rows.len() - 1);
        self.scroll_offset = (self.scroll_offset + amount).min(self.rows.len() - 1);
    }

    fn page_up(&mut self, amount: usize) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }

        let amount = amount.max(1);
        self.selected = self.selected.saturating_sub(amount);
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    fn mouse_scroll_down(&mut self) {
        let max_offset = self.rows.len().saturating_sub(self.viewport_height);
        let new_offset = self
            .scroll_offset
            .saturating_add(self.config.mouse_scroll_lines)
            .min(max_offset);
        self.apply_mouse_scroll_offset(new_offset);
    }

    fn mouse_scroll_up(&mut self) {
        let new_offset = self
            .scroll_offset
            .saturating_sub(self.config.mouse_scroll_lines);
        self.apply_mouse_scroll_offset(new_offset);
    }

    fn apply_mouse_scroll_offset(&mut self, new_offset: usize) {
        self.pending_g = false;
        if self.rows.is_empty() || self.viewport_height == 0 {
            return;
        }

        let old_offset = self.scroll_offset;
        let delta = new_offset as isize - old_offset as isize;
        self.scroll_offset = new_offset;
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
    }

    fn take_attach_request(&mut self) -> Option<AttachTarget> {
        self.attach_request.take()
    }

    fn clamp_scroll(&mut self) {
        if self.rows.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        self.scroll_offset = self.scroll_offset.min(self.rows.len() - 1);
    }

    fn keep_selected_visible_up(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    fn keep_selected_visible_down(&mut self) {
        self.clamp_scroll();
    }

    fn fit_scroll_to_height(&mut self, height: usize) {
        if self.rows.is_empty() || height == 0 {
            self.scroll_offset = 0;
            return;
        }

        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + height {
            self.scroll_offset = self.selected + 1 - height;
        }

        let max_offset = self.rows.len().saturating_sub(height);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    fn selected_attach_target(&self) -> Option<AttachTarget> {
        let row = self.rows.get(self.selected)?;
        match &row.id {
            NodeId::Pane {
                host,
                session,
                window,
                pane,
            } => Some(AttachTarget {
                host: host.clone(),
                session: session.clone(),
                window: window.clone(),
                pane: pane.clone(),
            }),
            NodeId::Window {
                host,
                session,
                window,
            } => {
                let pane = self
                    .trees
                    .iter()
                    .find(|tree| &tree.host == host)?
                    .panes
                    .iter()
                    .find(|pane| {
                        &pane.session_name == session
                            && &pane.window_index == window
                            && pane.active_pane
                    })
                    .or_else(|| {
                        self.trees
                            .iter()
                            .find(|tree| &tree.host == host)?
                            .panes
                            .iter()
                            .find(|pane| {
                                &pane.session_name == session && &pane.window_index == window
                            })
                    })?;
                Some(AttachTarget {
                    host: host.clone(),
                    session: session.clone(),
                    window: window.clone(),
                    pane: pane.pane_id.clone(),
                })
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        match self.mode.clone() {
            Mode::Normal => self.handle_normal_key(key),
            Mode::ContextMenu(_) => {
                self.handle_context_menu_key(key.code)?;
                Ok(false)
            }
            Mode::Search => {
                self.handle_search_key(key.code);
                Ok(false)
            }
            Mode::Prompt(_) => {
                self.handle_prompt_key(key.code)?;
                Ok(false)
            }
            Mode::SplitChoice(_) => {
                self.handle_split_choice_key(key.code)?;
                Ok(false)
            }
            Mode::Confirm(_) => {
                self.handle_confirm_key(key.code)?;
                Ok(false)
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if matches!(self.mode, Mode::ContextMenu(_)) {
            self.handle_context_menu_mouse(mouse);
            return;
        }

        if matches!(self.mode, Mode::SplitChoice(_)) {
            self.handle_split_choice_mouse(mouse);
            return;
        }

        if matches!(self.mode, Mode::Confirm(_)) {
            self.handle_confirm_mouse(mouse);
            return;
        }

        if matches!(self.mode, Mode::Prompt(_)) {
            return;
        }

        let row_index = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Left)
            | MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::Moved
            | MouseEventKind::Down(MouseButton::Right) => {
                self.row_at_mouse(mouse.column, mouse.row)
            }
            MouseEventKind::ScrollDown => {
                self.mouse_scroll_down();
                return;
            }
            MouseEventKind::ScrollUp => {
                self.mouse_scroll_up();
                return;
            }
            _ => None,
        };

        let Some(row_index) = row_index else {
            return;
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending_g = false;
                self.selected = row_index;
                self.toggle_selected();
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.pending_g = false;
                self.selected = row_index;
                self.open_context_menu(mouse.column, mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.selected = row_index;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.selected = row_index;
            }
            MouseEventKind::Moved => {}
            _ => {}
        }
    }

    fn row_at_mouse(&self, column: u16, row: u16) -> Option<usize> {
        let x_min = self.tree_area.x + 1;
        let x_max = self.tree_area.x + self.tree_area.width.saturating_sub(1);
        let y_min = self.tree_area.y + 1;
        let y_max = self.tree_area.y + self.tree_area.height.saturating_sub(1);

        if column < x_min || column >= x_max || row < y_min || row >= y_max {
            return None;
        }

        let visible_offset = (row - y_min) as usize;
        let index = self.scroll_offset + visible_offset;
        (index < self.rows.len()).then_some(index)
    }

    fn open_context_menu(&mut self, x: u16, y: u16) {
        let items = self.context_menu_items();
        if items.is_empty() {
            self.set_temp_status("no actions for this row");
            return;
        }
        let area = context_menu_area(&items, x, y, self.tree_area);

        self.mode = Mode::ContextMenu(ContextMenuState {
            items,
            selected: 0,
            area,
        });
    }

    fn context_menu_items(&self) -> Vec<ContextMenuItem> {
        let Some(row) = self.rows.get(self.selected) else {
            return Vec::new();
        };

        let mut items = Vec::new();
        if matches!(row.id, NodeId::Window { .. } | NodeId::Pane { .. }) {
            items.push(ContextMenuItem {
                label: "attach".to_string(),
                action: ContextAction::Attach,
                shortcut: Some('a'),
            });
        }
        if !matches!(row.id, NodeId::Host(_)) {
            items.push(ContextMenuItem {
                label: "kill".to_string(),
                action: ContextAction::Kill,
                shortcut: Some('x'),
            });
        }
        match row.id {
            NodeId::Host(_) => items.push(ContextMenuItem {
                label: "new session".to_string(),
                action: ContextAction::NewSession,
                shortcut: Some('s'),
            }),
            NodeId::Session { .. } => {
                items.push(ContextMenuItem {
                    label: "new session".to_string(),
                    action: ContextAction::NewSession,
                    shortcut: Some('s'),
                });
                items.push(ContextMenuItem {
                    label: "new window".to_string(),
                    action: ContextAction::NewWindow,
                    shortcut: Some('w'),
                });
            }
            NodeId::Window { .. } => {
                items.push(ContextMenuItem {
                    label: "new window".to_string(),
                    action: ContextAction::NewWindow,
                    shortcut: Some('w'),
                });
                items.push(ContextMenuItem {
                    label: "new pane".to_string(),
                    action: ContextAction::NewPane,
                    shortcut: Some('p'),
                });
            }
            NodeId::Pane { .. } => items.push(ContextMenuItem {
                label: "new pane".to_string(),
                action: ContextAction::NewPane,
                shortcut: Some('p'),
            }),
        }
        if !matches!(row.id, NodeId::Host(_)) {
            items.push(ContextMenuItem {
                label: "rename".to_string(),
                action: ContextAction::Rename,
                shortcut: Some('r'),
            });
        }
        items
    }

    fn handle_context_menu_key(&mut self, code: KeyCode) -> Result<()> {
        let Mode::ContextMenu(mut menu) = self.mode.clone() else {
            return Ok(());
        };

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                menu.selected = menu.selected.saturating_sub(1);
                self.mode = Mode::ContextMenu(menu);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                self.mode = Mode::ContextMenu(menu);
            }
            KeyCode::Char(ch) => {
                let action = menu
                    .items
                    .iter()
                    .find(|item| item.shortcut == Some(ch))
                    .map(|item| item.action);
                self.mode = Mode::Normal;
                if let Some(action) = action {
                    self.run_context_action(action)?;
                }
            }
            KeyCode::Enter => {
                let action = menu.items.get(menu.selected).map(|item| item.action);
                self.mode = Mode::Normal;
                if let Some(action) = action {
                    self.run_context_action(action)?;
                }
            }
            _ => self.mode = Mode::ContextMenu(menu),
        }

        Ok(())
    }

    fn handle_context_menu_mouse(&mut self, mouse: MouseEvent) {
        let Mode::ContextMenu(mut menu) = self.mode.clone() else {
            return;
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = menu_item_at_mouse(&menu, mouse.column, mouse.row) {
                    menu.selected = index;
                    let action = menu.items[index].action;
                    self.mode = Mode::Normal;
                    let result = self.run_context_action(action);
                    if let Err(err) = result {
                        self.set_temp_status(format!("operation failed: {err}; see {LOG_PATH}"));
                    }
                } else {
                    self.mode = Mode::Normal;
                }
            }
            MouseEventKind::Moved => {
                if let Some(index) = menu_item_at_mouse(&menu, mouse.column, mouse.row) {
                    menu.selected = index;
                    self.mode = Mode::ContextMenu(menu);
                }
            }
            MouseEventKind::ScrollDown => {
                menu.selected = (menu.selected + 1).min(menu.items.len().saturating_sub(1));
                self.mode = Mode::ContextMenu(menu);
            }
            MouseEventKind::ScrollUp => {
                menu.selected = menu.selected.saturating_sub(1);
                self.mode = Mode::ContextMenu(menu);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(row_index) = self.row_at_mouse(mouse.column, mouse.row) {
                    self.selected = row_index;
                    self.open_context_menu(mouse.column, mouse.row);
                } else {
                    self.mode = Mode::Normal;
                }
            }
            _ => {}
        }
    }

    fn run_context_action(&mut self, action: ContextAction) -> Result<()> {
        match action {
            ContextAction::Attach => self.attach_request = self.selected_attach_target(),
            ContextAction::Kill => self.start_kill(),
            ContextAction::NewSession => self.start_create_session(),
            ContextAction::NewWindow => self.start_create_window(),
            ContextAction::NewPane => self.start_create_pane(),
            ContextAction::Rename => self.start_rename(),
        }
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let code = key.code;
        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page_down(self.viewport_height / 2)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page_up(self.viewport_height / 2)
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page_down(self.viewport_height)
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page_up(self.viewport_height)
            }
            KeyCode::PageDown => self.page_down(self.viewport_height),
            KeyCode::PageUp => self.page_up(self.viewport_height),
            KeyCode::Char('/') => self.start_search(SearchDirection::Down),
            KeyCode::Char('?') => self.start_search(SearchDirection::Up),
            KeyCode::Char('n') => self.repeat_search(self.last_search_direction),
            KeyCode::Char('N') => self.repeat_search(self.last_search_direction.reversed()),
            KeyCode::Char('r') => {
                self.refresh();
                self.set_temp_status("refreshed");
            }
            KeyCode::Char('G') => self.select_last(),
            KeyCode::Char('g') if self.pending_g => self.select_first(),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('a') => self.start_create(),
            KeyCode::Char('x') => self.start_kill(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(),
            KeyCode::Right | KeyCode::Char('l') => self.expand_selected(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_selected(),
            KeyCode::Enter => {
                self.pending_g = false;
                self.attach_request = self.selected_attach_target();
            }
            _ => self.pending_g = false,
        }
        Ok(false)
    }

    fn start_search(&mut self, direction: SearchDirection) {
        self.pending_g = false;
        self.search_direction = direction;
        self.last_search_direction = direction;
        self.mode = Mode::Search;
        self.set_status(self.search_prompt());
    }

    fn finish_search(&mut self) {
        self.mode = Mode::Normal;
        self.set_temp_status(if self.search.is_empty() {
            "search closed".to_string()
        } else {
            format!("{}{}", self.search_direction.prefix(), self.search)
        });
    }

    fn clear_search(&mut self) {
        self.mode = Mode::Normal;
        self.set_temp_status("search cancelled");
    }

    fn handle_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.clear_search(),
            KeyCode::Enter => self.finish_search(),
            KeyCode::Backspace => self.pop_search(),
            KeyCode::Char('\u{7f}') => self.pop_search(),
            KeyCode::Char(ch) => self.push_search(ch),
            _ => {}
        }
    }

    fn start_create(&mut self) {
        self.pending_g = false;
        let items = self.create_menu_items();
        if items.is_empty() {
            self.set_temp_status("no create actions for this row");
            return;
        }

        if items.len() == 1 {
            let action = items[0].action;
            let _ = self.run_context_action(action);
            return;
        }

        let area = context_menu_area(
            &items,
            self.tree_area.x + 2,
            self.tree_area.y + 2,
            self.tree_area,
        );
        self.mode = Mode::ContextMenu(ContextMenuState {
            items,
            selected: 0,
            area,
        });
    }

    fn create_menu_items(&self) -> Vec<ContextMenuItem> {
        self.context_menu_items()
            .into_iter()
            .filter(|item| {
                matches!(
                    item.action,
                    ContextAction::NewSession | ContextAction::NewWindow | ContextAction::NewPane
                )
            })
            .collect()
    }

    fn start_create_session(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        let host = match &row.id {
            NodeId::Host(host)
            | NodeId::Session { host, .. }
            | NodeId::Window { host, .. }
            | NodeId::Pane { host, .. } => host,
        };
        self.mode = Mode::Prompt(PromptState {
            title: format!("new session on {host}"),
            value: String::new(),
            kind: PromptKind::CreateSession { host: host.clone() },
        });
    }

    fn start_create_window(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        match &row.id {
            NodeId::Session { host, session } => {
                let target = session_target_for(&self.trees, host, session)
                    .unwrap_or_else(|| session.clone());
                self.mode = Mode::Prompt(PromptState {
                    title: format!("new window in {host}:{session}"),
                    value: String::new(),
                    kind: PromptKind::CreateWindow {
                        host: host.clone(),
                        target,
                        after_window: None,
                    },
                });
            }
            NodeId::Window {
                host,
                session,
                window,
            } => {
                let target = session_target_for(&self.trees, host, session)
                    .unwrap_or_else(|| session.clone());
                let after_window = window_target_for(&self.trees, host, session, window)
                    .unwrap_or_else(|| format!("{session}:{window}"));
                self.mode = Mode::Prompt(PromptState {
                    title: format!("new window after {host}:{session}:{window}"),
                    value: String::new(),
                    kind: PromptKind::CreateWindow {
                        host: host.clone(),
                        target,
                        after_window: Some(after_window),
                    },
                });
            }
            _ => self.set_temp_status("select a session or window to create a window"),
        }
    }

    fn start_create_pane(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        match &row.id {
            NodeId::Window {
                host,
                session,
                window,
            } => {
                let Some(pane) = pane_for_window(&self.trees, host, session, window) else {
                    self.set_temp_status("window has no pane target");
                    return;
                };
                self.mode = Mode::SplitChoice(SplitChoiceState {
                    host: host.clone(),
                    pane,
                    selected: SplitChoice::Vertical,
                });
            }
            NodeId::Pane { host, pane, .. } => {
                self.mode = Mode::SplitChoice(SplitChoiceState {
                    host: host.clone(),
                    pane: pane.clone(),
                    selected: SplitChoice::Vertical,
                });
            }
            _ => self.set_temp_status("select a window or pane to create a pane"),
        }
    }

    fn start_rename(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        self.mode = match &row.id {
            NodeId::Host(_) => {
                self.set_temp_status("select a session, window, or pane to rename");
                Mode::Normal
            }
            NodeId::Session { host, session } => Mode::Prompt(PromptState {
                title: format!("rename session {host}:{session}"),
                value: session.clone(),
                kind: PromptKind::RenameSession {
                    host: host.clone(),
                    target: session_target_for(&self.trees, host, session)
                        .unwrap_or_else(|| session.clone()),
                },
            }),
            NodeId::Window {
                host,
                session,
                window,
            } => Mode::Prompt(PromptState {
                title: format!("rename window {host}:{session}:{window}"),
                value: window_name_for(&self.trees, host, session, window).unwrap_or_default(),
                kind: PromptKind::RenameWindow {
                    host: host.clone(),
                    target: window_target_for(&self.trees, host, session, window)
                        .unwrap_or_else(|| format!("{session}:{window}")),
                },
            }),
            NodeId::Pane { host, pane, .. } => Mode::Prompt(PromptState {
                title: format!("rename pane {host}:{pane}"),
                value: pane_title_for(&self.trees, host, pane).unwrap_or_default(),
                kind: PromptKind::RenamePane {
                    host: host.clone(),
                    pane: pane.clone(),
                },
            }),
        };
    }

    fn handle_prompt_key(&mut self, code: KeyCode) -> Result<()> {
        let Mode::Prompt(mut prompt) = self.mode.clone() else {
            return Ok(());
        };

        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.set_temp_status("prompt cancelled");
            }
            KeyCode::Enter => {
                let result = match &prompt.kind {
                    PromptKind::CreateSession { host } => create_remote_session(
                        host,
                        optional_name(&prompt.value),
                        self.config.connect_timeout_secs,
                    ),
                    PromptKind::CreateWindow {
                        host,
                        target,
                        after_window,
                    } => create_remote_window(
                        host,
                        target,
                        after_window.as_deref(),
                        optional_name(&prompt.value),
                        self.config.connect_timeout_secs,
                    ),
                    PromptKind::RenameSession { host, target } => rename_remote_session(
                        host,
                        target,
                        prompt.value.trim(),
                        self.config.connect_timeout_secs,
                    ),
                    PromptKind::RenameWindow { host, target } => rename_remote_window(
                        host,
                        target,
                        prompt.value.trim(),
                        self.config.connect_timeout_secs,
                    ),
                    PromptKind::RenamePane { host, pane } => rename_remote_pane(
                        host,
                        pane,
                        prompt.value.trim(),
                        self.config.connect_timeout_secs,
                    ),
                };
                self.mode = Mode::Normal;
                self.refresh();
                self.set_temp_status(result_status(result, prompt_success(&prompt.kind)));
            }
            KeyCode::Backspace => {
                prompt.value.pop();
                self.mode = Mode::Prompt(prompt);
            }
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    prompt.value.push(ch);
                }
                self.mode = Mode::Prompt(prompt);
            }
            _ => self.mode = Mode::Prompt(prompt),
        }

        Ok(())
    }

    fn handle_split_choice_key(&mut self, code: KeyCode) -> Result<()> {
        let Mode::SplitChoice(mut choice) = self.mode.clone() else {
            return Ok(());
        };

        match code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = Mode::Normal;
                self.set_temp_status("split cancelled");
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                choice.selected = match choice.selected {
                    SplitChoice::Vertical => SplitChoice::Horizontal,
                    SplitChoice::Horizontal => SplitChoice::Vertical,
                };
                self.mode = Mode::SplitChoice(choice);
            }
            KeyCode::Char('v') => {
                choice.selected = SplitChoice::Vertical;
                self.mode = Mode::SplitChoice(choice);
            }
            KeyCode::Char('h') => {
                choice.selected = SplitChoice::Horizontal;
                self.mode = Mode::SplitChoice(choice);
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let result = split_remote_pane(
                    &choice.host,
                    &choice.pane,
                    choice.selected,
                    self.config.connect_timeout_secs,
                );
                self.mode = Mode::Normal;
                self.refresh();
                self.set_temp_status(result_status(result, "pane split"));
            }
            _ => self.mode = Mode::SplitChoice(choice),
        }

        Ok(())
    }

    fn handle_split_choice_mouse(&mut self, mouse: MouseEvent) {
        let Mode::SplitChoice(mut choice) = self.mode.clone() else {
            return;
        };

        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(split) =
                    split_choice_at_mouse(mouse.column, mouse.row, self.screen_area)
                {
                    choice.selected = split;
                    self.mode = Mode::SplitChoice(choice);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(split) =
                    split_choice_at_mouse(mouse.column, mouse.row, self.screen_area)
                {
                    choice.selected = split;
                    let result = split_remote_pane(
                        &choice.host,
                        &choice.pane,
                        choice.selected,
                        self.config.connect_timeout_secs,
                    );
                    self.mode = Mode::Normal;
                    self.refresh();
                    self.set_temp_status(result_status(result, "pane split"));
                } else {
                    self.mode = Mode::Normal;
                    self.set_temp_status("split cancelled");
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                choice.selected = match choice.selected {
                    SplitChoice::Vertical => SplitChoice::Horizontal,
                    SplitChoice::Horizontal => SplitChoice::Vertical,
                };
                self.mode = Mode::SplitChoice(choice);
            }
            _ => {}
        }
    }

    fn start_kill(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };

        self.mode = match &row.id {
            NodeId::Host(_) => {
                self.set_temp_status("select a session, window, or pane to kill");
                Mode::Normal
            }
            NodeId::Session { host, session } => Mode::Confirm(ConfirmState {
                title: "kill session?".to_string(),
                detail: format!("{host}:{session}"),
                action: ConfirmAction::KillSession {
                    host: host.clone(),
                    target: session_target_for(&self.trees, host, session)
                        .unwrap_or_else(|| session.clone()),
                },
                selected_yes: false,
            }),
            NodeId::Window {
                host,
                session,
                window,
            } => Mode::Confirm(ConfirmState {
                title: "kill window?".to_string(),
                detail: format!("{host}:{session}:{window}"),
                action: ConfirmAction::KillWindow {
                    host: host.clone(),
                    target: window_target_for(&self.trees, host, session, window)
                        .unwrap_or_else(|| format!("{session}:{window}")),
                },
                selected_yes: false,
            }),
            NodeId::Pane { host, pane, .. } => Mode::Confirm(ConfirmState {
                title: "kill pane?".to_string(),
                detail: format!("{host}:{pane}"),
                action: ConfirmAction::KillPane {
                    host: host.clone(),
                    pane: pane.clone(),
                },
                selected_yes: false,
            }),
        };
    }

    fn handle_confirm_key(&mut self, code: KeyCode) -> Result<()> {
        let Mode::Confirm(mut confirm) = self.mode.clone() else {
            return Ok(());
        };

        match code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = Mode::Normal;
                self.set_temp_status("kill cancelled");
            }
            KeyCode::Char('y') => {
                let result = run_confirm_action(&confirm.action, self.config.connect_timeout_secs);
                self.mode = Mode::Normal;
                self.refresh();
                self.set_temp_status(result_status(result, "killed"));
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                confirm.selected_yes = !confirm.selected_yes;
                self.mode = Mode::Confirm(confirm);
            }
            KeyCode::Enter => {
                if confirm.selected_yes {
                    let result =
                        run_confirm_action(&confirm.action, self.config.connect_timeout_secs);
                    self.mode = Mode::Normal;
                    self.refresh();
                    self.set_temp_status(result_status(result, "killed"));
                } else {
                    self.mode = Mode::Normal;
                    self.set_temp_status("kill cancelled");
                }
            }
            _ => self.mode = Mode::Confirm(confirm),
        }

        Ok(())
    }

    fn handle_confirm_mouse(&mut self, mouse: MouseEvent) {
        let Mode::Confirm(mut confirm) = self.mode.clone() else {
            return;
        };

        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(selected_yes) =
                    confirm_choice_at_mouse(mouse.column, mouse.row, self.screen_area)
                {
                    confirm.selected_yes = selected_yes;
                    self.mode = Mode::Confirm(confirm);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(selected_yes) =
                    confirm_choice_at_mouse(mouse.column, mouse.row, self.screen_area)
                {
                    if selected_yes {
                        let result =
                            run_confirm_action(&confirm.action, self.config.connect_timeout_secs);
                        self.mode = Mode::Normal;
                        self.refresh();
                        self.set_temp_status(result_status(result, "killed"));
                    } else {
                        self.mode = Mode::Normal;
                        self.set_temp_status("kill cancelled");
                    }
                } else {
                    self.mode = Mode::Normal;
                    self.set_temp_status("kill cancelled");
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                confirm.selected_yes = !confirm.selected_yes;
                self.mode = Mode::Confirm(confirm);
            }
            _ => {}
        }
    }

    fn push_search(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        self.search.push(ch);
        self.apply_search_from_current();
        self.set_status(self.search_prompt());
    }

    fn pop_search(&mut self) {
        self.search.pop();
        self.apply_search_from_current();
        self.set_status(self.search_prompt());
    }

    fn apply_search_from_current(&mut self) {
        if self.search.is_empty() {
            return;
        }

        let needle = self.search.to_lowercase();
        let found = search_rows(
            &self.rows,
            self.selected,
            &needle,
            self.search_direction,
            SearchStart::Current,
        );

        if let Some(index) = found {
            self.selected = index;
            self.keep_selected_visible_up();
        }
    }

    fn repeat_search(&mut self, direction: SearchDirection) {
        self.pending_g = false;
        if self.search.is_empty() {
            self.set_temp_status("no previous search");
            return;
        }

        let needle = self.search.to_lowercase();
        if let Some(index) = search_rows(
            &self.rows,
            self.selected,
            &needle,
            direction,
            SearchStart::Next,
        ) {
            self.selected = index;
            self.keep_selected_visible_up();
            self.set_temp_status(format!("{}{}", direction.prefix(), self.search));
        } else {
            self.set_temp_status(format!("pattern not found: {}", self.search));
        }
    }

    fn search_prompt(&self) -> String {
        format!("{}{}", self.search_direction.prefix(), self.search)
    }
}

struct AttachTarget {
    host: String,
    session: String,
    window: String,
    pane: String,
}

fn draw_app(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.screen_area = area;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "tmux-gateway",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "server / session / window / pane",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, chunks[0]);

    app.tree_area = chunks[1];
    let tree_inner_height = chunks[1].height.saturating_sub(2) as usize;
    app.viewport_height = tree_inner_height.max(1);
    app.fit_scroll_to_height(tree_inner_height);
    let end = (app.scroll_offset + tree_inner_height).min(app.rows.len());

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .skip(app.scroll_offset)
        .take(end.saturating_sub(app.scroll_offset))
        .map(|(index, row)| {
            let selected = index == app.selected;
            let indent = "  ".repeat(row.depth);
            let marker = match row.id {
                NodeId::Pane { .. } => " ",
                _ if app.expanded.contains(&row.id) => "▾",
                _ => "▸",
            };
            let main_style = if row.selectable {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            let main_style = if row.busy_duration_secs.is_some() {
                main_style
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else {
                main_style
            };
            let detail_style = if row.busy_duration_secs.is_some() {
                Style::default().fg(Color::LightGreen)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let row_style = if selected && row.busy_duration_secs.is_some() {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let cursor = if selected { "➜ " } else { "  " };
            let detail = row.detail.clone();
            ListItem::new(Line::from(vec![
                Span::styled(cursor, row_style),
                Span::raw(indent),
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(row.label.clone(), main_style),
                Span::raw(" "),
                Span::styled(detail, detail_style),
            ]))
            .style(row_style)
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("tree"));
    frame.render_widget(list, chunks[1]);

    let status_text = if matches!(app.mode, Mode::Search) {
        app.search_prompt()
    } else if app.search.is_empty() {
        app.current_status()
    } else {
        format!(
            "{} | {}{}",
            app.current_status(),
            app.last_search_direction.prefix(),
            app.search
        )
    };
    let status = Paragraph::new(status_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(status, chunks[2]);

    draw_modal(frame, app);
}

fn draw_modal(frame: &mut ratatui::Frame<'_>, app: &App) {
    match &app.mode {
        Mode::ContextMenu(menu) => {
            frame.render_widget(Clear, menu.area);

            let lines: Vec<Line> = menu
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    padded_choice_line(
                        &item.display_label(),
                        menu.area.width,
                        index == menu.selected,
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("menu")),
                menu.area,
            );
        }
        Mode::Prompt(prompt) => {
            let area = centered_rect(64, 7, frame.area());
            frame.render_widget(Clear, area);
            let text = vec![
                Line::from(prompt.title.clone()),
                Line::from(""),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Yellow)),
                    Span::raw(prompt.value.clone()),
                ]),
                Line::from(Span::styled(
                    prompt_help(&prompt.kind),
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("prompt")),
                area,
            );
        }
        Mode::SplitChoice(choice) => {
            let area = split_choice_area(frame.area());
            frame.render_widget(Clear, area);
            let text = vec![
                Line::from("split current pane"),
                Line::from(""),
                padded_choice_line(
                    "vertical split(v)",
                    area.width,
                    choice.selected == SplitChoice::Vertical,
                ),
                padded_choice_line(
                    "horizontal split(h)",
                    area.width,
                    choice.selected == SplitChoice::Horizontal,
                ),
                Line::from(""),
                Line::from(Span::styled(
                    "j/k: choose | v/h: quick choose | Enter: confirm | Esc: cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("split")),
                area,
            );
        }
        Mode::Confirm(confirm) => {
            let area = confirm_area(frame.area());
            frame.render_widget(Clear, area);
            let text = vec![
                Line::from(confirm.title.clone()),
                Line::from(Span::styled(
                    confirm.detail.clone(),
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
                padded_choice_line("OK(y)", area.width, confirm.selected_yes),
                padded_choice_line("Cancel(n)", area.width, !confirm.selected_yes),
                Line::from(Span::styled(
                    "j/k: choose | y/n: quick answer | Enter: confirm",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("confirm")),
                area,
            );
        }
        _ => {}
    }
}

fn choice_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    }
}

fn padded_choice_line(label: &str, area_width: u16, selected: bool) -> Line<'static> {
    let inner_width = area_width.saturating_sub(2) as usize;
    let text = format!(" {:<width$}", label, width = inner_width.saturating_sub(1));
    Line::from(Span::styled(text, choice_style(selected)))
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn split_choice_area(area: Rect) -> Rect {
    centered_rect(50, 8, area)
}

fn confirm_area(area: Rect) -> Rect {
    centered_rect(52, 8, area)
}

fn split_choice_at_mouse(column: u16, row: u16, screen_area: Rect) -> Option<SplitChoice> {
    let area = split_choice_area(screen_area);
    if column < area.x + 1 || column >= area.x + area.width.saturating_sub(1) {
        return None;
    }

    match row {
        y if y == area.y + 3 => Some(SplitChoice::Vertical),
        y if y == area.y + 4 => Some(SplitChoice::Horizontal),
        _ => None,
    }
}

fn confirm_choice_at_mouse(column: u16, row: u16, screen_area: Rect) -> Option<bool> {
    let area = confirm_area(screen_area);
    if column < area.x + 1 || column >= area.x + area.width.saturating_sub(1) {
        return None;
    }

    match row {
        y if y == area.y + 4 => Some(true),
        y if y == area.y + 5 => Some(false),
        _ => None,
    }
}

fn context_menu_area(items: &[ContextMenuItem], x: u16, y: u16, bounds: Rect) -> Rect {
    let width = items
        .iter()
        .map(|item| item.display_label().len() as u16)
        .max()
        .unwrap_or(8)
        + 4;
    let height = items.len() as u16 + 2;
    let max_x = bounds.x + bounds.width.saturating_sub(width);
    let max_y = bounds.y + bounds.height.saturating_sub(height);
    Rect::new(x.min(max_x), y.min(max_y), width, height)
}

fn menu_item_at_mouse(menu: &ContextMenuState, column: u16, row: u16) -> Option<usize> {
    let x_min = menu.area.x + 1;
    let x_max = menu.area.x + menu.area.width.saturating_sub(1);
    let y_min = menu.area.y + 1;
    let y_max = menu.area.y + menu.area.height.saturating_sub(1);

    if column < x_min || column >= x_max || row < y_min || row >= y_max {
        return None;
    }

    let index = (row - y_min) as usize;
    (index < menu.items.len()).then_some(index)
}

fn collect_hosts(config: &Config) -> Vec<HostTree> {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.scan_concurrency)
        .build();

    match pool {
        Ok(pool) => pool.install(|| {
            config
                .hosts
                .par_iter()
                .map(|host| collect_host(host, config.connect_timeout_secs))
                .collect()
        }),
        Err(_) => config
            .hosts
            .iter()
            .map(|host| collect_host(host, config.connect_timeout_secs))
            .collect(),
    }
}

fn collect_host(host: &str, connect_timeout_secs: u64) -> HostTree {
    match list_remote_panes(host, connect_timeout_secs) {
        Ok(panes) => HostTree {
            host: host.to_string(),
            panes,
            error: None,
        },
        Err(err) => HostTree {
            host: host.to_string(),
            panes: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

fn list_remote_panes(host: &str, connect_timeout_secs: u64) -> Result<Vec<PaneInfo>> {
    let format = [
        "#{session_name}",
        "#{session_id}",
        "#{window_index}",
        "#{window_id}",
        "#{window_name}",
        "#{pane_index}",
        "#{pane_id}",
        "#{pane_pid}",
        "#{pane_current_command}",
        "#{pane_title}",
        "#{window_active}",
        "#{pane_active}",
    ]
    .join("\t");

    let remote_command = format!(
        "printf '%s\\n' __TMUX_GATEWAY_PANES__; tmux list-panes -a -F {}; printf '%s\\n' __TMUX_GATEWAY_PROCESSES__; ps -eo pid=,ppid=,etimes=,comm=,args= 2>/dev/null || true",
        shell_quote(&format),
    );
    let output = Command::new("ssh")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .output()
        .with_context(|| format!("failed to start ssh for host {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("no server running") || stderr.contains("No such file or directory") {
            return Ok(Vec::new());
        }
        bail!(
            "ssh/tmux command failed for host {host}: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("tmux output from host {host} was not utf-8"))?;
    parse_remote_snapshot(&stdout)
        .with_context(|| format!("failed to parse tmux panes from host {host}"))
}

fn parse_remote_snapshot(output: &str) -> Result<Vec<PaneInfo>> {
    let mut pane_lines = Vec::new();
    let mut process_lines = Vec::new();
    let mut section = "";

    for line in output.lines() {
        match line {
            "__TMUX_GATEWAY_PANES__" => {
                section = "panes";
                continue;
            }
            "__TMUX_GATEWAY_PROCESSES__" => {
                section = "processes";
                continue;
            }
            _ => {}
        }

        match section {
            "panes" => pane_lines.push(line),
            "processes" => process_lines.push(line),
            _ => {}
        }
    }

    let mut panes = parse_panes(&pane_lines.join("\n"))?;
    let processes = parse_processes(&process_lines.join("\n"));
    mark_busy_panes(&mut panes, &processes);
    Ok(panes)
}

fn parse_panes(output: &str) -> Result<Vec<PaneInfo>> {
    let mut panes = Vec::new();

    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 12 {
            bail!(
                "expected 12 tab-separated fields, got {} in line {line:?}",
                fields.len()
            );
        }

        panes.push(PaneInfo {
            session_name: fields[0].to_string(),
            session_id: fields[1].to_string(),
            window_index: fields[2].to_string(),
            window_id: fields[3].to_string(),
            window_name: fields[4].to_string(),
            pane_index: fields[5].to_string(),
            pane_id: fields[6].to_string(),
            pane_pid: fields[7].parse().unwrap_or(0),
            pane_current_command: fields[8].to_string(),
            pane_commandline: fields[8].to_string(),
            pane_title: fields[9].to_string(),
            active_window: fields[10] == "1",
            active_pane: fields[11] == "1",
            busy_duration_secs: None,
        });
    }

    Ok(panes)
}

fn parse_processes(output: &str) -> Vec<ProcessInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let elapsed_secs = parts.next()?.parse().ok()?;
            let command = parts.next()?.to_string();
            let commandline = parts.collect::<Vec<_>>().join(" ");
            Some(ProcessInfo {
                pid,
                ppid,
                elapsed_secs,
                command,
                commandline,
            })
        })
        .collect()
}

fn mark_busy_panes(panes: &mut [PaneInfo], processes: &[ProcessInfo]) {
    let commandline_by_pid: BTreeMap<u32, String> = processes
        .iter()
        .map(|process| (process.pid, process.commandline.clone()))
        .collect();
    let mut children_by_parent: BTreeMap<u32, Vec<&ProcessInfo>> = BTreeMap::new();
    for process in processes {
        children_by_parent
            .entry(process.ppid)
            .or_default()
            .push(process);
    }

    for pane in panes {
        if let Some(commandline) = commandline_by_pid.get(&pane.pane_pid) {
            pane.pane_commandline = commandline.clone();
        }
        if let Some(commandline) = pane_active_commandline(pane, &children_by_parent) {
            pane.pane_commandline = commandline;
        }
        pane.busy_duration_secs = pane_busy_duration(pane, &children_by_parent);
    }
}

fn pane_active_commandline(
    pane: &PaneInfo,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) -> Option<String> {
    if pane.pane_pid == 0 || !is_shell_command(&pane.pane_current_command) {
        return None;
    }

    let mut best: Option<(u64, String)> = None;
    let mut stack = children_by_parent
        .get(&pane.pane_pid)
        .cloned()
        .unwrap_or_default();
    while let Some(process) = stack.pop() {
        if !is_shell_command(&process.command) {
            match &best {
                Some((elapsed, _)) if *elapsed >= process.elapsed_secs => {}
                _ => best = Some((process.elapsed_secs, process.commandline.clone())),
            }
        }
        if let Some(children) = children_by_parent.get(&process.pid) {
            stack.extend(children.iter().copied());
        }
    }

    best.map(|(_, commandline)| commandline)
}

fn pane_busy_duration(
    pane: &PaneInfo,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) -> Option<u64> {
    if pane.pane_pid == 0 {
        return (!is_shell_command(&pane.pane_current_command)).then_some(0);
    }

    if !is_shell_command(&pane.pane_current_command) {
        return Some(0);
    }

    let mut max_elapsed = None;
    let mut stack = children_by_parent
        .get(&pane.pane_pid)
        .cloned()
        .unwrap_or_default();
    while let Some(process) = stack.pop() {
        if !is_shell_command(&process.command) {
            max_elapsed = Some(max_elapsed.unwrap_or(0).max(process.elapsed_secs));
        }
        if let Some(children) = children_by_parent.get(&process.pid) {
            stack.extend(children.iter().copied());
        }
    }

    max_elapsed
}

fn is_shell_command(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    matches!(
        command,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "pwsh"
            | "powershell"
    )
}

fn build_rows(
    trees: &[HostTree],
    expanded: &BTreeSet<NodeId>,
    line_formats: &LineFormats,
) -> Vec<VisibleRow> {
    let mut rows = Vec::new();

    for tree in trees {
        let host_id = NodeId::Host(tree.host.clone());
        rows.push(VisibleRow {
            id: host_id.clone(),
            depth: 0,
            label: format_server_line(tree, line_formats),
            detail: host_detail(tree),
            search_text: format!("{} {}", tree.host, host_detail(tree)),
            selectable: false,
            busy_duration_secs: (!expanded.contains(&host_id))
                .then(|| max_busy_duration(tree.panes.iter()))
                .flatten(),
        });

        if !expanded.contains(&host_id) {
            continue;
        }

        if tree.panes.is_empty() {
            continue;
        }

        let sessions = group_tree(tree);
        for (session_name, windows) in sessions {
            let session_id = NodeId::Session {
                host: tree.host.clone(),
                session: session_name.clone(),
            };
            rows.push(VisibleRow {
                id: session_id.clone(),
                depth: 1,
                label: format_session_line(tree, &session_name, windows.len(), line_formats),
                detail: format!("{} windows", windows.len()),
                search_text: session_name.clone(),
                selectable: false,
                busy_duration_secs: (!expanded.contains(&session_id))
                    .then(|| max_busy_duration(windows.values().flatten().copied()))
                    .flatten(),
            });

            if !expanded.contains(&session_id) {
                continue;
            }

            for (window_index, panes) in windows {
                let first = panes[0];
                let window_id = NodeId::Window {
                    host: tree.host.clone(),
                    session: session_name.clone(),
                    window: window_index.clone(),
                };
                rows.push(VisibleRow {
                    id: window_id.clone(),
                    depth: 2,
                    label: format_window_line(
                        &tree.host,
                        &session_name,
                        &window_index,
                        &panes,
                        line_formats,
                    ),
                    detail: format!("{} panes", panes.len()),
                    search_text: format!("{} {}", window_index, first.window_name),
                    selectable: true,
                    busy_duration_secs: (!expanded.contains(&window_id))
                        .then(|| max_busy_duration(panes.iter().copied()))
                        .flatten(),
                });

                if !expanded.contains(&window_id) {
                    continue;
                }

                for pane in panes {
                    let pane_id = NodeId::Pane {
                        host: tree.host.clone(),
                        session: session_name.clone(),
                        window: window_index.clone(),
                        pane: pane.pane_id.clone(),
                    };
                    rows.push(VisibleRow {
                        id: pane_id,
                        depth: 3,
                        label: format_pane_line(
                            &tree.host,
                            &session_name,
                            &window_index,
                            pane,
                            line_formats,
                        ),
                        detail: String::new(),
                        search_text: format!(
                            "{} {} {} {}",
                            pane.pane_index,
                            pane.pane_id,
                            pane.pane_current_command,
                            pane.pane_title
                        ),
                        selectable: true,
                        busy_duration_secs: pane.busy_duration_secs,
                    });
                }
            }
        }
    }

    rows
}

fn format_server_line(tree: &HostTree, line_formats: &LineFormats) -> String {
    let sessions = group_tree(tree);
    let window_count: usize = sessions.values().map(BTreeMap::len).sum();
    let pane_count = tree.panes.len();
    let busy_duration = max_busy_duration(tree.panes.iter());
    let mut values = BTreeMap::new();
    values.insert("server_name", tree.host.clone());
    values.insert("host", tree.host.clone());
    values.insert("session_count", sessions.len().to_string());
    values.insert("window_count", window_count.to_string());
    values.insert("pane_count", pane_count.to_string());
    insert_process_values(&mut values, busy_duration);
    format_line(&line_formats.server, &values)
}

fn format_session_line(
    tree: &HostTree,
    session_name: &str,
    window_count: usize,
    line_formats: &LineFormats,
) -> String {
    let panes: Vec<&PaneInfo> = tree
        .panes
        .iter()
        .filter(|pane| pane.session_name == session_name)
        .collect();
    let busy_duration = max_busy_duration(panes.iter().copied());
    let mut values = BTreeMap::new();
    values.insert("server_name", tree.host.clone());
    values.insert("host", tree.host.clone());
    values.insert("session_name", session_name.to_string());
    values.insert("window_count", window_count.to_string());
    values.insert("pane_count", panes.len().to_string());
    insert_process_values(&mut values, busy_duration);
    format_line(&line_formats.session, &values)
}

fn format_window_line(
    host: &str,
    session_name: &str,
    window_index: &str,
    panes: &[&PaneInfo],
    line_formats: &LineFormats,
) -> String {
    let first = panes[0];
    let busy_duration = max_busy_duration(panes.iter().copied());
    let mut values = BTreeMap::new();
    values.insert("server_name", host.to_string());
    values.insert("host", host.to_string());
    values.insert("session_name", session_name.to_string());
    values.insert("window_index", window_index.to_string());
    values.insert("window_name", first.window_name.clone());
    values.insert("window_panes", panes.len().to_string());
    values.insert(
        "is_active",
        if first.active_window { "*" } else { " " }.to_string(),
    );
    insert_process_values(&mut values, busy_duration);
    format_line(&line_formats.window, &values)
}

fn format_pane_line(
    host: &str,
    session_name: &str,
    window_index: &str,
    pane: &PaneInfo,
    line_formats: &LineFormats,
) -> String {
    let mut values = BTreeMap::new();
    values.insert("server_name", host.to_string());
    values.insert("host", host.to_string());
    values.insert("session_name", session_name.to_string());
    values.insert("window_index", window_index.to_string());
    values.insert("window_name", pane.window_name.clone());
    values.insert("pane_index", pane.pane_index.clone());
    values.insert("pane_id", pane.pane_id.clone());
    values.insert("pane_pid", pane.pane_pid.to_string());
    values.insert("pane_current_command", pane.pane_current_command.clone());
    values.insert("pane_command", pane.pane_current_command.clone());
    values.insert("pane_commandline", pane.pane_commandline.clone());
    values.insert("pane_title", pane.pane_title.clone());
    values.insert(
        "pane_title_prefix",
        if pane.pane_title.is_empty() {
            String::new()
        } else {
            format!(" - {}", pane.pane_title)
        },
    );
    values.insert(
        "is_active",
        if pane.active_pane { "*" } else { " " }.to_string(),
    );
    insert_process_values(&mut values, pane.busy_duration_secs);
    format_line(&line_formats.pane, &values)
}

fn insert_process_values(values: &mut BTreeMap<&'static str, String>, duration: Option<u64>) {
    let (status, elapsed) = match duration {
        Some(seconds) => ("running".to_string(), human_duration(seconds)),
        None => (String::new(), String::new()),
    };
    values.insert("process_status", status);
    values.insert("process_elapsed_time", elapsed);
}

fn format_line(template: &str, values: &BTreeMap<&'static str, String>) -> String {
    let mut output = String::new();
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '{' {
            output.push(ch);
            continue;
        }

        let mut key = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '}' {
                closed = true;
                break;
            }
            key.push(next);
        }

        if closed {
            if let Some(value) = values.get(key.as_str()) {
                output.push_str(value);
            } else {
                output.push('{');
                output.push_str(&key);
                output.push('}');
            }
        } else {
            output.push('{');
            output.push_str(&key);
        }
    }

    output.trim_end().to_string()
}

fn group_tree(tree: &HostTree) -> BTreeMap<String, BTreeMap<String, Vec<&PaneInfo>>> {
    let mut sessions: BTreeMap<String, BTreeMap<String, Vec<&PaneInfo>>> = BTreeMap::new();
    for pane in &tree.panes {
        sessions
            .entry(pane.session_name.clone())
            .or_default()
            .entry(pane.window_index.clone())
            .or_default()
            .push(pane);
    }
    sessions
}

fn max_busy_duration<'a>(panes: impl Iterator<Item = &'a PaneInfo>) -> Option<u64> {
    panes.filter_map(|pane| pane.busy_duration_secs).max()
}

fn human_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

fn search_rows(
    rows: &[VisibleRow],
    selected: usize,
    needle: &str,
    direction: SearchDirection,
    start: SearchStart,
) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }

    let len = rows.len();
    let start_index = match (direction, start) {
        (SearchDirection::Down, SearchStart::Current) => selected,
        (SearchDirection::Down, SearchStart::Next) => (selected + 1) % len,
        (SearchDirection::Up, SearchStart::Current) => selected,
        (SearchDirection::Up, SearchStart::Next) => (selected + len - 1) % len,
    };

    match direction {
        SearchDirection::Down => (0..len)
            .map(|offset| (start_index + offset) % len)
            .find(|&index| rows[index].search_text.to_lowercase().contains(needle)),
        SearchDirection::Up => (0..len)
            .map(|offset| (start_index + len - offset) % len)
            .find(|&index| rows[index].search_text.to_lowercase().contains(needle)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchStart {
    Current,
    Next,
}

fn host_detail(tree: &HostTree) -> String {
    if let Some(error) = &tree.error {
        return format!("unavailable: {error}");
    }
    if tree.panes.is_empty() {
        return "no tmux server for ssh user".to_string();
    }

    let sessions = group_tree(tree);
    let window_count: usize = sessions.values().map(BTreeMap::len).sum();
    format!(
        "{} sessions, {} windows, {} panes",
        sessions.len(),
        window_count,
        tree.panes.len()
    )
}

fn parent_id(id: &NodeId) -> Option<NodeId> {
    match id {
        NodeId::Host(_) => None,
        NodeId::Session { host, .. } => Some(NodeId::Host(host.clone())),
        NodeId::Window { host, session, .. } => Some(NodeId::Session {
            host: host.clone(),
            session: session.clone(),
        }),
        NodeId::Pane {
            host,
            session,
            window,
            ..
        } => Some(NodeId::Window {
            host: host.clone(),
            session: session.clone(),
            window: window.clone(),
        }),
    }
}

fn print_tree(trees: &[HostTree], line_formats: &LineFormats) {
    let mut stdout = io::stdout().lock();
    for row in build_rows(trees, &expanded_all(trees), line_formats) {
        let detail = row.detail;
        if writeln!(stdout, "{}{} {}", "  ".repeat(row.depth), row.label, detail).is_err() {
            return;
        }
    }
}

fn expanded_all(trees: &[HostTree]) -> BTreeSet<NodeId> {
    let mut expanded = BTreeSet::new();
    for tree in trees {
        expanded.insert(NodeId::Host(tree.host.clone()));
        for (session_name, windows) in group_tree(tree) {
            expanded.insert(NodeId::Session {
                host: tree.host.clone(),
                session: session_name.clone(),
            });
            for window_index in windows.keys() {
                expanded.insert(NodeId::Window {
                    host: tree.host.clone(),
                    session: session_name.clone(),
                    window: window_index.clone(),
                });
            }
        }
    }
    expanded
}

fn attach_host(
    host: &str,
    session: Option<&str>,
    window: Option<&str>,
    pane: Option<&str>,
    connect_timeout_secs: u64,
) -> Result<()> {
    let remote_command = match (session, window, pane) {
        (Some(session), Some(window), Some(pane)) => format!(
            "tmux switch-client -t {}; tmux select-window -t {}; tmux select-pane -t {}; tmux attach-session -t {}",
            shell_quote(session),
            shell_quote(&format!("{session}:{window}")),
            shell_quote(pane),
            shell_quote(session),
        ),
        (Some(session), _, _) => format!("tmux attach-session -t {}", shell_quote(session)),
        (None, _, _) => "tmux attach-session".to_string(),
    };

    let mut child = Command::new("ssh")
        .arg("-t")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start ssh for host {host}"))?;

    let status = child.wait()?;
    if !status.success() {
        bail!("ssh/tmux attach failed for host {host}: {status}");
    }

    Ok(())
}

fn create_remote_session(host: &str, name: Option<&str>, connect_timeout_secs: u64) -> Result<()> {
    let remote_command = match name {
        Some(name) => format!("tmux new-session -d -s {}", shell_quote(name)),
        None => "tmux new-session -d".to_string(),
    };
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn create_remote_window(
    host: &str,
    target: &str,
    after_window: Option<&str>,
    name: Option<&str>,
    connect_timeout_secs: u64,
) -> Result<()> {
    let target = after_window.unwrap_or(target);
    let mut remote_command = format!("tmux new-window -a -t {}", shell_quote(&target));
    if let Some(name) = name {
        remote_command.push_str(" -n ");
        remote_command.push_str(&shell_quote(name));
    }
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn split_remote_pane(
    host: &str,
    pane: &str,
    split: SplitChoice,
    connect_timeout_secs: u64,
) -> Result<()> {
    let flag = match split {
        SplitChoice::Vertical => "-v",
        SplitChoice::Horizontal => "-h",
    };
    let remote_command = format!("tmux split-window {flag} -t {}", shell_quote(pane));
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn rename_remote_session(
    host: &str,
    target: &str,
    new_name: &str,
    connect_timeout_secs: u64,
) -> Result<()> {
    if new_name.is_empty() {
        bail!("session name must not be empty");
    }
    let remote_command = format!(
        "tmux rename-session -t {} {}",
        shell_quote(target),
        shell_quote(new_name)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn rename_remote_window(
    host: &str,
    target: &str,
    new_name: &str,
    connect_timeout_secs: u64,
) -> Result<()> {
    if new_name.is_empty() {
        bail!("window name must not be empty");
    }
    let remote_command = format!(
        "tmux rename-window -t {} {}",
        shell_quote(target),
        shell_quote(new_name)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn rename_remote_pane(
    host: &str,
    pane: &str,
    new_title: &str,
    connect_timeout_secs: u64,
) -> Result<()> {
    let remote_command = format!(
        "tmux select-pane -t {} -T {}",
        shell_quote(pane),
        shell_quote(new_title)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs)
}

fn run_confirm_action(action: &ConfirmAction, connect_timeout_secs: u64) -> Result<()> {
    match action {
        ConfirmAction::KillSession { host, target } => run_remote_tmux(
            host,
            &format!("tmux kill-session -t {}", shell_quote(target)),
            connect_timeout_secs,
        ),
        ConfirmAction::KillWindow { host, target } => run_remote_tmux(
            host,
            &format!("tmux kill-window -t {}", shell_quote(target)),
            connect_timeout_secs,
        ),
        ConfirmAction::KillPane { host, pane } => run_remote_tmux(
            host,
            &format!("tmux kill-pane -t {}", shell_quote(pane)),
            connect_timeout_secs,
        ),
    }
}

fn run_remote_tmux(host: &str, remote_command: &str, connect_timeout_secs: u64) -> Result<()> {
    log_remote_command_start(host, remote_command);
    let output = Command::new("ssh")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .output()
        .with_context(|| format!("failed to start ssh for host {host}"))?;
    log_remote_command_output(host, remote_command, &output);

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(
        "remote tmux command failed for host {host}: {}",
        if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr
        }
    );
}

fn optional_name(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

fn prompt_success(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::CreateSession { .. } | PromptKind::CreateWindow { .. } => "created",
        PromptKind::RenameSession { .. }
        | PromptKind::RenameWindow { .. }
        | PromptKind::RenamePane { .. } => "renamed",
    }
}

fn prompt_help(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::CreateSession { .. } | PromptKind::CreateWindow { .. } => {
            "Enter: create with this name | empty: tmux default | Esc: cancel"
        }
        PromptKind::RenameSession { .. }
        | PromptKind::RenameWindow { .. }
        | PromptKind::RenamePane { .. } => "Enter: rename | Esc: cancel",
    }
}

fn window_name_for(trees: &[HostTree], host: &str, session: &str, window: &str) -> Option<String> {
    trees
        .iter()
        .find(|tree| tree.host == host)?
        .panes
        .iter()
        .find(|pane| pane.session_name == session && pane.window_index == window)
        .map(|pane| pane.window_name.clone())
}

fn session_target_for(trees: &[HostTree], host: &str, session: &str) -> Option<String> {
    trees
        .iter()
        .find(|tree| tree.host == host)?
        .panes
        .iter()
        .find(|pane| pane.session_name == session)
        .map(|pane| pane.session_id.clone())
}

fn window_target_for(
    trees: &[HostTree],
    host: &str,
    session: &str,
    window: &str,
) -> Option<String> {
    trees
        .iter()
        .find(|tree| tree.host == host)?
        .panes
        .iter()
        .find(|pane| pane.session_name == session && pane.window_index == window)
        .map(|pane| pane.window_id.clone())
}

fn pane_for_window(trees: &[HostTree], host: &str, session: &str, window: &str) -> Option<String> {
    trees
        .iter()
        .find(|tree| tree.host == host)?
        .panes
        .iter()
        .find(|pane| {
            pane.session_name == session && pane.window_index == window && pane.active_pane
        })
        .or_else(|| {
            trees
                .iter()
                .find(|tree| tree.host == host)?
                .panes
                .iter()
                .find(|pane| pane.session_name == session && pane.window_index == window)
        })
        .map(|pane| pane.pane_id.clone())
}

fn pane_title_for(trees: &[HostTree], host: &str, pane_id: &str) -> Option<String> {
    trees
        .iter()
        .find(|tree| tree.host == host)?
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .map(|pane| pane.pane_title.clone())
}

fn ssh_options(connect_timeout_secs: u64) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
    ]
}

fn result_status(result: Result<()>, success: &str) -> String {
    match result {
        Ok(()) => success.to_string(),
        Err(err) => format!("operation failed: {err}; see {LOG_PATH}"),
    }
}

fn log_remote_command_start(host: &str, remote_command: &str) {
    append_log(&format!(
        "[{}] START host={} command={}\n",
        log_timestamp(),
        host,
        remote_command
    ));
}

fn log_remote_command_output(host: &str, remote_command: &str, output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    append_log(&format!(
        "[{}] END host={} status={} command={}\nstdout:\n{}\nstderr:\n{}\n---\n",
        log_timestamp(),
        host,
        output.status,
        remote_command,
        stdout.trim_end(),
        stderr.trim_end(),
    ));
}

fn append_log(message: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = file.write_all(message.as_bytes());
    }
}

fn log_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_single_quote() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn parse_panes_reads_tmux_format() {
        let output = "s\t$1\t0\t@2\tzsh\t1\t%3\t123\tvim\ttitle\t1\t0\n";
        let panes = parse_panes(output).unwrap();

        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].session_name, "s");
        assert_eq!(panes[0].session_id, "$1");
        assert_eq!(panes[0].window_id, "@2");
        assert_eq!(panes[0].pane_id, "%3");
        assert!(panes[0].active_window);
        assert!(!panes[0].active_pane);
    }

    #[test]
    fn search_rows_wraps_by_direction() {
        let rows = vec![
            test_row("alpha"),
            test_row("beta"),
            test_row("gamma"),
            test_row("beta again"),
        ];

        assert_eq!(
            search_rows(
                &rows,
                2,
                "beta",
                SearchDirection::Down,
                SearchStart::Current,
            ),
            Some(3)
        );
        assert_eq!(
            search_rows(&rows, 2, "beta", SearchDirection::Up, SearchStart::Current,),
            Some(1)
        );
        assert_eq!(
            search_rows(&rows, 1, "beta", SearchDirection::Down, SearchStart::Next,),
            Some(3)
        );
        assert_eq!(
            search_rows(&rows, 1, "beta", SearchDirection::Up, SearchStart::Next,),
            Some(3)
        );
    }

    #[test]
    fn parse_ssh_config_hosts_reads_concrete_hosts() {
        let content = r#"
Host t1 t3
    User alice

Host *.internal !blocked t4
    User bob

host t1
    Port 22
"#;

        assert_eq!(parse_ssh_config_hosts(content), vec!["t1", "t3", "t4"]);
    }

    #[test]
    fn child_rows_do_not_inherit_host_search_text() {
        let tree = test_host_tree("t2");
        let rows = build_rows(
            &[tree.clone()],
            &expanded_all(&[tree]),
            &test_line_formats(),
        );

        assert!(rows[0].search_text.contains("t2"));
        assert!(
            rows.iter()
                .skip(1)
                .all(|row| !row.search_text.contains("t2"))
        );
    }

    #[test]
    fn pane_busy_detects_non_shell_descendant() {
        let pane = test_pane();
        let processes = vec![
            ProcessInfo {
                pid: 200,
                ppid: 123,
                elapsed_secs: 5,
                command: "bash".to_string(),
                commandline: "bash".to_string(),
            },
            ProcessInfo {
                pid: 201,
                ppid: 200,
                elapsed_secs: 3723,
                command: "python".to_string(),
                commandline: "python train.py --epochs 10".to_string(),
            },
        ];
        let mut children_by_parent: BTreeMap<u32, Vec<&ProcessInfo>> = BTreeMap::new();
        for process in &processes {
            children_by_parent
                .entry(process.ppid)
                .or_default()
                .push(process);
        }

        assert_eq!(pane_busy_duration(&pane, &children_by_parent), Some(3723));
    }

    #[test]
    fn busy_window_is_green_only_when_panes_hidden() {
        let mut tree = test_host_tree("t2");
        tree.panes[0].busy_duration_secs = Some(42);

        let mut expanded = BTreeSet::new();
        expanded.insert(NodeId::Host("t2".to_string()));
        expanded.insert(NodeId::Session {
            host: "t2".to_string(),
            session: "main".to_string(),
        });

        let rows = build_rows(&[tree.clone()], &expanded, &test_line_formats());
        assert!(rows.iter().any(
            |row| matches!(row.id, NodeId::Window { .. }) && row.busy_duration_secs == Some(42)
        ));
        assert!(!rows.iter().any(|row| matches!(row.id, NodeId::Pane { .. })));

        expanded.insert(NodeId::Window {
            host: "t2".to_string(),
            session: "main".to_string(),
            window: "0".to_string(),
        });
        let rows = build_rows(&[tree], &expanded, &test_line_formats());
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.id, NodeId::Window { .. })
                    && row.busy_duration_secs.is_some())
        );
        assert!(
            rows.iter()
                .any(|row| matches!(row.id, NodeId::Pane { .. })
                    && row.busy_duration_secs == Some(42))
        );
    }

    #[test]
    fn page_keys_move_selection_and_viewport() {
        let mut app = test_app_with_rows(20);
        app.viewport_height = 10;

        app.page_down(5);
        assert_eq!(app.selected, 5);
        assert_eq!(app.scroll_offset, 5);

        app.page_up(3);
        assert_eq!(app.selected, 2);
        assert_eq!(app.scroll_offset, 2);
    }

    #[test]
    fn mouse_wheel_scrolls_viewport_and_keeps_cursor_screen_row() {
        let mut app = test_app_with_rows(30);
        app.config.mouse_scroll_lines = 5;
        app.viewport_height = 10;
        app.selected = 3;
        app.scroll_offset = 0;

        app.mouse_scroll_down();
        assert_eq!(app.scroll_offset, 5);
        assert_eq!(app.selected, 8);
        assert_eq!(app.selected - app.scroll_offset, 3);

        app.mouse_scroll_up();
        assert_eq!(app.scroll_offset, 0);
        assert_eq!(app.selected, 3);
        assert_eq!(app.selected - app.scroll_offset, 3);
    }

    #[test]
    fn mouse_wheel_scroll_clamps_at_bottom() {
        let mut app = test_app_with_rows(12);
        app.config.mouse_scroll_lines = 5;
        app.viewport_height = 10;
        app.selected = 3;
        app.scroll_offset = 0;

        app.mouse_scroll_down();
        assert_eq!(app.scroll_offset, 2);
        assert_eq!(app.selected, 5);
    }

    #[test]
    fn mouse_row_mapping_uses_tree_viewport() {
        let mut app = test_app_with_rows(20);
        app.tree_area = Rect::new(10, 5, 40, 8);
        app.scroll_offset = 5;

        assert_eq!(app.row_at_mouse(11, 6), Some(5));
        assert_eq!(app.row_at_mouse(11, 10), Some(9));
        assert_eq!(app.row_at_mouse(9, 6), None);
        assert_eq!(app.row_at_mouse(11, 13), None);
    }

    #[test]
    fn mouse_single_click_toggles_tree_row() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);
        app.tree_area = Rect::new(0, 0, 80, 10);

        let session_row = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Session { .. }))
            .unwrap();
        let NodeId::Session { host, session } = app.rows[session_row].id.clone() else {
            unreachable!();
        };
        let session_id = NodeId::Session { host, session };
        assert!(app.expanded.contains(&session_id));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 1 + session_row as u16,
            modifiers: KeyModifiers::empty(),
        });

        assert!(!app.expanded.contains(&session_id));
    }

    #[test]
    fn context_menu_new_items_match_node_level() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);

        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Host(_)))
            .unwrap();
        assert_eq!(new_item_labels(&app), vec!["new session"]);

        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Session { .. }))
            .unwrap();
        assert_eq!(new_item_labels(&app), vec!["new session", "new window"]);

        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Window { .. }))
            .unwrap();
        assert_eq!(new_item_labels(&app), vec!["new window", "new pane"]);

        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Pane { .. }))
            .unwrap();
        assert_eq!(new_item_labels(&app), vec!["new pane"]);
    }

    #[test]
    fn keyboard_create_on_session_opens_create_menu() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);
        app.tree_area = Rect::new(0, 0, 80, 10);
        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Session { .. }))
            .unwrap();

        app.start_create();

        let Mode::ContextMenu(menu) = &app.mode else {
            panic!("expected create menu");
        };
        let labels: Vec<String> = menu
            .items
            .iter()
            .map(ContextMenuItem::display_label)
            .collect();
        assert_eq!(labels, vec!["new session(s)", "new window(w)"]);
    }

    #[test]
    fn keyboard_create_menu_shortcut_runs_selected_action() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);
        app.tree_area = Rect::new(0, 0, 80, 10);
        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Session { .. }))
            .unwrap();

        app.start_create();
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::empty()))
            .unwrap();

        let Mode::Prompt(prompt) = &app.mode else {
            panic!("expected prompt");
        };
        assert!(matches!(prompt.kind, PromptKind::CreateWindow { .. }));
    }

    #[test]
    fn context_menu_width_includes_shortcut_suffix() {
        let items = vec![ContextMenuItem {
            label: "new session".to_string(),
            action: ContextAction::NewSession,
            shortcut: Some('s'),
        }];

        let area = context_menu_area(&items, 0, 0, Rect::new(0, 0, 80, 24));
        assert!(area.width >= "new session(s)".len() as u16 + 4);
    }

    #[test]
    fn right_click_window_menu_can_attach() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);
        app.tree_area = Rect::new(0, 0, 80, 10);

        let window_row = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Window { .. }))
            .unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 2,
            row: 1 + window_row as u16,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(mouse);

        let Mode::ContextMenu(menu) = &app.mode else {
            panic!("expected context menu");
        };
        assert_eq!(menu.items[0].label, "attach");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .unwrap();

        let target = app.attach_request.unwrap();
        assert_eq!(target.host, "t2");
        assert_eq!(target.session, "main");
        assert_eq!(target.window, "0");
    }

    #[test]
    fn right_click_pane_menu_can_attach() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);
        app.tree_area = Rect::new(0, 0, 80, 10);

        let pane_row = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Pane { .. }))
            .unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 2,
            row: 1 + pane_row as u16,
            modifiers: KeyModifiers::empty(),
        };
        app.handle_mouse(mouse);

        let Mode::ContextMenu(menu) = &app.mode else {
            panic!("expected context menu");
        };
        assert_eq!(menu.items[0].label, "attach");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))
            .unwrap();

        let target = app.attach_request.unwrap();
        assert_eq!(target.host, "t2");
        assert_eq!(target.session, "main");
        assert_eq!(target.window, "0");
        assert_eq!(target.pane, "%0");
    }

    #[test]
    fn new_window_prompt_uses_stable_tmux_targets() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree(tree);

        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Session { .. }))
            .unwrap();
        app.start_create_window();
        let Mode::Prompt(prompt) = app.mode.clone() else {
            panic!("expected prompt");
        };
        let PromptKind::CreateWindow {
            target,
            after_window,
            ..
        } = prompt.kind
        else {
            panic!("expected create window");
        };
        assert_eq!(target, "$1");
        assert_eq!(after_window, None);

        app.mode = Mode::Normal;
        app.selected = app
            .rows
            .iter()
            .position(|row| matches!(row.id, NodeId::Window { .. }))
            .unwrap();
        app.start_create_window();
        let Mode::Prompt(prompt) = app.mode.clone() else {
            panic!("expected prompt");
        };
        let PromptKind::CreateWindow {
            target,
            after_window,
            ..
        } = prompt.kind
        else {
            panic!("expected create window");
        };
        assert_eq!(target, "$1");
        assert_eq!(after_window.as_deref(), Some("@2"));
    }

    #[test]
    fn confirm_modal_mouse_hover_selects_yes_and_no() {
        let screen = Rect::new(0, 0, 100, 40);
        let area = confirm_area(screen);

        assert_eq!(
            confirm_choice_at_mouse(area.x + 2, area.y + 4, screen),
            Some(true)
        );
        assert_eq!(
            confirm_choice_at_mouse(area.x + 2, area.y + 5, screen),
            Some(false)
        );
        assert_eq!(
            confirm_choice_at_mouse(area.x + 2, area.y + 3, screen),
            None
        );
    }

    #[test]
    fn split_modal_mouse_hover_selects_vertical_and_horizontal() {
        let screen = Rect::new(0, 0, 100, 40);
        let area = split_choice_area(screen);

        assert_eq!(
            split_choice_at_mouse(area.x + 2, area.y + 3, screen),
            Some(SplitChoice::Vertical)
        );
        assert_eq!(
            split_choice_at_mouse(area.x + 2, area.y + 4, screen),
            Some(SplitChoice::Horizontal)
        );
        assert_eq!(split_choice_at_mouse(area.x + 2, area.y + 5, screen), None);
    }

    #[test]
    fn format_line_replaces_placeholders_and_keeps_unknowns() {
        let mut values = BTreeMap::new();
        values.insert("pane_id", "%8".to_string());
        values.insert("process_status", "running".to_string());

        assert_eq!(
            format_line("{pane_id} {process_status} {missing}", &values),
            "%8 running {missing}"
        );
    }

    #[test]
    fn process_placeholders_control_busy_text() {
        let mut tree = test_host_tree("t2");
        tree.panes[0].busy_duration_secs = Some(42);
        let mut formats = test_line_formats();
        formats.window = "{window_index} {process_status} {process_elapsed_time}".to_string();

        let mut expanded = BTreeSet::new();
        expanded.insert(NodeId::Host("t2".to_string()));
        expanded.insert(NodeId::Session {
            host: "t2".to_string(),
            session: "main".to_string(),
        });

        let rows = build_rows(&[tree], &expanded, &formats);
        let window_row = rows
            .iter()
            .find(|row| matches!(row.id, NodeId::Window { .. }))
            .unwrap();
        assert_eq!(window_row.label, "0 running 42s");
        assert_eq!(window_row.detail, "1 panes");
    }

    #[test]
    fn pane_commandline_placeholder_uses_full_command_line() {
        let mut tree = test_host_tree("t2");
        tree.panes[0].pane_commandline = "python train.py --epochs 10".to_string();
        let mut formats = test_line_formats();
        formats.pane = "{pane_commandline}".to_string();

        let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
        let pane_row = rows
            .iter()
            .find(|row| matches!(row.id, NodeId::Pane { .. }))
            .unwrap();
        assert_eq!(pane_row.label, "python train.py --epochs 10");
    }

    #[test]
    fn default_auto_refresh_interval_is_configured() {
        let raw = RawConfig {
            hosts: Some(Value::Array(vec![Value::String("t2".to_string())])),
            connect_timeout_secs: None,
            scan_concurrency: None,
            mouse_scroll_lines: None,
            auto_refresh_secs: None,
            default_expand_level: None,
            server_line_text: None,
            session_line_text: None,
            window_line_text: None,
            pane_line_text: None,
        };

        let config = normalize_config(raw).unwrap();
        assert_eq!(config.auto_refresh_secs, DEFAULT_AUTO_REFRESH_SECS);
    }

    #[test]
    fn missing_config_file_is_created_empty() {
        let dir = std::env::temp_dir().join(format!(
            "tmux-gateway-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");

        let _ = load_config(&path).unwrap();

        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_expand_level_controls_initial_visible_depth() {
        let tree = test_host_tree("t2");
        let mut app = test_app_with_tree_at_level(tree.clone(), ExpandLevel::Server);
        app.expanded.clear();
        app.apply_trees_after_refresh();
        assert_eq!(app.rows.len(), 1);
        assert!(matches!(app.rows[0].id, NodeId::Host(_)));

        let mut app = test_app_with_tree_at_level(tree.clone(), ExpandLevel::Window);
        app.expanded.clear();
        app.apply_trees_after_refresh();
        assert!(
            app.rows
                .iter()
                .any(|row| matches!(row.id, NodeId::Window { .. }))
        );
        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row.id, NodeId::Pane { .. }))
        );

        let mut app = test_app_with_tree_at_level(tree, ExpandLevel::Pane);
        app.expanded.clear();
        app.apply_trees_after_refresh();
        assert!(
            app.rows
                .iter()
                .any(|row| matches!(row.id, NodeId::Pane { .. }))
        );
    }

    fn test_app_with_rows(count: usize) -> App {
        App {
            config: Config {
                hosts: Vec::new(),
                connect_timeout_secs: 1,
                scan_concurrency: 1,
                mouse_scroll_lines: DEFAULT_MOUSE_SCROLL_LINES,
                auto_refresh_secs: DEFAULT_AUTO_REFRESH_SECS,
                default_expand_level: DEFAULT_EXPAND_LEVEL,
                line_formats: test_line_formats(),
            },
            trees: Vec::new(),
            expanded: BTreeSet::new(),
            rows: (0..count)
                .map(|index| test_row(&format!("row-{index}")))
                .collect(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 1,
            screen_area: Rect::new(0, 0, 80, 24),
            tree_area: Rect::new(0, 0, 80, 10),
            status: String::new(),
            status_expires_at: None,
            search: String::new(),
            search_direction: SearchDirection::Down,
            last_search_direction: SearchDirection::Down,
            mode: Mode::Normal,
            pending_g: false,
            attach_request: None,
        }
    }

    fn test_app_with_tree(tree: HostTree) -> App {
        test_app_with_tree_at_level(tree, ExpandLevel::Pane)
    }

    fn test_app_with_tree_at_level(tree: HostTree, default_expand_level: ExpandLevel) -> App {
        let config = Config {
            hosts: vec![tree.host.clone()],
            connect_timeout_secs: 1,
            scan_concurrency: 1,
            mouse_scroll_lines: DEFAULT_MOUSE_SCROLL_LINES,
            auto_refresh_secs: DEFAULT_AUTO_REFRESH_SECS,
            default_expand_level,
            line_formats: test_line_formats(),
        };
        let expanded = expanded_all(std::slice::from_ref(&tree));
        let rows = build_rows(std::slice::from_ref(&tree), &expanded, &config.line_formats);
        App {
            config,
            trees: vec![tree],
            expanded,
            rows,
            selected: 0,
            scroll_offset: 0,
            viewport_height: 10,
            screen_area: Rect::new(0, 0, 80, 24),
            tree_area: Rect::new(0, 0, 80, 10),
            status: String::new(),
            status_expires_at: None,
            search: String::new(),
            search_direction: SearchDirection::Down,
            last_search_direction: SearchDirection::Down,
            mode: Mode::Normal,
            pending_g: false,
            attach_request: None,
        }
    }

    fn test_host_tree(host: &str) -> HostTree {
        HostTree {
            host: host.to_string(),
            panes: vec![test_pane()],
            error: None,
        }
    }

    fn test_line_formats() -> LineFormats {
        LineFormats {
            server: DEFAULT_SERVER_LINE_TEXT.to_string(),
            session: DEFAULT_SESSION_LINE_TEXT.to_string(),
            window: DEFAULT_WINDOW_LINE_TEXT.to_string(),
            pane: DEFAULT_PANE_LINE_TEXT.to_string(),
        }
    }

    fn test_pane() -> PaneInfo {
        PaneInfo {
            session_name: "main".to_string(),
            session_id: "$1".to_string(),
            window_index: "0".to_string(),
            window_id: "@2".to_string(),
            window_name: "pwsh".to_string(),
            pane_index: "0".to_string(),
            pane_id: "%0".to_string(),
            pane_pid: 123,
            pane_current_command: "pwsh".to_string(),
            pane_commandline: "pwsh -NoLogo".to_string(),
            pane_title: String::new(),
            active_window: true,
            active_pane: true,
            busy_duration_secs: None,
        }
    }

    fn new_item_labels(app: &App) -> Vec<String> {
        app.context_menu_items()
            .into_iter()
            .filter(|item| {
                matches!(
                    item.action,
                    ContextAction::NewSession | ContextAction::NewWindow | ContextAction::NewPane
                )
            })
            .map(|item| item.label)
            .collect()
    }

    fn test_row(search_text: &str) -> VisibleRow {
        VisibleRow {
            id: NodeId::Host(search_text.to_string()),
            depth: 0,
            label: search_text.to_string(),
            detail: String::new(),
            search_text: search_text.to_string(),
            selectable: false,
            busy_duration_secs: None,
        }
    }
}
