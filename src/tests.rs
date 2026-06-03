use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{
    App, ConfirmAction, ContextAction, ContextMenuItem, Mode, PromptKind, RefreshRequest,
    SearchDirection, SplitChoice,
};
use crate::config::{
    Config, DEFAULT_AUTO_REFRESH_SECS, DEFAULT_EXPAND_LEVEL, DEFAULT_MOUSE_SCROLL_LINES,
    DEFAULT_PANE_LINE_TEXT, DEFAULT_SERVER_LINE_TEXT, DEFAULT_SESSION_LINE_TEXT,
    DEFAULT_WINDOW_LINE_TEXT, ExpandLevel, LineFormats, RawConfig, load_config, normalize_config,
    parse_ssh_config_hosts,
};
use crate::model::{HostTree, NodeId, PaneInfo, ProcessInfo, RowStatus, VisibleRow};
use crate::remote::{pane_busy_duration, parse_panes, shell_quote};
use crate::tree::{SearchStart, build_rows, expanded_all, format_line, host_detail, search_rows};
use crate::ui::{
    confirm_area, confirm_choice_at_mouse, context_menu_area, split_choice_area,
    split_choice_at_mouse,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use toml::Value;

#[test]
fn shell_quote_handles_single_quote() {
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
}

#[test]
fn parse_panes_reads_tmux_format() {
    let output = "s\t$1\t0\t@2\tzsh\t1\t%3\t123\tvim\t/tmp/project\ttitle\t1\t0\n";
    let panes = parse_panes(output).unwrap();

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].session_name, "s");
    assert_eq!(panes[0].session_id, "$1");
    assert_eq!(panes[0].window_id, "@2");
    assert_eq!(panes[0].pane_id, "%3");
    assert_eq!(panes[0].pane_current_path, "/tmp/project");
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
    let process_by_pid: BTreeMap<u32, &ProcessInfo> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();

    assert_eq!(
        pane_busy_duration(&pane, &process_by_pid, &children_by_parent),
        Some(3723)
    );
}

