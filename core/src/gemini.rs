use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Context, Result};
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use crate::api::{send_stream_event, Message, StreamEvent, StreamEventSender};
use crate::child_io::{
    drain_stderr_tail, find_program, read_bounded_line, user_home_dir, CliProvider, ManagedChild,
};
use crate::cli::ImageAttachment;

const PRINT_TIMEOUT: &str = "5m";
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_WINDOWS_BATCH_PROMPT_BYTES: usize = 6 * 1024;
const MAX_WINDOWS_NATIVE_PROMPT_BYTES: usize = 24 * 1024;
const MAX_UNIX_PROMPT_BYTES: usize = 96 * 1024;

fn is_windows_batch_launcher(program: &Path) -> bool {
    program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
}

fn prompt_argument_limit(program: &Path, is_windows: bool) -> usize {
    if !is_windows {
        MAX_UNIX_PROMPT_BYTES
    } else if is_windows_batch_launcher(program) {
        MAX_WINDOWS_BATCH_PROMPT_BYTES
    } else {
        MAX_WINDOWS_NATIVE_PROMPT_BYTES
    }
}

fn validate_prompt_argument(program: &Path, prompt: &str, is_windows: bool) -> Result<()> {
    let limit = prompt_argument_limit(program, is_windows);
    if prompt.len() > limit {
        return Err(color_eyre::eyre::eyre!(
            "The Antigravity CLI prompt is too long for this installed launcher ({} bytes; limit {} KiB). Start a shorter conversation or use the Google AI API connector for long context.",
            prompt.len(),
            limit / 1024
        ));
    }
    Ok(())
}

/// Locate Google's supported consumer CLI. Personal-account service through
/// the former Gemini CLI ended on 2026-06-18; Antigravity CLI is its official
/// replacement and still provides Gemini models through Google OAuth.
pub fn find_gemini() -> Option<PathBuf> {
    find_program(CliProvider::Gemini)
}

#[derive(Clone)]
pub struct GeminiBackend {
    program: PathBuf,
    model: Arc<Mutex<Option<String>>>,
}

fn conversation_prompt(messages: &[Message], agent: bool) -> String {
    let mut prompt = String::new();
    if agent {
        prompt.push_str(
            "You are Gemini running inside CONSILIUM agent mode. Complete the user's task directly, report concrete results, and stop when the task is handled.\n\n",
        );
    } else {
        prompt.push_str(
            "You are Gemini running inside CONSILIUM chat mode. Answer conversationally and do not use tools unless the user explicitly asks for local work.\n\n",
        );
    }
    prompt.push_str("Conversation so far:\n");
    for message in messages {
        match message.role.as_str() {
            "user" => {
                prompt.push_str("\nUser:\n");
                prompt.push_str(&message.content);
                prompt.push('\n');
            }
            "assistant" => {
                prompt.push_str("\nAssistant:\n");
                prompt.push_str(&message.content);
                prompt.push('\n');
            }
            _ => {}
        }
    }
    prompt.push_str("\nRespond to the latest user message.");
    prompt
}

fn error_detail(stdout: &str, stderr: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let truncated = detail.chars().count() > 4096;
    let mut detail = detail.chars().take(4096).collect::<String>();
    if truncated {
        detail.push_str("...");
    }
    detail
}

fn looks_like_auth_error(stdout: &str, stderr: &str) -> bool {
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    text.contains("authentication required")
        || text.contains("not logged into antigravity")
        || text.contains("authentication failed")
        || text.contains("authorization code")
}

impl GeminiBackend {
    pub fn new(program: PathBuf, model: Option<String>) -> Self {
        Self {
            program,
            model: Arc::new(Mutex::new(model)),
        }
    }

    pub fn model(&self) -> Option<String> {
        self.model.lock().expect("model lock poisoned").clone()
    }

    pub fn set_model(&self, model: Option<String>) {
        let model = model.filter(|model| {
            let model = model.trim();
            !model.is_empty()
                && model != "default"
                && model != "gemini-cli-default"
                && model != "antigravity-default"
        });
        *self.model.lock().expect("model lock poisoned") = model;
    }

