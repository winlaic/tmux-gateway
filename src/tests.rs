use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{
    App, AttachDestination, ConfirmAction, ContextAction, ContextMenuItem, Mode, PageMode,
    PromptKind, RefreshRequest, SearchDirection, SplitChoice, attach_remote_command,
    parse_created_pane_target, parse_created_session_target, parse_created_window_target,
};
use crate::config::{
    Config, DEFAULT_ACTIVE_PANE_LINE_TEXT, DEFAULT_AUTO_REFRESH_SECS, DEFAULT_EXPAND_LEVEL,
    DEFAULT_MOUSE_SCROLL_LINES, DEFAULT_PANE_LINE_TEXT, DEFAULT_SERVER_LINE_TEXT,
    DEFAULT_SESSION_LINE_TEXT, DEFAULT_START_PAGE, DEFAULT_WINDOW_LINE_TEXT, ExpandLevel,
    LineFormats, RawConfig, StartPage, load_config, normalize_config, parse_ssh_config_hosts,
};
use crate::model::{
    GpuBadge, GpuInfo, HostTree, HostUpdate, NodeId, PaneInfo, ProcessInfo, RowStatus, VisibleRow,
};
use crate::remote::{
    mark_created_times_at, mark_pane_gpu_indices, pane_busy_duration, parse_gpu_processes,
    parse_gpu_snapshot, parse_gpus, parse_panes, parse_process_cwds, shell_quote,
};
use crate::tree::{
    SearchStart, build_active_pane_rows, build_rows, expanded_all, format_line, host_detail,
    search_rows,
};
use crate::ui::{
    confirm_area, confirm_choice_at_mouse, context_menu_area, split_choice_area,
    split_choice_at_mouse, test_render_row_line,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use toml::Value;

#[test]
fn shell_quote_handles_single_quote() {
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
}

#[test]
fn parse_panes_reads_tmux_format() {
    let output = "s\t$1\t1700000000\t0\t@2\tzsh\t1\t%3\t123\tvim\t/tmp/project\ttitle\t1\t0\n";
    let panes = parse_panes(output).unwrap();

    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].session_name, "s");
    assert_eq!(panes[0].session_id, "$1");
    assert_eq!(panes[0].session_created, Some(1_700_000_000));
    assert_eq!(panes[0].window_id, "@2");
    assert_eq!(panes[0].window_created, None);
    assert_eq!(panes[0].pane_id, "%3");
    assert_eq!(panes[0].pane_created, None);
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
        processes: Vec::new(),
        gpus: Vec::new(),
        gpu_processes: Vec::new(),
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
    app.apply_scan_results(vec![pane_update_from_tree(&tree)]);
    assert!(!app.expanded.contains(&host_id));
    assert!(!app.rows[0].expandable);

    tree.panes.push(test_pane());
    app.apply_scan_results(vec![pane_update_from_tree(&tree)]);
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

    let mut active_tree = test_host_tree("t2");
    active_tree.panes[0].busy_duration_secs = Some(42);
    let mut active_app = test_app_with_tree(active_tree);
    active_app
        .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()))
        .unwrap();
    active_app.selected = active_app
        .rows
        .iter()
        .position(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();
    assert_eq!(
        new_item_labels(&active_app),
        vec!["new session", "new window", "new pane"]
    );
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
fn keyboard_create_on_active_pane_opens_direct_split_menu() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut app = test_app_with_tree(tree);
    app.tree_area = Rect::new(0, 0, 80, 10);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()))
        .unwrap();
    app.selected = app
        .rows
        .iter()
        .position(|row| matches!(row.id, NodeId::Pane { .. }))
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
    assert_eq!(
        labels,
        vec![
            "new session(s)",
            "new window(w)",
            "vertical split(v)",
            "horizontal split(h)",
        ]
    );
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
fn active_pane_new_window_uses_owning_window_target() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut app = test_app_with_tree(tree);

    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()))
        .unwrap();
    app.selected = app
        .rows
        .iter()
        .position(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();

    app.start_create_window();

    let Mode::Prompt(prompt) = app.mode.clone() else {
        panic!("expected prompt");
    };
    let PromptKind::CreateWindow {
        host,
        target,
        after_window,
    } = prompt.kind
    else {
        panic!("expected create window prompt");
    };
    assert_eq!(host, "t2");
    assert_eq!(target, "$1");
    assert_eq!(after_window.as_deref(), Some("@2"));
}

