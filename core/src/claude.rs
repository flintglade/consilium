use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::api::{send_stream_event, StreamEvent, StreamEventSender};
use crate::child_io::{
    application_data_dir, drain_stderr_tail, find_program, is_canonical_session_id,
    read_bounded_line, user_home_dir, CliProvider, ManagedChild, CLI_STDOUT_LINE_LIMIT,
    CLI_STDOUT_TOTAL_LIMIT,
};

pub fn find_claude() -> Option<PathBuf> {
    find_program(CliProvider::Claude)
}

fn data_dir() -> PathBuf {
    application_data_dir()
}

fn neutral_cwd() -> Result<PathBuf> {
    let path = data_dir().join("claude-chat-cwd");
    std::fs::create_dir_all(&path).context("failed to create Claude chat workspace")?;
    Ok(path)
}

#[derive(Debug, PartialEq, Eq)]
enum ClaudeLine {
    Token(String),
    Thought(String),
    Session(String),
    Finished(Option<String>),
    Error(String),
    Skip,
}

fn parse_line(line: &str) -> ClaudeLine {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return ClaudeLine::Skip;
    };
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        return ClaudeLine::Error(
            value
                .get("result")
                .or_else(|| value.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Claude Code reported an error")
                .to_string(),
        );
    }
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "system" => value
            .get("session_id")
            .and_then(Value::as_str)
            .map(|id| ClaudeLine::Session(id.to_string()))
            .unwrap_or(ClaudeLine::Skip),
        "stream_event" => {
            let event = value.get("event");
            if event
                .and_then(|event| event.get("type"))
                .and_then(Value::as_str)
                == Some("content_block_start")
            {
                let block = event.and_then(|event| event.get("content_block"));
                if block
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    == Some("tool_use")
                {
                    return block
                        .and_then(|block| block.get("name"))
                        .and_then(Value::as_str)
                        .map(|name| ClaudeLine::Thought(format!("Using tool: {name}\n")))
                        .unwrap_or_else(|| ClaudeLine::Thought("Using a tool\n".to_string()));
                }
            }
            let delta = event.and_then(|event| event.get("delta"));
            match delta
                .and_then(|delta| delta.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => delta
                    .and_then(|delta| delta.get("text"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| ClaudeLine::Token(text.to_string()))
                    .unwrap_or(ClaudeLine::Skip),
                "thinking_delta" => delta
                    .and_then(|delta| delta.get("thinking"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| ClaudeLine::Thought(text.to_string()))
                    .unwrap_or(ClaudeLine::Skip),
                _ => ClaudeLine::Skip,
            }
        }
        "result" => {
            let result = value
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            if value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                ClaudeLine::Error(
                    result.unwrap_or_else(|| "Claude Code reported a failed turn".to_string()),
                )
            } else {
                ClaudeLine::Finished(result)
            }
        }
        "error" => ClaudeLine::Error(
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Claude Code reported an error")
                .to_string(),
        ),
        _ => ClaudeLine::Skip,
    }
}

#[derive(Clone)]
pub struct ClaudeBackend {
    program: PathBuf,
    model: Arc<Mutex<Option<String>>>,
    effort: Arc<Mutex<Option<String>>>,
    session_id: Arc<Mutex<Option<String>>>,
    session_agent: Arc<Mutex<Option<bool>>>,
}

impl ClaudeBackend {
    pub fn new(program: PathBuf, model: Option<String>) -> Self {
        Self {
            program,
            model: Arc::new(Mutex::new(model)),
            effort: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
            session_agent: Arc::new(Mutex::new(None)),
        }
    }

    pub fn model(&self) -> Option<String> {
        self.model
            .lock()
            .expect("Claude model lock poisoned")
            .clone()
    }

    pub fn set_model(&self, model: Option<String>) {
        *self.model.lock().expect("Claude model lock poisoned") = model;
        self.reset_session();
    }

    pub fn effort(&self) -> Option<String> {
        self.effort
            .lock()
            .expect("Claude effort lock poisoned")
            .clone()
    }

    pub fn set_effort(&self, effort: Option<String>) {
        *self.effort.lock().expect("Claude effort lock poisoned") = effort;
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .expect("Claude session lock poisoned")
            .clone()
    }

    pub fn set_session(&self, session_id: Option<String>) {
        *self
            .session_id
            .lock()
            .expect("Claude session lock poisoned") = session_id;
    }

    pub fn set_session_context(&self, session_id: Option<String>, agent: Option<bool>) {
        self.set_session(session_id);
        *self
            .session_agent
            .lock()
            .expect("Claude session mode lock poisoned") = agent;
    }

