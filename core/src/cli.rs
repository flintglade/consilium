use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use crate::api::{send_stream_event, StreamEvent, StreamEventSender};
use crate::child_io::{
    application_data_dir, drain_stderr_tail, find_program, is_canonical_session_id,
    read_bounded_line, user_home_dir, CliProvider, ManagedChild, CLI_STDOUT_LINE_LIMIT,
    CLI_STDOUT_TOTAL_LIMIT,
};

/// A base64-encoded image sent alongside a prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub mime: String,
}

/// ACP content blocks for `--prompt-file`. Images can be megabytes of
/// base64, far past the ~128 KiB Linux per-argument limit, so they go
/// through a temp file instead of argv.
fn build_content_blocks(prompt: &str, images: &[ImageAttachment]) -> String {
    let mut blocks = vec![serde_json::json!({ "type": "text", "text": prompt })];
    for img in images {
        blocks.push(serde_json::json!({
            "type": "image",
            "data": img.data,
            "mimeType": img.mime,
        }));
    }
    serde_json::Value::Array(blocks).to_string()
}

/// Deletes the private prompt file when the request is done (or cancelled).
struct TempPromptFile(PathBuf);

impl TempPromptFile {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempPromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Conversational agent definition for chat mode. Headless `grok -p`
/// otherwise runs the workspace-aware "cursor" coding agent, which gives
/// terse, tool-seeking replies and stalls when tools are disabled.
const CHAT_AGENT_DEF: &str = include_str!("chat-agent.md");

/// App data dir for grok-chat's own files (chat-agent def, neutral cwd).
fn data_dir() -> PathBuf {
    application_data_dir()
}

/// Writes the embedded chat-agent definition to disk and returns its path.
fn ensure_chat_agent() -> Result<PathBuf> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).context("failed to create data dir")?;
    let path = dir.join("chat-agent.md");
    // rewrite every launch so prompt tweaks ship with the binary
    std::fs::write(&path, CHAT_AGENT_DEF).context("failed to write chat agent")?;
    Ok(path)
}

/// A neutral, empty working directory for chat mode, so grok has no
/// project to "continue" or inspect.
fn ensure_neutral_cwd() -> Result<PathBuf> {
    let dir = data_dir().join("chat-cwd");
    std::fs::create_dir_all(&dir).context("failed to create neutral cwd")?;
    Ok(dir)
}

static PROMPT_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn prompt_dir() -> Result<PathBuf> {
    let dir = data_dir().join("prompt-files");
    std::fs::create_dir_all(&dir).context("failed to create private prompt directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .context("failed to restrict private prompt directory")?;
    }
    Ok(dir)
}