#[test]
fn active_pane_page_filters_to_busy_panes_only() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut idle = test_pane();
    idle.pane_id = "%1".to_string();
    idle.pane_index = "1".to_string();
    tree.panes.push(idle);

    let rows = build_active_pane_rows(&[tree], &test_line_formats());

    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].id, NodeId::Pane { .. }));
    assert_eq!(rows[0].busy_duration_secs, Some(42));
}

#[test]
fn active_pane_page_uses_independent_line_template() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut formats = test_line_formats();
    formats.active_pane = "{server_name} {pane_id} {process_elapsed_time}".to_string();
    formats.pane = "{pane_commandline}".to_string();

    let rows = build_active_pane_rows(&[tree], &formats);

    assert_eq!(rows[0].label, "t2 %0 42s");
}

#[test]
fn active_pane_page_adds_structure_prefix_for_panes_in_same_window() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);

    let mut sibling = test_pane();
    sibling.pane_index = "1".to_string();
    sibling.pane_id = "%1".to_string();
    sibling.busy_duration_secs = Some(21);

    let mut other_window = test_pane();
    other_window.window_index = "1".to_string();
    other_window.window_id = "@3".to_string();
    other_window.pane_id = "%2".to_string();
    other_window.busy_duration_secs = Some(7);

    tree.panes.push(sibling);
    tree.panes.push(other_window);

    let rows = build_active_pane_rows(&[tree], &test_line_formats());

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].structure_prefix, "╭─");
    assert_eq!(rows[1].structure_prefix, "╰─");
    assert_eq!(rows[2].structure_prefix, "  ");
}

#[test]
fn active_pane_page_uses_top_middle_bottom_prefixes_for_three_panes() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);

    let mut middle = test_pane();
    middle.pane_index = "1".to_string();
    middle.pane_id = "%1".to_string();
    middle.busy_duration_secs = Some(21);

    let mut last = test_pane();
    last.pane_index = "2".to_string();
    last.pane_id = "%2".to_string();
    last.busy_duration_secs = Some(7);

    tree.panes.push(middle);
    tree.panes.push(last);

    let rows = build_active_pane_rows(&[tree], &test_line_formats());

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].structure_prefix, "╭─");
    assert_eq!(rows[1].structure_prefix, "├─");
    assert_eq!(rows[2].structure_prefix, "╰─");
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
    assert_eq!(
        target.destination,
        AttachDestination::Window {
            session: "main".to_string(),
            window: "0".to_string(),
        }
    );
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
    assert_eq!(
        target.destination,
        AttachDestination::Pane {
            session: "main".to_string(),
            window: "0".to_string(),
            pane: "%0".to_string(),
        }
    );
}

#[test]
fn window_attach_remote_command_targets_window_directly() {
    let command = attach_remote_command(&AttachDestination::Window {
        session: "main".to_string(),
        window: "2".to_string(),
    });

    assert_eq!(command, "tmux attach-session -t 'main:2'");
    assert!(!command.contains("switch-client"));
    assert!(!command.contains("new-session"));
}

#[test]
fn pane_attach_remote_command_targets_pane_directly() {
    let command = attach_remote_command(&AttachDestination::Pane {
        session: "main".to_string(),
        window: "2".to_string(),
        pane: "%9".to_string(),
    });

    assert_eq!(command, "tmux attach-session -t 'main:2.%9'");
    assert!(!command.contains("switch-client"));
    assert!(!command.contains("new-session"));
}

#[test]
fn parse_created_session_target_reads_tmux_output() {
    let target = parse_created_session_target("t2", "scratch\n").unwrap();

    assert_eq!(target.host, "t2");
    assert_eq!(
        target.destination,
        AttachDestination::Session {
            session: "scratch".to_string(),
        }
    );
}

#[test]
fn parse_created_window_target_reads_tmux_output() {
    let target = parse_created_window_target("t2", "main\t3\n").unwrap();

    assert_eq!(target.host, "t2");
    assert_eq!(
        target.destination,
        AttachDestination::Window {
            session: "main".to_string(),
            window: "3".to_string(),
        }
    );
}

