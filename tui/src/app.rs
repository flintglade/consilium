use color_eyre::eyre::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use grok_chat_core::api::{stream_event_channel, Message, StreamEvent};
use grok_chat_core::Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Connecting,
    Streaming,
    Stopping,
}

/// A slash command: name, argument hint, description. Every entry here is
/// implemented — the menu must never advertise something that does nothing.
pub const COMMANDS: [(&str, &str, &str); 5] = [
    ("/help", "", "show commands and keys"),
    ("/clear", "", "clear the transcript (keeps context)"),
    ("/new", "", "start a fresh session (resets context)"),
    ("/model", "[name]", "show or set the model"),
    ("/quit", "", "exit"),
];

pub struct TaggedStreamEvent {
    pub request_id: u64,
    pub event: Option<StreamEvent>,
}

pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub cursor_pos: usize,
    pub scroll: u16,
    pub connection: ConnectionState,
    pub tick: u8,
    pub menu_selection: usize,
    pub force_redraw: bool,
    /// Set by the draw pass so scrolling knows the transcript bounds.
    pub max_scroll: u16,
    pub page_height: u16,
    sent_history: Vec<String>,
    hist_index: Option<usize>,
    streaming_message: Option<usize>,
    cancel_token: Option<CancellationToken>,
    next_request_id: u64,
    active_request_id: Option<u64>,
    visible_message_start: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll: 0,
            connection: ConnectionState::Idle,
            tick: 0,
            menu_selection: 0,
            force_redraw: false,
            max_scroll: 0,
            page_height: 10,
            sent_history: Vec::new(),
            hist_index: None,
            streaming_message: None,
            cancel_token: None,
            next_request_id: 0,
            active_request_id: None,
            visible_message_start: 0,
        }
    }

    pub fn visible_messages(&self) -> &[Message] {
        &self.messages[self.visible_message_start.min(self.messages.len())..]
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
        FRAMES[(self.tick as usize / 2) % FRAMES.len()]
    }

    pub fn busy(&self) -> bool {
        self.connection != ConnectionState::Idle
    }

    /// Commands matching the current input, when the composer holds the
    /// start of a slash command (no arguments yet).
    pub fn menu_matches(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        if !self.input.starts_with('/') || self.input.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|(name, _, _)| name.starts_with(&self.input))
            .copied()
            .collect()
    }

    fn menu_open(&self) -> bool {
        !self.menu_matches().is_empty()
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        backend: &Backend,
        stream_tx: &Sender<TaggedStreamEvent>,
    ) -> Result<bool> {
        // Ctrl+C: interrupt an active stream; quit when idle.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.busy() {
                self.interrupt_stream();
                return Ok(false);
            }
            return Ok(true);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
            self.force_redraw = true;
            return Ok(false);
        }

        // Newline in the composer: Alt+Enter, or Ctrl+J.
        let is_newline = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL));
        if is_newline {
            self.insert_char('\n');
            return Ok(false);
        }

        match key.code {
            KeyCode::Enter => {
                if self.input.starts_with('/') {
                    if self.busy() {
                        self.push_info(
                            "Finish or interrupt the active response before running a command.",
                        );
                        return Ok(false);
                    }
                    return self.run_command(backend);
                }
                if !self.busy() && !self.input.trim().is_empty() {
                    self.send_message(backend, stream_tx)?;
                }
            }
            KeyCode::Tab => {
                let matches = self.menu_matches();
                if let Some((name, _, _)) =
                    matches.get(self.menu_selection.min(matches.len().saturating_sub(1)))
                {
                    self.input = name.to_string();
                    self.cursor_pos = self.input.len();
                }
            }
            KeyCode::Esc => {
                if self.input.is_empty() {
                    return Ok(true);
                }
                self.input.clear();
                self.cursor_pos = 0;
                self.hist_index = None;
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                self.menu_selection = 0;
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor_left(),
            KeyCode::Right => self.cursor_right(),
            KeyCode::Home => self.cursor_pos = 0,
            KeyCode::End => self.cursor_pos = self.input.len(),
            KeyCode::Up => {
                if self.menu_open() {
                    self.menu_selection = self.menu_selection.saturating_sub(1);
                } else {
                    self.history_prev();
                }
            }
            KeyCode::Down => {
                if self.menu_open() {
                    let len = self.menu_matches().len();
                    self.menu_selection = (self.menu_selection + 1).min(len.saturating_sub(1));
                } else {
                    self.history_next();
                }
            }
            KeyCode::PageUp => {
                self.scroll = self
                    .scroll
                    .saturating_add(self.page_height)
                    .min(self.max_scroll);
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page_height),
            _ => {}
        }

        Ok(false)
    }

    // cursor_pos is a byte index into input, always kept on a char
    // boundary — multi-byte characters move it by their UTF-8 width.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some((idx, _)) = self.input[..self.cursor_pos].char_indices().next_back() {
            self.input.remove(idx);
            self.cursor_pos = idx;
        }
    }

    fn delete(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
        }
    }

    fn cursor_left(&mut self) {
        if let Some((idx, _)) = self.input[..self.cursor_pos].char_indices().next_back() {
            self.cursor_pos = idx;
        }
    }

    fn cursor_right(&mut self) {
        if let Some(c) = self.input[self.cursor_pos..].chars().next() {
            self.cursor_pos += c.len_utf8();
        }
    }

    fn history_prev(&mut self) {
        if self.sent_history.is_empty() {
            return;
        }
        let next = match self.hist_index {
            None => self.sent_history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_index = Some(next);
        self.input = self.sent_history[next].clone();
        self.cursor_pos = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(i) = self.hist_index else {
            return;
        };
        if i + 1 < self.sent_history.len() {
            self.hist_index = Some(i + 1);
            self.input = self.sent_history[i + 1].clone();
        } else {
            self.hist_index = None;
            self.input.clear();
        }
        self.cursor_pos = self.input.len();
    }

    pub fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_add(2).min(self.max_scroll);
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(2);
            }
            _ => {}
        }
    }

    fn push_info(&mut self, text: impl Into<String>) {
        self.messages.push(Message {
            role: "info".to_string(),
            content: text.into(),
        });
    }

    fn run_command(&mut self, backend: &Backend) -> Result<bool> {
        let line = std::mem::take(&mut self.input);
        self.cursor_pos = 0;
        self.menu_selection = 0;
        let mut parts = line.trim_start_matches('/').split_whitespace();
        let cmd = parts.next().unwrap_or_default();
        let arg = parts.next();

        match cmd {
            "help" => {
                let mut text = String::from("Commands\n");
                for (name, args, desc) in COMMANDS {
                    let full = if args.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name} {args}")
                    };
                    text.push_str(&format!("  {full:<16} {desc}\n"));
                }
                text.push_str(
                    "\nKeys\n  Enter send · Alt+Enter/Ctrl+J newline · Tab complete command\n  Up/Down input history · PgUp/PgDn or wheel scroll\n  Ctrl+C interrupt/quit · Esc clear input or quit · Ctrl+L redraw",
                );
                self.push_info(text);
            }
            "clear" => {
                self.clear_visible_transcript();
            }
            "new" => {
                self.reset_conversation_view();
                backend.reset_session();
                self.push_info("Started a new session — context reset.");
            }
            "model" => match arg {
                Some(name) => match backend.set_model(name) {
                    Ok(()) => {
                        self.reset_conversation_view();
                        self.push_info(format!(
                            "Model set to {}; started a fresh conversation so models never inherit one another's context.",
                            backend.model_label()
                        ));
                    }
                    Err(e) => self.push_info(format!("Cannot set model: {e}")),
                },
                None => self.push_info(format!("Current model: {}", backend.model_label())),
            },
            "quit" => return Ok(true),
            other => self.push_info(format!("Unknown command: /{other} — try /help")),
        }
        Ok(false)
    }

    fn reset_conversation_view(&mut self) {
        self.messages.clear();
        self.visible_message_start = 0;
        self.scroll = 0;
    }

    /// Returns true when the provider-native session should be discarded so
    /// the next request rebuilds context from the locally marked transcript.
    pub fn handle_stream_event(&mut self, request_id: u64, event: Option<StreamEvent>) -> bool {
        if self.active_request_id != Some(request_id) {
            return false;
        }
        let Some(event) = event else {
            self.cancel_token = None;
            self.active_request_id = None;
            if self.connection == ConnectionState::Stopping {
                self.connection = ConnectionState::Idle;
                return false;
            }
            self.connection = ConnectionState::Idle;
            let had_partial = self.mark_streaming_response_incomplete();
            self.messages.push(Message {
                role: "error".to_string(),
                content: "The provider closed without reporting completion.".to_string(),
            });
            return had_partial;
        };
        match event {
            StreamEvent::Token(token) => {
                self.connection = ConnectionState::Streaming;
                if let Some(idx) = self.streaming_message {
                    if let Some(msg) = self.messages.get_mut(idx) {
                        msg.content.push_str(&token);
                    }
                }
                false
            }
            // reasoning tokens keep the "thinking" state alive in the TUI
            StreamEvent::Thought(_) => {
                if self.connection != ConnectionState::Streaming {
                    self.connection = ConnectionState::Connecting;
                }
                false
            }
            StreamEvent::Finished => {
                self.connection = ConnectionState::Idle;
                self.streaming_message = None;
                self.cancel_token = None;
                self.active_request_id = None;
                false
            }
            StreamEvent::Error(err) => {
                self.connection = ConnectionState::Idle;
                let had_partial = self.mark_streaming_response_incomplete();
                self.cancel_token = None;
                self.active_request_id = None;
                self.messages.push(Message {
                    role: "error".to_string(),
                    content: err,
                });
                had_partial
            }
        }
    }

    fn clear_visible_transcript(&mut self) {
        // Keep the complete conversation for stateless routes while clearing
        // only what is currently painted in the terminal.
        self.visible_message_start = self.messages.len();
        self.scroll = 0;
    }

    fn mark_streaming_response_incomplete(&mut self) -> bool {
        const MARKER: &str = "\n\n[Incomplete response — the provider stopped with an error.]";
        let Some(idx) = self.streaming_message.take() else {
            return false;
        };
        let has_partial = self
            .messages
            .get(idx)
            .is_some_and(|message| !message.content.is_empty());
        if has_partial {
            if let Some(message) = self.messages.get_mut(idx) {
                message.content.push_str(MARKER);
            }
        } else if idx < self.messages.len() {
            self.messages.remove(idx);
        }
        has_partial
    }

    fn send_message(
        &mut self,
        backend: &Backend,
        stream_tx: &Sender<TaggedStreamEvent>,
    ) -> Result<()> {
        let content = std::mem::take(&mut self.input);
        self.cursor_pos = 0;
        self.scroll = 0;
        self.hist_index = None;
        self.sent_history.push(content.clone());

        self.messages.push(Message {
            role: "user".to_string(),
            content,
        });

        self.messages.push(Message {
            role: "assistant".to_string(),
            content: String::new(),
        });
        self.streaming_message = Some(self.messages.len() - 1);
        self.connection = ConnectionState::Connecting;

        let cancel = CancellationToken::new();
        self.cancel_token = Some(cancel.clone());
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request_id = self.next_request_id;
        self.active_request_id = Some(request_id);

        // Info/error notices are display-only — never conversation history.
        let messages: Vec<Message> = self.messages[..self.messages.len() - 1]
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .cloned()
            .collect();

        let (provider_tx, mut provider_rx) = stream_event_channel();
        let app_tx = stream_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = provider_rx.recv().await {
                let terminal = matches!(event, StreamEvent::Finished | StreamEvent::Error(_));
                if app_tx
                    .send(TaggedStreamEvent {
                        request_id,
                        event: Some(event),
                    })
                    .await
                    .is_err()
                    || terminal
                {
                    break;
                }
            }
            let _ = app_tx
                .send(TaggedStreamEvent {
                    request_id,
                    event: None,
                })
                .await;
        });
        backend.stream_chat(messages, Vec::new(), false, cancel, provider_tx);

        Ok(())
    }

    /// Stop the active stream, keeping whatever text already arrived.
    fn interrupt_stream(&mut self) {
        if self.connection == ConnectionState::Stopping {
            return;
        }
        self.cancel_stream();
        self.connection = ConnectionState::Stopping;
        if let Some(idx) = self.streaming_message.take() {
            match self.messages.get_mut(idx) {
                Some(msg) if msg.content.is_empty() => {
                    self.messages.remove(idx);
                    self.push_info("Interrupted.");
                }
                Some(msg) => msg.content.push_str(" ⏹ interrupted"),
                None => {}
            }
        }
    }

    pub fn cancel_stream(&mut self) {
        if let Some(token) = self.cancel_token.take() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grok_chat_core::cli::CliBackend;
    use std::path::PathBuf;

    fn test_backend() -> Backend {
        Backend::Cli(CliBackend::new(PathBuf::from("grok"), None))
    }

    #[test]
    fn multibyte_input_editing_does_not_panic() {
        let mut app = App::new();
        for c in "héllo, wörld — ★".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "héllo, wörld — ★");

        app.cursor_left();
        app.insert_char('é');
        app.backspace();
        assert_eq!(app.input, "héllo, wörld — ★");

        app.cursor_right();
        app.backspace();
        assert_eq!(app.input, "héllo, wörld — ");

        // insert mid-string right after the multi-byte 'é'
        app.cursor_pos = 0;
        app.cursor_right();
        app.cursor_right();
        app.insert_char('★');
        assert_eq!(app.input, "hé★llo, wörld — ");

        app.delete();
        assert_eq!(app.input, "hé★lo, wörld — ");
    }

    #[test]
    fn cursor_movement_stays_on_char_boundaries() {
        let mut app = App::new();
        app.insert_char('日');
        app.insert_char('本');
        app.cursor_left();
        assert!(app.input.is_char_boundary(app.cursor_pos));
        app.cursor_left();
        assert_eq!(app.cursor_pos, 0);
        app.cursor_left(); // at start — no-op, no underflow
        assert_eq!(app.cursor_pos, 0);
        app.cursor_right();
        app.cursor_right();
        app.cursor_right(); // at end — no-op
        assert_eq!(app.cursor_pos, app.input.len());
    }

    #[test]
    fn command_menu_filters_by_prefix() {
        let mut app = App::new();
        assert!(app.menu_matches().is_empty());
        app.input = "/".to_string();
        assert_eq!(app.menu_matches().len(), COMMANDS.len());
        app.input = "/m".to_string();
        assert_eq!(
            app.menu_matches(),
            vec![("/model", "[name]", "show or set the model")]
        );
        app.input = "/model default".to_string(); // args typed -> menu closes
        assert!(app.menu_matches().is_empty());
    }

    #[test]
    fn input_history_navigation() {
        let mut app = App::new();
        app.sent_history = vec!["first".into(), "second".into()];
        app.history_prev();
        assert_eq!(app.input, "second");
        app.history_prev();
        assert_eq!(app.input, "first");
        app.history_prev(); // clamped at oldest
        assert_eq!(app.input, "first");
        app.history_next();
        assert_eq!(app.input, "second");
        app.history_next(); // past newest -> empty composer
        assert_eq!(app.input, "");
        assert!(app.hist_index.is_none());
    }

    #[test]
    fn info_and_error_messages_are_excluded_from_api_history() {
        let mut app = App::new();
        app.push_info("some notice");
        app.messages.push(Message {
            role: "error".to_string(),
            content: "boom".to_string(),
        });
        app.messages.push(Message {
            role: "user".to_string(),
            content: "hi".to_string(),
        });
        let kept: Vec<&Message> = app
            .messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].content, "hi");
    }

    #[test]
    fn late_events_from_an_interrupted_request_are_ignored() {
        let mut app = App::new();
        app.messages.push(Message {
            role: "assistant".to_string(),
            content: String::new(),
        });
        app.streaming_message = Some(0);
        app.connection = ConnectionState::Connecting;
        app.active_request_id = Some(2);

        app.handle_stream_event(1, Some(StreamEvent::Token("old".to_string())));
        app.handle_stream_event(1, Some(StreamEvent::Finished));
        assert_eq!(app.messages[0].content, "");
        assert_eq!(app.connection, ConnectionState::Connecting);

        app.handle_stream_event(2, Some(StreamEvent::Token("current".to_string())));
        assert_eq!(app.messages[0].content, "current");
        assert_eq!(app.connection, ConnectionState::Streaming);
    }

    #[test]
    fn interrupted_request_stays_busy_until_provider_cleanup_closes() {
        let mut app = App::new();
        app.messages.push(Message {
            role: "assistant".to_string(),
            content: "partial".to_string(),
        });
        app.streaming_message = Some(0);
        app.connection = ConnectionState::Streaming;
        app.active_request_id = Some(7);
        app.cancel_token = Some(CancellationToken::new());

        app.interrupt_stream();
        assert_eq!(app.connection, ConnectionState::Stopping);
        assert!(app.busy());
        assert_eq!(app.messages[0].content, "partial ⏹ interrupted");

        app.handle_stream_event(7, None);
        assert_eq!(app.connection, ConnectionState::Idle);
        assert!(!app.busy());
    }

    #[test]
    fn clear_hides_the_view_but_retains_context_for_stateless_routes() {
        let mut app = App::new();
        app.messages = vec![
            Message {
                role: "user".to_string(),
                content: "first question".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: "first answer".to_string(),
            },
        ];
        app.input = "/clear".to_string();

        app.run_command(&test_backend()).unwrap();

        assert!(app.visible_messages().is_empty());
        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].content, "first question");

        app.messages.push(Message {
            role: "user".to_string(),
            content: "follow-up".to_string(),
        });
        assert_eq!(app.visible_messages().len(), 1);
        assert_eq!(app.visible_messages()[0].content, "follow-up");
    }

    #[test]
    fn changing_models_starts_a_fresh_conversation_with_an_explicit_notice() {
        use grok_chat_core::api::ApiClient;

        let backend = Backend::Api(
            ApiClient::new(
                "key".to_string(),
                "old-model".to_string(),
                5,
                "http://127.0.0.1:1".to_string(),
            )
            .unwrap(),
        );
        let mut app = App::new();
        app.messages = vec![
            Message {
                role: "user".to_string(),
                content: "context from the old model".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: "old answer".to_string(),
            },
        ];
        app.visible_message_start = 1;
        app.scroll = 9;
        app.input = "/model new-model".to_string();

        app.run_command(&backend).unwrap();

        assert_eq!(backend.model_label(), "new-model");
        assert_eq!(app.visible_message_start, 0);
        assert_eq!(app.scroll, 0);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, "info");
        assert!(app.messages[0].content.contains("Model set to new-model"));
        assert!(app.messages[0]
            .content
            .contains("started a fresh conversation"));
        assert!(!app.messages[0].content.contains("old answer"));
    }

    #[test]
    fn provider_error_marks_partial_answer_before_it_reenters_context() {
        let mut app = App::new();
        app.messages = vec![
            Message {
                role: "user".to_string(),
                content: "question".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: "partial answer".to_string(),
            },
        ];
        app.streaming_message = Some(1);
        app.connection = ConnectionState::Streaming;
        app.active_request_id = Some(11);

        let reset_native_session =
            app.handle_stream_event(11, Some(StreamEvent::Error("provider failed".to_string())));

        assert!(reset_native_session);
        assert!(app.messages[1].content.contains("Incomplete response"));
        assert_eq!(app.messages[2].role, "error");
        let future_context = app
            .messages
            .iter()
            .filter(|message| message.role == "user" || message.role == "assistant")
            .collect::<Vec<_>>();
        assert_eq!(future_context.len(), 2);
        assert!(future_context[1].content.contains("Incomplete response"));
    }
}