fn create_prompt_file(prompt: &str, images: &[ImageAttachment]) -> Result<TempPromptFile> {
    let dir = prompt_dir()?;
    let payload = build_content_blocks(prompt, images);
    for _ in 0..32 {
        let sequence = PROMPT_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("prompt-{}-{sequence}.json", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(payload.as_bytes())
                    .context("failed to write private prompt file")?;
                file.sync_all()
                    .context("failed to finish private prompt file")?;
                return Ok(TempPromptFile(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create private prompt file"),
        }
    }
    Err(color_eyre::eyre::eyre!(
        "could not allocate a unique private prompt file"
    ))
}

/// Chat backend that shells out to `grok -p --output-format streaming-json`.
/// Auth (SuperGrok subscription session, token refresh) is handled entirely
/// by the grok CLI — this process never touches credentials. Conversation
/// context is carried by grok sessions via --resume, so only the newest
/// user message is sent per request.
#[derive(Clone)]
pub struct CliBackend {
    program: PathBuf,
    model: Arc<Mutex<Option<String>>>,
    effort: Arc<Mutex<Option<String>>>,
    session_id: Arc<Mutex<Option<String>>>,
    session_runtime: Arc<Mutex<Option<RuntimeContext>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeContext {
    agent: bool,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct CliEvent {
    #[serde(rename = "type")]
    kind: String,
    data: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    message: Option<String>,
}

/// One parsed NDJSON line from grok's streaming-json output.
#[derive(Debug, PartialEq, Eq)]
enum CliLine {
    Token(String),
    Thought(String),
    End {
        session_id: Option<String>,
        stop_reason: Option<String>,
    },
    Error(String),
    Skip,
}

/// Outcome of one grok invocation, so the caller can decide whether a
/// transport drop needs an automatic resume.
struct AttemptOutcome {
    saw_token: bool,
    /// stopReason from the final "end" event (e.g. "EndTurn", "Cancelled").
    stop_reason: Option<String>,
    /// True when the stream ended with no clean "end" event — a hard
    /// transport drop or crash.
    dropped: bool,
    /// Last few stderr lines, for diagnostics when everything fails.
    err_tail: String,
}

/// How many times to auto-resume a session that ended on a transport drop
/// before giving up. Drops are frequent (several per minute), so a healthy
/// margin matters for long agent tasks.
const MAX_AUTO_RESUMES: u32 = 8;

fn parse_cli_line(line: &str) -> CliLine {
    let line = line.trim();
    if line.is_empty() {
        return CliLine::Skip;
    }
    let Ok(event) = serde_json::from_str::<CliEvent>(line) else {
        return CliLine::Skip;
    };
    match event.kind.as_str() {
        "text" => match event.data {
            Some(data) if !data.is_empty() => CliLine::Token(data),
            _ => CliLine::Skip,
        },
        "thought" => match event.data {
            Some(data) if !data.is_empty() => CliLine::Thought(data),
            _ => CliLine::Skip,
        },
        "end" => CliLine::End {
            session_id: event.session_id,
            stop_reason: event.stop_reason,
        },
        "error" => CliLine::Error(
            event
                .message
                .unwrap_or_else(|| "unknown grok CLI error".to_string()),
        ),
        // "max_turns_reached", "auto_compact_*", future event types
        _ => CliLine::Skip,
    }
}

/// Locate the grok CLI binary: $GROK_CLI_BIN override, then PATH,
/// then the known install locations. Desktop-grid launches may not
/// have ~/.local/bin on PATH, so the explicit fallbacks matter.
pub fn find_grok() -> Option<PathBuf> {
    find_program(CliProvider::Grok)
}

impl CliBackend {
    pub fn new(program: PathBuf, model: Option<String>) -> Self {
        Self {
            program,
            model: Arc::new(Mutex::new(model)),
            effort: Arc::new(Mutex::new(None)),
            session_id: Arc::new(Mutex::new(None)),
            session_runtime: Arc::new(Mutex::new(None)),
        }
    }

    pub fn effort(&self) -> Option<String> {
        self.effort.lock().expect("effort lock poisoned").clone()
    }

    /// Reasoning effort for future messages. None uses grok's default.
    pub fn set_effort(&self, effort: Option<String>) {
        *self.effort.lock().expect("effort lock poisoned") = effort;
    }

    pub fn model(&self) -> Option<String> {
        self.model.lock().expect("model lock poisoned").clone()
    }

    pub fn set_model(&self, model: Option<String>) {
        *self.model.lock().expect("model lock poisoned") = model;
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .expect("session lock poisoned")
            .clone()
    }

    /// Forget the grok session so the next message starts a fresh one.
    pub fn reset_session(&self) {
        *self.session_id.lock().expect("session lock poisoned") = None;
        *self.session_runtime.lock().expect("runtime lock poisoned") = None;
    }

    /// Point at an existing grok session (e.g. when reopening a saved
    /// chat) so the next message resumes it.
    pub fn set_session(&self, id: Option<String>) {
        *self.session_id.lock().expect("session lock poisoned") = id;
        *self.session_runtime.lock().expect("runtime lock poisoned") = None;
    }

    /// Point at an existing grok session and, when known, remember the
    /// app runtime that created it. Old pre-metadata sessions pass None
    /// here, so selecting agent mode or a non-default effort will fork
    /// instead of blindly resuming an incompatible runtime.
    pub fn set_session_context(
        &self,
        id: Option<String>,
        agent: Option<bool>,
        effort: Option<String>,
    ) {
        *self.session_id.lock().expect("session lock poisoned") = id;
        let runtime = agent.map(|agent| RuntimeContext { agent, effort });
        *self.session_runtime.lock().expect("runtime lock poisoned") = runtime;
    }

    fn selected_runtime(&self, agent: bool) -> RuntimeContext {
        RuntimeContext {
            agent,
            effort: self.effort(),
        }
    }

    fn should_fork_for_runtime(&self, current: &RuntimeContext) -> bool {
        if self.session_id().is_none() {
            return false;
        }
        match self
            .session_runtime
            .lock()
            .expect("runtime lock poisoned")
            .as_ref()
        {
            Some(previous) => previous != current,
            None => current.agent || current.effort.is_some(),
        }
    }

    fn remember_runtime(&self, current: RuntimeContext) {
        if self.session_id().is_some() {
            *self.session_runtime.lock().expect("runtime lock poisoned") = Some(current);
        }
    }

    pub async fn stream_chat(
        &self,
        prompt: String,
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) {
        let fresh_prompt = prompt.clone();
        self.stream_chat_with_fresh_prompt(prompt, fresh_prompt, images, agent, cancel, tx)
            .await;
    }

    pub async fn stream_chat_with_fresh_prompt(
        &self,
        prompt: String,
        fresh_prompt: String,
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) {
        let result = self
            .stream_chat_inner(
                prompt,
                fresh_prompt,
                images,
                agent,
                cancel.clone(),
                tx.clone(),
            )
            .await;

        if cancel.is_cancelled() {
            return;
        }

        match result {
            Ok(()) => {
                let _ = tx.send(StreamEvent::Finished).await;
            }
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(format!("[Error] {e}"))).await;
            }
        }
    }

    /// Runs grok once, resuming automatically when a transport drop cuts
    /// the stream mid-task (grok's gateway closes the connection, ending
    /// with stopReason "Cancelled" or no clean end at all). Without this a
    /// single dropped connection abandons the whole response — the cause of
    /// the "gets partway then stops / shows a dismissive fallback" bug.
    async fn stream_chat_inner(
        &self,
        prompt: String,
        fresh_prompt: String,
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: CancellationToken,
        tx: StreamEventSender,
    ) -> Result<()> {
        let mut saw_token_total = false;
        let mut attempt: u32 = 0;
        let still_incomplete;
        let last_err_tail;
        let current_runtime = self.selected_runtime(agent);
        let fork_first_attempt = self.should_fork_for_runtime(&current_runtime);

        loop {
            let is_resume = attempt > 0;
            let this_prompt = if is_resume {
                if agent {
                    "Continue exactly where you left off and finish the entire task. Do not restart or re-summarize; carry on until every part is complete, then stop.".to_string()
                } else {
                    "Please continue your previous reply from exactly where it was cut off."
                        .to_string()
                }
            } else {
                prompt.clone()
            };
            let this_images = if is_resume {
                Vec::new()
            } else {
                images.clone()
            };

            let fork_session = attempt == 0 && fork_first_attempt;
            let outcome = match self
                .run_attempt(
                    &this_prompt,
                    &this_images,
                    agent,
                    fork_session,
                    &cancel,
                    &tx,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(err)
                    if attempt == 0
                        && self.session_id().is_some()
                        && is_session_compatibility_error(&err.to_string()) =>
                {
                    if !send_stream_event(
                        &tx,
                        &cancel,
                        StreamEvent::Thought(
                            "The saved Grok runtime is incompatible with the selected mode; starting a fresh runtime.\n".to_string(),
                        ),
                    )
                    .await
                    {
                        cancel.cancel();
                        return Ok(());
                    }
                    self.reset_session();
                    self.run_attempt(&fresh_prompt, &this_images, agent, false, &cancel, &tx)
                        .await?
                }
                Err(err) => return Err(err),
            };
            saw_token_total |= outcome.saw_token;

            if cancel.is_cancelled() {
                return Ok(());
            }

            if !outcome.dropped && matches!(outcome.stop_reason.as_deref(), Some("EndTurn")) {
                still_incomplete = false;
                last_err_tail = outcome.err_tail;
                break;
            }

            // Resume only interruption-shaped endings. Other stop reasons
            // are terminal but not complete, so surface them to the caller.
            let needs_resume =
                matches!(outcome.stop_reason.as_deref(), Some("Cancelled")) || outcome.dropped;
            if !needs_resume {
                still_incomplete = true;
                last_err_tail = outcome.err_tail;
                break;
            }
            // Give up if we've exhausted resumes or have no session to resume.
            if attempt >= MAX_AUTO_RESUMES || self.session_id().is_none() {
                still_incomplete = true;
                last_err_tail = outcome.err_tail;
                break;
            }
            attempt += 1;
        }

        self.remember_runtime(current_runtime);

        if still_incomplete {
            let detail = if last_err_tail.is_empty() {
                String::new()
            } else {
                format!(" — {last_err_tail}")
            };
            return Err(color_eyre::eyre::eyre!(
                "Grok's response remained incomplete after retrying. Please try again.{detail}"
            ));
        }

        if !saw_token_total {
            let event = if !agent {
                // Chat-only: an ambiguous prompt can end a turn with no text.
                // Never do this in agent mode — the work speaks for itself.
                StreamEvent::Token(
                    "I didn't catch a clear request there — what would you like help with?"
                        .to_string(),
                )
            } else {
                StreamEvent::Token(
                    "Grok finished the agent turn without returning a visible text summary."
                        .to_string(),
                )
            };
            if !send_stream_event(&tx, &cancel, event).await {
                cancel.cancel();
                return Ok(());
            }
        }

        Ok(())
    }

    async fn run_attempt(
        &self,
        prompt: &str,
        images: &[ImageAttachment],
        agent: bool,
        fork_session: bool,
        cancel: &CancellationToken,
        tx: &StreamEventSender,
    ) -> Result<AttemptOutcome> {
        let mut cmd = tokio::process::Command::new(&self.program);
        // CLI mode is the SuperGrok/OAuth path. A project .env may contain an
        // invalid XAI_API_KEY for direct API mode; do not let it poison grok's
        // cached-token auth selection in the child process.
        cmd.env_remove("XAI_API_KEY");
        let prompt_file = create_prompt_file(prompt, images)?;
        cmd.arg("--prompt-file").arg(prompt_file.path());
        cmd.args(["--output-format", "streaming-json"])
            .arg("--no-auto-update");
        if agent {
            // Agent mode: grok's full-access default agent, running in the
            // user's home so it can do real work, with cross-session memory
            // ON and Grok's headless --check verification loop appended.
            // Permissions are full-access (yolo) by ~/.grok/config.toml.
            cmd.args(["--experimental-memory", "--check"]);
            if let Some(home) = user_home_dir() {
                cmd.arg("--cwd").arg(home);
            }
        } else {
            // Chat mode: conversational agent, no tools, neutral cwd, and no
            // memory — a clean stateless Q&A experience (context is carried
            // by the app's own --resume sessions, not grok's global memory).
            cmd.args(["--tools", ""]).arg("--no-memory");
            if let Ok(agent_file) = ensure_chat_agent() {
                cmd.arg("--agent").arg(agent_file);
            }
            if let Ok(cwd) = ensure_neutral_cwd() {
                cmd.arg("--cwd").arg(cwd);
            }
        }
        if let Some(model) = self.model() {
            cmd.args(["-m", &model]);
        }
        if let Some(effort) = self.effort() {
            cmd.args(["--effort", &effort]);
        }
        let resume = self
            .session_id
            .lock()
            .expect("session lock poisoned")
            .clone();
        if let Some(id) = &resume {
            if !is_canonical_session_id(id) {
                return Err(color_eyre::eyre::eyre!(
                    "The saved Grok session identifier is invalid. Start a new conversation and try again."
                ));
            }
            cmd.args(["--resume", id]);
            if fork_session {
                cmd.arg("--fork-session");
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut cmd).context("failed to start grok CLI")?;

        let stdout = child.take_stdout().expect("stdout was piped");
        let stderr = child.take_stderr().expect("stderr was piped");
        let stderr_task = drain_stderr_tail(stderr);
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut total_output = 0usize;
        let mut saw_end = false;
        let mut saw_token = false;
        let mut stop_reason: Option<String> = None;

        loop {
            let read_result = tokio::select! {
                _ = cancel.cancelled() => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Ok(AttemptOutcome {
                        saw_token,
                        stop_reason: None,
                        dropped: false,
                        err_tail: String::new(),
                    });
                }
                read = read_bounded_line(&mut reader, &mut line, CLI_STDOUT_LINE_LIMIT) => read,
            };

            let read = match read_result {
                Ok(read) => read,
                Err(error) => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "error reading grok CLI output: {error}"
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
                        "Grok CLI output exceeded the 32 MiB size limit"
                    ));
                }
            };
            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(error) => {
                    child.terminate().await;
                    let _ = stderr_task.await;
                    return Err(color_eyre::eyre::eyre!(
                        "Grok CLI output was not valid UTF-8: {error}"
                    ));
                }
            };

            match parse_cli_line(line) {
                CliLine::Token(token) => {
                    saw_token = true;
                    if !send_stream_event(tx, cancel, StreamEvent::Token(token)).await {
                        cancel.cancel();
                        child.terminate().await;
                        let _ = stderr_task.await;
                        return Ok(AttemptOutcome {
                            saw_token,
                            stop_reason: Some("Cancelled".to_string()),
                            dropped: false,
                            err_tail: String::new(),
                        });
                    }
                }
                CliLine::Thought(thought) => {
                    if !send_stream_event(tx, cancel, StreamEvent::Thought(thought)).await {
                        cancel.cancel();
                        child.terminate().await;
                        let _ = stderr_task.await;
                        return Ok(AttemptOutcome {
                            saw_token,
                            stop_reason: Some("Cancelled".to_string()),
                            dropped: false,
                            err_tail: String::new(),
                        });
                    }
                }
                CliLine::End {
                    session_id,
                    stop_reason: sr,
                } => {
                    if let Some(id) = session_id {
                        if !is_canonical_session_id(&id) {
                            child.terminate().await;
                            let _ = stderr_task.await;
                            return Err(color_eyre::eyre::eyre!(
                                "Grok returned an invalid session identifier"
                            ));
                        }
                        *self.session_id.lock().expect("session lock poisoned") = Some(id);
                    }
                    stop_reason = sr;
                    saw_end = true;
                }
                CliLine::Error(message) => {
                    child.terminate().await;
                    let err_tail = stderr_task.await.unwrap_or_default();
                    let detail = if err_tail.is_empty() {
                        String::new()
                    } else {
                        format!(" — {err_tail}")
                    };
                    return Err(color_eyre::eyre::eyre!("{message}{detail}"));
                }
                CliLine::Skip => {}
            }
        }

        let status = child.wait().await.context("grok CLI did not exit")?;
        let stderr_tail = stderr_task.await.unwrap_or_default();

        // No clean "end" event → a transport drop or crash. Capture stderr
        // for diagnostics; the caller (stream_chat_inner) decides whether to
        // resume. We deliberately do NOT error here, so a drop can be retried.
        let dropped = !saw_end;
        let err_tail = if dropped && !status.success() {
            stderr_tail.clone()
        } else {
            String::new()
        };
        if saw_end && !status.success() && !matches!(stop_reason.as_deref(), Some("Cancelled")) {
            let detail = if stderr_tail.is_empty() {
                String::new()
            } else {
                format!(" — {stderr_tail}")
            };
            return Err(color_eyre::eyre::eyre!(
                "grok CLI exited with status {status}{detail}"
            ));
        }

        Ok(AttemptOutcome {
            saw_token,
            stop_reason,
            dropped,
            err_tail,
        })
    }
}