#[test]
fn parse_created_pane_target_reads_tmux_output() {
    let target = parse_created_pane_target("t2", "main\t3\t%9\n").unwrap();

    assert_eq!(target.host, "t2");
    assert_eq!(
        target.destination,
        AttachDestination::Pane {
            session: "main".to_string(),
            window: "3".to_string(),
            pane: "%9".to_string(),
        }
    );
}

#[test]
fn switch_key_toggles_between_tree_and_active_pane_pages() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut app = test_app_with_tree(tree);

    assert_eq!(app.page_mode, PageMode::Tree);
    assert!(app.rows.iter().any(|row| matches!(row.id, NodeId::Host(_))));

    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()))
        .unwrap();

    assert_eq!(app.page_mode, PageMode::ActivePanes);
    assert_eq!(app.rows.len(), 1);
    assert!(matches!(app.rows[0].id, NodeId::Pane { .. }));

    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::empty()))
        .unwrap();

    assert_eq!(app.page_mode, PageMode::Tree);
    assert!(app.rows.iter().any(|row| matches!(row.id, NodeId::Host(_))));
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
fn created_time_marker_derives_pane_and_window_created_times() {
    let mut first = test_pane();
    first.pane_pid = 11;
    let mut second = test_pane();
    second.pane_id = "%1".to_string();
    second.pane_index = "1".to_string();
    second.pane_pid = 22;
    let mut panes = vec![first, second];
    let processes = vec![
        ProcessInfo {
            pid: 11,
            ppid: 1,
            elapsed_secs: 40,
            command: "zsh".to_string(),
            commandline: "zsh".to_string(),
        },
        ProcessInfo {
            pid: 22,
            ppid: 1,
            elapsed_secs: 90,
            command: "zsh".to_string(),
            commandline: "zsh".to_string(),
        },
    ];

    mark_created_times_at(&mut panes, &processes, 1_000);

    assert_eq!(panes[0].pane_created, Some(960));
    assert_eq!(panes[1].pane_created, Some(910));
    assert_eq!(panes[0].window_created, Some(910));
    assert_eq!(panes[1].window_created, Some(910));
}

#[test]
fn uptime_placeholders_use_human_duration_text() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut tree = test_host_tree("t2");
    tree.panes[0].session_created = Some(now - 93_600);
    tree.panes[0].window_created = Some(now - 7_380);
    tree.panes[0].pane_created = Some(now - 75);
    let mut formats = test_line_formats();
    formats.session = "{session_name} {session_uptime}".to_string();
    formats.window = "{window_index} {session_uptime} {window_uptime}".to_string();
    formats.pane = "{pane_id} {session_uptime} {window_uptime} {pane_uptime}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let session_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Session { .. }))
        .unwrap();
    let window_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Window { .. }))
        .unwrap();
    let pane_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();

    assert_eq!(session_row.label, "main 1d2h");
    assert_eq!(window_row.label, "0 1d2h 2h3m");
    assert_eq!(pane_row.label, "%0 1d2h 2h3m 1m15s");
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
fn pane_command_cwd_placeholder_is_available() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].pane_command_cwd = "/tmp/worker".to_string();
    let mut formats = test_line_formats();
    formats.pane = "{pane_command_cwd}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let pane_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();
    assert_eq!(pane_row.label, "/tmp/worker");
}

#[test]
fn collapse_user_shortens_home_prefixed_pane_values() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].pane_commandline = "/star-home/yelingxuan/code/train.py --epochs 1".to_string();
    tree.panes[0].pane_current_path = "/star-home/yelingxuan/code".to_string();
    tree.panes[0].pane_command_cwd = "/star-home/yelingxuan/code/run".to_string();
    let mut formats = test_line_formats();
    formats.pane = "{pane_commandline} {pane_current_path} {pane_command_cwd}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let pane_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();

    assert_eq!(
        pane_row.label,
        "~/code/train.py --epochs 1 ~/code ~/code/run"
    );
}

#[test]
fn parse_process_cwds_reads_batched_pwdx_output() {
    let output = "123: /tmp/project\n456: /star-home/yelingxuan/code run\n";

    let cwd_by_pid = parse_process_cwds(output);

    assert_eq!(
        cwd_by_pid.get(&123).map(String::as_str),
        Some("/tmp/project")
    );
    assert_eq!(
        cwd_by_pid.get(&456).map(String::as_str),
        Some("/star-home/yelingxuan/code run")
    );
}

