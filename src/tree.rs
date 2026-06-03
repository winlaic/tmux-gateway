use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use crate::app::SearchDirection;
use crate::config::LineFormats;
use crate::model::{GpuBadge, HostTree, NodeId, PaneInfo, RowStatus, VisibleRow};

pub(crate) fn build_rows(
    trees: &[HostTree],
    expanded: &BTreeSet<NodeId>,
    line_formats: &LineFormats,
) -> Vec<VisibleRow> {
    let mut rows = Vec::new();

    for tree in trees {
        let host_id = NodeId::Host(tree.host.clone());
        let detail = host_detail(tree);
        rows.push(VisibleRow {
            id: host_id.clone(),
            depth: 0,
            label: format_server_line(tree, line_formats),
            detail: detail.clone(),
            search_text: format!("{} {}", tree.host, detail),
            selectable: false,
            expandable: !tree.panes.is_empty(),
            status: if tree.error.is_some() {
                RowStatus::Unavailable
            } else {
                RowStatus::Normal
            },
            busy_duration_secs: (!expanded.contains(&host_id))
                .then(|| max_busy_duration(tree.panes.iter()))
                .flatten(),
            gpu_badges: server_gpu_badges(tree),
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
                expandable: !windows.is_empty(),
                status: RowStatus::Normal,
                busy_duration_secs: (!expanded.contains(&session_id))
                    .then(|| max_busy_duration(windows.values().flatten().copied()))
                    .flatten(),
                gpu_badges: process_gpu_badges(&tree.gpus, windows.values().flatten().copied()),
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
                    expandable: !panes.is_empty(),
                    status: RowStatus::Normal,
                    busy_duration_secs: (!expanded.contains(&window_id))
                        .then(|| max_busy_duration(panes.iter().copied()))
                        .flatten(),
                    gpu_badges: process_gpu_badges(&tree.gpus, panes.iter().copied()),
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
                        expandable: false,
                        status: RowStatus::Normal,
                        busy_duration_secs: pane.busy_duration_secs,
                        gpu_badges: process_gpu_badges(&tree.gpus, std::iter::once(pane)),
                    });
                }
            }
        }
    }

    rows
}

pub(crate) fn expandable_node_ids(trees: &[HostTree]) -> BTreeSet<NodeId> {
    let mut ids = BTreeSet::new();

    for tree in trees {
        if tree.panes.is_empty() {
            continue;
        }

        ids.insert(NodeId::Host(tree.host.clone()));
        for (session_name, windows) in group_tree(tree) {
            if windows.is_empty() {
                continue;
            }

            ids.insert(NodeId::Session {
                host: tree.host.clone(),
                session: session_name.clone(),
            });

            for (window_index, panes) in windows {
                if panes.is_empty() {
                    continue;
                }
                ids.insert(NodeId::Window {
                    host: tree.host.clone(),
                    session: session_name.clone(),
                    window: window_index,
                });
            }
        }
    }

    ids
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
    values.insert("pane_current_path", pane.pane_current_path.clone());
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

pub(crate) fn format_line(template: &str, values: &BTreeMap<&'static str, String>) -> String {
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

pub(crate) fn group_tree(tree: &HostTree) -> BTreeMap<String, BTreeMap<String, Vec<&PaneInfo>>> {
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

pub(crate) fn max_busy_duration<'a>(panes: impl Iterator<Item = &'a PaneInfo>) -> Option<u64> {
    panes.filter_map(|pane| pane.busy_duration_secs).max()
}

fn server_gpu_badges(tree: &HostTree) -> Vec<GpuBadge> {
    let active_indices = active_gpu_indices(tree.panes.iter());
    tree.gpus
        .iter()
        .map(|gpu| {
            let decile = rounded_memory_decile(gpu.memory_used_mib, gpu.memory_total_mib);
            GpuBadge::Memory {
                digit: decile_digit(decile as u8),
                level: decile_level(decile as u8),
                active: active_indices.contains(&gpu.index),
            }
        })
        .collect()
}

fn process_gpu_badges<'a>(
    gpus: &[crate::model::GpuInfo],
    panes: impl Iterator<Item = &'a PaneInfo>,
) -> Vec<GpuBadge> {
    let mut memory_by_index: BTreeMap<usize, u64> = BTreeMap::new();
    for pane in panes {
        for (index, memory_used_mib) in &pane.gpu_memory_by_index {
            *memory_by_index.entry(*index).or_default() += *memory_used_mib;
        }
    }
    if memory_by_index.is_empty() {
        return Vec::new();
    }

    let total_by_index: BTreeMap<usize, u64> = gpus
        .iter()
        .map(|gpu| (gpu.index, gpu.memory_total_mib))
        .collect();
    memory_by_index
        .into_iter()
        .filter_map(|(index, memory_used_mib)| {
            let decile = rounded_memory_decile(memory_used_mib, *total_by_index.get(&index)?);
            Some(GpuBadge::Memory {
                digit: decile_digit(decile),
                level: decile_level(decile),
                active: true,
            })
        })
        .collect()
}

fn active_gpu_indices<'a>(panes: impl Iterator<Item = &'a PaneInfo>) -> BTreeSet<usize> {
    panes
        .flat_map(|pane| pane.gpu_indices.iter().copied())
        .collect()
}

fn rounded_memory_decile(memory_used_mib: u64, memory_total_mib: u64) -> u8 {
    if memory_total_mib == 0 {
        return 0;
    }

    ((memory_used_mib.saturating_mul(100) / memory_total_mib + 5) / 10).min(10) as u8
}

fn decile_digit(decile: u8) -> char {
    match decile.min(10) {
        10 => 'A',
        value => char::from(b'0' + value),
    }
}

fn decile_level(decile: u8) -> u8 {
    match decile.min(10) {
        0..=2 => 0,
        3..=5 => 1,
        6..=8 => 2,
        _ => 3,
    }
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

pub(crate) fn search_rows(
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
pub(crate) enum SearchStart {
    Current,
    Next,
}

pub(crate) fn host_detail(tree: &HostTree) -> String {
    if tree.connecting {
        return "connecting ...".to_string();
    }
    if let Some(error) = &tree.error {
        return format!("unavailable: {error}");
    }
    if tree.panes.is_empty() {
        return String::new();
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

pub(crate) fn parent_id(id: &NodeId) -> Option<NodeId> {
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

pub(crate) fn print_tree(trees: &[HostTree], line_formats: &LineFormats) {
    let mut stdout = io::stdout().lock();
    for row in build_rows(trees, &expanded_all(trees), line_formats) {
        let detail = row.detail;
        if writeln!(stdout, "{}{} {}", "  ".repeat(row.depth), row.label, detail).is_err() {
            return;
        }
    }
}

pub(crate) fn expanded_all(trees: &[HostTree]) -> BTreeSet<NodeId> {
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