fn is_session_compatibility_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("model_switch_incompatible_agent")
        || message.contains("incompatible agent")
        || message.contains("incompatible_agent")
        || message.contains("cannot switch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_events_become_tokens() {
        assert_eq!(
            parse_cli_line(r#"{"type":"text","data":"Hello"}"#),
            CliLine::Token("Hello".to_string())
        );
    }

    #[test]
    fn thought_events_are_surfaced() {
        assert_eq!(
            parse_cli_line(r#"{"type":"thought","data":"hmm"}"#),
            CliLine::Thought("hmm".to_string())
        );
    }

    #[test]
    fn unknown_events_are_skipped() {
        assert_eq!(
            parse_cli_line(r#"{"type":"max_turns_reached"}"#),
            CliLine::Skip
        );
        assert_eq!(parse_cli_line(""), CliLine::Skip);
        assert_eq!(parse_cli_line("not json"), CliLine::Skip);
    }

    #[test]
    fn end_event_carries_session_id_and_stop_reason() {
        assert_eq!(
            parse_cli_line(r#"{"type":"end","stopReason":"EndTurn","sessionId":"abc123"}"#),
            CliLine::End {
                session_id: Some("abc123".to_string()),
                stop_reason: Some("EndTurn".to_string()),
            }
        );
        assert_eq!(
            parse_cli_line(r#"{"type":"end","stopReason":"Cancelled","sessionId":"x"}"#),
            CliLine::End {
                session_id: Some("x".to_string()),
                stop_reason: Some("Cancelled".to_string()),
            }
        );
    }

    #[test]
    fn error_event_carries_message() {
        assert_eq!(
            parse_cli_line(r#"{"type":"error","message":"boom"}"#),
            CliLine::Error("boom".to_string())
        );
    }

    #[test]
    fn content_blocks_carry_text_and_images_in_acp_shape() {
        let images = vec![ImageAttachment {
            data: "QUJD".to_string(),
            mime: "image/png".to_string(),
        }];
        let blocks: serde_json::Value =
            serde_json::from_str(&build_content_blocks("look at this", &images)).unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "look at this");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["data"], "QUJD");
        assert_eq!(blocks[1]["mimeType"], "image/png");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_sends_use_prompt_file_and_clean_up() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grok-chat-imgtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let capture = dir.join("blocks-copy.json");
        let prompt_metadata = dir.join("prompt-metadata.txt");
        let arguments = dir.join("arguments.txt");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        // fake grok: copies the --prompt-file payload so the test can
        // inspect it even after the temp file is cleaned up
        writeln!(
            f,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--prompt-file\" ]; then\n    cp \"$2\" '{}'\n    printf '%s\\n' \"$2\" \"$(stat -c %a \"$2\")\" \"$(stat -c %a \"$(dirname \"$2\")\")\" > '{}'\n  fi\n  shift\ndone\nprintf '{{\"type\":\"text\",\"data\":\"seen\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111111\"}}\\n'",
            arguments.display(),
            capture.display(),
            prompt_metadata.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        let images = vec![ImageAttachment {
            data: "QUJD".to_string(),
            mime: "image/jpeg".to_string(),
        }];
        backend
            .stream_chat(
                "describe".to_string(),
                images,
                false,
                CancellationToken::new(),
                tx,
            )
            .await;
        while rx.recv().await.is_some() {}

        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).unwrap()).unwrap();
        assert_eq!(payload[0]["text"], "describe");
        assert_eq!(payload[1]["mimeType"], "image/jpeg");
        let metadata = std::fs::read_to_string(prompt_metadata).unwrap();
        let metadata = metadata.lines().collect::<Vec<_>>();
        assert_eq!(metadata.len(), 3);
        assert_eq!(metadata[1], "600");
        assert_eq!(metadata[2], "700");
        assert!(
            !std::path::Path::new(metadata[0]).exists(),
            "private prompt file was not cleaned up"
        );
        let arguments = std::fs::read_to_string(arguments).unwrap();
        assert!(arguments.contains("--prompt-file"));
        assert!(!arguments.contains("describe"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_mode_does_not_inherit_xai_api_key() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use tokio::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::const_new(());
        let _guard = ENV_LOCK.lock().await;

        let dir = std::env::temp_dir().join(format!("grok-chat-envtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env_log = dir.join("env.log");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\nprintf '%s' \"${{XAI_API_KEY-unset}}\" > {}\nprintf '{{\"type\":\"text\",\"data\":\"ok\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111111\"}}\\n'",
            env_log.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let previous = std::env::var_os("XAI_API_KEY");
        std::env::set_var("XAI_API_KEY", "bad-key-from-env");

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "hi".to_string(),
                Vec::new(),
                false,
                CancellationToken::new(),
                tx,
            )
            .await;
        while rx.recv().await.is_some() {}

        assert_eq!(std::fs::read_to_string(&env_log).unwrap(), "unset");

        if let Some(value) = previous {
            std::env::set_var("XAI_API_KEY", value);
        } else {
            std::env::remove_var("XAI_API_KEY");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_from_fake_grok_and_resumes_session() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // fake grok: logs its args, emits two tokens and an end event
        let dir = std::env::temp_dir().join(format!("grok-chat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.log");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\necho \"$@\" >> {}\nprintf '{{\"type\":\"thought\",\"data\":\"x\"}}\\n'\nprintf '{{\"type\":\"text\",\"data\":\"Hel\"}}\\n'\nprintf '{{\"type\":\"text\",\"data\":\"lo\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111111\"}}\\n'",
            args_log.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script.clone(), None);

        // turn 0: chat mode, defaults; turn 1: agent mode with effort set
        for turn in 0..2 {
            if turn == 1 {
                backend.set_effort(Some("high".to_string()));
            }
            let (tx, mut rx) = crate::api::stream_event_channel();
            backend
                .stream_chat(
                    "PROMPT_SHOULD_NOT_BE_ARGV".to_string(),
                    Vec::new(),
                    turn == 1,
                    CancellationToken::new(),
                    tx,
                )
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
                    StreamEvent::Error(e) => panic!("turn {turn}: unexpected error: {e}"),
                }
            }
            assert_eq!(text, "Hello");
            assert!(finished);
        }

        let log = std::fs::read_to_string(&args_log).unwrap();
        let calls: Vec<&str> = log.lines().collect();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call.contains("--prompt-file")));
        assert!(calls
            .iter()
            .all(|call| !call.contains("PROMPT_SHOULD_NOT_BE_ARGV")));
        assert!(!calls[0].contains("--resume"), "first call must not resume");
        assert!(
            calls[1].contains("--resume 11111111-1111-4111-8111-111111111111"),
            "second call must resume the session from the first: {}",
            calls[1]
        );
        // chat mode: no tools + conversational agent definition;
        // agent mode: keeps tools, uses grok's default agent
        assert!(calls[0].contains("--tools"), "chat mode must pass --tools");
        assert!(calls[0].contains("--agent"), "chat mode must pass --agent");
        assert!(
            !calls[1].contains("--tools"),
            "agent mode must not disable tools: {}",
            calls[1]
        );
        assert!(
            !calls[1].contains("--agent"),
            "agent mode must use grok's default agent: {}",
            calls[1]
        );
        assert!(
            calls[1].contains("--experimental-memory"),
            "agent mode must force memory on: {}",
            calls[1]
        );
        assert!(
            calls[1].contains("--check"),
            "agent mode must append Grok's verification loop: {}",
            calls[1]
        );
        assert!(
            !calls[0].contains("--check"),
            "chat mode must not append agent verification: {}",
            calls[0]
        );
        // effort only after set_effort
        assert!(!calls[0].contains("--effort"));
        assert!(
            calls[1].contains("--effort high"),
            "second call must carry effort: {}",
            calls[1]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_change_forks_old_session_before_agent_max_send() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grok-chat-fork-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.log");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\necho \"$@\" >> {}\nprintf '{{\"type\":\"text\",\"data\":\"ok\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111112\"}}\\n'",
            args_log.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        backend.set_session_context(
            Some("11111111-1111-4111-8111-111111111113".to_string()),
            None,
            None,
        );
        backend.set_effort(Some("max".to_string()));

        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "debug this".to_string(),
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            )
            .await;
        while rx.recv().await.is_some() {}

        let log = std::fs::read_to_string(&args_log).unwrap();
        assert!(
            log.contains("--resume 11111111-1111-4111-8111-111111111113"),
            "{log}"
        );
        assert!(log.contains("--fork-session"), "{log}");
        assert!(log.contains("--effort max"), "{log}");
        assert_eq!(
            backend.session_id().as_deref(),
            Some("11111111-1111-4111-8111-111111111112")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incompatible_resumed_session_retries_fresh() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("grok-chat-incompat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.log");
        let prompt_capture = dir.join("fresh-prompt.json");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\necho \"$@\" >> {}\nif echo \"$@\" | grep -q -- --resume; then\n  printf '{{\"type\":\"error\",\"message\":\"MODEL_SWITCH_INCOMPATIBLE_AGENT\"}}\\n'\nelse\n  previous=\"\"\n  for argument in \"$@\"; do\n    if [ \"$previous\" = \"--prompt-file\" ]; then cp \"$argument\" {}; fi\n    previous=\"$argument\"\n  done\n  printf '{{\"type\":\"text\",\"data\":\"fresh ok\"}}\\n'\n  printf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111115\"}}\\n'\nfi",
            args_log.display(),
            prompt_capture.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        backend.set_session_context(
            Some("11111111-1111-4111-8111-111111111114".to_string()),
            Some(false),
            None,
        );

        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat_with_fresh_prompt(
                "latest-only".to_string(),
                "full labeled transcript".to_string(),
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            )
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

        let log = std::fs::read_to_string(&args_log).unwrap();
        let calls = log.lines().collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{log}");
        assert!(
            calls[0].contains("--resume 11111111-1111-4111-8111-111111111114"),
            "{log}"
        );
        assert!(!calls[1].contains("--resume"), "{log}");
        assert_eq!(text, "fresh ok");
        assert!(finished);
        let fresh_prompt: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(prompt_capture).expect("fresh prompt was captured"),
        )
        .unwrap();
        assert_eq!(fresh_prompt[0]["text"], "full labeled transcript");
        assert_eq!(
            backend.session_id().as_deref(),
            Some("11111111-1111-4111-8111-111111111115")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_clean_end_without_text_gets_visible_fallback() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("grok-chat-empty-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111116\"}}\\n'"
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "do quiet work".to_string(),
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            )
            .await;

        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(t) => text.push_str(&t),
                StreamEvent::Thought(_) => {}
                StreamEvent::Finished => break,
                StreamEvent::Error(e) => panic!("unexpected error: {e}"),
            }
        }

        assert!(text.contains("without returning a visible text summary"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn heavy_stderr_cannot_deadlock_a_grok_stream() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("grok-chat-stderr-drain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-grok.sh");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 20000 ]; do printf 'diagnostic-%s\\n' \"$i\" >&2; i=$((i + 1)); done\nprintf '{{\"type\":\"text\",\"data\":\"not blocked\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111117\"}}\\n'"
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            backend.stream_chat(
                "hello".to_string(),
                Vec::new(),
                false,
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
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transport_drop_auto_resumes_until_clean_finish() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // fake grok: first call drops (stopReason Cancelled) after a partial
        // token; the resume call (detected via --resume) finishes cleanly.
        let dir = std::env::temp_dir().join(format!("grok-chat-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let calls_log = dir.join("calls.log");
        let script = dir.join("fake-grok.sh");
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(
            f,
            "#!/bin/sh\necho \"$@\" >> {}\nif echo \"$@\" | grep -q -- --resume; then\n  printf '{{\"type\":\"text\",\"data\":\"world\"}}\\n'\n  printf '{{\"type\":\"end\",\"stopReason\":\"EndTurn\",\"sessionId\":\"11111111-1111-4111-8111-111111111118\"}}\\n'\nelse\n  printf '{{\"type\":\"text\",\"data\":\"Hello \"}}\\n'\n  printf '{{\"type\":\"end\",\"stopReason\":\"Cancelled\",\"sessionId\":\"11111111-1111-4111-8111-111111111118\"}}\\n'\nfi",
            calls_log.display()
        )
        .unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "hi".to_string(),
                Vec::new(),
                true,
                CancellationToken::new(),
                tx,
            )
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
                StreamEvent::Error(e) => panic!("unexpected error (should have resumed): {e}"),
            }
        }
        // partial from the dropped attempt + the resumed remainder
        assert_eq!(text, "Hello world");
        assert!(finished);

        // exactly two invocations: the drop, then one resume
        let calls: Vec<String> = std::fs::read_to_string(&calls_log)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(calls.len(), 2, "expected 1 drop + 1 resume, got {calls:?}");
        assert!(!calls[0].contains("--resume"), "first call is not a resume");
        assert!(
            calls[1].contains("--resume 11111111-1111-4111-8111-111111111118"),
            "second call resumes: {}",
            calls[1]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incomplete_grok_turn_keeps_partial_tokens_and_ends_with_error() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-grok-partial-error-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("grok");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nprintf '{{\"type\":\"text\",\"data\":\"partial\"}}\\n'\nprintf '{{\"type\":\"end\",\"stopReason\":\"Cancelled\"}}\\n'"
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "hello".into(),
                Vec::new(),
                false,
                CancellationToken::new(),
                tx,
            )
            .await;
        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(text)) if text == "partial"));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(error)) if error.contains("incomplete")
        ));
        assert!(rx.recv().await.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newline_free_oversized_grok_output_is_rejected() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-grok-output-limit-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("grok");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(file, "#!/bin/sh\nhead -c 1048577 /dev/zero | tr '\\000' x").unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "hello".into(),
                Vec::new(),
                false,
                CancellationToken::new(),
                tx,
            )
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
            "consilium-grok-invalid-session-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("launched");
        let script = dir.join("grok");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(file, "#!/bin/sh\ntouch '{}'", marker.display()).unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CliBackend::new(script, None);
        backend.set_session_context(Some("--continue".into()), Some(false), None);
        let (tx, mut rx) = crate::api::stream_event_channel();
        backend
            .stream_chat(
                "hello".into(),
                Vec::new(),
                false,
                CancellationToken::new(),
                tx,
            )
            .await;
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Error(message)) if message.contains("invalid")
        ));
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