#[test]
fn explicit_template_color_overrides_default_row_foreground_only_for_placeholder() {
    let tree = test_host_tree("t2");
    let mut formats = test_line_formats();
    formats.pane = "[Pane] {pane_id:red} {pane_current_command}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let pane_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();

    assert_eq!(pane_row.label, "[Pane] %0 pwsh");
    assert_eq!(
        pane_row
            .label_spans
            .iter()
            .find(|span| span.text == "%0")
            .and_then(|span| span.fg),
        Some(Color::Red)
    );
    assert!(
        pane_row
            .label_spans
            .iter()
            .find(|span| span.text.contains("pwsh"))
            .is_some_and(|span| span.fg.is_none())
    );
}

#[test]
fn explicit_hex_template_color_is_supported() {
    let tree = test_host_tree("t2");
    let mut formats = test_line_formats();
    formats.session = "{session_name:#112233}".to_string();

    let rows = build_rows(&[tree], &expanded_all(&[test_host_tree("t2")]), &formats);
    let session_row = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Session { .. }))
        .unwrap();

    assert_eq!(session_row.label, "main");
    assert_eq!(
        session_row.label_spans[0].fg,
        Some(Color::Rgb(0x11, 0x22, 0x33))
    );
}

#[test]
fn gpu_parsing_and_pane_matching_marks_only_owned_gpus() {
    let gpus = parse_gpus(
        "0, GPU-a, 1024, 8192\n\
         1, GPU-b, 7800, 8192\n",
    );
    let gpu_processes = parse_gpu_processes("GPU-a, 201, 512\nGPU-b, 999, 4096\n");
    let mut panes = vec![test_pane()];
    let processes = vec![
        ProcessInfo {
            pid: 123,
            ppid: 1,
            elapsed_secs: 10,
            command: "pwsh".to_string(),
            commandline: "pwsh".to_string(),
        },
        ProcessInfo {
            pid: 201,
            ppid: 123,
            elapsed_secs: 10,
            command: "python".to_string(),
            commandline: "python train.py".to_string(),
        },
    ];

    mark_pane_gpu_indices(&mut panes, &processes, &gpus, &gpu_processes);

    assert_eq!(panes[0].gpu_indices, vec![0]);
    assert_eq!(panes[0].gpu_memory_by_index, vec![(0, 512)]);
}

#[test]
fn parse_gpu_snapshot_splits_gpu_and_process_sections() {
    let output = "__TMUX_GATEWAY_GPUS__\n\
                  0, GPU-a, 1024, 8192\n\
                  __TMUX_GATEWAY_GPU_PROCESSES__\n\
                  GPU-a, 201, 512\n";

    let (gpus, gpu_processes) = parse_gpu_snapshot(output).unwrap();

    assert_eq!(gpus.len(), 1);
    assert_eq!(gpus[0].uuid, "GPU-a");
    assert_eq!(gpu_processes.len(), 1);
    assert_eq!(gpu_processes[0].pid, 201);
    assert_eq!(gpu_processes[0].used_memory_mib, 512);
}

#[test]
fn gpu_badges_roll_up_from_pane_to_window_session_and_server() {
    let mut tree = test_host_tree("t2");
    tree.gpus = vec![
        GpuInfo {
            index: 0,
            uuid: "GPU-a".to_string(),
            memory_used_mib: 1024,
            memory_total_mib: 8192,
        },
        GpuInfo {
            index: 1,
            uuid: "GPU-b".to_string(),
            memory_used_mib: 7800,
            memory_total_mib: 8192,
        },
    ];
    tree.panes[0].gpu_indices = vec![1];
    tree.panes[0].gpu_memory_by_index = vec![(1, 2048)];
    let mut expanded = BTreeSet::new();
    expanded.insert(NodeId::Host("t2".to_string()));
    expanded.insert(NodeId::Session {
        host: "t2".to_string(),
        session: "main".to_string(),
    });
    let rows = build_rows(&[tree.clone()], &expanded, &test_line_formats());

    let host = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Host(_)))
        .unwrap();
    assert_eq!(
        host.gpu_badges,
        vec![
            GpuBadge::Memory {
                digit: '1',
                level: 0,
                active: false,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: 'A',
                level: 3,
                active: true,
                placeholder: false,
            },
        ]
    );

    let window = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Window { .. }))
        .unwrap();
    let session = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Session { .. }))
        .unwrap();
    assert!(session.gpu_badges.is_empty());
    assert_eq!(
        window.gpu_badges,
        vec![
            GpuBadge::Memory {
                digit: ' ',
                level: 0,
                active: false,
                placeholder: true,
            },
            GpuBadge::Memory {
                digit: '3',
                level: 1,
                active: true,
                placeholder: false,
            },
        ]
    );

    expanded.insert(NodeId::Window {
        host: "t2".to_string(),
        session: "main".to_string(),
        window: "0".to_string(),
    });
    let rows = build_rows(&[tree], &expanded, &test_line_formats());
    let window = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Window { .. }))
        .unwrap();
    let pane = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();
    assert!(window.gpu_badges.is_empty());
    assert_eq!(
        pane.gpu_badges,
        vec![
            GpuBadge::Memory {
                digit: ' ',
                level: 0,
                active: false,
                placeholder: true,
            },
            GpuBadge::Memory {
                digit: '3',
                level: 1,
                active: true,
                placeholder: false,
            },
        ]
    );
}

