use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::api::{
    read_error_body_prefix, send_stream_event, LineFlow, Message, SseLineBuffer, StreamEvent,
    StreamEventSender,
};

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    stream: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum AnthropicLine {
    Token(String),
    Thought(String),
    StopReason(String),
    Finished,
    Error(String),
    Skip,
}

fn parse_sse_line(line: &str) -> AnthropicLine {
    let Some(data) = line.trim().strip_prefix("data:").map(str::trim_start) else {
        return AnthropicLine::Skip;
    };
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return AnthropicLine::Skip;
    };
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "content_block_delta" => {
            let delta = value.get("delta");
            match delta
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => delta
                    .and_then(|delta| delta.get("text"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| AnthropicLine::Token(text.to_string()))
                    .unwrap_or(AnthropicLine::Skip),
                "thinking_delta" => delta
                    .and_then(|delta| delta.get("thinking"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| AnthropicLine::Thought(text.to_string()))
                    .unwrap_or(AnthropicLine::Skip),
                _ => AnthropicLine::Skip,
            }
        }
        "message_delta" => value
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
            .map(|reason| AnthropicLine::StopReason(reason.to_string()))
            .unwrap_or(AnthropicLine::Skip),
        "message_stop" => AnthropicLine::Finished,
        "error" => AnthropicLine::Error(
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Anthropic API reported an error")
                .to_string(),
        ),
        _ => AnthropicLine::Skip,
    }
}

#[derive(Clone)]
pub struct AnthropicApiClient {
    client: Client,
    api_key: String,
    model: Arc<Mutex<String>>,
    url: String,
}

impl AnthropicApiClient {
    pub fn new(api_key: String, model: String, timeout_secs: u64, url: String) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build Anthropic HTTP client")?;
        Ok(Self {
            client,
            api_key,
            model: Arc::new(Mutex::new(model)),
            url,
        })
    }

    pub fn model(&self) -> String {
        self.model
            .lock()
            .expect("Anthropic model lock poisoned")
            .clone()
    }

    pub fn set_model(&self, model: String) {
        *self.model.lock().expect("Anthropic model lock poisoned") = model;
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
            Err(error) => {
                let _ = tx
                    .send(StreamEvent::Error(format!("[Error] {error}")))
                    .await;
            }
        }
    }

    async fn stream_chat_inner(
        &self,
        messages: Vec<Message>,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) -> Result<()> {
        let messages = messages
            .into_iter()
            .filter(|message| message.role == "user" || message.role == "assistant")
            .collect();
        let request = self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&AnthropicRequest {
                model: self.model(),
                messages,
                max_tokens: 8192,
                stream: true,
            })
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            response = request => response.context("Anthropic API request failed")?,
        };
        let status = response.status();
        if !status.is_success() {
            let Some(body) = read_error_body_prefix(response, &cancel).await else {
                return Ok(());
            };
            let suffix = if body.truncated { "…" } else { "" };
            return Err(color_eyre::eyre::eyre!(
                "Anthropic API returned {status}: {}{suffix}",
                body.text
            ));
        }

        let mut stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new(cancel.clone());
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.context("error reading Anthropic stream")?;
            let mut events = Vec::new();
            let flow = lines.push(&chunk, |line| match parse_sse_line(&line) {
                AnthropicLine::Token(text) => {
                    events.push(StreamEvent::Token(text));
                    Ok(LineFlow::Continue)
                }
                AnthropicLine::Thought(text) => {
                    events.push(StreamEvent::Thought(text));
                    Ok(LineFlow::Continue)
                }
                AnthropicLine::StopReason(reason) => {
                    if is_natural_stop_reason(&reason) {
                        Ok(LineFlow::Continue)
                    } else {
                        Err(incomplete_completion_error(&reason))
                    }
                }
                AnthropicLine::Finished => Ok(LineFlow::Stop),
                AnthropicLine::Error(message) => Err(color_eyre::eyre::eyre!(message)),
                AnthropicLine::Skip => Ok(LineFlow::Continue),
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
        if let Some(line) = lines.finish()? {
            match parse_sse_line(&line) {
                AnthropicLine::Token(text) => {
                    if !send_stream_event(&tx, &cancel, StreamEvent::Token(text)).await {
                        return Ok(());
                    }
                }
                AnthropicLine::Thought(text) => {
                    if !send_stream_event(&tx, &cancel, StreamEvent::Thought(text)).await {
                        return Ok(());
                    }
                }
                AnthropicLine::StopReason(reason) => {
                    if !is_natural_stop_reason(&reason) {
                        return Err(incomplete_completion_error(&reason));
                    }
                }
                AnthropicLine::Finished => return Ok(()),
                AnthropicLine::Error(message) => {
                    return Err(color_eyre::eyre::eyre!(message));
                }
                AnthropicLine::Skip => {}
            }
        }
        Err(color_eyre::eyre::eyre!(
            "Anthropic stream closed before message_stop"
        ))
    }
}

