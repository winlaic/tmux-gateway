use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use regex::Regex;

use crate::config::{Config, ExpandLevel, StartPage};
use crate::model::{HostTree, HostUpdate, NodeId, VisibleRow};
use crate::remote::{
    collect_gpu_updates_streaming, collect_pane_updates_streaming, mark_pane_gpu_indices,
    shell_quote, sort_trees_by_config, ssh_options,
};
use crate::tree::{
    SearchStart, build_pane_rows, build_rows, compile_search_regex, expandable_node_ids,
    group_tree, parent_id, search_rows,
};
use crate::ui::{
    confirm_choice_at_mouse, context_menu_area, menu_item_at_mouse, split_choice_at_mouse,
};

const DEFAULT_STATUS: &str = "s switch page | . hide/show idle panes | Enter attach | right-click menu | a/x add/kill | r reload | /? n/N | ^u/^d | gg/G | q";
const STATUS_TTL: Duration = Duration::from_secs(3);
const GPU_SCAN_START_DELAY: Duration = Duration::from_millis(750);
pub(crate) const SERVER_PICKER_MAX_VISIBLE: usize = 12;

pub(crate) struct App {
    pub(crate) config: Config,
    pub(crate) trees: Vec<HostTree>,
    pub(crate) expanded: BTreeSet<NodeId>,
    pub(crate) rows: Vec<VisibleRow>,
    pub(crate) selected: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) viewport_height: usize,
    pub(crate) screen_area: Rect,
    pub(crate) tree_area: Rect,
    pub(crate) status: String,
    pub(crate) status_expires_at: Option<Instant>,
    pub(crate) search: String,
    pub(crate) search_regex: Option<Regex>,
    pub(crate) search_error: Option<String>,
    pub(crate) search_direction: SearchDirection,
    pub(crate) last_search_direction: SearchDirection,
    pub(crate) mode: Mode,
    pub(crate) pending_g: bool,
    pub(crate) default_expand_pending: bool,
    pub(crate) page_mode: PageMode,
    pub(crate) hide_idle_panes: bool,
    pub(crate) attach_request: Option<AttachTarget>,
    pub(crate) refresh_request: Option<RefreshRequest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageMode {
    Tree,
    Panes,
}

impl PageMode {
    fn toggled(self) -> Self {
        match self {
            Self::Tree => Self::Panes,
            Self::Panes => Self::Tree,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Panes => "panes",
        }
    }