#[test]
fn child_gpu_badges_are_hidden_without_gpu_processes() {
    let mut tree = test_host_tree("t2");
    tree.gpus = vec![GpuInfo {
        index: 0,
        uuid: "GPU-a".to_string(),
        memory_used_mib: 4000,
        memory_total_mib: 8000,
    }];
    let rows = build_rows(
        &[tree.clone()],
        &expanded_all(&[tree]),
        &test_line_formats(),
    );

    assert!(
        rows.iter()
            .find(|row| matches!(row.id, NodeId::Host(_)))
            .unwrap()
            .gpu_badges
            .len()
            == 1
    );
    assert!(
        rows.iter()
            .filter(|row| !matches!(row.id, NodeId::Host(_)))
            .all(|row| row.gpu_badges.is_empty())
    );
}

#[test]
fn child_gpu_badges_sum_memory_from_child_panes() {
    let mut tree = test_host_tree("t2");
    tree.gpus = vec![GpuInfo {
        index: 0,
        uuid: "GPU-a".to_string(),
        memory_used_mib: 4096,
        memory_total_mib: 8192,
    }];
    tree.panes[0].gpu_indices = vec![0];
    tree.panes[0].gpu_memory_by_index = vec![(0, 1024)];
    let mut second = test_pane();
    second.pane_id = "%1".to_string();
    second.pane_index = "1".to_string();
    second.gpu_indices = vec![0];
    second.gpu_memory_by_index = vec![(0, 2048)];
    tree.panes.push(second);

    let mut expanded = BTreeSet::new();
    expanded.insert(NodeId::Host("t2".to_string()));
    expanded.insert(NodeId::Session {
        host: "t2".to_string(),
        session: "main".to_string(),
    });
    let rows = build_rows(&[tree], &expanded, &test_line_formats());
    let window = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Window { .. }))
        .unwrap();

    assert_eq!(
        window.gpu_badges,
        vec![GpuBadge::Memory {
            digit: '4',
            level: 1,
            active: true,
            placeholder: false,
        }]
    );
}

#[test]
fn child_gpu_badges_keep_gpu_index_placeholders() {
    let mut tree = test_host_tree("t2");
    tree.gpus = (0..4)
        .map(|index| GpuInfo {
            index,
            uuid: format!("GPU-{index}"),
            memory_used_mib: 0,
            memory_total_mib: 1000,
        })
        .collect();
    tree.panes[0].gpu_indices = vec![0, 2];
    tree.panes[0].gpu_memory_by_index = vec![(0, 100), (2, 300)];

    let mut expanded = BTreeSet::new();
    expanded.insert(NodeId::Host("t2".to_string()));
    expanded.insert(NodeId::Session {
        host: "t2".to_string(),
        session: "main".to_string(),
    });
    let rows = build_rows(&[tree], &expanded, &test_line_formats());
    let window = rows
        .iter()
        .find(|row| matches!(row.id, NodeId::Window { .. }))
        .unwrap();

    assert_eq!(
        window.gpu_badges,
        vec![
            GpuBadge::Memory {
                digit: '1',
                level: 0,
                active: true,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: ' ',
                level: 0,
                active: false,
                placeholder: true,
            },
            GpuBadge::Memory {
                digit: '3',
                level: 1,
                active: true,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: ' ',
                level: 0,
                active: false,
                placeholder: true,
            },
        ]
    );
}

