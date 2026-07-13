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
struct GooglePart {
    text: String,
}

#[derive(Serialize)]
struct GoogleContent {
    role: String,
    parts: Vec<GooglePart>,
}

#[derive(Serialize)]
struct GoogleRequest {
    contents: Vec<GoogleContent>,
}

#[derive(Debug, PartialEq, Eq)]
struct GoogleChunk {
    text: String,
    completion: Option<GoogleCompletion>,
}

#[derive(Debug, PartialEq, Eq)]
enum GoogleCompletion {
    Natural,
    Incomplete(String),
}

fn parse_sse_line(line: &str) -> Option<GoogleChunk> {
    let data = line.trim().strip_prefix("data:").map(str::trim_start)?;
    let value = serde_json::from_str::<Value>(data).ok()?;
    if let Some(reason) = value
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Some(GoogleChunk {
            text: String::new(),
            completion: Some(GoogleCompletion::Incomplete(reason.to_string())),
        });
    }
    let candidate = value.get("candidates")?.as_array()?.first()?;
    let text = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default();
    let completion = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map(|reason| {
            if reason.eq_ignore_ascii_case("STOP") {
                GoogleCompletion::Natural
            } else {
                GoogleCompletion::Incomplete(reason.to_string())
            }
        });
    Some(GoogleChunk { text, completion })
}

#[derive(Clone)]
pub struct GoogleApiClient {
    client: Client,
    api_key: String,
    model: Arc<Mutex<String>>,
    base_url: String,
}

impl GoogleApiClient {
    pub fn new(
        api_key: String,
        model: String,
        timeout_secs: u64,
        base_url: String,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build Google AI HTTP client")?;
        Ok(Self {
            client,
            api_key,
            model: Arc::new(Mutex::new(model)),
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    pub fn model(&self) -> String {
        self.model
            .lock()
            .expect("Google model lock poisoned")
            .clone()
    }

    pub fn set_model(&self, model: String) {
        *self.model.lock().expect("Google model lock poisoned") = model;
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
        let contents = messages
            .into_iter()
            .filter_map(|message| {
                let role = match message.role.as_str() {
                    "user" => "user",
                    "assistant" => "model",
                    _ => return None,
                };
                Some(GoogleContent {
                    role: role.to_string(),
                    parts: vec![GooglePart {
                        text: message.content,
                    }],
                })
            })
            .collect();
        let model = self.model();
        let model = model.strip_prefix("models/").unwrap_or(&model);
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        );
        let request = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.api_key)
            .json(&GoogleRequest { contents })
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            response = request => response.context("Google AI API request failed")?,
        };
        let status = response.status();
        if !status.is_success() {
            let Some(body) = read_error_body_prefix(response, &cancel).await else {
                return Ok(());
            };
            let suffix = if body.truncated { "…" } else { "" };
            return Err(color_eyre::eyre::eyre!(
                "Google AI API returned {status}: {}{suffix}",
                body.text
            ));
        }
        let mut stream = response.bytes_stream();
        let mut lines = SseLineBuffer::new(cancel.clone());
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.context("error reading Google AI stream")?;
            let mut events = Vec::new();
            let flow = lines.push(&chunk, |line| {
                if let Some(parsed) = parse_sse_line(&line) {
                    if !parsed.text.is_empty() {
                        events.push(StreamEvent::Token(parsed.text));
                    }
                    match parsed.completion {
                        Some(GoogleCompletion::Natural) => return Ok(LineFlow::Stop),
                        Some(GoogleCompletion::Incomplete(reason)) => {
                            return Err(incomplete_completion_error(&reason));
                        }
                        None => {}
                    }
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
        if let Some(line) = lines.finish()? {
            let Some(parsed) = parse_sse_line(&line) else {
                return Err(color_eyre::eyre::eyre!(
                    "Google AI stream closed before a finish reason"
                ));
            };
            if !parsed.text.is_empty()
                && !send_stream_event(&tx, &cancel, StreamEvent::Token(parsed.text)).await
            {
                return Ok(());
            }
            match parsed.completion {
                Some(GoogleCompletion::Natural) => return Ok(()),
                Some(GoogleCompletion::Incomplete(reason)) => {
                    return Err(incomplete_completion_error(&reason));
                }
                None => {}
            }
        }
        Err(color_eyre::eyre::eyre!(
            "Google AI stream closed before a finish reason"
        ))
    }
}

fn incomplete_completion_error(reason: &str) -> color_eyre::Report {
    color_eyre::eyre::eyre!(
        "Google AI response ended before natural completion (finish reason: {reason})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_google_stream_chunk() {
        assert_eq!(
            parse_sse_line(
                r#"data: {"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#
            ),
            Some(GoogleChunk {
                text: "hello".into(),
                completion: Some(GoogleCompletion::Natural)
            })
        );
        assert_eq!(
            parse_sse_line(
                r#"data:{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}"#
            )
            .unwrap()
            .completion,
            Some(GoogleCompletion::Natural)
        );
        assert_eq!(
            parse_sse_line(
                r#"data:{"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}"#
            ),
            Some(GoogleChunk {
                text: "partial".into(),
                completion: Some(GoogleCompletion::Incomplete("MAX_TOKENS".into()))
            })
        );
        assert_eq!(
            parse_sse_line(r#"data:{"promptFeedback":{"blockReason":"SAFETY"}}"#),
            Some(GoogleChunk {
                text: String::new(),
                completion: Some(GoogleCompletion::Incomplete("SAFETY".into()))
            })
        );
    }

    #[tokio::test]
    async fn streams_from_mock_google_api_without_putting_key_in_url() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request
                .starts_with("POST /models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("x-goog-api-key: test-key"));
            assert!(!request.contains("key=test-key"));
            let body = "data:{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}]}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let client = GoogleApiClient::new(
            "test-key".into(),
            "models/gemini-test".into(),
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
    async fn partial_output_with_non_natural_finish_reason_ends_with_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = "data:{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"MAX_TOKENS\"}]}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let client = GoogleApiClient::new(
            "test-key".into(),
            "gemini-test".into(),
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
            Some(StreamEvent::Error(error)) if error.contains("MAX_TOKENS")
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
        let client = GoogleApiClient::new(
            "test-key".into(),
            "gemini-test".into(),
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
            .expect("Google AI request did not stop after cancellation")
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
            assert!(request.contains("x-goog-api-key: test-key"));
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/redirected\r\nContent-Length: 0\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_millis(250), listener.accept())
                .await
                .is_ok()
        });

        let client = GoogleApiClient::new(
            "test-key".into(),
            "gemini-test".into(),
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
