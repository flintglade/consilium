use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use futures::StreamExt;
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_API_URL: &str = "https://api.x.ai/v1/chat/completions";
pub const STREAM_EVENT_CHANNEL_CAPACITY: usize = 64;
pub(crate) const ERROR_BODY_LIMIT: usize = 16 * 1024;
pub(crate) const STREAM_FRAME_LIMIT: usize = 1024 * 1024;
pub(crate) const STREAM_TOTAL_LIMIT: usize = 32 * 1024 * 1024;

pub(crate) struct ErrorBodyPrefix {
    pub text: String,
    pub truncated: bool,
}

/// Reads only a small prefix from an unsuccessful response. Dropping the
/// response stops the remaining body from being buffered by the application.
pub(crate) async fn read_error_body_prefix(
    response: Response,
    cancel: &CancellationToken,
) -> Option<ErrorBodyPrefix> {
    let content_length = response.content_length();
    let mut stream = response.bytes_stream();
    let initial_capacity = content_length
        .unwrap_or_default()
        .min(ERROR_BODY_LIMIT as u64) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut truncated = content_length.is_some_and(|length| length > ERROR_BODY_LIMIT as u64);

    while bytes.len() < ERROR_BODY_LIMIT {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return None,
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = ERROR_BODY_LIMIT - bytes.len();
        let take = remaining.min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            truncated = true;
            break;
        }
    }

    if bytes.len() == ERROR_BODY_LIMIT && content_length.is_none() {
        truncated = true;
    }

    Some(ErrorBodyPrefix {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

pub(crate) struct SseLineBuffer {
    pending: Vec<u8>,
    total: usize,
    frame_limit: usize,
    total_limit: usize,
    cancel: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineFlow {
    Continue,
    Stop,
}

impl SseLineBuffer {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self::with_limits(cancel, STREAM_FRAME_LIMIT, STREAM_TOTAL_LIMIT)
    }

    fn with_limits(cancel: CancellationToken, frame_limit: usize, total_limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            total: 0,
            frame_limit,
            total_limit,
            cancel,
        }
    }

    pub(crate) fn push<F>(&mut self, chunk: &[u8], mut handle: F) -> Result<LineFlow>
    where
        F: FnMut(String) -> Result<LineFlow>,
    {
        let Some(new_total) = self.total.checked_add(chunk.len()) else {
            self.cancel.cancel();
            return Err(color_eyre::eyre::eyre!(
                "provider response exceeded the {} MiB limit",
                self.total_limit / (1024 * 1024)
            ));
        };
        if new_total > self.total_limit {
            self.cancel.cancel();
            return Err(color_eyre::eyre::eyre!(
                "provider response exceeded the {} MiB limit",
                self.total_limit / (1024 * 1024)
            ));
        }
        self.total = new_total;

        let mut start = 0;
        while let Some(relative_newline) = chunk[start..].iter().position(|byte| *byte == b'\n') {
            let newline = start + relative_newline;
            self.extend_pending(&chunk[start..newline])?;
            if handle(self.take_line()?)? == LineFlow::Stop {
                return Ok(LineFlow::Stop);
            }
            start = newline + 1;
        }
        self.extend_pending(&chunk[start..])?;
        Ok(LineFlow::Continue)
    }

    pub(crate) fn finish(&mut self) -> Result<Option<String>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        self.take_line().map(Some)
    }

    fn extend_pending(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.frame_limit.saturating_sub(self.pending.len()) {
            self.cancel.cancel();
            return Err(color_eyre::eyre::eyre!(
                "provider stream frame exceeded the {} MiB limit",
                self.frame_limit / (1024 * 1024)
            ));
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    fn take_line(&mut self) -> Result<String> {
        let mut bytes = std::mem::take(&mut self.pending);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8(bytes).context("provider stream contained invalid UTF-8")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

pub enum StreamEvent {
    Token(String),
    /// Reasoning tokens (grok CLI only) — display-only, never history.
    Thought(String),
    Finished,
    Error(String),
}

pub type StreamEventSender = mpsc::Sender<StreamEvent>;
pub type StreamEventReceiver = mpsc::Receiver<StreamEvent>;

pub fn stream_event_channel() -> (StreamEventSender, StreamEventReceiver) {
    mpsc::channel(STREAM_EVENT_CHANNEL_CAPACITY)
}

/// Delivers an event while allowing cancellation to release a producer that
/// is waiting for queue capacity. A closed receiver is a normal end state.
pub(crate) async fn send_stream_event(
    tx: &StreamEventSender,
    cancel: &CancellationToken,
    event: StreamEvent,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        result = tx.send(event) => result.is_ok(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Completion {
    Natural,
    Incomplete(String),
}

/// Outcome of parsing one SSE line. Any content arriving in the same chunk
/// as a completion reason is still delivered before that reason is handled.
#[derive(Debug, PartialEq, Eq)]
struct ParsedLine {
    tokens: Vec<String>,
    thoughts: Vec<String>,
    completion: Option<Completion>,
    malformed: bool,
}

impl ParsedLine {
    fn skip() -> Self {
        Self {
            tokens: Vec::new(),
            thoughts: Vec::new(),
            completion: None,
            malformed: false,
        }
    }
}

fn parse_sse_line(line: &str) -> ParsedLine {
    let line = line.trim();
    if line.is_empty() {
        return ParsedLine::skip();
    }

    let Some(rest) = line.strip_prefix("data:").map(str::trim_start) else {
        return ParsedLine::skip();
    };

    if rest == "[DONE]" {
        return ParsedLine {
            tokens: Vec::new(),
            thoughts: Vec::new(),
            completion: Some(Completion::Natural),
            malformed: false,
        };
    }

    let chunk: StreamChunk = match serde_json::from_str(rest) {
        Ok(chunk) => chunk,
        Err(_) => {
            return ParsedLine {
                tokens: Vec::new(),
                thoughts: Vec::new(),
                completion: None,
                malformed: true,
            }
        }
    };

    let mut parsed = ParsedLine::skip();
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                parsed.tokens.push(content);
            }
        }
        if let Some(thought) = choice
            .delta
            .reasoning_content
            .or(choice.delta.reasoning)
            .filter(|thought| !thought.is_empty())
        {
            parsed.thoughts.push(thought);
        }
        if let Some(reason) = choice.finish_reason {
            let completion = if reason.eq_ignore_ascii_case("stop") {
                Completion::Natural
            } else {
                Completion::Incomplete(reason)
            };
            if matches!(completion, Completion::Incomplete(_)) || parsed.completion.is_none() {
                parsed.completion = Some(completion);
            }
        }
    }
    parsed
}

fn warn_malformed(err_line: &str) {
    // Raw-mode TUI owns the terminal; writing there would corrupt the
    // display, so only log when stderr is redirected somewhere else.
    if !std::io::stderr().is_terminal() {
        eprintln!("warning: malformed SSE frame ({} bytes)", err_line.len());
    }
}

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    url: String,
    api_key: Option<String>,
    model: Arc<Mutex<String>>,
    provider_id: String,
    mode_label: String,
}

impl ApiClient {
    pub fn model(&self) -> String {
        self.model.lock().expect("API model lock poisoned").clone()
    }

    pub fn set_model(&self, model: String) {
        *self.model.lock().expect("API model lock poisoned") = model;
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn mode_label(&self) -> &str {
        &self.mode_label
    }

    pub fn new(api_key: String, model: String, timeout_secs: u64, url: String) -> Result<Self> {
        Self::openai_compatible(
            "xai-api",
            "xAI API",
            Some(api_key),
            model,
            timeout_secs,
            url,
        )
    }

    pub fn openai_compatible(
        provider_id: impl Into<String>,
        mode_label: impl Into<String>,
        api_key: Option<String>,
        model: String,
        timeout_secs: u64,
        url: String,
    ) -> Result<Self> {
        // read_timeout (time between chunks), not a total-request timeout:
        // a healthy stream may take longer than GROK_TIMEOUT_SECS to finish.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            url,
            api_key,
            model: Arc::new(Mutex::new(model)),
            provider_id: provider_id.into(),
            mode_label: mode_label.into(),
        })
    }

    pub async fn stream_chat(
        &self,
        messages: Vec<Message>,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) {
        let request_cancel = cancel.child_token();
        let result = self
            .stream_chat_inner(messages, request_cancel, tx.clone())
            .await;

        if cancel.is_cancelled() {
            return;
        }

        match result {
            Ok(()) => {
                let _ = tx.send(StreamEvent::Finished).await;
            }
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
            }
        }
    }

    async fn stream_chat_inner(
        &self,
        messages: Vec<Message>,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) -> Result<()> {
        let request = ChatRequest {
            model: self.model(),
            messages,
            stream: true,
        };

        let mut request_builder = self.client.post(&self.url).json(&request);
        if let Some(api_key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            request_builder = request_builder.header("Authorization", format!("Bearer {api_key}"));
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            response = request_builder.send() => response,
        };

        let response = match response {
            Ok(resp) => resp,
            Err(e) => {
                if e.is_timeout() {
                    return Err(color_eyre::eyre::eyre!("request timed out"));
                }
                return Err(e.into());
            }
        };

        let status = response.status();
        if !status.is_success() {
            let Some(body) = read_error_body_prefix(response, &cancel).await else {
                return Ok(());
            };
            let suffix = if body.truncated { "…" } else { "" };
            let detail = match status.as_u16() {
                401 => format!("Unauthorized — check credentials for {}", self.mode_label),
                429 => "Rate limited. Wait and try again.".to_string(),
                500 => "Server error. Try again later.".to_string(),
                code => {
                    if body.text.is_empty() {
                        format!("HTTP {code}")
                    } else {
                        format!("HTTP {code} — {}{suffix}", body.text)
                    }
                }
            };
            return Err(color_eyre::eyre::eyre!("[Error] {status} — {detail}"));
        }

        let mut byte_stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new(cancel.clone());

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                next = byte_stream.next() => next,
            };

            let Some(chunk) = chunk else {
                if let Some(line) = lines.finish()? {
                    let parsed = parse_sse_line(&line);
                    for token in parsed.tokens {
                        if !send_stream_event(&tx, &cancel, StreamEvent::Token(token)).await {
                            return Ok(());
                        }
                    }
                    for thought in parsed.thoughts {
                        if !send_stream_event(&tx, &cancel, StreamEvent::Thought(thought)).await {
                            return Ok(());
                        }
                    }
                    match parsed.completion {
                        Some(Completion::Natural) => return Ok(()),
                        Some(Completion::Incomplete(reason)) => {
                            return Err(incomplete_completion_error(&reason));
                        }
                        None => {}
                    }
                }
                return Err(color_eyre::eyre::eyre!(
                    "provider stream closed before a completion marker"
                ));
            };

            let chunk = chunk.context("error reading response stream")?;
            let mut events = Vec::new();
            let flow = lines.push(&chunk, |line| {
                let parsed = parse_sse_line(&line);
                if parsed.malformed {
                    warn_malformed(line.trim());
                }
                events.extend(parsed.tokens.into_iter().map(StreamEvent::Token));
                events.extend(parsed.thoughts.into_iter().map(StreamEvent::Thought));
                match parsed.completion {
                    Some(Completion::Natural) => return Ok(LineFlow::Stop),
                    Some(Completion::Incomplete(reason)) => {
                        return Err(incomplete_completion_error(&reason));
                    }
                    None => {}
                }
                Ok(LineFlow::Continue)
            });
            for event in events {
                if !send_stream_event(&tx, &cancel, event).await {
                    return Ok(());
                }
            }
            let flow = flow?;
            if flow == LineFlow::Stop {
                return Ok(());
            }
        }
    }
}