#[test]
fn gpu_memory_badges_round_to_nearest_decile() {
    let mut tree = test_host_tree("t2");
    tree.gpus = vec![
        GpuInfo {
            index: 0,
            uuid: "GPU-0".to_string(),
            memory_used_mib: 49,
            memory_total_mib: 1000,
        },
        GpuInfo {
            index: 1,
            uuid: "GPU-1".to_string(),
            memory_used_mib: 50,
            memory_total_mib: 1000,
        },
        GpuInfo {
            index: 2,
            uuid: "GPU-2".to_string(),
            memory_used_mib: 949,
            memory_total_mib: 1000,
        },
        GpuInfo {
            index: 3,
            uuid: "GPU-3".to_string(),
            memory_used_mib: 950,
            memory_total_mib: 1000,
        },
    ];
    let rows = build_rows(&[tree], &BTreeSet::new(), &test_line_formats());

    assert_eq!(
        rows[0].gpu_badges,
        vec![
            GpuBadge::Memory {
                digit: '0',
                level: 0,
                active: false,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: '1',
                level: 0,
                active: false,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: '9',
                level: 3,
                active: false,
                placeholder: false,
            },
            GpuBadge::Memory {
                digit: 'A',
                level: 3,
                active: false,
                placeholder: false,
            },
        ]
    );
}

#[test]
fn active_pane_gpu_badges_show_total_heat_and_pane_usage_digit() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    tree.gpus = vec![
        GpuInfo {
            index: 0,
            uuid: "GPU-a".to_string(),
            memory_used_mib: 4096,
            memory_total_mib: 8192,
        },
        GpuInfo {
            index: 1,
            uuid: "GPU-b".to_string(),
            memory_used_mib: 7680,
            memory_total_mib: 8192,
        },
    ];
    tree.panes[0].gpu_memory_by_index = vec![(1, 2048)];

    let rows = build_active_pane_rows(&[tree], &test_line_formats());

    assert_eq!(
        rows[0].gpu_badges,
        vec![
            GpuBadge::ActivePaneMemory {
                digit: '5',
                level: 1,
                pane_active: false,
            },
            GpuBadge::ActivePaneMemory {
                digit: '3',
                level: 3,
                pane_active: true,
            },
        ]
    );
    assert_eq!(rows[0].gpu_badges.len(), 2);
}

#[test]
fn active_pane_gpu_badges_render_explicit_blue_pane_usage_cell() {
    let mut row = test_row("python train.py");
    row.id = NodeId::Pane {
        host: "t2".to_string(),
        session: "main".to_string(),
        window: "0".to_string(),
        pane: "%0".to_string(),
    };
    row.gpu_badges = vec![GpuBadge::ActivePaneMemory {
        digit: '3',
        level: 3,
        pane_active: true,
    }];

    let line = test_render_row_line(&row, 20);
    let pane_span = line.spans.last().unwrap();

    assert_eq!(pane_span.content.as_ref(), "3");
    assert_eq!(pane_span.style.fg, Some(Color::White));
    assert_eq!(pane_span.style.bg, Some(Color::Blue));
}

#[test]
fn active_pane_gpu_badges_keep_one_cell_per_gpu() {
    let mut row = test_row("python train.py");
    row.id = NodeId::Pane {
        host: "t2".to_string(),
        session: "main".to_string(),
        window: "0".to_string(),
        pane: "%0".to_string(),
    };
    row.gpu_badges = vec![
        GpuBadge::ActivePaneMemory {
            digit: '5',
            level: 1,
            pane_active: false,
        },
        GpuBadge::ActivePaneMemory {
            digit: '3',
            level: 3,
            pane_active: true,
        },
    ];

    let line = test_render_row_line(&row, 20);

    assert_eq!(line.spans[line.spans.len() - 2].content.as_ref(), "5");
    assert_eq!(line.spans[line.spans.len() - 1].content.as_ref(), "3");
}

#[test]
fn active_pane_structure_prefix_renders_before_label() {
    let mut row = test_row("python train.py");
    row.id = NodeId::Pane {
        host: "t2".to_string(),
        session: "main".to_string(),
        window: "0".to_string(),
        pane: "%0".to_string(),
    };
    row.structure_prefix = "╰─".to_string();

    let line = test_render_row_line(&row, 32);
    let rendered: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert!(
        rendered.contains("╰─python train.py"),
        "rendered={rendered:?}"
    );
}

