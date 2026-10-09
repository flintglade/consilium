use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ConnectionState};
use grok_chat_core::Backend;

// Consilium's true-black, steel, neon-red palette is painted everywhere so
// the terminal profile never shows through.
const BG: Color = Color::Rgb(0, 0, 0);
const PANEL_BG: Color = Color::Rgb(6, 6, 7);
const FG: Color = Color::Rgb(236, 236, 236);
const DIM: Color = Color::Rgb(138, 141, 147);
const ACCENT: Color = Color::Rgb(255, 23, 69);
const USER: Color = Color::Rgb(45, 226, 230);
const ERROR: Color = Color::Rgb(255, 180, 84);
const INFO: Color = Color::Rgb(138, 141, 147);
const SELECT_BG: Color = Color::Rgb(22, 9, 12);

const SIDEBAR_WIDTH: u16 = 24;
const MAX_COMPOSER_LINES: u16 = 6;

pub fn draw(frame: &mut Frame, app: &mut App, backend: &Backend) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);

    let main = if area.width >= 80 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(30)])
            .split(area);
        draw_sidebar(frame, cols[0], app, backend);
        cols[1]
    } else {
        area
    };

    let composer_width = main.width.saturating_sub(4) as usize; // margin + prompt
    let input_lines = composer_lines(&app.input, composer_width);
    let composer_height = (input_lines.len() as u16).clamp(1, MAX_COMPOSER_LINES) + 1; // + top border

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),               // header
            Constraint::Min(3),                  // transcript
            Constraint::Length(1),               // status strip
            Constraint::Length(composer_height), // composer
            Constraint::Length(1),               // footer
        ])
        .split(main);

    draw_header(frame, rows[0], app, backend);
    draw_transcript(frame, rows[1], app);
    draw_status(frame, rows[2], app, backend);
    draw_composer(frame, rows[3], app, &input_lines, composer_width);
    draw_footer(frame, rows[4]);
    draw_command_menu(frame, rows[3], app);
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App, backend: &Backend) {
    frame.render_widget(Block::default().style(Style::default().bg(PANEL_BG)), area);

    let label = Style::default().fg(DIM).bg(PANEL_BG);
    let value = Style::default().fg(FG).bg(PANEL_BG);
    let state = match app.connection {
        ConnectionState::Idle => "idle",
        ConnectionState::Connecting => "thinking…",
        ConnectionState::Streaming => "writing…",
        ConnectionState::Stopping => "stopping…",
    };

    let mut lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  ✦ Consilium",
            Style::default()
                .fg(ACCENT)
                .bg(PANEL_BG)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("  MODE", label)),
        Line::from(Span::styled(format!("    {}", backend.mode_label()), value)),
        Line::from(Span::styled("  MODEL", label)),
        Line::from(Span::styled(
            format!("    {}", backend.model_label()),
            value,
        )),
        Line::from(Span::styled("  SESSION", label)),
        Line::from(Span::styled(
            format!("    {}", backend.session_label()),
            value,
        )),
        Line::from(Span::styled("  STATE", label)),
        Line::from(Span::styled(format!("    {state}"), value)),
        Line::from(""),
        Line::from(Span::styled("  COMMANDS", label)),
    ];
    for (name, _, _) in crate::app::COMMANDS {
        lines.push(Line::from(Span::styled(
            format!("    {name}"),
            Style::default().fg(ACCENT).bg(PANEL_BG),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  /help for keys",
        Style::default().fg(DIM).bg(PANEL_BG),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, backend: &Backend) {
    let state = match app.connection {
        ConnectionState::Idle => "ready".to_string(),
        ConnectionState::Connecting => format!("{} thinking", app.spinner()),
        ConnectionState::Streaming => format!("{} writing", app.spinner()),
        ConnectionState::Stopping => format!("{} stopping", app.spinner()),
    };
    let left = " ✦ Consilium";
    let right = format!(
        "{} · {} · {state} ",
        backend.mode_label(),
        backend.model_label()
    );
    let pad = (area.width as usize).saturating_sub(left.width() + right.width());
    let line = Line::from(vec![
        Span::styled(
            left,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.visible_messages().is_empty() {
        app.max_scroll = 0;
        app.page_height = area.height;
        let empty = vec![
            Line::from(Span::styled("✻", Style::default().fg(ACCENT))).centered(),
            Line::from(""),
            Line::from(Span::styled(
                "Ask anything, then press Enter.",
                Style::default().fg(FG),
            ))
            .centered(),
            Line::from(Span::styled(
                "Type / for commands · /help for keys",
                Style::default().fg(DIM),
            ))
            .centered(),
        ];
        let top = area.height.saturating_sub(empty.len() as u16) / 2;
        let mut lines = vec![Line::from(""); top as usize];
        lines.extend(empty);
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let width = area.width.saturating_sub(4) as usize; // 2 margin each side
    let mut lines: Vec<Line<'static>> = Vec::new();

    for msg in app.visible_messages() {
        let (name, name_style, body_style) = match msg.role.as_str() {
            "user" => (
                "You",
                Style::default().fg(USER).add_modifier(Modifier::BOLD),
                Style::default().fg(FG),
            ),
            "assistant" => (
                "Consilium",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                Style::default().fg(FG),
            ),
            "error" => (
                "✗ error",
                Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
                Style::default().fg(ERROR),
            ),
            _ => ("·", Style::default().fg(INFO), Style::default().fg(DIM)),
        };
        lines.push(Line::from(Span::styled(format!("  {name}"), name_style)));
        for raw in msg.content.split('\n') {
            for wrapped in wrap_line(raw, width) {
                lines.push(Line::from(Span::styled(format!("  {wrapped}"), body_style)));
            }
        }
        lines.push(Line::from(""));
    }

    let total = lines.len() as u16;
    let visible = area.height;
    app.max_scroll = total.saturating_sub(visible);
    app.scroll = app.scroll.min(app.max_scroll);
    app.page_height = visible.saturating_sub(1).max(1);
    let offset = app.max_scroll - app.scroll;

    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, backend: &Backend) {
    let line = match app.connection {
        ConnectionState::Idle => Line::from(Span::styled(
            format!("  session {}", backend.session_label()),
            Style::default().fg(DIM),
        )),
        ConnectionState::Connecting => Line::from(Span::styled(
            format!("  {} thinking… (ctrl+c to interrupt)", app.spinner()),
            Style::default().fg(ACCENT),
        )),
        ConnectionState::Streaming => Line::from(Span::styled(
            format!("  {} writing… (ctrl+c to interrupt)", app.spinner()),
            Style::default().fg(ACCENT),
        )),
        ConnectionState::Stopping => Line::from(Span::styled(
            format!("  {} stopping provider…", app.spinner()),
            Style::default().fg(ACCENT),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_composer(frame: &mut Frame, area: Rect, app: &App, input_lines: &[String], width: usize) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(55, 55, 66)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let prompt_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();

    if app.input.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(" ❯ ", prompt_style),
            Span::styled(
                "Ask anything… · / for commands",
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else {
        for (i, text) in input_lines.iter().enumerate() {
            let prefix = if i == 0 { " ❯ " } else { "   " };
            lines.push(Line::from(vec![
                Span::styled(prefix, prompt_style),
                Span::styled(text.clone(), Style::default().fg(FG)),
            ]));
        }
    }

    // keep the cursor's row visible when input exceeds the composer height
    let (cur_row, cur_col) = composer_cursor(&app.input, app.cursor_pos, width);
    let offset = (cur_row + 1).saturating_sub(inner.height);
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), inner);
    frame.set_cursor_position(Position::new(
        inner.x + 3 + cur_col.min(width as u16),
        inner.y + cur_row - offset,
    ));
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        "  Enter send · Alt+Enter newline · Up/Down history · PgUp/PgDn scroll · Ctrl+C quit",
        Style::default().fg(DIM),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_command_menu(frame: &mut Frame, composer: Rect, app: &App) {
    let matches = app.menu_matches();
    if matches.is_empty() {
        return;
    }
    let height = matches.len() as u16;
    let y = composer.y.saturating_sub(height);
    let area = Rect::new(composer.x, y, composer.width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(PANEL_BG)), area);

    let selected = app.menu_selection.min(matches.len() - 1);
    let mut lines: Vec<Line> = Vec::new();
    for (i, (name, args, desc)) in matches.iter().enumerate() {
        let bg = if i == selected { SELECT_BG } else { PANEL_BG };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {name:<8}"),
                Style::default()
                    .fg(ACCENT)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{args:<8}"), Style::default().fg(DIM).bg(bg)),
            Span::styled(
                format!("{desc:<width$}", width = area.width as usize),
                Style::default().fg(FG).bg(bg),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayUnit {
    start: usize,
    end: usize,
    width: usize,
}

/// Segment once and measure each complete cluster once. Measuring every prefix
/// of a long combining sequence makes untrusted model output quadratic.
fn display_units(text: &str) -> Vec<DisplayUnit> {
    text.grapheme_indices(true)
        .map(|(start, grapheme)| DisplayUnit {
            start,
            end: start + grapheme.len(),
            width: grapheme.width(),
        })
        .collect()
}

/// Word-wrap one logical line to `width` terminal columns, preserving
/// internal spacing and never splitting a combining or emoji display unit.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let units = display_units(line);
    if units.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let mut used = 0usize;
        let mut fit_end = start;
        let mut overflow = None;
        let mut last_space = None;

        for (index, unit) in units.iter().enumerate().skip(start) {
            if index > start && used.saturating_add(unit.width) > width {
                overflow = Some(index);
                break;
            }
            used = used.saturating_add(unit.width);
            fit_end = index + 1;
            if index > start && &line[unit.start..unit.end] == " " {
                last_space = Some(index);
            }
        }

        if fit_end == units.len() {
            out.push(line[units[start].start..].to_string());
            break;
        }

        let break_space = overflow
            .filter(|index| &line[units[*index].start..units[*index].end] == " ")
            .or(last_space);
        if let Some(space) = break_space {
            out.push(line[units[start].start..units[space].start].to_string());
            start = space + 1;
        } else {
            let end = fit_end.max(start + 1);
            out.push(line[units[start].start..units[end - 1].end].to_string());
            start = end;
        }
    }
    out
}

/// Composer display lines: split on newlines, then hard-wrap by terminal
/// columns while keeping each display unit intact.
pub fn composer_lines(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in input.split('\n') {
        let units = display_units(logical);
        if units.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut start = 0;
        let mut used = 0usize;
        for unit in &units {
            if used > 0 && used.saturating_add(unit.width) > width {
                out.push(logical[start..unit.start].to_string());
                start = unit.start;
                used = 0;
            }
            used = used.saturating_add(unit.width);
            if used >= width {
                out.push(logical[start..unit.end].to_string());
                start = unit.end;
                used = 0;
            }
        }
        if start < logical.len() {
            out.push(logical[start..].to_string());
        } else if used == 0 {
            // Keep the cursor row real when the final unit exactly fills a line.
            out.push(String::new());
        }
    }
    out
}

/// (row, col) of the cursor within the composer_lines layout.
pub fn composer_cursor(input: &str, cursor_byte: usize, width: usize) -> (u16, u16) {
    let width = width.max(1);
    let mut row: u16 = 0;
    let mut col = 0usize;
    let prefix = &input[..cursor_byte.min(input.len())];
    for (logical_index, logical) in prefix.split('\n').enumerate() {
        if logical_index > 0 {
            row = row.saturating_add(1);
            col = 0;
        }
        for unit in display_units(logical) {
            if col > 0 && col.saturating_add(unit.width) > width {
                row = row.saturating_add(1);
                col = 0;
            }
            col = col.saturating_add(unit.width);
            if col >= width {
                row = row.saturating_add(1);
                col = 0;
            }
        }
    }
    (row, col.min(u16::MAX as usize) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_combining_sequence_keeps_one_cluster_without_prefix_rescanning() {
        let cluster = format!("e{}", "\u{301}".repeat(100_000));
        let text = format!("{cluster}X");
        let started = std::time::Instant::now();
        assert_eq!(wrap_line(&text, 1), vec![cluster, "X".to_string()]);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn wrap_preserves_internal_spacing() {
        // indentation (as in code blocks) survives wrapping
        let wrapped = wrap_line("    let x = 1;", 40);
        assert_eq!(wrapped, vec!["    let x = 1;".to_string()]);
    }

    #[test]
    fn wrap_breaks_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_line("hello brave new world", 11),
            vec!["hello brave".to_string(), "new world".to_string()]
        );
        assert_eq!(
            wrap_line("abcdefghij", 4),
            vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string()]
        );
    }

    #[test]
    fn wrapping_uses_terminal_width_and_keeps_unicode_sequences_intact() {
        assert_eq!(
            wrap_line("你好世界", 4),
            vec!["你好".to_string(), "世界".to_string()]
        );
        assert_eq!(
            wrap_line("e\u{301}e\u{301}", 1),
            vec!["e\u{301}".to_string(), "e\u{301}".to_string()]
        );
        assert_eq!(
            wrap_line("👩‍💻👨‍🔬", 2),
            vec!["👩‍💻".to_string(), "👨‍🔬".to_string()]
        );
        assert_eq!(
            wrap_line("🇺🇸🇯🇵", 2),
            vec!["🇺🇸".to_string(), "🇯🇵".to_string()]
        );
        for line in wrap_line("A界 e\u{301} 👩‍💻", 4) {
            assert!(line.width() <= 4, "line is too wide: {line:?}");
        }
    }

    #[test]
    fn composer_cursor_tracks_newlines_and_wraps() {
        // "ab\ncd" cursor at end: row 1, col 2
        assert_eq!(composer_cursor("ab\ncd", 5, 10), (1, 2));
        // wrap at width 3: "abcd" cursor after 4 chars -> row 1, col 1
        assert_eq!(composer_cursor("abcd", 4, 3), (1, 1));
        // multibyte: cursor after 'é' (2 bytes) is col 1
        assert_eq!(composer_cursor("éx", 2, 10), (0, 1));
        // CJK characters occupy two columns, and wrap before overflowing.
        assert_eq!(composer_cursor("ab界", "ab界".len(), 3), (1, 2));
        // Combining marks stay in their base character's display column.
        assert_eq!(composer_cursor("e\u{301}x", "e\u{301}".len(), 10), (0, 1));
        // A complete ZWJ emoji sequence is a single two-column display unit.
        assert_eq!(composer_cursor("👩‍💻x", "👩‍💻".len(), 10), (0, 2));
    }

    #[test]
    fn composer_lines_match_cursor_layout() {
        assert_eq!(
            composer_lines("ab\ncd", 10),
            vec!["ab".to_string(), "cd".to_string()]
        );
        assert_eq!(
            composer_lines("abcd", 3),
            vec!["abc".to_string(), "d".to_string()]
        );
        assert_eq!(
            composer_lines("A界B", 3),
            vec!["A界".to_string(), "B".to_string()]
        );
        assert_eq!(
            composer_lines("e\u{301}x", 1),
            vec!["e\u{301}".to_string(), "x".to_string(), String::new()]
        );
        assert_eq!(
            composer_lines("👩‍💻x", 2),
            vec!["👩‍💻".to_string(), "x".to_string()]
        );
        assert_eq!(composer_lines("", 10), vec![String::new()]);
    }

    #[test]
    fn shell_renders_sidebar_empty_state_and_composer() {
        use crate::app::App;
        use grok_chat_core::api::ApiClient;
        use grok_chat_core::Backend;
        use ratatui::{backend::TestBackend, Terminal};

        let client = ApiClient::new(
            "k".into(),
            "grok-4.5".into(),
            5,
            "http://127.0.0.1:1".into(),
        )
        .unwrap();
        let backend = Backend::Api(client);
        let mut app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &backend)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..30 {
            for x in 0..100 {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Consilium"), "header/sidebar brand missing");
        assert!(
            text.contains("Ask anything, then press Enter."),
            "empty state missing"
        );
        assert!(text.contains("COMMANDS"), "sidebar commands missing");
        assert!(text.contains("grok-4.5"), "model label missing");
        assert!(text.contains("❯"), "composer prompt missing");
        assert!(text.contains("PgUp/PgDn scroll"), "footer hints missing");
    }

    #[test]
    fn transcript_renders_messages_with_labels() {
        use crate::app::App;
        use grok_chat_core::api::{ApiClient, Message};
        use grok_chat_core::Backend;
        use ratatui::{backend::TestBackend, Terminal};

        let client = ApiClient::new(
            "k".into(),
            "grok-4.5".into(),
            5,
            "http://127.0.0.1:1".into(),
        )
        .unwrap();
        let backend = Backend::Api(client);
        let mut app = App::new();
        app.messages.push(Message {
            role: "user".into(),
            content: "hello there".into(),
        });
        app.messages.push(Message {
            role: "assistant".into(),
            content: "hi, how can I help?".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app, &backend)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..30 {
            for x in 0..100 {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("You"), "user label missing");
        assert!(text.contains("hello there"));
        assert!(text.contains("Consilium"), "assistant label missing");
        assert!(text.contains("hi, how can I help?"));
    }
}
