use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::style::Color;

use crate::app::SearchDirection;
use crate::config::LineFormats;
use crate::model::{GpuBadge, HostTree, NodeId, PaneInfo, RowLabelSpan, RowStatus, VisibleRow};

#[derive(Clone, Debug)]
struct RenderedLine {
    plain: String,
    spans: Vec<RowLabelSpan>,
}

pub(crate) fn build_rows(
    trees: &[HostTree],
    expanded: &BTreeSet<NodeId>,
    line_formats: &LineFormats,
) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    let now_epoch = current_unix_epoch();

    for tree in trees {
        let host_id = NodeId::Host(tree.host.clone());
        let detail = host_detail(tree);
        let label = format_server_line(tree, line_formats);
        rows.push(VisibleRow {
            id: host_id.clone(),
            depth: 0,
            structure_prefix: String::new(),
            label: label.plain,
            label_spans: label.spans,
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
            let label =
                format_session_line(tree, &session_name, windows.len(), line_formats, now_epoch);
            rows.push(VisibleRow {
                id: session_id.clone(),
                depth: 1,
                structure_prefix: String::new(),
                label: label.plain,
                label_spans: label.spans,
                detail: format!("{} windows", windows.len()),
                search_text: session_name.clone(),
                selectable: false,
                expandable: !windows.is_empty(),
                status: RowStatus::Normal,
                busy_duration_secs: (!expanded.contains(&session_id))
                    .then(|| max_busy_duration(windows.values().flatten().copied()))
                    .flatten(),
                gpu_badges: (!expanded.contains(&session_id))
                    .then(|| process_gpu_badges(&tree.gpus, windows.values().flatten().copied()))
                    .unwrap_or_default(),
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
                let label = format_window_line(
                    &tree.host,
                    &session_name,
                    &window_index,
                    &panes,
                    line_formats,
                    now_epoch,
                );
                rows.push(VisibleRow {
                    id: window_id.clone(),
                    depth: 2,
                    structure_prefix: String::new(),
                    label: label.plain,
                    label_spans: label.spans,
                    detail: format!("{} panes", panes.len()),
                    search_text: format!("{} {}", window_index, first.window_name),
                    selectable: true,
                    expandable: !panes.is_empty(),
                    status: RowStatus::Normal,
                    busy_duration_secs: (!expanded.contains(&window_id))
                        .then(|| max_busy_duration(panes.iter().copied()))
                        .flatten(),
                    gpu_badges: (!expanded.contains(&window_id))
                        .then(|| process_gpu_badges(&tree.gpus, panes.iter().copied()))
                        .unwrap_or_default(),
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
                    let label = format_pane_line(
                        &tree.host,
                        &session_name,
                        &window_index,
                        pane,
                        &line_formats.pane,
                        now_epoch,
                        line_formats,
                    );
                    rows.push(VisibleRow {
                        id: pane_id,
                        depth: 3,
                        structure_prefix: String::new(),
                        label: label.plain,
                        label_spans: label.spans,
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

pub(crate) fn build_pane_rows(
    trees: &[HostTree],
    line_formats: &LineFormats,
    hide_idle_panes: bool,
) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    let now_epoch = current_unix_epoch();

    for tree in trees {
        for (session_name, windows) in group_panes_for_page(tree, hide_idle_panes) {
            for (window_index, panes) in windows {
                let pane_count = panes.len();
                for (pane_position, pane) in panes.into_iter().enumerate() {
                    let label = format_pane_line(
                        &tree.host,
                        &session_name,
                        &window_index,
                        pane,
                        &line_formats.active_pane,
                        now_epoch,
                        line_formats,
                    );
                    rows.push(VisibleRow {
                        id: NodeId::Pane {
                            host: tree.host.clone(),
                            session: session_name.clone(),
                            window: window_index.clone(),
                            pane: pane.pane_id.clone(),
                        },
                        depth: 0,
                        structure_prefix: pane_page_structure_prefix(pane_position, pane_count),
                        label: label.plain,
                        label_spans: label.spans,
                        detail: String::new(),
                        search_text: format!(
                            "{} {} {} {} {} {} {}",
                            tree.host,
                            pane.session_name,
                            pane.window_index,
                            pane.pane_index,
                            pane.pane_id,
                            pane.pane_current_command,
                            pane.pane_commandline
                        ),
                        selectable: true,
                        expandable: false,
                        status: RowStatus::Normal,
                        busy_duration_secs: pane.busy_duration_secs,
                        gpu_badges: active_pane_gpu_badges(&tree.gpus, pane),
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

fn format_server_line(tree: &HostTree, line_formats: &LineFormats) -> RenderedLine {
    let sessions = group_tree(tree);
    let window_count: usize = sessions.values().map(BTreeMap::len).sum();
    let pane_count = tree.panes.len();
    let busy_duration = max_busy_duration(tree.panes.iter());
    let mut values = BTreeMap::new();
    insert_template_value(&mut values, "server_name", tree.host.clone(), line_formats);
    insert_template_value(&mut values, "host", tree.host.clone(), line_formats);
    insert_template_value(
        &mut values,
        "session_count",
        sessions.len().to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_count",
        window_count.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_count",
        pane_count.to_string(),
        line_formats,
    );
    insert_process_values(&mut values, busy_duration, line_formats);
    render_line(&line_formats.server, &values)
}

fn format_session_line(
    tree: &HostTree,
    session_name: &str,
    window_count: usize,
    line_formats: &LineFormats,
    now_epoch: u64,
) -> RenderedLine {
    let panes: Vec<&PaneInfo> = tree
        .panes
        .iter()
        .filter(|pane| pane.session_name == session_name)
        .collect();
    let busy_duration = max_busy_duration(panes.iter().copied());
    let mut values = BTreeMap::new();
    insert_template_value(&mut values, "server_name", tree.host.clone(), line_formats);
    insert_template_value(&mut values, "host", tree.host.clone(), line_formats);
    insert_template_value(
        &mut values,
        "session_name",
        session_name.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "session_uptime",
        uptime_value(
            min_created_epoch(panes.iter().map(|pane| pane.session_created)),
            now_epoch,
        ),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_count",
        window_count.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_count",
        panes.len().to_string(),
        line_formats,
    );
    insert_process_values(&mut values, busy_duration, line_formats);
    render_line(&line_formats.session, &values)
}

fn format_window_line(
    host: &str,
    session_name: &str,
    window_index: &str,
    panes: &[&PaneInfo],
    line_formats: &LineFormats,
    now_epoch: u64,
) -> RenderedLine {
    let first = panes[0];
    let busy_duration = max_busy_duration(panes.iter().copied());
    let mut values = BTreeMap::new();
    insert_template_value(&mut values, "server_name", host.to_string(), line_formats);
    insert_template_value(&mut values, "host", host.to_string(), line_formats);
    insert_template_value(
        &mut values,
        "session_name",
        session_name.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "session_uptime",
        uptime_value(
            min_created_epoch(panes.iter().map(|pane| pane.session_created)),
            now_epoch,
        ),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_index",
        window_index.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_uptime",
        uptime_value(
            min_created_epoch(panes.iter().map(|pane| pane.window_created)),
            now_epoch,
        ),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_name",
        first.window_name.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_panes",
        panes.len().to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "is_active",
        if first.active_window { "*" } else { " " }.to_string(),
        line_formats,
    );
    insert_process_values(&mut values, busy_duration, line_formats);
    render_line(&line_formats.window, &values)
}

fn format_pane_line(
    host: &str,
    session_name: &str,
    window_index: &str,
    pane: &PaneInfo,
    template: &str,
    now_epoch: u64,
    line_formats: &LineFormats,
) -> RenderedLine {
    let mut values = BTreeMap::new();
    insert_template_value(&mut values, "server_name", host.to_string(), line_formats);
    insert_template_value(&mut values, "host", host.to_string(), line_formats);
    insert_template_value(
        &mut values,
        "session_name",
        session_name.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "session_uptime",
        uptime_value(pane.session_created, now_epoch),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_index",
        window_index.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_uptime",
        uptime_value(pane.window_created, now_epoch),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "window_name",
        pane.window_name.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_index",
        pane.pane_index.clone(),
        line_formats,
    );
    insert_template_value(&mut values, "pane_id", pane.pane_id.clone(), line_formats);
    insert_template_value(
        &mut values,
        "pane_uptime",
        uptime_value(pane.pane_created, now_epoch),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_pid",
        pane.pane_pid.to_string(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_current_command",
        pane.pane_current_command.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_command",
        pane.pane_current_command.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_commandline",
        pane.pane_commandline.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_current_path",
        pane.pane_current_path.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_command_cwd",
        pane.pane_command_cwd.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_title",
        pane.pane_title.clone(),
        line_formats,
    );
    insert_template_value(
        &mut values,
        "pane_title_prefix",
        if pane.pane_title.is_empty() {
            String::new()
        } else {
            format!(" - {}", pane.pane_title)
        },
        line_formats,
    );
    insert_template_value(
        &mut values,
        "is_active",
        if pane.active_pane { "*" } else { " " }.to_string(),
        line_formats,
    );
    insert_process_values(&mut values, pane.busy_duration_secs, line_formats);
    render_line(template, &values)
}

fn current_unix_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn min_created_epoch(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.flatten().min()
}

fn uptime_value(created_epoch: Option<u64>, now_epoch: u64) -> String {
    created_epoch
        .map(|created_epoch| human_duration(now_epoch.saturating_sub(created_epoch)))
        .unwrap_or_default()
}

fn insert_process_values(
    values: &mut BTreeMap<&'static str, String>,
    duration: Option<u64>,
    line_formats: &LineFormats,
) {
    let (status, elapsed) = match duration {
        Some(seconds) => ("running".to_string(), human_duration(seconds)),
        None => (String::new(), String::new()),
    };
    insert_template_value(values, "process_status", status, line_formats);
    insert_template_value(values, "process_elapsed_time", elapsed, line_formats);
}

pub(crate) fn format_line(template: &str, values: &BTreeMap<&'static str, String>) -> String {
    render_line(template, values).plain
}

fn render_line(template: &str, values: &BTreeMap<&'static str, String>) -> RenderedLine {
    let mut output = String::new();
    let mut spans = Vec::new();
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '{' {
            output.push(ch);
            push_row_span(&mut spans, ch.to_string(), None);
            continue;
        }

        let mut placeholder = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '}' {
                closed = true;
                break;
            }
            placeholder.push(next);
        }

        if closed {
            let (key, color) = split_placeholder(&placeholder);
            if let Some(value) = values.get(key) {
                output.push_str(value);
                push_row_span(&mut spans, value.clone(), color.and_then(parse_color_spec));
            } else {
                output.push('{');
                output.push_str(&placeholder);
                output.push('}');
                push_row_span(&mut spans, format!("{{{placeholder}}}"), None);
            }
        } else {
            output.push('{');
            output.push_str(&placeholder);
            push_row_span(&mut spans, format!("{{{placeholder}"), None);
        }
    }

    trim_rendered_line_end(RenderedLine {
        plain: output,
        spans,
    })
}

fn insert_template_value(
    values: &mut BTreeMap<&'static str, String>,
    key: &'static str,
    value: String,
    line_formats: &LineFormats,
) {
    values.insert(key, collapse_user_prefix(&value, line_formats));
}

fn collapse_user_prefix(value: &str, line_formats: &LineFormats) -> String {
    if !line_formats.collapse_user {
        return value.to_string();
    }

    let Some(home) = line_formats.user_home.as_deref() else {
        return value.to_string();
    };

    if value == home {
        return "~".to_string();
    }

    let Some(suffix) = value.strip_prefix(home) else {
        return value.to_string();
    };

    if suffix.is_empty() || suffix.starts_with('/') {
        return format!("~{suffix}");
    }

    value.to_string()
}

fn split_placeholder(placeholder: &str) -> (&str, Option<&str>) {
    match placeholder.split_once(':') {
        Some((key, color)) if !key.is_empty() => (key, Some(color)),
        _ => (placeholder, None),
    }
}

fn parse_color_spec(spec: &str) -> Option<Color> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    match spec.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        _ => None,
    }
}

fn push_row_span(spans: &mut Vec<RowLabelSpan>, text: String, fg: Option<Color>) {
    if text.is_empty() {
        return;
    }

    if let Some(last) = spans.last_mut()
        && last.fg == fg
    {
        last.text.push_str(&text);
        return;
    }

    spans.push(RowLabelSpan { text, fg });
}

fn trim_rendered_line_end(mut rendered: RenderedLine) -> RenderedLine {
    let trailing_chars = rendered
        .plain
        .chars()
        .rev()
        .take_while(|ch| ch.is_whitespace())
        .count();
    if trailing_chars == 0 {
        return rendered;
    }

    rendered.plain = rendered.plain.trim_end().to_string();
    let mut remaining = trailing_chars;
    while remaining > 0 {
        let Some(last) = rendered.spans.last_mut() else {
            break;
        };
        let span_len = last.text.chars().count();
        if span_len <= remaining {
            remaining -= span_len;
            rendered.spans.pop();
            continue;
        }
        let keep = span_len - remaining;
        last.text = last.text.chars().take(keep).collect();
        break;
    }

    rendered
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

fn group_panes_for_page(
    tree: &HostTree,
    hide_idle_panes: bool,
) -> BTreeMap<String, BTreeMap<String, Vec<&PaneInfo>>> {
    let mut sessions: BTreeMap<String, BTreeMap<String, Vec<&PaneInfo>>> = BTreeMap::new();
    for pane in tree
        .panes
        .iter()
        .filter(|pane| !hide_idle_panes || pane.busy_duration_secs.is_some())
    {
        sessions
            .entry(pane.session_name.clone())
            .or_default()
            .entry(pane.window_index.clone())
            .or_default()
            .push(pane);
    }
    sessions
}

fn pane_page_structure_prefix(pane_position: usize, pane_count: usize) -> String {
    if pane_count <= 1 {
        return "  ".to_string();
    }

    if pane_position == 0 {
        return "╭─".to_string();
    }

    if pane_position + 1 == pane_count {
        "╰─".to_string()
    } else {
        "├─".to_string()
    }
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
                placeholder: false,
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

    gpus.iter()
        .map(|gpu| {
            let Some(memory_used_mib) = memory_by_index.get(&gpu.index) else {
                return GpuBadge::Memory {
                    digit: ' ',
                    level: 0,
                    active: false,
                    placeholder: true,
                };
            };
            let decile = rounded_memory_decile(*memory_used_mib, gpu.memory_total_mib);
            GpuBadge::Memory {
                digit: decile_digit(decile),
                level: decile_level(decile),
                active: true,
                placeholder: false,
            }
        })
        .collect()
}

fn active_pane_gpu_badges(gpus: &[crate::model::GpuInfo], pane: &PaneInfo) -> Vec<GpuBadge> {
    let pane_memory_by_index: BTreeMap<usize, u64> =
        pane.gpu_memory_by_index.iter().copied().collect();

    gpus.iter()
        .map(|gpu| {
            let total_decile = rounded_memory_decile(gpu.memory_used_mib, gpu.memory_total_mib);
            match pane_memory_by_index.get(&gpu.index) {
                Some(memory_used_mib) => GpuBadge::ActivePaneMemory {
                    digit: decile_digit(rounded_memory_decile(
                        *memory_used_mib,
                        gpu.memory_total_mib,
                    )),
                    level: decile_level(total_decile),
                    pane_active: true,
                },
                None => GpuBadge::ActivePaneMemory {
                    digit: decile_digit(total_decile),
                    level: decile_level(total_decile),
                    pane_active: false,
                },
            }
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