#[test]
fn tree_pane_rows_keep_marker_slot_spacing_for_alignment() {
    let mut row = test_row("python train.py");
    row.id = NodeId::Pane {
        host: "t2".to_string(),
        session: "main".to_string(),
        window: "0".to_string(),
        pane: "%0".to_string(),
    };

    let line = test_render_row_line(&row, 32);
    let rendered: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert!(
        rendered.contains("    python train.py"),
        "rendered={rendered:?}"
    );
}

#[test]
fn gpu_badges_keep_right_edge_when_row_text_is_long() {
    let mut row = test_row("very-long-command-line-that-would-overflow-the-row");
    row.gpu_badges = vec![
        GpuBadge::Memory {
            digit: '0',
            level: 0,
            active: true,
            placeholder: false,
        },
        GpuBadge::Memory {
            digit: '1',
            level: 0,
            active: true,
            placeholder: false,
        },
    ];

    let line = test_render_row_line(&row, 20);

    assert_eq!(line_width(&line), 20);
    assert_eq!(line.spans[line.spans.len() - 2].content.as_ref(), "0");
    assert_eq!(line.spans[line.spans.len() - 1].content.as_ref(), "1");
}

#[test]
fn rows_without_gpu_badges_keep_original_untrimmed_text() {
    let row = test_row("very-long-command-line-that-would-overflow-the-row");

    let line = test_render_row_line(&row, 20);
    let rendered = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

    assert!(rendered.contains("very-long-command-line-that-would-overflow-the-row"));
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

    app.apply_scan_results(vec![pane_update_from_tree(&test_host_tree("t2"))]);

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
fn scan_task_accepts_pane_and_gpu_updates_independently() {
    let mut app = test_app_with_hosts(vec!["t2".to_string()]);

    app.apply_scan_results(vec![pane_update_from_tree(&test_host_tree("t2"))]);
    let t2 = app.trees.iter().find(|tree| tree.host == "t2").unwrap();
    assert_eq!(t2.panes.len(), 1);
    assert!(t2.gpus.is_empty());

    app.apply_scan_results(vec![HostUpdate::Gpus {
        host: "t2".to_string(),
        gpus: vec![GpuInfo {
            index: 0,
            uuid: "GPU-a".to_string(),
            memory_used_mib: 50,
            memory_total_mib: 100,
        }],
        gpu_processes: Vec::new(),
    }]);
    let t2 = app.trees.iter().find(|tree| tree.host == "t2").unwrap();
    assert_eq!(t2.panes.len(), 1);
    assert_eq!(t2.gpus.len(), 1);
}

#[test]
fn host_detail_is_empty_when_tmux_is_not_running() {
    let tree = HostTree {
        host: "t2".to_string(),
        panes: Vec::new(),
        processes: Vec::new(),
        gpus: Vec::new(),
        gpu_processes: Vec::new(),
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
        processes: Vec::new(),
        gpus: Vec::new(),
        gpu_processes: Vec::new(),
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
fn scan_refresh_preserves_current_selection_instead_of_reapplying_search() {
    let mut app = test_app_with_tree(test_host_tree("needle-host"));
    let pane_index = app
        .rows
        .iter()
        .position(|row| matches!(row.id, NodeId::Pane { .. }))
        .unwrap();
    app.selected = pane_index;
    app.search = "needle".to_string();
    let selected_id = app.rows[app.selected].id.clone();

    app.apply_scan_results(vec![HostUpdate::Gpus {
        host: "needle-host".to_string(),
        gpus: Vec::new(),
        gpu_processes: Vec::new(),
    }]);

    assert_eq!(app.rows[app.selected].id, selected_id);
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
        start_page: None,
        collapse_user: None,
        log_path: None,
        server_line_text: None,
        session_line_text: None,
        window_line_text: None,
        pane_line_text: None,
        active_pane_line_text: None,
    };

    let config = normalize_config(raw).unwrap();
    assert_eq!(config.auto_refresh_secs, DEFAULT_AUTO_REFRESH_SECS);
    assert_eq!(config.start_page, DEFAULT_START_PAGE);
    assert!(config.collapse_user);
}

#[test]
fn start_page_can_default_to_active_view() {
    let raw = RawConfig {
        hosts: Some(Value::Array(vec![Value::String("t2".to_string())])),
        connect_timeout_secs: None,
        scan_concurrency: None,
        mouse_scroll_lines: None,
        auto_refresh_secs: None,
        default_expand_level: None,
        start_page: Some("active".to_string()),
        collapse_user: None,
        log_path: None,
        server_line_text: None,
        session_line_text: None,
        window_line_text: None,
        pane_line_text: None,
        active_pane_line_text: None,
    };

    let config = normalize_config(raw).unwrap();
    assert_eq!(config.start_page, StartPage::Active);
}

#[test]
fn app_can_start_on_active_page_from_config() {
    let mut tree = test_host_tree("t2");
    tree.panes[0].busy_duration_secs = Some(42);
    let mut app = App::new(Config {
        hosts: vec!["t2".to_string()],
        connect_timeout_secs: 1,
        scan_concurrency: 1,
        mouse_scroll_lines: DEFAULT_MOUSE_SCROLL_LINES,
        auto_refresh_secs: DEFAULT_AUTO_REFRESH_SECS,
        default_expand_level: DEFAULT_EXPAND_LEVEL,
        start_page: StartPage::Active,
        collapse_user: true,
        log_path: None,
        line_formats: test_line_formats(),
    });

    app.apply_scan_results(vec![pane_update_from_tree(&tree)]);

    assert_eq!(app.page_mode, PageMode::ActivePanes);
    assert_eq!(app.rows.len(), 1);
    assert!(matches!(app.rows[0].id, NodeId::Pane { .. }));
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
            start_page: StartPage::Tree,
            collapse_user: true,
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
        page_mode: PageMode::Tree,
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
        start_page: StartPage::Tree,
        collapse_user: true,
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
        page_mode: PageMode::Tree,
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
        start_page: StartPage::Tree,
        collapse_user: true,
        log_path: None,
        line_formats: test_line_formats(),
    })
}