#[test]
fn pane_busy_ignores_unconfirmed_transient_command() {
    let mut pane = test_pane();
    pane.pane_current_command = "tmux".to_string();
    let process_by_pid = BTreeMap::new();
    let children_by_parent = BTreeMap::new();

    assert_eq!(
        pane_busy_duration(&pane, &process_by_pid, &children_by_parent),
        None
    );
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
    assert!(
        rows.iter().any(
            |row| matches!(row.id, NodeId::Window { .. }) && row.busy_duration_secs == Some(42)
        )
    );
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
            .any(|row| matches!(row.id, NodeId::Window { .. }) && row.busy_duration_secs.is_some())
    );
    assert!(
        rows.iter()
            .any(|row| matches!(row.id, NodeId::Pane { .. }) && row.busy_duration_secs == Some(42))
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
fn arrow_navigation_wraps_at_tree_edges() {
    let mut app = test_app_with_rows(10);
    app.viewport_height = 5;

    app.select_previous();
    assert_eq!(app.selected, 9);

    app.select_next();
    assert_eq!(app.selected, 0);
    assert_eq!(app.scroll_offset, 0);
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
fn empty_host_is_not_expandable() {
    let tree = HostTree {
        host: "empty".to_string(),
        panes: Vec::new(),
        error: None,
        connecting: false,
    };
    let mut app = test_app_with_tree_at_level(tree, ExpandLevel::Server);

    assert_eq!(app.rows.len(), 1);
    assert!(!app.rows[0].expandable);

    app.expand_selected();
    assert!(app.expanded.is_empty());

    app.toggle_selected();
    assert!(app.expanded.is_empty());
}

#[test]
fn refreshed_empty_node_loses_previous_expand_state() {
    let mut tree = test_host_tree("t2");
    let host_id = NodeId::Host("t2".to_string());
    let mut app = test_app_with_tree_at_level(tree.clone(), ExpandLevel::Pane);
    assert!(app.expanded.contains(&host_id));
    assert!(!app.default_expand_pending);

    tree.panes.clear();
    app.apply_scan_results(vec![tree.clone()]);
    assert!(!app.expanded.contains(&host_id));
    assert!(!app.rows[0].expandable);

    tree.panes.push(test_pane());
    app.apply_scan_results(vec![tree]);
    assert!(!app.expanded.contains(&host_id));
    assert!(app.rows[0].expandable);
    assert_eq!(app.rows.len(), 1);
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
fn pane_current_path_placeholder_is_available() {
    let tree = test_host_tree("t2");
    let mut formats = test_line_formats();
    formats.pane = "{pane_current_path}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let pane_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();
    assert_eq!(pane_row.label, "/tmp/project");
}

#[test]
fn app_starts_with_connecting_placeholders() {
    let app = test_app_with_hosts(vec!["t1".to_string(), "t2".to_string()]);

    assert_eq!(app.trees.len(), 2);
    assert!(app.trees.iter().all(|tree| tree.connecting));
    assert_eq!(host_detail(&app.trees[0]), "connecting ...");
}

#[test]
fn scan_results_replace_single_connecting_host() {
    let mut app = test_app_with_hosts(vec!["t1".to_string(), "t2".to_string()]);

    app.apply_scan_results(vec![test_host_tree("t2")]);

    assert!(
        app.trees
            .iter()
            .find(|tree| tree.host == "t1")
            .unwrap()
            .connecting
    );
    let t2 = app.trees.iter().find(|tree| tree.host == "t2").unwrap();
    assert!(!t2.connecting);
    assert_eq!(t2.panes.len(), 1);
}

#[test]
fn host_detail_is_empty_when_tmux_is_not_running() {
    let tree = HostTree {
        host: "t2".to_string(),
        panes: Vec::new(),
        error: None,
        connecting: false,
    };
    let rows = build_rows(&[tree.clone()], &BTreeSet::new(), &test_line_formats());

    assert_eq!(host_detail(&tree), "");
    assert_eq!(rows[0].detail, "");
    assert_eq!(rows[0].status, RowStatus::Normal);
}

#[test]
fn unavailable_host_row_is_marked_for_error_styling() {
    let tree = HostTree {
        host: "t2".to_string(),
        panes: Vec::new(),
        error: Some("ssh timeout".to_string()),
        connecting: false,
    };
    let rows = build_rows(&[tree], &BTreeSet::new(), &test_line_formats());

    assert_eq!(rows[0].status, RowStatus::Unavailable);
    assert!(rows[0].detail.contains("ssh timeout"));
}

#[test]
fn refresh_requests_can_target_all_or_one_host() {
    let mut app = test_app_with_hosts(vec!["t1".to_string(), "t2".to_string()]);

    app.request_refresh_all();
    assert_eq!(app.take_refresh_request(), Some(RefreshRequest::All));

    app.request_refresh_host("t2");
    assert_eq!(
        app.take_refresh_request(),
        Some(RefreshRequest::Hosts(vec!["t2".to_string()]))
    );
}

#[test]
fn starting_search_clears_previous_input() {
    let mut app = test_app_with_rows(3);
    app.search = "old".to_string();

    app.start_search(SearchDirection::Down);

    assert_eq!(app.search, "");
    assert_eq!(app.search_prompt(), "/");
}

#[test]
fn tmux_mutation_actions_report_their_host() {
    assert_eq!(
        PromptKind::RenameWindow {
            host: "t2".to_string(),
            target: "@1".to_string(),
        }
        .host(),
        "t2"
    );
    assert_eq!(
        ConfirmAction::KillPane {
            host: "t3".to_string(),
            pane: "%4".to_string(),
        }
        .host(),
        "t3"
    );
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
        log_path: None,
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
    let app = test_app_with_tree_at_level(tree.clone(), ExpandLevel::Server);
    assert_eq!(app.rows.len(), 1);
    assert!(matches!(app.rows[0].id, NodeId::Host(_)));

    let app = test_app_with_tree_at_level(tree.clone(), ExpandLevel::Window);
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

    let app = test_app_with_tree_at_level(tree, ExpandLevel::Pane);
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
            log_path: None,
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
        default_expand_pending: false,
        attach_request: None,
        refresh_request: None,
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
        log_path: None,
        line_formats: test_line_formats(),
    };
    let mut app = App {
        config,
        trees: vec![tree],
        expanded: BTreeSet::new(),
        rows: Vec::new(),
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
        default_expand_pending: true,
        attach_request: None,
        refresh_request: None,
    };
    app.apply_trees_after_refresh();
    app
}

fn test_app_with_hosts(hosts: Vec<String>) -> App {
    App::new(Config {
        hosts,
        connect_timeout_secs: 1,
        scan_concurrency: 1,
        mouse_scroll_lines: DEFAULT_MOUSE_SCROLL_LINES,
        auto_refresh_secs: DEFAULT_AUTO_REFRESH_SECS,
        default_expand_level: DEFAULT_EXPAND_LEVEL,
        log_path: None,
        line_formats: test_line_formats(),
    })
}

fn test_host_tree(host: &str) -> HostTree {
    HostTree {
        host: host.to_string(),
        panes: vec![test_pane()],
        error: None,
        connecting: false,
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
        pane_current_path: "/tmp/project".to_string(),
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
        expandable: false,
        status: RowStatus::Normal,
        busy_duration_secs: None,
    }
}