    pub(crate) fn subtitle(self) -> &'static str {
        match self {
            Self::Tree => "server / session / window / pane",
            Self::Panes => "flat view of panes (. toggles idle panes)",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RefreshRequest {
    All,
    Hosts(Vec<String>),
}

pub(crate) struct ScanTask {
    config: Config,
    receiver: Option<Receiver<HostUpdate>>,
    pending: usize,
}

impl ScanTask {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            receiver: None,
            pending: 0,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.receiver.is_some()
    }

    pub(crate) fn start_all(&mut self) {
        self.start_hosts(self.config.hosts.clone());
    }

    pub(crate) fn start_hosts(&mut self, hosts: Vec<String>) {
        if self.is_running() {
            return;
        }
        if hosts.is_empty() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.pending = hosts.len() * 2;
        self.receiver = Some(receiver);
        let config = self.config.clone();
        let gpu_config = config.clone();
        let gpu_hosts = hosts.clone();
        let gpu_sender = sender.clone();
        thread::spawn(move || collect_pane_updates_streaming(&config, &hosts, sender));
        thread::spawn(move || {
            thread::sleep(GPU_SCAN_START_DELAY);
            collect_gpu_updates_streaming(&gpu_config, &gpu_hosts, gpu_sender);
        });
    }

    pub(crate) fn drain(&mut self) -> Vec<HostUpdate> {
        let mut updates = Vec::new();
        let Some(receiver) = self.receiver.take() else {
            return updates;
        };

        loop {
            match receiver.try_recv() {
                Ok(update) => {
                    self.pending = self.pending.saturating_sub(1);
                    updates.push(update);
                    if self.pending == 0 {
                        return updates;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.receiver = Some(receiver);
                    return updates;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = 0;
                    return updates;
                }
            }
        }
    }
}

pub(crate) struct AutoRefresh {
    interval: Option<Duration>,
    next_refresh: Instant,
}

impl AutoRefresh {
    pub(crate) fn new(config: &Config) -> Self {
        let interval =
            (config.auto_refresh_secs > 0).then(|| Duration::from_secs(config.auto_refresh_secs));
        Self {
            interval,
            next_refresh: Instant::now() + interval.unwrap_or_default(),
        }
    }

    pub(crate) fn should_start(&mut self, scan_task: &ScanTask) -> bool {
        let Some(interval) = self.interval else {
            return false;
        };
        if scan_task.is_running() {
            return false;
        }
        if Instant::now() >= self.next_refresh {
            self.next_refresh = Instant::now() + interval;
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchDirection {
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

    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Down => "/",
            Self::Up => "?",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Mode {
    Normal,
    Search,
    ContextMenu(ContextMenuState),
    Prompt(PromptState),
    SplitChoice(SplitChoiceState),
    Confirm(ConfirmState),
    ServerPicker(ServerPickerState),
}

#[derive(Clone, Debug)]
pub(crate) struct ServerPickerState {
    pub(crate) all_hosts: Vec<String>,
    pub(crate) filter: String,
    pub(crate) filtered: Vec<String>,
    pub(crate) selected: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) searching: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ContextMenuState {
    pub(crate) items: Vec<ContextMenuItem>,
    pub(crate) selected: usize,
    pub(crate) area: Rect,
}

#[derive(Clone, Debug)]
pub(crate) struct ContextMenuItem {
    pub(crate) label: String,
    pub(crate) action: ContextAction,
    pub(crate) shortcut: Option<char>,
}

impl ContextMenuItem {
    pub(crate) fn display_label(&self) -> String {
        match self.shortcut {
            Some(shortcut) => format!("{}({shortcut})", self.label),
            None => self.label.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextAction {
    Attach,
    Kill,
    NewSession,
    NewWindow,
    NewPane,
    SplitVertical,
    SplitHorizontal,
    Rename,
}

#[derive(Clone, Debug)]
pub(crate) struct PromptState {
    pub(crate) title: String,
    pub(crate) value: String,
    pub(crate) kind: PromptKind,
}

#[derive(Clone, Debug)]
pub(crate) enum PromptKind {
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

impl PromptKind {
    pub(crate) fn host(&self) -> &str {
        match self {
            Self::CreateSession { host }
            | Self::CreateWindow { host, .. }
            | Self::RenameSession { host, .. }
            | Self::RenameWindow { host, .. }
            | Self::RenamePane { host, .. } => host,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SplitChoiceState {
    host: String,
    pane: String,
    pub(crate) selected: SplitChoice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SplitChoice {
    Vertical,
    Horizontal,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfirmState {
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) action: ConfirmAction,
    pub(crate) selected_yes: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum ConfirmAction {
    KillSession { host: String, target: String },
    KillWindow { host: String, target: String },
    KillPane { host: String, pane: String },
}

impl ConfirmAction {
    pub(crate) fn host(&self) -> &str {
        match self {
            Self::KillSession { host, .. }
            | Self::KillWindow { host, .. }
            | Self::KillPane { host, .. } => host,
        }
    }
}

impl App {
    pub(crate) fn new(config: Config) -> Self {
        let page_mode = match config.start_page {
            StartPage::Tree => PageMode::Tree,
            StartPage::Panes => PageMode::Panes,
        };
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
            search_regex: None,
            search_error: None,
            search_direction: SearchDirection::Down,
            last_search_direction: SearchDirection::Down,
            mode: Mode::Normal,
            pending_g: false,
            default_expand_pending: true,
            page_mode,
            hide_idle_panes: true,
            attach_request: None,
            refresh_request: None,
        };
        app.initialize_connecting_hosts();
        app
    }

    fn initialize_connecting_hosts(&mut self) {
        self.trees = self
            .config
            .hosts
            .iter()
            .map(|host| HostTree {
                host: host.clone(),
                panes: Vec::new(),
                processes: Vec::new(),
                gpus: Vec::new(),
                gpu_processes: Vec::new(),
                error: None,
                connecting: true,
            })
            .collect();
        self.apply_trees_after_refresh();
        self.set_status(DEFAULT_STATUS);
    }

    pub(crate) fn apply_scan_results(&mut self, updates: Vec<HostUpdate>) {
        if updates.is_empty() {
            return;
        }

        for update in updates {
            match update {
                HostUpdate::Panes {
                    host,
                    panes,
                    processes,
                    error,
                } => {
                    let tree = self.ensure_tree(&host);
                    tree.panes = panes;
                    tree.processes = processes;
                    tree.error = error;
                    tree.connecting = false;
                    mark_pane_gpu_indices(
                        &mut tree.panes,
                        &tree.processes,
                        &tree.gpus,
                        &tree.gpu_processes,
                    );
                }
                HostUpdate::Gpus {
                    host,
                    gpus,
                    gpu_processes,
                } => {
                    let tree = self.ensure_tree(&host);
                    tree.gpus = gpus;
                    tree.gpu_processes = gpu_processes;
                    mark_pane_gpu_indices(
                        &mut tree.panes,
                        &tree.processes,
                        &tree.gpus,
                        &tree.gpu_processes,
                    );
                }
            }
        }
        sort_trees_by_config(&mut self.trees, &self.config.hosts);
        let selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
        self.apply_trees_after_refresh_preserving(selected_id);
    }

    fn ensure_tree(&mut self, host: &str) -> &mut HostTree {
        if let Some(index) = self.trees.iter().position(|tree| tree.host == host) {
            return &mut self.trees[index];
        }

        self.trees.push(HostTree {
            host: host.to_string(),
            panes: Vec::new(),
            processes: Vec::new(),
            gpus: Vec::new(),
            gpu_processes: Vec::new(),
            error: None,
            connecting: true,
        });
        self.trees.last_mut().expect("tree was just pushed")
    }

    pub(crate) fn apply_trees_after_refresh(&mut self) {
        self.apply_trees_after_refresh_preserving(None);
    }

    fn apply_trees_after_refresh_preserving(&mut self, selected_id: Option<NodeId>) {
        self.expand_initial_if_ready();
        self.rebuild_rows();
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.rows.iter().position(|row| row.id == selected_id) {
                self.selected = index;
            } else {
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            }
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
        self.clamp_scroll();
        self.fit_scroll_to_height(self.viewport_height);
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_expires_at = None;
    }

    pub(crate) fn set_temp_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_expires_at = Some(Instant::now() + STATUS_TTL);
    }

    pub(crate) fn current_status(&mut self) -> String {
        if self
            .status_expires_at
            .is_some_and(|expires_at| Instant::now() >= expires_at)
        {
            self.set_status(DEFAULT_STATUS);
        }
        self.status.clone()
    }

    fn expand_initial_if_ready(&mut self) {
        if !self.default_expand_pending || self.trees.iter().any(|tree| tree.connecting) {
            return;
        }
        self.default_expand_pending = false;

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
        let expandable = expandable_node_ids(&self.trees);
        self.expanded.retain(|id| expandable.contains(id));
        self.rows = match self.page_mode {
            PageMode::Tree => build_rows(&self.trees, &self.expanded, &self.config.line_formats),
            PageMode::Panes => {
                build_pane_rows(&self.trees, &self.config.line_formats, self.hide_idle_panes)
            }
        };
    }

    fn toggle_page_mode(&mut self) {
        self.pending_g = false;
        let selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
        self.page_mode = self.page_mode.toggled();
        self.rebuild_rows();
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.rows.iter().position(|row| row.id == selected_id) {
                self.selected = index;
            } else {
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            }
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
        self.clamp_scroll();
        self.fit_scroll_to_height(self.viewport_height);
    }

    fn toggle_idle_panes(&mut self) {
        self.pending_g = false;
        if self.page_mode != PageMode::Panes {
            return;
        }

        let selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
        self.hide_idle_panes = !self.hide_idle_panes;
        self.rebuild_rows();
        if let Some(selected_id) = selected_id {
            if let Some(index) = self.rows.iter().position(|row| row.id == selected_id) {
                self.selected = index;
            } else {
                self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            }
        } else {
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        }
        self.clamp_scroll();
        self.fit_scroll_to_height(self.viewport_height);
        self.set_temp_status(if self.hide_idle_panes {
            "panes page: hiding idle panes"
        } else {
            "panes page: showing all panes"
        });
    }

    pub(crate) fn select_next(&mut self) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }
        if self.selected + 1 >= self.rows.len() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected += 1;
            self.keep_selected_visible_down();
        }
    }

    pub(crate) fn select_previous(&mut self) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.rows.len() - 1;
            self.keep_selected_visible_down();
        } else {
            self.selected -= 1;
            self.keep_selected_visible_up();
        }
    }

    pub(crate) fn expand_selected(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.expandable {
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

    pub(crate) fn toggle_selected(&mut self) {
        self.pending_g = false;
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.expandable {
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

    pub(crate) fn page_down(&mut self, amount: usize) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }

        let amount = amount.max(1);
        self.selected = (self.selected + amount).min(self.rows.len() - 1);
        self.scroll_offset = (self.scroll_offset + amount).min(self.rows.len() - 1);
    }

    pub(crate) fn page_up(&mut self, amount: usize) {
        self.pending_g = false;
        if self.rows.is_empty() {
            return;
        }

        let amount = amount.max(1);
        self.selected = self.selected.saturating_sub(amount);
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    pub(crate) fn mouse_scroll_down(&mut self) {
        let max_offset = self.rows.len().saturating_sub(self.viewport_height);
        let new_offset = self
            .scroll_offset
            .saturating_add(self.config.mouse_scroll_lines)
            .min(max_offset);
        self.apply_mouse_scroll_offset(new_offset);
    }

    pub(crate) fn mouse_scroll_up(&mut self) {
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

    pub(crate) fn take_attach_request(&mut self) -> Option<AttachTarget> {
        self.attach_request.take()
    }

    pub(crate) fn request_refresh_all(&mut self) {
        self.refresh_request = Some(RefreshRequest::All);
    }

    pub(crate) fn request_refresh_host(&mut self, host: impl Into<String>) {
        self.refresh_request = Some(RefreshRequest::Hosts(vec![host.into()]));
    }

    pub(crate) fn take_refresh_request(&mut self) -> Option<RefreshRequest> {
        self.refresh_request.take()
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

    pub(crate) fn fit_scroll_to_height(&mut self, height: usize) {
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
                destination: AttachDestination::Pane {
                    session: session.clone(),
                    window: window.clone(),
                    pane: pane.clone(),
                },
            }),
            NodeId::Window {
                host,
                session,
                window,
            } => Some(AttachTarget {
                host: host.clone(),
                destination: AttachDestination::Window {
                    session: session.clone(),
                    window: window.clone(),
                },
            }),
            _ => None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
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
            Mode::ServerPicker(_) => {
                self.handle_server_picker_key(key.code)?;
                Ok(false)
            }
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
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

        if matches!(self.mode, Mode::ServerPicker(_)) {
            self.handle_server_picker_mouse(mouse);
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

    pub(crate) fn row_at_mouse(&self, column: u16, row: u16) -> Option<usize> {
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

    pub(crate) fn context_menu_items(&self) -> Vec<ContextMenuItem> {
        let Some(row) = self.rows.get(self.selected) else {
            return Vec::new();
        };
        let active_pane_row =
            self.page_mode == PageMode::Panes && matches!(row.id, NodeId::Pane { .. });

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
            NodeId::Pane { .. } => {
                if active_pane_row {
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
                items.push(ContextMenuItem {
                    label: "new pane".to_string(),
                    action: ContextAction::NewPane,
                    shortcut: Some('p'),
                });
            }
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
                        self.set_temp_status(result_status::<()>(
                            Err(err),
                            "completed",
                            self.config.log_path.as_deref(),
                        ));
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
            ContextAction::SplitVertical => self.start_split_selected_pane(SplitChoice::Vertical),
            ContextAction::SplitHorizontal => {
                self.start_split_selected_pane(SplitChoice::Horizontal)
            }
            ContextAction::Rename => self.start_rename(),
        }
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let code = key.code;
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Esc => self.pending_g = false,
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
                self.request_refresh_all();
                self.set_temp_status("refreshing in background");
            }
            KeyCode::Char('s') => self.toggle_page_mode(),
            KeyCode::Char('.') => self.toggle_idle_panes(),
            KeyCode::Char('G') => self.select_last(),
            KeyCode::Char('g') if self.pending_g => self.select_first(),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char('a') => self.start_create(),
            KeyCode::Char('A') => self.start_server_picker(),
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

    pub(crate) fn start_search(&mut self, direction: SearchDirection) {
        self.pending_g = false;
        self.search.clear();
        self.search_regex = None;
        self.search_error = None;
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

    pub(crate) fn start_create(&mut self) {
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
        let Some(row) = self.rows.get(self.selected) else {
            return Vec::new();
        };

        if self.page_mode == PageMode::Panes && matches!(row.id, NodeId::Pane { .. }) {
            return vec![
                ContextMenuItem {
                    label: "new session".to_string(),
                    action: ContextAction::NewSession,
                    shortcut: Some('s'),
                },
                ContextMenuItem {
                    label: "new window".to_string(),
                    action: ContextAction::NewWindow,
                    shortcut: Some('w'),
                },
                ContextMenuItem {
                    label: "vertical split".to_string(),
                    action: ContextAction::SplitVertical,
                    shortcut: Some('v'),
                },
                ContextMenuItem {
                    label: "horizontal split".to_string(),
                    action: ContextAction::SplitHorizontal,
                    shortcut: Some('h'),
                },
            ];
        }

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

    fn is_host_unavailable(&self, host: &str) -> bool {
        self.trees
            .iter()
            .find(|t| t.host == host)
            .map(|t| t.error.is_some() || t.connecting)
            .unwrap_or(true)
    }

    fn start_server_picker(&mut self) {
        self.pending_g = false;
        let all_hosts = self.config.hosts.clone();
        if all_hosts.is_empty() {
            self.set_temp_status("no hosts configured");
            return;
        }
        let filtered = all_hosts.clone();
        self.mode = Mode::ServerPicker(ServerPickerState {
            all_hosts,
            filter: String::new(),
            filtered,
            selected: 0,
            scroll_offset: 0,
            searching: false,
        });
    }

    fn handle_server_picker_key(&mut self, code: KeyCode) -> Result<()> {
        let Mode::ServerPicker(mut state) = self.mode.clone() else {
            return Ok(());
        };

        if state.searching {
            match code {
                KeyCode::Esc => {
                    state.searching = false;
                    state.filter.clear();
                    state.filtered = state.all_hosts.clone();
                    state.selected = state.selected.min(state.filtered.len().saturating_sub(1));
                    state.scroll_offset = 0;
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Enter => {
                    state.searching = false;
                    update_server_picker_filter(&mut state);
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Backspace => {
                    state.filter.pop();
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Char(ch) if !ch.is_control() => {
                    state.filter.push(ch);
                    self.mode = Mode::ServerPicker(state);
                }
                _ => self.mode = Mode::ServerPicker(state),
            }
        } else {
            match code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.set_temp_status("server picker cancelled");
                }
                KeyCode::Char('/') => {
                    state.searching = true;
                    state.filter.clear();
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    server_picker_move_up(&mut state);
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    server_picker_move_down(&mut state);
                    self.mode = Mode::ServerPicker(state);
                }
                KeyCode::Enter => {
                    if let Some(host) = state.filtered.get(state.selected).cloned() {
                        if self.is_host_unavailable(&host) {
                            self.set_temp_status(format!(
                                "{host}: unavailable, cannot create session"
                            ));
                            self.mode = Mode::ServerPicker(state);
                        } else {
                            let (result, attach_target) =
                                split_attach_result(create_remote_session(
                                    &host,
                                    None,
                                    self.config.connect_timeout_secs,
                                    self.config.log_path.as_deref(),
                                ));
                            self.mode = Mode::Normal;
                            self.request_refresh_host(host);
                            if let Some(target) = attach_target {
                                self.attach_request = Some(target);
                            }
                            self.set_temp_status(result_status(
                                result,
                                "session created",
                                self.config.log_path.as_deref(),
                            ));
                        }
                    } else {
                        self.mode = Mode::ServerPicker(state);
                        self.set_temp_status("no matching server");
                    }
                }
                _ => self.mode = Mode::ServerPicker(state),
            }
        }

        Ok(())
    }

    fn handle_server_picker_mouse(&mut self, mouse: MouseEvent) {
        let Mode::ServerPicker(mut state) = self.mode.clone() else {
            return;
        };

        let area = server_picker_list_area(self.screen_area, state.filtered.len());

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = server_picker_item_at_mouse(&state, &area, mouse.row) {
                    state.selected = index;
                    self.mode = Mode::ServerPicker(state);
                }
            }
            MouseEventKind::ScrollUp => {
                server_picker_move_up(&mut state);
                self.mode = Mode::ServerPicker(state);
            }
            MouseEventKind::ScrollDown => {
                server_picker_move_down(&mut state);
                self.mode = Mode::ServerPicker(state);
            }
            _ => self.mode = Mode::ServerPicker(state),
        }
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

    pub(crate) fn start_create_window(&mut self) {
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
            }
            | NodeId::Pane {
                host,
                session,
                window,
                ..
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
        let Some((host, pane)) = self.selected_split_target() else {
            self.set_temp_status("select a window or pane to create a pane");
            return;
        };
        self.mode = Mode::SplitChoice(SplitChoiceState {
            host,
            pane,
            selected: SplitChoice::Vertical,
        });
    }

    fn start_split_selected_pane(&mut self, split: SplitChoice) {
        self.pending_g = false;
        let Some((host, pane)) = self.selected_split_target() else {
            self.set_temp_status("select a window or pane to split");
            return;
        };
        let (result, attach_target) = split_attach_result(split_remote_pane(
            &host,
            &pane,
            split,
            self.config.connect_timeout_secs,
            self.config.log_path.as_deref(),
        ));
        self.request_refresh_host(host.clone());
        if let Some(target) = attach_target {
            self.attach_request = Some(target);
        }
        self.set_temp_status(result_status(
            result,
            "pane split",
            self.config.log_path.as_deref(),
        ));
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

    fn selected_split_target(&self) -> Option<(String, String)> {
        let row = self.rows.get(self.selected)?;
        match &row.id {
            NodeId::Window {
                host,
                session,
                window,
            } => {
                let pane = pane_for_window(&self.trees, host, session, window)?;
                Some((host.clone(), pane))
            }
            NodeId::Pane { host, pane, .. } => Some((host.clone(), pane.clone())),
            _ => None,
        }
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
                let (result, attach_target) = match &prompt.kind {
                    PromptKind::CreateSession { host } => {
                        split_attach_result(create_remote_session(
                            host,
                            optional_name(&prompt.value),
                            self.config.connect_timeout_secs,
                            self.config.log_path.as_deref(),
                        ))
                    }
                    PromptKind::CreateWindow {
                        host,
                        target,
                        after_window,
                    } => split_attach_result(create_remote_window(
                        host,
                        target,
                        after_window.as_deref(),
                        optional_name(&prompt.value),
                        self.config.connect_timeout_secs,
                        self.config.log_path.as_deref(),
                    )),
                    PromptKind::RenameSession { host, target } => (
                        rename_remote_session(
                            host,
                            target,
                            prompt.value.trim(),
                            self.config.connect_timeout_secs,
                            self.config.log_path.as_deref(),
                        ),
                        None,
                    ),
                    PromptKind::RenameWindow { host, target } => (
                        rename_remote_window(
                            host,
                            target,
                            prompt.value.trim(),
                            self.config.connect_timeout_secs,
                            self.config.log_path.as_deref(),
                        ),
                        None,
                    ),
                    PromptKind::RenamePane { host, pane } => (
                        rename_remote_pane(
                            host,
                            pane,
                            prompt.value.trim(),
                            self.config.connect_timeout_secs,
                            self.config.log_path.as_deref(),
                        ),
                        None,
                    ),
                };
                self.mode = Mode::Normal;
                self.request_refresh_host(prompt.kind.host().to_string());
                if let Some(target) = attach_target {
                    self.attach_request = Some(target);
                }
                self.set_temp_status(result_status(
                    result,
                    prompt_success(&prompt.kind),
                    self.config.log_path.as_deref(),
                ));
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
                let (result, attach_target) = split_attach_result(split_remote_pane(
                    &choice.host,
                    &choice.pane,
                    choice.selected,
                    self.config.connect_timeout_secs,
                    self.config.log_path.as_deref(),
                ));
                self.mode = Mode::Normal;
                self.request_refresh_host(choice.host.clone());
                if let Some(target) = attach_target {
                    self.attach_request = Some(target);
                }
                self.set_temp_status(result_status(
                    result,
                    "pane split",
                    self.config.log_path.as_deref(),
                ));
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
                    let (result, attach_target) = split_attach_result(split_remote_pane(
                        &choice.host,
                        &choice.pane,
                        choice.selected,
                        self.config.connect_timeout_secs,
                        self.config.log_path.as_deref(),
                    ));
                    self.mode = Mode::Normal;
                    self.request_refresh_host(choice.host.clone());
                    if let Some(target) = attach_target {
                        self.attach_request = Some(target);
                    }
                    self.set_temp_status(result_status(
                        result,
                        "pane split",
                        self.config.log_path.as_deref(),
                    ));
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
                let result = run_confirm_action(
                    &confirm.action,
                    self.config.connect_timeout_secs,
                    self.config.log_path.as_deref(),
                );
                self.mode = Mode::Normal;
                self.request_refresh_host(confirm.action.host().to_string());
                self.set_temp_status(result_status(
                    result,
                    "killed",
                    self.config.log_path.as_deref(),
                ));
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                confirm.selected_yes = !confirm.selected_yes;
                self.mode = Mode::Confirm(confirm);
            }
            KeyCode::Enter => {
                if confirm.selected_yes {
                    let result = run_confirm_action(
                        &confirm.action,
                        self.config.connect_timeout_secs,
                        self.config.log_path.as_deref(),
                    );
                    self.mode = Mode::Normal;
                    self.request_refresh_host(confirm.action.host().to_string());
                    self.set_temp_status(result_status(
                        result,
                        "killed",
                        self.config.log_path.as_deref(),
                    ));
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
                        let result = run_confirm_action(
                            &confirm.action,
                            self.config.connect_timeout_secs,
                            self.config.log_path.as_deref(),
                        );
                        self.mode = Mode::Normal;
                        self.request_refresh_host(confirm.action.host().to_string());
                        self.set_temp_status(result_status(
                            result,
                            "killed",
                            self.config.log_path.as_deref(),
                        ));
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
        self.refresh_search_regex();
        self.apply_search_from_current();
        self.set_status(self.search_prompt());
    }

    fn pop_search(&mut self) {
        self.search.pop();
        self.refresh_search_regex();
        self.apply_search_from_current();
        self.set_status(self.search_prompt());
    }

    fn apply_search_from_current(&mut self) {
        let Some(regex) = self.search_regex.as_ref() else {
            return;
        };

        let found = search_rows(
            &self.rows,
            self.selected,
            regex,
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

        let Some(regex) = self.search_regex.as_ref() else {
            let message = self
                .search_error
                .clone()
                .unwrap_or_else(|| "invalid regex".to_string());
            self.set_temp_status(format!("invalid regex: {message}"));
            return;
        };

        if let Some(index) = search_rows(
            &self.rows,
            self.selected,
            regex,
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

    pub(crate) fn search_prompt(&self) -> String {
        match &self.search_error {
            Some(error) => format!(
                "{}{}  [invalid regex: {error}]",
                self.search_direction.prefix(),
                self.search
            ),
            None => format!("{}{}", self.search_direction.prefix(), self.search),
        }
    }

    pub(crate) fn active_search_regex(&self) -> Option<&Regex> {
        self.search_regex.as_ref()
    }

    fn refresh_search_regex(&mut self) {
        if self.search.is_empty() {
            self.search_regex = None;
            self.search_error = None;
            return;
        }

        match compile_search_regex(&self.search) {
            Ok(regex) => {
                self.search_regex = Some(regex);
                self.search_error = None;
            }
            Err(err) => {
                self.search_regex = None;
                self.search_error = Some(err.to_string());
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AttachDestination {
    Default,
    Session {
        session: String,
    },
    Window {
        session: String,
        window: String,
    },
    Pane {
        session: String,
        window: String,
        pane: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AttachTarget {
    pub(crate) host: String,
    pub(crate) destination: AttachDestination,
}

pub(crate) fn attach_host(target: &AttachTarget, connect_timeout_secs: u64) -> Result<()> {
    let remote_command = attach_remote_command(&target.destination);

    let mut child = Command::new("ssh")
        .arg("-t")
        .args(ssh_options(connect_timeout_secs))
        .arg(&target.host)
        .arg(remote_command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start ssh for host {}", target.host))?;

    let status = child.wait()?;
    if !status.success() {
        bail!("ssh/tmux attach failed for host {}: {status}", target.host);
    }

    Ok(())
}

pub(crate) fn attach_remote_command(destination: &AttachDestination) -> String {
    match destination {
        AttachDestination::Default => "tmux attach-session".to_string(),
        AttachDestination::Session { session } => {
            format!("tmux attach-session -t {}", shell_quote(session))
        }
        AttachDestination::Window { session, window } => {
            format!(
                "tmux attach-session -t {}",
                shell_quote(&format!("{session}:{window}"))
            )
        }
        AttachDestination::Pane {
            session,
            window,
            pane,
        } => format!(
            "tmux attach-session -t {}",
            shell_quote(&format!("{session}:{window}.{pane}"))
        ),
    }
}

fn create_remote_session(
    host: &str,
    name: Option<&str>,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<AttachTarget> {
    let mut remote_command = format!(
        "tmux new-session -d -P -F {}",
        shell_quote("#{session_name}")
    );
    if let Some(name) = name {
        remote_command.push_str(" -s ");
        remote_command.push_str(&shell_quote(name));
    }
    let output = run_remote_tmux_capture(host, &remote_command, connect_timeout_secs, log_path)?;
    parse_created_session_target(host, &output)
}

fn create_remote_window(
    host: &str,
    target: &str,
    after_window: Option<&str>,
    name: Option<&str>,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<AttachTarget> {
    let target = after_window.unwrap_or(target);
    let mut remote_command = format!(
        "tmux new-window -a -P -F {} -t {}",
        shell_quote("#{session_name}\t#{window_index}"),
        shell_quote(&target)
    );
    if let Some(name) = name {
        remote_command.push_str(" -n ");
        remote_command.push_str(&shell_quote(name));
    }
    let output = run_remote_tmux_capture(host, &remote_command, connect_timeout_secs, log_path)?;
    parse_created_window_target(host, &output)
}

fn split_remote_pane(
    host: &str,
    pane: &str,
    split: SplitChoice,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<AttachTarget> {
    let flag = match split {
        SplitChoice::Vertical => "-v",
        SplitChoice::Horizontal => "-h",
    };
    let remote_command = format!(
        "tmux split-window {flag} -P -F {} -t {}",
        shell_quote("#{session_name}\t#{window_index}\t#{pane_id}"),
        shell_quote(pane)
    );
    let output = run_remote_tmux_capture(host, &remote_command, connect_timeout_secs, log_path)?;
    parse_created_pane_target(host, &output)
}

fn rename_remote_session(
    host: &str,
    target: &str,
    new_name: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<()> {
    if new_name.is_empty() {
        bail!("session name must not be empty");
    }
    let remote_command = format!(
        "tmux rename-session -t {} {}",
        shell_quote(target),
        shell_quote(new_name)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs, log_path)
}

fn rename_remote_window(
    host: &str,
    target: &str,
    new_name: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<()> {
    if new_name.is_empty() {
        bail!("window name must not be empty");
    }
    let remote_command = format!(
        "tmux rename-window -t {} {}",
        shell_quote(target),
        shell_quote(new_name)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs, log_path)
}

fn rename_remote_pane(
    host: &str,
    pane: &str,
    new_title: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<()> {
    let remote_command = format!(
        "tmux select-pane -t {} -T {}",
        shell_quote(pane),
        shell_quote(new_title)
    );
    run_remote_tmux(host, &remote_command, connect_timeout_secs, log_path)
}

fn run_confirm_action(
    action: &ConfirmAction,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<()> {
    match action {
        ConfirmAction::KillSession { host, target } => run_remote_tmux(
            host,
            &format!("tmux kill-session -t {}", shell_quote(target)),
            connect_timeout_secs,
            log_path,
        ),
        ConfirmAction::KillWindow { host, target } => run_remote_tmux(
            host,
            &format!("tmux kill-window -t {}", shell_quote(target)),
            connect_timeout_secs,
            log_path,
        ),
        ConfirmAction::KillPane { host, pane } => run_remote_tmux(
            host,
            &format!("tmux kill-pane -t {}", shell_quote(pane)),
            connect_timeout_secs,
            log_path,
        ),
    }
}

fn run_remote_tmux(
    host: &str,
    remote_command: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<()> {
    run_remote_tmux_output(host, remote_command, connect_timeout_secs, log_path).map(|_| ())
}

fn run_remote_tmux_capture(
    host: &str,
    remote_command: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<String> {
    let output = run_remote_tmux_output(host, remote_command, connect_timeout_secs, log_path)?;
    String::from_utf8(output.stdout).context("remote tmux command output was not valid utf-8")
}

fn run_remote_tmux_output(
    host: &str,
    remote_command: &str,
    connect_timeout_secs: u64,
    log_path: Option<&std::path::Path>,
) -> Result<Output> {
    log_remote_command_start(log_path, host, remote_command);
    let output = Command::new("ssh")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .output()
        .with_context(|| format!("failed to start ssh for host {host}"))?;
    log_remote_command_output(log_path, host, remote_command, &output);

    if output.status.success() {
        return Ok(output);
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

pub(crate) fn parse_created_session_target(host: &str, output: &str) -> Result<AttachTarget> {
    let session = first_created_fields(output, 1)?
        .into_iter()
        .next()
        .expect("field count already validated");
    Ok(AttachTarget {
        host: host.to_string(),
        destination: AttachDestination::Session { session },
    })
}

pub(crate) fn parse_created_window_target(host: &str, output: &str) -> Result<AttachTarget> {
    let mut fields = first_created_fields(output, 2)?.into_iter();
    let session = fields.next().expect("field count already validated");
    let window = fields.next().expect("field count already validated");
    Ok(AttachTarget {
        host: host.to_string(),
        destination: AttachDestination::Window { session, window },
    })
}

pub(crate) fn parse_created_pane_target(host: &str, output: &str) -> Result<AttachTarget> {
    let mut fields = first_created_fields(output, 3)?.into_iter();
    let session = fields.next().expect("field count already validated");
    let window = fields.next().expect("field count already validated");
    let pane = fields.next().expect("field count already validated");
    Ok(AttachTarget {
        host: host.to_string(),
        destination: AttachDestination::Pane {
            session,
            window,
            pane,
        },
    })
}

fn first_created_fields(output: &str, expected_fields: usize) -> Result<Vec<String>> {
    let line = output
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("remote tmux command did not print the created target")?;
    let fields: Vec<String> = line.split('\t').map(str::to_string).collect();
    if fields.len() < expected_fields
        || fields
            .iter()
            .take(expected_fields)
            .any(|field| field.is_empty())
    {
        bail!("remote tmux command returned an invalid created target: {line}");
    }
    Ok(fields)
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

pub(crate) fn prompt_help(kind: &PromptKind) -> &'static str {
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

fn update_server_picker_filter(state: &mut ServerPickerState) {
    if state.filter.is_empty() {
        state.filtered = state.all_hosts.clone();
    } else {
        let regex = Regex::new(&format!("(?i){}", &state.filter));
        state.filtered = match regex {
            Ok(re) => state
                .all_hosts
                .iter()
                .filter(|host| re.is_match(host))
                .cloned()
                .collect(),
            Err(_) => state.all_hosts.clone(),
        };
    }
    state.selected = state.selected.min(state.filtered.len().saturating_sub(1));
    state.scroll_offset = state.scroll_offset.min(
        state
            .filtered
            .len()
            .saturating_sub(SERVER_PICKER_MAX_VISIBLE),
    );
    if state.selected < state.scroll_offset {
        state.scroll_offset = state.selected;
    }
}

fn server_picker_move_up(state: &mut ServerPickerState) {
    if state.filtered.is_empty() {
        return;
    }
    if state.selected == 0 {
        state.selected = state.filtered.len() - 1;
        state.scroll_offset = state
            .filtered
            .len()
            .saturating_sub(SERVER_PICKER_MAX_VISIBLE);
    } else {
        state.selected -= 1;
        if state.selected < state.scroll_offset {
            state.scroll_offset = state.selected;
        }
    }
}

fn server_picker_move_down(state: &mut ServerPickerState) {
    if state.filtered.is_empty() {
        return;
    }
    if state.selected >= state.filtered.len() - 1 {
        state.selected = 0;
        state.scroll_offset = 0;
    } else {
        state.selected += 1;
        if state.selected >= state.scroll_offset + SERVER_PICKER_MAX_VISIBLE {
            state.scroll_offset = state.selected - SERVER_PICKER_MAX_VISIBLE + 1;
        }
    }
}

pub(crate) fn server_picker_list_area(screen_area: Rect, item_count: usize) -> Rect {
    let max_visible = SERVER_PICKER_MAX_VISIBLE;
    let visible_count = item_count.min(max_visible);
    let width: u16 = 44_u16.min(screen_area.width);
    let height = ((visible_count as u16) + 5).min(screen_area.height);
    Rect {
        x: screen_area.x + screen_area.width.saturating_sub(width) / 2,
        y: screen_area.y + screen_area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn server_picker_item_at_mouse(
    state: &ServerPickerState,
    area: &Rect,
    row: u16,
) -> Option<usize> {
    let list_start_y = area.y + 3;
    if row < list_start_y {
        return None;
    }
    let vi = (row - list_start_y) as usize;
    let max_visible = SERVER_PICKER_MAX_VISIBLE;
    let visible_count = state.filtered.len().min(max_visible);
    if vi >= visible_count {
        return None;
    }
    let absolute = state.scroll_offset + vi;
    if absolute < state.filtered.len() {
        Some(absolute)
    } else {
        None
    }
}

fn split_attach_result(result: Result<AttachTarget>) -> (Result<()>, Option<AttachTarget>) {
    match result {
        Ok(target) => (Ok(()), Some(target)),
        Err(err) => (Err(err), None),
    }
}

fn result_status<T>(
    result: Result<T>,
    success: &str,
    log_path: Option<&std::path::Path>,
) -> String {
    match result {
        Ok(_) => success.to_string(),
        Err(err) => match log_path {
            Some(path) => format!("operation failed: {err}; see {}", path.display()),
            None => format!("operation failed: {err}"),
        },
    }
}

fn log_remote_command_start(log_path: Option<&std::path::Path>, host: &str, remote_command: &str) {
    append_log(
        log_path,
        &format!(
            "[{}] START host={} command={}\n",
            log_timestamp(),
            host,
            remote_command
        ),
    );
}

fn log_remote_command_output(
    log_path: Option<&std::path::Path>,
    host: &str,
    remote_command: &str,
    output: &std::process::Output,
) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    append_log(
        log_path,
        &format!(
            "[{}] END host={} status={} command={}\nstdout:\n{}\nstderr:\n{}\n---\n",
            log_timestamp(),
            host,
            output.status,
            remote_command,
            stdout.trim_end(),
            stderr.trim_end(),
        ),
    );
}

fn append_log(log_path: Option<&std::path::Path>, message: &str) {
    let Some(log_path) = log_path else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = file.write_all(message.as_bytes());
    }
}

fn log_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
