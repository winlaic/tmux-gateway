use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::app::{App, ContextMenuItem, ContextMenuState, Mode, SplitChoice, prompt_help};
#[cfg(test)]
use crate::model::NodeId;
use crate::model::{GpuBadge, RowStatus, VisibleRow};

pub(crate) fn draw_app(frame: &mut ratatui::Frame<'_>, app: &mut App) {
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
            let marker = if !row.expandable {
                " "
            } else if app.expanded.contains(&row.id) {
                "▾"
            } else {
                "▸"
            };
            let main_style = if row.selectable {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            let main_style = if row.status == RowStatus::Unavailable {
                main_style.fg(Color::LightRed).add_modifier(Modifier::BOLD)
            } else if row.busy_duration_secs.is_some() {
                main_style
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD)
            } else {
                main_style
            };
            let detail_style = if row.status == RowStatus::Unavailable {
                Style::default().fg(Color::Rgb(120, 48, 48))
            } else if row.busy_duration_secs.is_some() {
                Style::default().fg(Color::Rgb(55, 120, 70))
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let row_style = if selected && row.status == RowStatus::Unavailable {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD)
            } else if selected && row.busy_duration_secs.is_some() {
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
            ListItem::new(row_line(
                row,
                RowRenderParts {
                    cursor,
                    indent,
                    marker,
                    row_style,
                    main_style,
                    detail_style,
                    width: chunks[1].width.saturating_sub(2),
                },
            ))
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

struct RowRenderParts {
    cursor: &'static str,
    indent: String,
    marker: &'static str,
    row_style: Style,
    main_style: Style,
    detail_style: Style,
    width: u16,
}

fn row_line(row: &VisibleRow, parts: RowRenderParts) -> Line<'static> {
    if row.gpu_badges.is_empty() {
        return Line::from(vec![
            Span::styled(parts.cursor, parts.row_style),
            Span::raw(parts.indent),
            Span::styled(parts.marker, Style::default().fg(Color::Yellow)),
            Span::raw(" "),
            Span::styled(row.label.clone(), parts.main_style),
            Span::raw(" "),
            Span::styled(row.detail.clone(), parts.detail_style),
        ]);
    }

    let fixed_width = parts.cursor.chars().count() as u16
        + parts.indent.chars().count() as u16
        + parts.marker.chars().count() as u16
        + 2;
    let badge_width = row.gpu_badges.len() as u16;
    let badge_gap = if badge_width > 0 { 1 } else { 0 };
    let text_width = parts
        .width
        .saturating_sub(fixed_width)
        .saturating_sub(badge_width)
        .saturating_sub(badge_gap);
    let (label, detail) = trim_row_text(&row.label, &row.detail, text_width);

    let mut spans = vec![
        Span::styled(parts.cursor, parts.row_style),
        Span::raw(parts.indent),
        Span::styled(parts.marker, Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(label.clone(), parts.main_style),
    ];
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(detail.clone(), parts.detail_style));
    }

    add_right_aligned_gpu_badges(&mut spans, row, parts.width, fixed_width + text_width);
    Line::from(spans)
}

fn add_right_aligned_gpu_badges(
    spans: &mut Vec<Span<'static>>,
    row: &VisibleRow,
    width: u16,
    reserved_left_width: u16,
) {
    if row.gpu_badges.is_empty() || width == 0 {
        return;
    }

    let badge_width = row.gpu_badges.len() as u16;
    let actual_left_width = line_width(spans);
    let left_width = actual_left_width.min(reserved_left_width);
    let gap = width.saturating_sub(left_width).saturating_sub(badge_width);
    spans.push(Span::raw(" ".repeat(gap as usize)));
    spans.extend(row.gpu_badges.iter().map(gpu_badge_span));
}

fn trim_row_text(label: &str, detail: &str, width: u16) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }

    let label_width = label.chars().count() as u16;
    if detail.is_empty() || label_width >= width {
        return (trim_to_width(label, width), String::new());
    }

    let detail_width = width.saturating_sub(label_width).saturating_sub(1);
    (label.to_string(), trim_to_width(detail, detail_width))
}

fn trim_to_width(value: &str, width: u16) -> String {
    value.chars().take(width as usize).collect()
}

fn line_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| span.content.chars().count() as u16)
        .sum()
}

fn gpu_badge_span(badge: &GpuBadge) -> Span<'static> {
    match badge {
        GpuBadge::Memory {
            digit,
            level,
            active,
        } => Span::styled(
            digit.to_string(),
            Style::default()
                .fg(gpu_badge_foreground(*active))
                .bg(if *active {
                    Color::Blue
                } else {
                    gpu_memory_color(*level)
                }),
        ),
    }
}

fn gpu_memory_color(level: u8) -> Color {
    match level {
        0 => Color::Green,
        1 => Color::Yellow,
        2 => Color::Rgb(255, 165, 0),
        _ => Color::Red,
    }
}

fn gpu_badge_foreground(active: bool) -> Color {
    if active { Color::White } else { Color::Black }
}

#[cfg(test)]
pub(crate) fn test_render_row_line(row: &VisibleRow, width: u16) -> Line<'static> {
    row_line(
        row,
        RowRenderParts {
            cursor: "  ",
            indent: "  ".repeat(row.depth),
            marker: match row.id {
                NodeId::Pane { .. } => " ",
                _ => "▸",
            },
            row_style: Style::default(),
            main_style: Style::default(),
            detail_style: Style::default(),
            width,
        },
    )
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

pub(crate) fn split_choice_area(area: Rect) -> Rect {
    centered_rect(50, 8, area)
}

pub(crate) fn confirm_area(area: Rect) -> Rect {
    centered_rect(52, 8, area)
}

pub(crate) fn split_choice_at_mouse(
    column: u16,
    row: u16,
    screen_area: Rect,
) -> Option<SplitChoice> {
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

pub(crate) fn confirm_choice_at_mouse(column: u16, row: u16, screen_area: Rect) -> Option<bool> {
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

pub(crate) fn context_menu_area(items: &[ContextMenuItem], x: u16, y: u16, bounds: Rect) -> Rect {
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

pub(crate) fn menu_item_at_mouse(menu: &ContextMenuState, column: u16, row: u16) -> Option<usize> {
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