fn is_natural_stop_reason(reason: &str) -> bool {
    matches!(reason, "end_turn" | "stop_sequence")
}

fn incomplete_completion_error(reason: &str) -> color_eyre::Report {
    color_eyre::eyre::eyre!(
        "Anthropic response ended before natural completion (stop reason: {reason})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anthropic_stream_events() {
        assert_eq!(
            parse_sse_line(
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#
            ),
            AnthropicLine::Token("hi".into())
        );
        assert_eq!(
            parse_sse_line(
                r#"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}"#
            ),
            AnthropicLine::Thought("hmm".into())
        );
        assert_eq!(
            parse_sse_line(r#"data: {"type":"message_stop"}"#),
            AnthropicLine::Finished
        );
        assert_eq!(
            parse_sse_line(r#"data:{"type":"message_stop"}"#),
            AnthropicLine::Finished
        );
        assert_eq!(
            parse_sse_line(
                r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#
            ),
            AnthropicLine::StopReason("max_tokens".into())
        );
    }

    #[tokio::test]
    async fn streams_from_mock_messages_api_with_required_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("x-api-key: test-key"));
            assert!(request.contains("anthropic-version: 2023-06-01"));
            assert!(request.contains(r#""model":"claude-test""#));
            let body = concat!(
                "data:{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
                "data:{\"type\":\"message_stop\"}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let client = AnthropicApiClient::new(
            "test-key".into(),
            "claude-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        client
            .stream_chat(
                vec![Message {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                CancellationToken::new(),
                tx,
            )
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "hello"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Finished)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn partial_output_with_non_natural_stop_reason_ends_with_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = concat!(
                "data:{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                "data:{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
                "data:{\"type\":\"message_stop\"}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let client = AnthropicApiClient::new(
            "test-key".into(),
            "claude-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        client
            .stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "partial"));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(error)) if error.contains("max_tokens")
        ));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_headers_returns_without_events() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let _ = accepted_tx.send(());
            std::future::pending::<()>().await;
        });
        let client = AnthropicApiClient::new(
            "test-key".into(),
            "claude-test".into(),
            30,
            format!("http://{addr}"),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let (tx, mut rx) = crate::api::stream_event_channel();
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { client.stream_chat(Vec::new(), cancel, tx).await }
        });
        accepted_rx.await.unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("Anthropic request did not stop after cancellation")
            .unwrap();
        assert!(rx.recv().await.is_none());
        server.abort();
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
            assert!(request.contains("x-api-key: test-key"));
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/redirected\r\nContent-Length: 0\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });

        let client = AnthropicApiClient::new(
            "test-key".into(),
            "claude-test".into(),
            5,
            format!("http://{addr}"),
        )
        .unwrap();
        let (tx, mut rx) = crate::api::stream_event_channel();
        client
            .stream_chat(Vec::new(), CancellationToken::new(), tx)
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Error(_))));
        assert!(
            !server.await.unwrap(),
            "redirected POST was unexpectedly replayed"
        );
    }
}
