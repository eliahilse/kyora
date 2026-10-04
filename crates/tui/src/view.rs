use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{App, Entry, Focus},
    event::NodeKind,
};

fn accent(app: &App) -> Style {
    if app.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan)
    }
}

fn pane(title: String, focused: bool, style: Style) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(if focused { style } else { Style::default() })
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let input_height = (app.input.lines().len() as u16)
        .saturating_add(2)
        .clamp(3, 7);
    let [body, input, footer] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_height),
        Constraint::Length(2),
    ])
    .areas(area);
    let [conversation, tree] = if area.width >= 70 {
        Layout::horizontal([
            Constraint::Min(30),
            Constraint::Length((area.width / 3).clamp(32, 48)),
        ])
        .areas(body)
    } else {
        Layout::vertical([
            Constraint::Min(3),
            Constraint::Length((body.height / 2).min(10)),
        ])
        .areas(body)
    };
    draw_conversation(frame, app, conversation);
    draw_tree(frame, app, tree);

    let style = accent(app);
    app.input.set_style(Style::default());
    app.input.set_cursor_line_style(Style::default());
    app.input.set_cursor_style(
        if app.focus == Focus::Input && !app.help && !app.confirm_quit {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        },
    );
    app.input
        .set_selection_style(Style::default().add_modifier(Modifier::REVERSED));
    app.input
        .set_placeholder_text("Type a prompt. Enter plays the offline script.");
    app.input
        .set_placeholder_style(Style::default().add_modifier(Modifier::DIM));
    app.input.set_block(pane(
        "Input | Enter send | Shift+Enter newline".into(),
        app.focus == Focus::Input,
        style,
    ));
    frame.render_widget(&app.input, input);

    let (tokens, cost) = app.totals();
    let model = &app.nodes[&0].spec.model;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{model} | {tokens} tok | est ${:.4} | {} left",
                cost as f64 / 1_000_000.0,
                app.remaining
            )),
            Line::from("Tab focus  ? help  Esc cancel  q quit*  Ctrl-C quit  PgUp/PgDn scroll"),
        ]),
        footer,
    );

    if app.help {
        popup(
            frame,
            area,
            "Help",
            &[
                "Offline prototype: each prompt plays the same fixture.",
                "Tab / Shift+Tab: cycle input, conversation, tree",
                "Enter: send prompt   Shift+Enter / Ctrl-J: newline",
                "Input: arrows edit, paste supports multiple lines",
                "Tree: Up/Down selects a node and its transcript",
                "Conversation: Up/Down selects a tool",
                "Enter / Space: expand or collapse the selected tool",
                "PgUp/PgDn: scroll transcript   End: follow stream",
                "Esc: cancel turn   ?: close help",
                "q: quit outside input   Ctrl-C: quit anywhere",
                "Active runs ask for quit confirmation (y/n).",
            ],
            style,
        );
    }
    if app.confirm_quit {
        popup(
            frame,
            area,
            "Quit",
            &[
                "A run is active. Cancel it and quit?",
                "y / Enter: quit    n / Esc: return",
            ],
            style,
        );
    }
}

fn draw_conversation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let style = accent(app);
    let block = pane(
        format!("Conversation #{}", app.selected_node),
        app.focus == Focus::Conversation,
        style,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    let mut tool_index = 0;
    for entry in app
        .entries
        .iter()
        .filter(|entry| entry.node() == app.selected_node)
    {
        match entry {
            Entry::User { text, .. } => {
                lines.push(Line::styled("You", style));
                lines.extend(text.lines().map(|line| Line::raw(line.to_owned())));
            }
            Entry::Assistant { text, .. } => {
                lines.push(Line::styled("Assistant", style));
                lines.extend(text.lines().map(|line| Line::raw(line.to_owned())));
            }
            Entry::Tool {
                name,
                args,
                result,
                status,
                expanded,
                ..
            } => {
                let selected = app.focus == Focus::Conversation && tool_index == app.selected_tool;
                let marker = if *expanded { "[-]" } else { "[+]" };
                lines.push(Line::styled(
                    format!("{marker} {name} [{}]", status.label()),
                    if selected {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    },
                ));
                if *expanded {
                    lines.extend(args.lines().map(|line| Line::raw(format!("  {line}"))));
                    lines.push(Line::raw("  Result:"));
                    lines.extend(
                        result
                            .as_deref()
                            .unwrap_or("waiting...")
                            .lines()
                            .map(|line| Line::raw(format!("  {line}"))),
                    );
                } else {
                    lines.push(Line::raw(format!("  args: {}", preview(args, 70))));
                    lines.push(Line::raw(format!(
                        "  result: {}",
                        preview(result.as_deref().unwrap_or("waiting..."), 70)
                    )));
                }
                tool_index += 1;
            }
        }
        lines.push(Line::raw(""));
    }
    if lines.is_empty() {
        lines.extend([
            Line::styled("kyora", style),
            Line::raw("Explore a recursive run without API keys."),
            Line::raw("Send a prompt to stream a Python tool call,"),
            Line::raw("three child agents and a batch of llm() calls."),
            Line::raw("Select tree nodes to inspect their transcripts."),
        ]);
    }
    lines.push(Line::styled(
        &app.notice,
        Style::default().add_modifier(Modifier::DIM),
    ));
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let end = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph.scroll((end.saturating_sub(app.scroll_back), 0)),
        inner,
    );
}

fn draw_tree(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let style = accent(app);
    let rows = app.tree_rows();
    let items: Vec<_> = rows
        .iter()
        .map(|(id, depth)| {
            let node = &app.nodes[id];
            let kind = match node.kind {
                NodeKind::Agent => "agent",
                NodeKind::Cell => "repl",
                NodeKind::Llm => "llm()",
            };
            let indent = "  ".repeat((*depth).min(6));
            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(format!("{indent}#{id} {kind} {} ", node.spec.name)),
                    Span::styled(node.status.label(), style),
                ]),
                Line::raw(format!(
                    "{indent}  {} {}t r{}",
                    node.spec.model,
                    node.tokens,
                    node.remaining.unwrap_or(app.remaining)
                )),
            ])
        })
        .collect();
    let mut state = ListState::default()
        .with_selected(rows.iter().position(|(id, _)| *id == app.selected_node));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane(
                "Recursion | Up/Down inspect".into(),
                app.focus == Focus::Tree,
                style,
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn preview(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let mut preview: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn popup(frame: &mut Frame<'_>, area: Rect, title: &str, lines: &[&str], style: Style) {
    let width = area.width.min(64);
    let height = area.height.min(lines.len() as u16 + 2);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(style),
            ),
        popup,
    );
}