fn incomplete_completion_error(reason: &str) -> color_eyre::Report {
    color_eyre::eyre::eyre!(
        "provider response ended before natural completion (finish reason: {reason})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_event_channel_backpressures_until_the_receiver_advances() {
        let cancel = CancellationToken::new();
        let (tx, mut rx) = stream_event_channel();
        for index in 0..STREAM_EVENT_CHANNEL_CAPACITY {
            tx.send(StreamEvent::Token(index.to_string()))
                .await
                .unwrap();
        }

        let blocked_tx = tx.clone();
        let blocked_cancel = cancel.clone();
        let blocked = tokio::spawn(async move {
            send_stream_event(
                &blocked_tx,
                &blocked_cancel,
                StreamEvent::Token("after-capacity".into()),
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());

        assert!(rx.recv().await.is_some());
        assert!(tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("sender did not resume when capacity became available")
            .unwrap());

        let mut saw_final = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, StreamEvent::Token(token) if token == "after-capacity") {
                saw_final = true;
            }
        }
        assert!(saw_final);
    }

    #[tokio::test]
    async fn blocked_stream_sender_stops_when_cancelled_or_receiver_closes() {
        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(StreamEvent::Token("queued".into())).await.unwrap();
        let blocked_tx = tx.clone();
        let blocked_cancel = cancel.clone();
        let blocked = tokio::spawn(async move {
            send_stream_event(
                &blocked_tx,
                &blocked_cancel,
                StreamEvent::Token("blocked".into()),
            )
            .await
        });
        cancel.cancel();
        assert!(!tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("cancelled sender remained blocked")
            .unwrap());

        drop(rx);
        assert!(
            !send_stream_event(
                &tx,
                &CancellationToken::new(),
                StreamEvent::Token("closed".into()),
            )
            .await
        );
    }

    #[test]
    fn line_buffer_preserves_utf8_split_between_chunks() {
        let cancel = CancellationToken::new();
        let mut lines = SseLineBuffer::with_limits(cancel, 256, 1024);
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"héllo\"}}]}\n";
        let split = frame.find('é').unwrap() + 1;
        let mut output = Vec::new();

        lines
            .push(&frame.as_bytes()[..split], |line| {
                output.push(line);
                Ok(LineFlow::Continue)
            })
            .unwrap();
        assert!(output.is_empty());
        lines
            .push(&frame.as_bytes()[split..], |line| {
                output.push(line);
                Ok(LineFlow::Continue)
            })
            .unwrap();
        assert_eq!(output, vec![frame.trim_end().to_string()]);
    }

    #[test]
    fn line_buffer_cancels_work_when_either_limit_is_exceeded() {
        let parent = CancellationToken::new();
        let frame_cancel = parent.child_token();
        let mut frame_limited = SseLineBuffer::with_limits(frame_cancel.clone(), 4, 32);
        assert!(frame_limited
            .push(b"1234\n", |_| Ok(LineFlow::Continue))
            .is_ok());
        assert!(frame_limited
            .push(b"12345", |_| Ok(LineFlow::Continue))
            .is_err());
        assert!(frame_cancel.is_cancelled());
        assert!(!parent.is_cancelled());

        let total_cancel = parent.child_token();
        let mut total_limited = SseLineBuffer::with_limits(total_cancel.clone(), 8, 5);
        assert!(total_limited
            .push(b"a\nb\n", |_| Ok(LineFlow::Continue))
            .is_ok());
        assert!(total_limited
            .push(b"cc", |_| Ok(LineFlow::Continue))
            .is_err());
        assert!(total_cancel.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[test]
    fn keep_alive_and_non_data_lines_are_skipped() {
        assert_eq!(parse_sse_line(""), ParsedLine::skip());
        assert_eq!(parse_sse_line("\n"), ParsedLine::skip());
        assert_eq!(parse_sse_line(": ping"), ParsedLine::skip());
    }

    #[test]
    fn done_sentinel_ends_stream() {
        let parsed = parse_sse_line("data: [DONE]");
        assert_eq!(parsed.completion, Some(Completion::Natural));
        assert!(parsed.tokens.is_empty());
    }

    #[test]
    fn data_field_without_a_space_is_valid_sse() {
        assert_eq!(
            parse_sse_line("data:[DONE]").completion,
            Some(Completion::Natural)
        );
    }

    #[test]
    fn content_token_is_extracted() {
        let parsed = parse_sse_line(r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#);
        assert_eq!(parsed.tokens, vec!["Hello".to_string()]);
        assert_eq!(parsed.completion, None);
    }

    #[test]
    fn empty_delta_and_null_content_are_skipped() {
        let empty = parse_sse_line(r#"data: {"choices":[{"delta":{}}]}"#);
        assert!(empty.tokens.is_empty() && empty.completion.is_none() && !empty.malformed);

        let null = parse_sse_line(r#"data: {"choices":[{"delta":{"content":null}}]}"#);
        assert!(null.tokens.is_empty() && null.completion.is_none() && !null.malformed);
    }

    #[test]
    fn finish_reason_stop_ends_stream_without_losing_same_chunk_content() {
        let parsed = parse_sse_line(
            r#"data: {"choices":[{"delta":{"content":"bye"},"finish_reason":"stop"}]}"#,
        );
        assert_eq!(parsed.tokens, vec!["bye".to_string()]);
        assert_eq!(parsed.completion, Some(Completion::Natural));
    }

    #[test]
    fn non_natural_finish_reason_is_incomplete() {
        let parsed = parse_sse_line(
            r#"data: {"choices":[{"delta":{"content":"last"},"finish_reason":"length"}]}"#,
        );
        assert_eq!(parsed.tokens, vec!["last".to_string()]);
        assert_eq!(
            parsed.completion,
            Some(Completion::Incomplete("length".to_string()))
        );
    }

    #[test]
    fn malformed_json_is_flagged_not_fatal() {
        let parsed = parse_sse_line("data: {not json");
        assert!(parsed.malformed);
        assert!(parsed.tokens.is_empty() && parsed.completion.is_none());
    }

    #[tokio::test]
    async fn streams_tokens_from_mock_sse_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let api = ApiClient::new(
            "test-key".to_string(),
            "grok-4.5".to_string(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();

        let (tx, mut rx) = crate::api::stream_event_channel();
        let messages = vec![Message {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        api.stream_chat(messages, CancellationToken::new(), tx)
            .await;

        let mut text = String::new();
        let mut finished = false;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(t) => text.push_str(&t),
                StreamEvent::Thought(_) => {}
                StreamEvent::Finished => {
                    finished = true;
                    break;
                }
                StreamEvent::Error(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(text, "Hello");
        assert!(finished);
    }

    #[tokio::test]
    async fn partial_output_with_length_finish_reason_ends_with_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        api.stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;

        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "partial"));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(error)) if error.contains("length")
        ));
        assert!(rx.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn partial_output_followed_by_transport_close_ends_with_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        api.stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "partial"));
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::Error(error)) if error.contains("closed"))
        );
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_headers_returns_promptly_without_events() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let _ = accepted_tx.send(());
            std::future::pending::<()>().await;
        });
        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            30,
            format!("http://{addr}"),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = crate::api::stream_event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { api.stream_chat(Vec::new(), cancel, tx).await }
        });
        accepted_rx.await.unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("request did not stop after cancellation")
            .unwrap();
        assert!(rx.recv().await.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_while_reading_an_error_body_returns_promptly() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 500 Error\r\nConnection: keep-alive\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}"))
            .send()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let read = tokio::spawn({
            let cancel = cancel.clone();
            async move { read_error_body_prefix(response, &cancel).await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), read)
            .await
            .expect("error-body read did not stop after cancellation")
            .unwrap()
            .is_none());
        server.abort();
    }

    #[tokio::test]
    async fn unsuccessful_response_body_is_bounded_before_display() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = format!("{}SHOULD_NOT_APPEAR", "a".repeat(ERROR_BODY_LIMIT));
            let headers = format!(
                "HTTP/1.1 418 I'm a teapot\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            let _ = socket.write_all(body.as_bytes()).await;
        });

        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        api.stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        let Some(StreamEvent::Error(error)) = rx.recv().await else {
            panic!("expected a bounded error event")
        };
        assert!(!error.contains("SHOULD_NOT_APPEAR"));
        assert!(error.contains('…'));
        assert!(error.len() < ERROR_BODY_LIMIT + 256);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_stream_frame_reports_an_error_without_finishing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = vec![b'a'; STREAM_FRAME_LIMIT + 1];
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
        });

        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        api.stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        let Some(StreamEvent::Error(error)) = rx.recv().await else {
            panic!("expected a frame-limit error event")
        };
        assert!(error.contains("stream frame exceeded"));
        assert!(rx.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn credentialed_post_is_not_replayed_after_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer test-key"));
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/redirected\r\nContent-Length: 0\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });

        let api = ApiClient::new(
            "test-key".into(),
            "grok-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        api.stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Error(_))));
        assert!(
            !server.await.unwrap(),
            "redirected POST was unexpectedly replayed"
        );
    }
}