fn test_host_tree(host: &str) -> HostTree {
    HostTree {
        host: host.to_string(),
        panes: vec![test_pane()],
        processes: Vec::new(),
        gpus: Vec::new(),
        gpu_processes: Vec::new(),
        error: None,
        connecting: false,
    }
}

fn pane_update_from_tree(tree: &HostTree) -> HostUpdate {
    HostUpdate::Panes {
        host: tree.host.clone(),
        panes: tree.panes.clone(),
        processes: tree.processes.clone(),
        error: tree.error.clone(),
    }
}

fn test_line_formats() -> LineFormats {
    LineFormats {
        server: DEFAULT_SERVER_LINE_TEXT.to_string(),
        session: DEFAULT_SESSION_LINE_TEXT.to_string(),
        window: DEFAULT_WINDOW_LINE_TEXT.to_string(),
        pane: DEFAULT_PANE_LINE_TEXT.to_string(),
        active_pane: DEFAULT_ACTIVE_PANE_LINE_TEXT.to_string(),
        collapse_user: true,
        user_home: Some("/star-home/yelingxuan".to_string()),
    }
}

fn test_pane() -> PaneInfo {
    PaneInfo {
        session_name: "main".to_string(),
        session_id: "$1".to_string(),
        session_created: None,
        window_index: "0".to_string(),
        window_id: "@2".to_string(),
        window_created: None,
        window_name: "pwsh".to_string(),
        pane_index: "0".to_string(),
        pane_id: "%0".to_string(),
        pane_created: None,
        pane_pid: 123,
        pane_current_command: "pwsh".to_string(),
        pane_commandline: "pwsh -NoLogo".to_string(),
        pane_current_path: "/tmp/project".to_string(),
        pane_command_cwd: "/tmp/project".to_string(),
        pane_title: String::new(),
        active_window: true,
        active_pane: true,
        busy_duration_secs: None,
        gpu_indices: Vec::new(),
        gpu_memory_by_index: Vec::new(),
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
        structure_prefix: String::new(),
        label: search_text.to_string(),
        label_spans: vec![crate::model::RowLabelSpan {
            text: search_text.to_string(),
            fg: None,
        }],
        detail: String::new(),
        search_text: search_text.to_string(),
        selectable: false,
        expandable: false,
        status: RowStatus::Normal,
        busy_duration_secs: None,
        gpu_badges: Vec::new(),
    }
}

fn line_width(line: &ratatui::text::Line<'_>) -> u16 {
    line.spans
        .iter()
        .map(|span| span.content.chars().count() as u16)
        .sum()
}