    pub fn reset_session(&self) {
        self.set_session(None);
        *self
            .session_agent
            .lock()
            .expect("Claude session mode lock poisoned") = None;
    }

    pub(crate) fn prepare_mode(&self, agent: bool) {
        let previous = *self
            .session_agent
            .lock()
            .expect("Claude session mode lock poisoned");
        if self.session_id().is_some() && previous != Some(agent) {
            self.reset_session();
        }
        *self
            .session_agent
            .lock()
            .expect("Claude session mode lock poisoned") = Some(agent);
    }

    pub async fn stream_chat(
        &self,
        prompt: String,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) {
        let result = self
            .stream_chat_inner(prompt, agent, cancel.clone(), tx.clone())
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
        prompt: String,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) -> Result<()> {
        self.prepare_mode(agent);
        let mut command = tokio::process::Command::new(&self.program);
        command.env_remove("ANTHROPIC_API_KEY");
        command.args([
            "--print",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
            "--safe-mode",
        ]);
        if agent {
            command.args(["--permission-mode", "auto"]);
            if let Some(home) = user_home_dir() {
                command.current_dir(home);
            }
        } else {
            command.args([
                "--tools",
                "",
                "--permission-mode",
                "plan",
                "--system-prompt",
                "You are Claude inside Consilium chat mode. Answer conversationally. Do not inspect files or use tools.",
            ]);
            command.current_dir(neutral_cwd()?);
        }
        if let Some(model) = self.model() {
            command.args(["--model", &model]);
        }
        if let Some(effort) = self.effort() {
            command.args(["--effort", &effort]);
        }
        if let Some(session_id) = self.session_id() {
            if !is_canonical_session_id(&session_id) {
                return Err(color_eyre::eyre::eyre!(
                    "The saved Claude session identifier is invalid. Start a new conversation and try again."
                ));
            }
            command.args(["--resume", &session_id]);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = ManagedChild::spawn(&mut command).context("failed to start Claude Code")?;
        let stdout = child.take_stdout().expect("Claude stdout was piped");
        let stderr = child.take_stderr().expect("Claude stderr was piped");
        let stderr_task = drain_stderr_tail(stderr);
        let mut stdin = child.take_stdin().expect("Claude stdin was piped");
        let write_result = tokio::select! {
            _ = cancel.cancelled() => {
                child.terminate().await;
                let _ = stderr_task.await;
                return Ok(());
            }
            result = async {
                stdin.write_all(prompt.as_bytes()).await?;
                stdin.shutdown().await
            } => result,
        };
        if let Err(error) = write_result {
            child.terminate().await;
            let _ = stderr_task.await;
            return Err(error).context("failed to send the prompt to Claude Code");
        }
        drop(stdin);
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut total_output = 0usize;
        let mut saw_token = false;
        let mut saw_finished = false;
        let mut fallback_result = None;

        loop {
            let read_result = tokio::select! {
                _ = cancel.cancelled() => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Ok(());
                }
                read = read_bounded_line(&mut reader, &mut line, CLI_STDOUT_LINE_LIMIT) => read,
            };
            let read = match read_result {
                Ok(read) => read,
                Err(error) => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "error reading Claude Code output: {error}"
                    ));
                }
            };
            if read == 0 {
                break;
            }
            total_output = match total_output.checked_add(read) {
                Some(total) if total <= CLI_STDOUT_TOTAL_LIMIT => total,
                _ => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "Claude Code output exceeded the 32 MiB size limit"
                    ));
                }
            };
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(error) => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "Claude Code output was not valid UTF-8: {error}"
                    ));
                }
            };
            match parse_line(line) {
                ClaudeLine::Token(text) => {
                    saw_token = true;
                    if !send_stream_event(&tx, &cancel, StreamEvent::Token(text)).await {
                        cancel.cancel();
                        child.terminate().await;
                        let _ = stderr_task.await;
                        return Ok(());
                    }
                }
                ClaudeLine::Thought(text) => {
                    if !send_stream_event(&tx, &cancel, StreamEvent::Thought(text)).await {
                        cancel.cancel();
                        child.terminate().await;
                        let _ = stderr_task.await;
                        return Ok(());
                    }
                }
                ClaudeLine::Session(id) => {
                    if !is_canonical_session_id(&id) {
                        child.terminate().await;
                        let _ = stderr_task.await;
                        return Err(color_eyre::eyre::eyre!(
                            "Claude Code returned an invalid session identifier"
                        ));
                    }
                    self.set_session(Some(id));
                }
                ClaudeLine::Finished(result) => {
                    fallback_result = result;
                    saw_finished = true;
                }
                ClaudeLine::Error(message) => {
                    child.terminate().await;
                    let stderr = stderr_task.await.unwrap_or_default();
                    return Err(color_eyre::eyre::eyre!(
                        "{}{}",
                        message,
                        if stderr.is_empty() {
                            String::new()
                        } else {
                            format!(" — {stderr}")
                        }
                    ));
                }
                ClaudeLine::Skip => {}
            }
        }

        let status = child.wait().await.context("Claude Code did not exit")?;
        let stderr = stderr_task.await.unwrap_or_default();
        if !status.success() {
            return Err(color_eyre::eyre::eyre!(
                "Claude Code exited with status {status}{}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(" — {stderr}")
                }
            ));
        }
        if !saw_finished {
            return Err(color_eyre::eyre::eyre!(
                "Claude Code closed before reporting a completed turn"
            ));
        }
        if !saw_token {
            if let Some(result) = fallback_result {
                if !send_stream_event(&tx, &cancel, StreamEvent::Token(result)).await {
                    cancel.cancel();
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_text_thinking_session_and_result() {
        assert_eq!(
            parse_line(r#"{"type":"system","session_id":"session-1"}"#),
            ClaudeLine::Session("session-1".into())
        );
        assert_eq!(
            parse_line(
                r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello"}}}"#
            ),
            ClaudeLine::Token("hello".into())
        );
        assert_eq!(
            parse_line(
                r#"{"type":"stream_event","event":{"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#
            ),
            ClaudeLine::Thought("hmm".into())
        );
        assert_eq!(
            parse_line(
                r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Read"}}}"#
            ),
            ClaudeLine::Thought("Using tool: Read\n".into())
        );
        assert_eq!(
            parse_line(r#"{"type":"result","result":"fallback"}"#),
            ClaudeLine::Finished(Some("fallback".into()))
        );
        assert_eq!(
            parse_line(r#"{"type":"result","is_error":true,"result":"denied"}"#),
            ClaudeLine::Error("denied".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_claude_streams_and_resumes_session() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("consilium-claude-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        let arguments = dir.join("args.txt");
        let prompts = dir.join("prompts.txt");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncat >> '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"22222222-2222-4222-8222-222222222222\"}}' '{{\"type\":\"stream_event\",\"event\":{{\"delta\":{{\"type\":\"text_delta\",\"text\":\"hello\"}}}}}}' '{{\"type\":\"result\",\"result\":\"hello\"}}'",
            arguments.display(),
            prompts.display()
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = ClaudeBackend::new(script, Some("sonnet".into()));
        for prompt in ["first", "second"] {
            let (tx, mut rx) = crate::api::stream_event_channel();
            backend
                .stream_chat(prompt.into(), false, CancellationToken::new(), tx)
                .await;
            assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "hello"));
            assert!(matches!(rx.recv().await, Some(StreamEvent::Finished)));
        }
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat("agent turn".into(), true, CancellationToken::new(), tx)
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(_))));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Finished)));
        let args = std::fs::read_to_string(arguments).unwrap();
        assert!(args.contains("--output-format\nstream-json"));
        assert!(args.contains("--model\nsonnet"));
        assert!(args.contains("--resume\n22222222-2222-4222-8222-222222222222"));
        assert!(args.contains("--permission-mode\nauto"));
        assert!(!args.contains("dangerously-skip-permissions"));
        assert!(!args.contains("first"));
        assert!(!args.contains("second"));
        assert!(!args.contains("agent turn"));
        assert_eq!(
            std::fs::read_to_string(prompts).unwrap(),
            "firstsecondagent turn"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn switching_between_chat_and_agent_starts_a_fresh_session() {
        let backend = ClaudeBackend::new(PathBuf::from("claude"), None);
        backend.set_session_context(
            Some("22222222-2222-4222-8222-222222222222".into()),
            Some(false),
        );
        backend.prepare_mode(true);
        assert_eq!(backend.session_id(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newline_free_oversized_claude_output_is_rejected() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-claude-output-limit-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(file, "#!/bin/sh\nhead -c 1048577 /dev/zero | tr '\\000' x").unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = ClaudeBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat("hello".into(), false, CancellationToken::new(), tx)
            .await;
        let Some(StreamEvent::Error(error)) = rx.recv().await else {
            panic!("expected an output-limit error")
        };
        assert!(error.contains("output line exceeded"), "{error}");
        assert!(rx.recv().await.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_restored_session_is_rejected_before_launch() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-claude-invalid-session-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("launched");
        let script = dir.join("claude");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(file, "#!/bin/sh\ntouch '{}'", marker.display()).unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = ClaudeBackend::new(script, None);
        backend.set_session_context(Some("--continue".into()), Some(false));
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat("hello".into(), false, CancellationToken::new(), tx)
            .await;
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(message)) if message.contains("invalid")
        ));
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