    pub async fn stream_chat(
        &self,
        messages: Vec<Message>,
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) {
        let result = self
            .stream_chat_inner(messages, images, agent, cancel.clone(), tx.clone())
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
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) -> Result<()> {
        if !agent {
            return Err(color_eyre::eyre::eyre!(
                "Gemini through Antigravity is available in Agent mode only. Turn on Agent, or choose a Google AI API connector for regular chat."
            ));
        }
        if !images.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "Gemini image attachments are not wired through CONSILIUM yet. Send text for now."
            ));
        }

        let current_dir = user_home_dir().unwrap_or_else(std::env::temp_dir);
        let prompt = conversation_prompt(&messages, agent);
        // Antigravity 1.1.1 exposes print prompts only as a flag value. Keep
        // the argument below both native Windows and .cmd wrapper ceilings,
        // and below the per-argument limit on Linux.
        validate_prompt_argument(&self.program, &prompt, cfg!(windows))?;

        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg("--print")
            .arg(prompt)
            .args(["--print-timeout", PRINT_TIMEOUT])
            .args(["--mode", "accept-edits", "--sandbox"])
            .current_dir(current_dir);
        if let Some(model) = self.model() {
            command.args(["--model", &model]);
        }

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut command)
            .context("failed to start Antigravity CLI for Gemini")?;

        let stdout = child.take_stdout().expect("stdout was piped");
        let stderr = child.take_stderr().expect("stderr was piped");
        let stderr_task = drain_stderr_tail(stderr);
        let mut reader = BufReader::new(stdout);
        let mut response = Vec::new();
        let mut line = Vec::new();

        loop {
            let read_result = tokio::select! {
                _ = cancel.cancelled() => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Ok(());
                }
                read = read_bounded_line(&mut reader, &mut line, MAX_RESPONSE_LINE_BYTES) => read,
            };
            let read = match read_result {
                Ok(read) => read,
                Err(error) => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "error reading Antigravity CLI output: {error}"
                    ));
                }
            };
            if read == 0 {
                break;
            }
            if response.len().saturating_add(line.len()) > MAX_RESPONSE_BYTES {
                child.terminate().await;
                let _ = stderr_task.await;
                return Err(color_eyre::eyre::eyre!(
                    "Antigravity CLI response exceeded the 32 MiB size limit"
                ));
            }
            response.extend_from_slice(&line);
        }

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                child.terminate().await;
                let _ = stderr_task.await;
                return Ok(());
            }
            status = child.wait() => status.context("Antigravity CLI did not exit")?,
        };
        let stderr = stderr_task.await.unwrap_or_default();
        let response = String::from_utf8_lossy(&response);

        if !status.success() {
            if looks_like_auth_error(&response, &stderr) {
                return Err(color_eyre::eyre::eyre!(
                    "Antigravity CLI is not signed in. Click the Gemini connector in CONSILIUM to launch Google login in your browser."
                ));
            }
            let detail = error_detail(response.as_ref(), &stderr);
            return Err(color_eyre::eyre::eyre!(
                "Antigravity CLI exited with status {status}: {detail}"
            ));
        }

        let response = response.trim_end_matches(['\r', '\n']);
        if response.trim().is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "Antigravity CLI completed without a Gemini response"
            ));
        }
        if !send_stream_event(&tx, &cancel, StreamEvent::Token(response.to_string())).await {
            cancel.cancel();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_conversation_and_mode_boundary() {
        let prompt = conversation_prompt(
            &[
                Message {
                    role: "user".to_string(),
                    content: "first".to_string(),
                },
                Message {
                    role: "assistant".to_string(),
                    content: "second".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: "third".to_string(),
                },
            ],
            true,
        );
        assert!(prompt.contains("CONSILIUM agent mode"));
        assert!(prompt.contains("User:\nfirst"));
        assert!(prompt.contains("Assistant:\nsecond"));
        assert!(prompt.ends_with("Respond to the latest user message."));
    }

    #[test]
    fn prompt_argument_limits_cover_windows_native_and_command_launchers() {
        let native = PathBuf::from("C:/tools/agy.exe");
        let command = PathBuf::from("C:/Users/Ada/AppData/Roaming/npm/agy.CMD");
        assert_eq!(
            prompt_argument_limit(&native, true),
            MAX_WINDOWS_NATIVE_PROMPT_BYTES
        );
        assert_eq!(
            prompt_argument_limit(&command, true),
            MAX_WINDOWS_BATCH_PROMPT_BYTES
        );
        assert!(validate_prompt_argument(
            &command,
            &"x".repeat(MAX_WINDOWS_BATCH_PROMPT_BYTES),
            true
        )
        .is_ok());
        let error = validate_prompt_argument(
            &command,
            &"x".repeat(MAX_WINDOWS_BATCH_PROMPT_BYTES + 1),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("limit 6 KiB"));
        assert!(error.contains("Google AI API"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn heavy_stderr_cannot_deadlock_an_antigravity_response() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-antigravity-stderr-drain-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agy.sh");
        let args_capture = dir.join("arguments.txt");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ni=0\nwhile [ $i -lt 20000 ]; do printf 'diagnostic-%s\\n' \"$i\" >&2; i=$((i + 1)); done\nprintf 'not blocked\\n'",
            args_capture.display()
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = GeminiBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.stream_chat(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            ),
        )
        .await
        .expect("provider blocked on its stderr pipe");

        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(token) => text.push_str(&token),
                StreamEvent::Finished => break,
                StreamEvent::Thought(_) => {}
                StreamEvent::Error(error) => panic!("unexpected error: {error}"),
            }
        }
        assert_eq!(text, "not blocked");
        let args = std::fs::read_to_string(args_capture).unwrap();
        assert!(args.lines().any(|arg| arg == "--print"));
        assert!(args.lines().any(|arg| arg == "--mode"));
        assert!(args.lines().any(|arg| arg == "accept-edits"));
        assert!(args.lines().any(|arg| arg == "--sandbox"));
        assert!(!args.contains("dangerously-skip-permissions"));
        assert!(args.lines().any(|arg| arg == PRINT_TIMEOUT));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn authentication_failures_are_actionable_and_do_not_leak_the_url() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-antigravity-auth-error-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agy-auth.sh");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nprintf 'Authentication required: https://accounts.google.com/private-link\\n'\nexit 1"
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = GeminiBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            )
            .await;

        let event = rx.recv().await.unwrap();
        let StreamEvent::Error(error) = event else {
            panic!("expected an authentication error");
        };
        assert!(error.contains("Click the Gemini connector"));
        assert!(!error.contains("accounts.google.com"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn chat_mode_has_a_clear_agent_only_message_without_launching() {
        let backend = GeminiBackend::new(PathBuf::from("definitely-not-a-real-agy"), None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                Vec::new(),
                false,
                CancellationToken::new(),
                tx,
            )
            .await;
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(message))
                if message.contains("Agent mode only") && message.contains("Google AI API")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_newline_free_output_is_stopped_at_the_line_limit() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-antigravity-line-limit-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agy-large-line.sh");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nhead -c {} /dev/zero | tr '\\0' x\nsleep 30",
            MAX_RESPONSE_LINE_BYTES + 1
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = GeminiBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.stream_chat(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            ),
        )
        .await
        .expect("large provider line was not stopped");
        let event = rx.recv().await.expect("missing size-limit result");
        match event {
            StreamEvent::Error(message) => {
                assert!(message.contains("exceeded"), "unexpected error: {message}")
            }
            StreamEvent::Token(token) => panic!("unexpected token: {token}"),
            StreamEvent::Thought(thought) => panic!("unexpected thought: {thought}"),
            StreamEvent::Finished => panic!("provider unexpectedly finished"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
