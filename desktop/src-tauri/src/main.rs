#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod sessions;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use grok_chat_core::api::{stream_event_channel, Message, StreamEvent};
use grok_chat_core::cli::{self, ImageAttachment};
use grok_chat_core::open_models::{open_model_catalog, OpenModelInfo};
use grok_chat_core::providers::{provider_catalog, ProviderInfo};
use grok_chat_core::routing::{
    decide, routing_profiles, RouteCandidate, RouteDecision, RoutingProfile,
};
use grok_chat_core::{claude, codex, gemini};
use grok_chat_core::{load_backend, Backend, GROK_DEFAULT_MODEL};
use sessions::{SessionMeta, SessionStore, SessionStoreHealth, StoredSession};
use tauri::{AppHandle, Emitter, State};
use tokio_util::sync::CancellationToken;

const MAX_REQUEST_ID_BYTES: usize = 96;
const MAX_REQUEST_MESSAGES: usize = 256;
const MAX_USER_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ASSISTANT_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_REQUEST_TEXT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 12;
const MAX_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_STREAM_EVENT_BYTES: usize = 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 32 * 1024 * 1024;
const MAX_THOUGHT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct ActiveRequest {
    request_id: String,
    cancel: CancellationToken,
}

struct AppState {
    backends: Arc<Mutex<HashMap<String, Backend>>>,
    selected_provider: Arc<Mutex<String>>,
    /// Provider whose native runtime (if any) is consistent with the local
    /// transcript. This is deliberately separate from `selected_provider`:
    /// an automatic profile can select its primary route after a fallback
    /// answered the preceding turn.
    session_owner_provider: Arc<Mutex<Option<String>>>,
    routing_profile: Arc<Mutex<String>>,
    config_error: Arc<Mutex<Option<String>>>,
    active_request: Arc<Mutex<Option<ActiveRequest>>>,
    store: SessionStore,
}

impl AppState {
    fn selected_provider(&self) -> String {
        self.selected_provider
            .lock()
            .expect("selected provider lock poisoned")
            .clone()
    }

    fn backend_for(&self, provider_id: &str) -> Result<Backend, String> {
        if let Some(backend) = self
            .backends
            .lock()
            .expect("backends lock poisoned")
            .get(provider_id)
            .cloned()
        {
            return Ok(backend);
        }
        let backend = load_backend(provider_id).map_err(|error| error.to_string())?;
        self.backends
            .lock()
            .expect("backends lock poisoned")
            .insert(provider_id.to_string(), backend.clone());
        Ok(backend)
    }

    fn with_backend<T>(&self, f: impl FnOnce(&Backend) -> Result<T, String>) -> Result<T, String> {
        let provider = self.selected_provider();
        let backend = self.backend_for(&provider).map_err(|error| {
            self.config_error
                .lock()
                .expect("config lock poisoned")
                .clone()
                .unwrap_or(error)
        })?;
        f(&backend)
    }

    fn with_idle<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let active = self
            .active_request
            .lock()
            .expect("active request lock poisoned");
        if active.is_some() {
            return Err(
                "Finish or stop the active response before changing its runtime settings."
                    .to_string(),
            );
        }
        action()
    }
}

fn clear_active_request(active: &Arc<Mutex<Option<ActiveRequest>>>, request_id: &str) {
    let mut slot = active.lock().expect("active request lock poisoned");
    if slot
        .as_ref()
        .is_some_and(|request| request.request_id == request_id)
    {
        *slot = None;
    }
}

fn validate_send_input(
    request_id: &str,
    messages: &[Message],
    images: &[ImageAttachment],
) -> Result<(), String> {
    if request_id.is_empty()
        || request_id.len() > MAX_REQUEST_ID_BYTES
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("request_id must be a short ASCII identifier".to_string());
    }
    if messages.is_empty() || messages.len() > MAX_REQUEST_MESSAGES {
        return Err(format!(
            "A request must contain 1 to {MAX_REQUEST_MESSAGES} messages."
        ));
    }
    let mut text_bytes = 0usize;
    for message in messages {
        if !matches!(message.role.as_str(), "user" | "assistant" | "system") {
            return Err("A message has an unsupported role.".to_string());
        }
        let message_limit = if message.role == "assistant" {
            MAX_ASSISTANT_MESSAGE_BYTES
        } else {
            MAX_USER_MESSAGE_BYTES
        };
        if message.content.len() > message_limit {
            return Err(format!(
                "A {} message exceeds the {} MiB conversation limit.",
                message.role,
                message_limit / 1024 / 1024
            ));
        }
        text_bytes = text_bytes
            .checked_add(message.content.len())
            .ok_or_else(|| "The request text is too large.".to_string())?;
        if text_bytes > MAX_REQUEST_TEXT_BYTES {
            return Err("The combined conversation is too large to send safely.".to_string());
        }
    }
    if images.len() > MAX_ATTACHMENTS {
        return Err(format!(
            "Attach at most {MAX_ATTACHMENTS} images to one request."
        ));
    }
    let attachment_bytes = images.iter().try_fold(0usize, |total, image| {
        if image.mime.len() > 128 || !image.mime.starts_with("image/") {
            return Err("An attachment has an unsupported image type.".to_string());
        }
        total
            .checked_add(image.data.len())
            .ok_or_else(|| "The combined attachments are too large.".to_string())
    })?;
    if attachment_bytes > MAX_ATTACHMENT_BYTES {
        return Err("The combined attachments are too large to send safely.".to_string());
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct Status {
    provider: String,
    mode: String,
    model: String,
    effort: String,
    session: String,
    session_id: Option<String>,
    routing_profile: String,
    route_explanation: String,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct EffortInfo {
    id: String,
    label: String,
    description: String,
    default: bool,
}

#[derive(serde::Serialize)]
struct ModelInfo {
    id: String,
    name: String,
    description: Option<String>,
    context_window: Option<u64>,
    supports_effort: bool,
    efforts: Vec<EffortInfo>,
}

#[derive(serde::Serialize)]
struct SmokeConfig {
    enabled: bool,
    prompt: String,
    provider: String,
    agent: bool,
    effort: String,
    session_id: String,
}

#[derive(serde::Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    publisher: &'static str,
    license: &'static str,
    repository: &'static str,
    config_path: String,
    session_path: String,
}

#[derive(Clone, serde::Serialize)]
struct ChatTextEvent {
    request_id: String,
    text: String,
}

#[derive(Clone, serde::Serialize)]
struct ChatDoneEvent {
    request_id: String,
}

fn make_status(state: &AppState) -> Status {
    let provider_id = state.selected_provider();
    let profile = state
        .routing_profile
        .lock()
        .expect("routing profile lock poisoned")
        .clone();
    match state.backend_for(&provider_id) {
        Ok(backend) => Status {
            provider: backend.provider_id().to_string(),
            mode: backend.mode_label().to_string(),
            model: backend.model_label(),
            effort: backend.effort_label(),
            session: backend.session_label(),
            session_id: backend.session_id(),
            route_explanation: status_route_explanation(&profile),
            routing_profile: profile,
            error: None,
        },
        Err(error) => Status {
            provider: String::new(),
            mode: String::new(),
            model: String::new(),
            effort: String::new(),
            session: String::new(),
            session_id: None,
            routing_profile: profile,
            route_explanation: String::new(),
            error: state
                .config_error
                .lock()
                .expect("config lock poisoned")
                .clone()
                .or(Some(error)),
        },
    }
}

fn status_route_explanation(profile: &str) -> String {
    if profile == "manual" {
        "Manual provider selection; automatic fallback is disabled.".to_string()
    } else {
        format!(
            "Automatic routing profile “{profile}” is active. Open Why this route? for the current mode-specific decision."
        )
    }
}

#[tauri::command]
fn status(state: State<AppState>) -> Status {
    make_status(&state)
}

#[tauri::command]
async fn send(
    request_id: String,
    messages: Vec<Message>,
    images: Vec<ImageAttachment>,
    agent: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_send_input(&request_id, &messages, &images)?;
    let cancel = CancellationToken::new();
    {
        let mut active = state
            .active_request
            .lock()
            .expect("active request lock poisoned");
        if active.is_some() {
            return Err("Another request is already running.".to_string());
        }
        *active = Some(ActiveRequest {
            request_id: request_id.clone(),
            cancel: cancel.clone(),
        });
    }

    let selected_provider = state.selected_provider();
    let profile_id = state
        .routing_profile
        .lock()
        .expect("routing profile lock poisoned")
        .clone();
    let catalog = provider_catalog();
    let candidate_ids = if profile_id == "manual" {
        let Some(selected) = catalog
            .iter()
            .find(|provider| provider.id == selected_provider)
        else {
            clear_active_request(&state.active_request, &request_id);
            return Err(format!("Unknown provider: {selected_provider}"));
        };
        if agent && !selected.supports_agent {
            clear_active_request(&state.active_request, &request_id);
            return Err(format!(
                "{} supports Chat mode only. Choose Grok, Claude Code, Codex, Gemini, or an Agent and tools routing profile.",
                selected.name
            ));
        }
        if !agent && !selected.supports_chat {
            clear_active_request(&state.active_request, &request_id);
            return Err(format!(
                "{} is available in Agent mode only. Turn on Agent or choose a direct/local Chat route.",
                selected.name
            ));
        }
        vec![selected_provider]
    } else {
        decide(&profile_id, &catalog, agent)
            .candidates
            .into_iter()
            .map(|candidate| candidate.provider_id)
            .collect()
    };
    let mut routes = Vec::new();
    let mut load_errors = Vec::new();
    for provider_id in candidate_ids {
        match state.backend_for(&provider_id) {
            Ok(backend) => {
                let name = catalog
                    .iter()
                    .find(|provider| provider.id == provider_id)
                    .map(|provider| provider.name.clone())
                    .unwrap_or_else(|| provider_id.clone());
                routes.push((provider_id, name, backend));
            }
            Err(error) => load_errors.push(format!("{provider_id}: {error}")),
        }
    }
    if routes.is_empty() {
        clear_active_request(&state.active_request, &request_id);
        return Err(if load_errors.is_empty() {
            "No route satisfies the selected profile.".to_string()
        } else {
            format!("No route could start: {}", load_errors.join("; "))
        });
    }

    let selected_state = Arc::clone(&state.selected_provider);
    let session_owner_state = Arc::clone(&state.session_owner_provider);
    let active_request = Arc::clone(&state.active_request);

    tauri::async_runtime::spawn(async move {
        let route_count = routes.len();
        let mut last_error = None;
        let mut total_thought_bytes = 0usize;
        for (index, (provider_id, provider_name, backend)) in routes.into_iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let _ = app.emit(
                "chat-thought",
                ChatTextEvent {
                    request_id: request_id.clone(),
                    text: format!("Route: {provider_name}\n"),
                },
            );
            let (route_tx, mut route_rx) = stream_event_channel();
            let transcript_owner = session_owner_state
                .lock()
                .expect("session owner lock poisoned")
                .clone();
            if route_requires_context_rebuild(&provider_id, transcript_owner.as_deref()) {
                // Selection and transcript ownership are separate. In
                // particular, primary -> fallback -> primary must rebuild the
                // primary runtime from the complete labeled transcript rather
                // than resume a native session that predates the fallback.
                backend.reset_session();
            }
            backend.stream_chat(
                messages.clone(),
                images.clone(),
                agent,
                cancel.clone(),
                route_tx,
            );
            let mut saw_answer = false;
            let mut saw_provider_progress = false;
            let mut terminal_event = false;
            let mut route_failed = false;
            let mut answer_bytes = 0usize;
            while let Some(event) = route_rx.recv().await {
                match event {
                    StreamEvent::Token(token) => {
                        if token.len() > MAX_STREAM_EVENT_BYTES
                            || answer_bytes.saturating_add(token.len()) > MAX_ANSWER_BYTES
                        {
                            cancel.cancel();
                            terminal_event = true;
                            let _ = app.emit(
                                "chat-error",
                                ChatTextEvent {
                                    request_id: request_id.clone(),
                                    text: "The provider response exceeded Consilium's 32 MiB limit and was stopped."
                                        .to_string(),
                                },
                            );
                            break;
                        }
                        answer_bytes += token.len();
                        saw_answer = true;
                        *selected_state
                            .lock()
                            .expect("selected provider lock poisoned") = provider_id.clone();
                        *session_owner_state
                            .lock()
                            .expect("session owner lock poisoned") = Some(provider_id.clone());
                        let _ = app.emit(
                            "chat-token",
                            ChatTextEvent {
                                request_id: request_id.clone(),
                                text: token,
                            },
                        );
                    }
                    StreamEvent::Thought(thought) => {
                        if thought.len() > MAX_STREAM_EVENT_BYTES
                            || total_thought_bytes.saturating_add(thought.len()) > MAX_THOUGHT_BYTES
                        {
                            cancel.cancel();
                            terminal_event = true;
                            let _ = app.emit(
                                "chat-error",
                                ChatTextEvent {
                                    request_id: request_id.clone(),
                                    text: "The provider reasoning stream exceeded Consilium's 16 MiB limit and was stopped."
                                        .to_string(),
                                },
                            );
                            break;
                        }
                        total_thought_bytes += thought.len();
                        saw_provider_progress = true;
                        let _ = app.emit(
                            "chat-thought",
                            ChatTextEvent {
                                request_id: request_id.clone(),
                                text: thought,
                            },
                        );
                    }
                    StreamEvent::Finished => {
                        if !saw_answer {
                            terminal_event = true;
                            route_failed = true;
                            let error =
                                format!("{provider_name} completed without returning answer text");
                            last_error = Some(error.clone());
                            if can_fallback(
                                saw_answer,
                                saw_provider_progress,
                                agent,
                                index,
                                route_count,
                            ) {
                                let _ = app.emit(
                                    "chat-thought",
                                    ChatTextEvent {
                                        request_id: request_id.clone(),
                                        text: format!(
                                            "{provider_name} returned no answer; trying the next fallback.\n"
                                        ),
                                    },
                                );
                            } else {
                                let _ = app.emit(
                                    "chat-error",
                                    ChatTextEvent {
                                        request_id: request_id.clone(),
                                        text: error,
                                    },
                                );
                            }
                            break;
                        }
                        *selected_state
                            .lock()
                            .expect("selected provider lock poisoned") = provider_id.clone();
                        *session_owner_state
                            .lock()
                            .expect("session owner lock poisoned") = Some(provider_id.clone());
                        terminal_event = true;
                        let _ = app.emit(
                            "chat-finished",
                            ChatDoneEvent {
                                request_id: request_id.clone(),
                            },
                        );
                        break;
                    }
                    StreamEvent::Error(error) => {
                        terminal_event = true;
                        route_failed = true;
                        if can_fallback(
                            saw_answer,
                            saw_provider_progress,
                            agent,
                            index,
                            route_count,
                        ) {
                            last_error = Some(error.clone());
                            let _ = app.emit(
                                "chat-thought",
                                ChatTextEvent {
                                    request_id: request_id.clone(),
                                    text: format!(
                                        "{provider_name} failed before returning text; trying the next fallback.\n"
                                    ),
                                },
                            );
                        } else {
                            let _ = app.emit(
                                "chat-error",
                                ChatTextEvent {
                                    request_id: request_id.clone(),
                                    text: error,
                                },
                            );
                        }
                        break;
                    }
                }
            }
            if cancel.is_cancelled() {
                break;
            }
            if terminal_event {
                if route_failed
                    && can_fallback(saw_answer, saw_provider_progress, agent, index, route_count)
                {
                    continue;
                }
                clear_active_request(&active_request, &request_id);
                return;
            }
            let error = format!("{provider_name} closed before reporting success or failure");
            last_error = Some(error.clone());
            if can_fallback(saw_answer, saw_provider_progress, agent, index, route_count) {
                let _ = app.emit(
                    "chat-thought",
                    ChatTextEvent {
                        request_id: request_id.clone(),
                        text: format!(
                            "{provider_name} closed before producing output; trying the next fallback.\n"
                        ),
                    },
                );
                continue;
            }
            let _ = app.emit(
                "chat-error",
                ChatTextEvent {
                    request_id: request_id.clone(),
                    text: error,
                },
            );
            clear_active_request(&active_request, &request_id);
            return;
        }
        if !cancel.is_cancelled() {
            let _ = app.emit(
                "chat-error",
                ChatTextEvent {
                    request_id: request_id.clone(),
                    text: last_error.unwrap_or_else(|| "All configured routes failed.".to_string()),
                },
            );
        }
        clear_active_request(&active_request, &request_id);
    });

    Ok(())
}

fn can_fallback(
    saw_answer: bool,
    _saw_provider_progress: bool,
    agent: bool,
    route_index: usize,
    route_count: usize,
) -> bool {
    !agent && !saw_answer && route_index + 1 < route_count
}

fn route_requires_context_rebuild(
    attempted_provider: &str,
    transcript_owner: Option<&str>,
) -> bool {
    transcript_owner != Some(attempted_provider)
}

#[tauri::command]
async fn interrupt(state: State<'_, AppState>) -> Result<(), String> {
    let token = state
        .active_request
        .lock()
        .expect("active request lock poisoned")
        .as_ref()
        .map(|request| request.cancel.clone());
    if let Some(token) = token {
        token.cancel();
        for _ in 0..1_500 {
            if state
                .active_request
                .lock()
                .expect("active request lock poisoned")
                .is_none()
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        return Err(
            "The provider is still stopping. Consilium kept the request locked; try Stop again in a moment."
                .to_string(),
        );
    }
    Ok(())
}

#[tauri::command]
fn new_session(state: State<AppState>) -> Result<Status, String> {
    state.with_idle(|| {
        state.backend_for(&state.selected_provider())?;
        for backend in state
            .backends
            .lock()
            .expect("backends lock poisoned")
            .values()
        {
            backend.reset_session();
        }
        *state
            .session_owner_provider
            .lock()
            .expect("session owner lock poisoned") = None;
        Ok(make_status(&state))
    })
}

#[tauri::command]
fn resume_session(
    grok_session: Option<String>,
    agent: Option<bool>,
    effort: Option<String>,
    owner_provider: Option<String>,
    state: State<AppState>,
) -> Result<Status, String> {
    state.with_idle(|| {
        let selected_provider = state.selected_provider();
        let selected = state.backend_for(&selected_provider)?;
        let backends = state.backends.lock().expect("backends lock poisoned");
        restore_selected_native_session(&backends, &selected, grok_session, agent, effort);
        drop(backends);
        *state
            .session_owner_provider
            .lock()
            .expect("session owner lock poisoned") =
            owner_provider.filter(|owner| owner == &selected_provider);
        Ok(make_status(&state))
    })
}

fn restore_selected_native_session(
    cached_backends: &HashMap<String, Backend>,
    selected: &Backend,
    session_id: Option<String>,
    agent: Option<bool>,
    effort: Option<String>,
) {
    // A loaded chat owns at most one provider-native runtime. Forget every
    // cached runtime first so switching between saved chats cannot leave a
    // stale session attached to an inactive provider.
    for backend in cached_backends.values() {
        backend.reset_session();
    }
    selected.set_session(session_id, agent, effort);
}

#[tauri::command]
fn set_model(model: String, state: State<AppState>) -> Result<Status, String> {
    state.with_idle(|| {
        state.with_backend(|backend| backend.set_model(&model))?;
        *state
            .session_owner_provider
            .lock()
            .expect("session owner lock poisoned") = None;
        Ok(make_status(&state))
    })
}

#[tauri::command]
fn restore_model_for_session(
    model: Option<String>,
    state: State<AppState>,
) -> Result<Status, String> {
    state.with_idle(|| {
        state.with_backend(|backend| backend.restore_model_for_session(model))?;
        Ok(make_status(&state))
    })
}

#[tauri::command]
fn set_effort(effort: String, state: State<AppState>) -> Result<Status, String> {
    state.with_idle(|| {
        let value = if effort.is_empty() || effort == "default" {
            None
        } else {
            Some(effort)
        };
        state.with_backend(|backend| backend.set_effort(value))?;
        Ok(make_status(&state))
    })
}

fn cli_reasoning_efforts() -> Vec<EffortInfo> {
    vec![
        EffortInfo {
            id: "low".into(),
            label: "Low Effort".into(),
            description: "Quick, fast implementations".into(),
            default: false,
        },
        EffortInfo {
            id: "medium".into(),
            label: "Medium Effort".into(),
            description: "Balanced effort with standard implementation and testing".into(),
            default: false,
        },
        EffortInfo {
            id: "high".into(),
            label: "High Effort".into(),
            description: "Highest implementation quality with extensive reasoning".into(),
            default: true,
        },
        EffortInfo {
            id: "xhigh".into(),
            label: "Extra High Effort".into(),
            description: "More exhaustive reasoning for difficult builds and debugging".into(),
            default: false,
        },
        EffortInfo {
            id: "max".into(),
            label: "Max Effort".into(),
            description: "Maximum available Grok Build reasoning effort".into(),
            default: false,
        },
    ]
}

fn codex_reasoning_efforts() -> Vec<EffortInfo> {
    cli_reasoning_efforts()
        .into_iter()
        .filter(|effort| effort.id != "max")
        .map(|mut effort| {
            if effort.id == "xhigh" {
                effort.description =
                    "Extra-high Codex reasoning for difficult builds and debugging".into();
            }
            effort
        })
        .collect()
}

fn effort_rank(id: &str) -> usize {
    match id {
        "low" => 0,
        "medium" => 1,
        "high" => 2,
        "xhigh" => 3,
        "max" => 4,
        _ => 99,
    }
}

fn fallback_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: GROK_DEFAULT_MODEL.into(),
            name: "Grok 4.5".into(),
            description: Some(
                "Grok Build default for coding, agentic tasks, and knowledge work".into(),
            ),
            context_window: Some(500_000),
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
        ModelInfo {
            id: "grok-composer-2.5-fast".into(),
            name: "Composer 2.5 Fast".into(),
            description: Some("Cursor's latest fast coding model".into()),
            context_window: None,
            supports_effort: false,
            efforts: Vec::new(),
        },
    ]
}

fn model_from_map(map: &serde_json::Map<String, serde_json::Value>) -> Option<ModelInfo> {
    let hidden = map.get("hidden").and_then(|x| x.as_bool()).unwrap_or(false);
    let id = map.get("id").and_then(|x| x.as_str())?;
    let name = map.get("name").and_then(|x| x.as_str())?;
    if hidden {
        return None;
    }

    let efforts = map
        .get("reasoning_efforts")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|effort| {
                    let map = effort.as_object()?;
                    let id = map.get("id").and_then(|x| x.as_str())?;
                    let label = map.get("label").and_then(|x| x.as_str()).unwrap_or(id);
                    Some(EffortInfo {
                        id: id.to_string(),
                        label: label.to_string(),
                        description: map
                            .get("description")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        default: map
                            .get("default")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let supports_effort = map
        .get("supports_reasoning_effort")
        .and_then(|x| x.as_bool())
        .unwrap_or(!efforts.is_empty());

    let mut efforts = if supports_effort && efforts.is_empty() && id == GROK_DEFAULT_MODEL {
        cli_reasoning_efforts()
    } else {
        efforts
    };
    if supports_effort && id == GROK_DEFAULT_MODEL {
        for effort in cli_reasoning_efforts() {
            if !efforts.iter().any(|existing| existing.id == effort.id) {
                efforts.push(effort);
            }
        }
    }
    efforts.sort_by_key(|effort| effort_rank(&effort.id));

    Some(ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: map
            .get("description")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        context_window: map.get("context_window").and_then(|x| x.as_u64()),
        supports_effort,
        efforts,
    })
}

/// Models the grok CLI knows about, read from its local cache; falls
/// back to the known Grok 4.5 default if the cache is missing or unreadable.
fn grok_models() -> Vec<ModelInfo> {
    let Some(home) = user_home_dir() else {
        return fallback_models();
    };
    let path = home.join(".grok/models_cache.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return fallback_models();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return fallback_models();
    };
    let mut models = value
        .get("models")
        .and_then(|x| x.as_object())
        .map(|models| {
            models
                .values()
                .filter_map(|model| {
                    model
                        .get("info")
                        .and_then(|x| x.as_object())
                        .or_else(|| model.as_object())
                })
                .filter_map(model_from_map)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if models.is_empty() {
        fallback_models()
    } else {
        models.sort_by_key(|m| if m.id == GROK_DEFAULT_MODEL { 0 } else { 1 });
        models
    }
}

fn gemini_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "antigravity-default".into(),
        name: "Gemini via Antigravity".into(),
        description: Some("Google-managed default Gemini model".into()),
        context_window: Some(1_000_000),
        supports_effort: false,
        efforts: Vec::new(),
    }]
}

fn claude_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "default".into(),
            name: "Claude Code default".into(),
            description: Some(
                "Uses the model selected by the installed Claude Code client.".into(),
            ),
            context_window: None,
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
        ModelInfo {
            id: "sonnet".into(),
            name: "Claude Sonnet alias".into(),
            description: Some(
                "Claude Code's current Sonnet alias; the CLI resolves the exact release.".into(),
            ),
            context_window: None,
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
        ModelInfo {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            description: Some(
                "Pins Claude Code to Sonnet 4.6 instead of following the moving Sonnet alias."
                    .into(),
            ),
            context_window: None,
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
        ModelInfo {
            id: "opus".into(),
            name: "Claude Opus alias".into(),
            description: Some(
                "Claude Code's current Opus alias; the CLI resolves the exact release.".into(),
            ),
            context_window: None,
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
        ModelInfo {
            id: "haiku".into(),
            name: "Claude Haiku alias".into(),
            description: Some(
                "Claude Code's current Haiku alias; the CLI resolves the exact release.".into(),
            ),
            context_window: Some(200_000),
            supports_effort: true,
            efforts: cli_reasoning_efforts(),
        },
    ]
}

fn codex_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "default".into(),
        name: "Codex CLI default".into(),
        description: Some(
            "Uses the default model selected by the installed Codex CLI and signed-in plan.".into(),
        ),
        context_window: None,
        supports_effort: true,
        efforts: codex_reasoning_efforts(),
    }]
}

fn open_model_suggestions(provider: &str) -> Vec<OpenModelInfo> {
    if provider == "openai-compatible" {
        open_model_catalog()
    } else {
        Vec::new()
    }
}

fn current_api_model(provider: &str, state: &AppState) -> Vec<ModelInfo> {
    let current = state
        .backend_for(provider)
        .map(|backend| backend.model_label())
        .unwrap_or_else(|_| "configured model".to_string());
    let mut models = vec![ModelInfo {
        id: current.clone(),
        name: current,
        description: Some("The exact model ID configured for this endpoint.".into()),
        context_window: provider_catalog()
            .into_iter()
            .find(|candidate| candidate.id == provider)
            .and_then(|candidate| {
                (candidate.context_window > 0).then_some(candidate.context_window)
            }),
        supports_effort: false,
        efforts: Vec::new(),
    }];

    // Checkpoint IDs from the open-model catalog are useful suggestions for
    // user-controlled compatible endpoints. They are not necessarily valid
    // model IDs at vendors' fixed hosted APIs, which expose only the exact ID
    // the user configured above.
    let suggested = open_model_suggestions(provider);
    for model in suggested {
        if models.iter().any(|existing| existing.id == model.id) {
            continue;
        }
        models.push(ModelInfo {
            id: model.id,
            name: model.name,
            description: Some(format!(
                "{} Endpoint suggestion only; use the exact ID exposed by your server.",
                model.note
            )),
            context_window: (model.context_window > 0).then_some(model.context_window),
            supports_effort: false,
            efforts: Vec::new(),
        });
    }
    models
}

#[tauri::command]
fn list_models(state: State<AppState>) -> Vec<ModelInfo> {
    let provider = state.selected_provider();
    match provider.as_str() {
        "grok" => grok_models(),
        "gemini" => gemini_models(),
        "claude" => claude_models(),
        "codex" => codex_models(),
        _ => current_api_model(&provider, &state),
    }
}

#[tauri::command]
fn list_providers() -> Vec<ProviderInfo> {
    provider_catalog()
}

#[tauri::command]
fn list_routing_profiles() -> Vec<RoutingProfile> {
    routing_profiles()
}

#[tauri::command]
fn routing_decision(agent: bool, state: State<AppState>) -> RouteDecision {
    let profile = state
        .routing_profile
        .lock()
        .expect("routing profile lock poisoned")
        .clone();
    if profile != "manual" {
        return decide(&profile, &provider_catalog(), agent);
    }
    let selected = state.selected_provider();
    let provider = provider_catalog()
        .into_iter()
        .find(|candidate| candidate.id == selected);
    let candidates = provider
        .as_ref()
        .map(|provider| {
            vec![RouteCandidate {
                provider_id: provider.id.clone(),
                provider_name: provider.name.clone(),
                score: 0,
                explanation: "Explicitly selected; automatic scoring and fallback are disabled."
                    .to_string(),
            }]
        })
        .unwrap_or_default();
    RouteDecision {
        profile_id: "manual".to_string(),
        profile_name: "Manual".to_string(),
        selected_provider: provider.map(|provider| provider.id),
        candidates,
        explanation:
            "Manual provider selection; Consilium will not switch providers automatically."
                .to_string(),
    }
}

#[tauri::command]
fn set_routing_profile(
    profile_id: String,
    agent: bool,
    state: State<AppState>,
) -> Result<Status, String> {
    state.with_idle(|| {
        if profile_id == "manual" {
            *state
                .routing_profile
                .lock()
                .expect("routing profile lock poisoned") = profile_id;
            return Ok(make_status(&state));
        }
        if !routing_profiles()
            .iter()
            .any(|profile| profile.id == profile_id)
        {
            return Err(format!("Unknown routing profile: {profile_id}"));
        }
        let decision = decide(&profile_id, &provider_catalog(), agent);
        let selected = decision
            .selected_provider
            .ok_or_else(|| decision.explanation.clone())?;
        state.backend_for(&selected)?;
        *state
            .selected_provider
            .lock()
            .expect("selected provider lock poisoned") = selected;
        *state
            .routing_profile
            .lock()
            .expect("routing profile lock poisoned") = profile_id;
        *state.config_error.lock().expect("config lock poisoned") = None;
        Ok(make_status(&state))
    })
}

#[tauri::command]
fn list_open_models() -> Vec<OpenModelInfo> {
    open_model_catalog()
}

#[tauri::command]
fn set_provider(provider_id: String, state: State<AppState>) -> Result<Status, String> {
    state.with_idle(|| {
        let provider = provider_catalog()
            .into_iter()
            .find(|provider| provider.id == provider_id && provider.active)
            .ok_or_else(|| format!("{provider_id} is not an active provider adapter"))?;
        if !provider.available {
            return Err(format!(
                "{} is not ready. {}",
                provider.name, provider.login_command
            ));
        }
        state.backend_for(&provider_id)?;
        *state
            .selected_provider
            .lock()
            .expect("selected provider lock poisoned") = provider_id;
        *state
            .routing_profile
            .lock()
            .expect("routing profile lock poisoned") = "manual".to_string();
        *state.config_error.lock().expect("config lock poisoned") = None;
        Ok(make_status(&state))
    })
}

fn resolve_user_home<F>(is_windows: bool, mut env: F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<OsString>,
{
    if is_windows {
        if let Some(profile) = env("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (env("HOMEDRIVE"), env("HOMEPATH")) {
            let mut combined = drive;
            combined.push(path);
            return Some(PathBuf::from(combined));
        }
    }
    env("HOME").map(PathBuf::from)
}

fn user_home_dir() -> Option<PathBuf> {
    resolve_user_home(cfg!(windows), |key| std::env::var_os(key))
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn find_binary(binary: &str, extra_paths: &[&str]) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(home) = user_home_dir() {
        for rel in extra_paths {
            let candidate = home.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn find_brave() -> Option<PathBuf> {
    ["brave", "brave-browser", "brave-browser-stable"]
        .into_iter()
        .find_map(|name| find_binary(name, &[]))
        .or_else(|| {
            let snap = PathBuf::from("/snap/bin/brave");
            snap.is_file().then_some(snap)
        })
}

#[cfg(unix)]
fn preferred_browser_override() -> Option<OsString> {
    std::env::var_os("BROWSER")
        .filter(|value| !value.is_empty())
        .or_else(|| find_brave().map(PathBuf::into_os_string))
}

/// Runs Google's supported Antigravity login TUI in a pseudo-terminal. The
/// first selection is Google OAuth, so seed one Enter key and then forward
/// all user input directly. Antigravity owns OAuth and keyring persistence.
#[cfg(unix)]
fn run_gemini_login_helper(antigravity: &Path) -> Result<(), String> {
    let script = find_binary("script", &[])
        .ok_or_else(|| "the `script` pseudo-terminal utility is not installed".to_string())?;
    let browser = preferred_browser_override();
    let command = shell_quote(&antigravity.display().to_string());

    println!("CONSILIUM Gemini sign-in");
    println!(
        "Google's Antigravity CLI will open verification in your configured or default browser."
    );
    println!("Complete sign-in there, then paste the code here if requested.\n");

    let mut login = Command::new(script);
    login.args(["-qefc", &command, "/dev/null"]);
    if let Some(browser) = browser {
        login.env("BROWSER", browser);
    }
    let mut child = login
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to start Antigravity login: {e}"))?;
    let mut child_in = child
        .stdin
        .take()
        .ok_or_else(|| "failed to control the Antigravity login terminal".to_string())?;

    std::thread::spawn(move || {
        let _ = child_in.write_all(b"\r");
        let _ = child_in.flush();
        let mut terminal_in = std::io::stdin().lock();
        let _ = std::io::copy(&mut terminal_in, &mut child_in);
    });

    let status = child
        .wait()
        .map_err(|e| format!("failed waiting for Antigravity login: {e}"))?;
    if !status.success() {
        return Err(format!("Antigravity login exited with status {status}"));
    }
    Ok(())
}

#[cfg(unix)]
fn terminal_launch_args(
    terminal: &str,
    title: &str,
    cwd: &Path,
    script: &str,
) -> Option<Vec<String>> {
    let cwd = cwd.display().to_string();
    match terminal {
        "alacritty" => Some(vec![
            "--title".into(),
            title.into(),
            "-e".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "kitty" => Some(vec![
            "--title".into(),
            title.into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "wezterm" => Some(vec![
            "start".into(),
            "--cwd".into(),
            cwd,
            "--".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "gnome-terminal" | "mate-terminal" => Some(vec![
            format!("--title={title}"),
            "--".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "konsole" => Some(vec![
            "--new-tab".into(),
            "-p".into(),
            format!("tabtitle={title}"),
            "--workdir".into(),
            cwd,
            "-e".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "xfce4-terminal" => Some(vec![
            "--title".into(),
            title.into(),
            "--working-directory".into(),
            cwd,
            "--execute".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        "x-terminal-emulator" | "xterm" => Some(vec![
            "-T".into(),
            title.into(),
            "-e".into(),
            "bash".into(),
            "-lc".into(),
            script.into(),
        ]),
        _ => None,
    }
}

#[cfg(unix)]
fn spawn_terminal(
    title: &str,
    cwd: &Path,
    program: &Path,
    arguments: &[OsString],
) -> Result<(), String> {
    let invocation = std::iter::once(program.as_os_str())
        .chain(arguments.iter().map(OsString::as_os_str))
        .map(|value| shell_quote(&value.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "cd {}; {}; printf '\\n%s\\n' {}; read -r -p 'Press Enter to close this terminal...'",
        shell_quote(&cwd.display().to_string()),
        invocation,
        shell_quote(
            "Authentication command finished. Return to CONSILIUM and select the provider."
        )
    );

    let mut candidates: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for terminal in [
        "alacritty",
        "kitty",
        "wezterm",
        "gnome-terminal",
        "mate-terminal",
        "konsole",
        "xfce4-terminal",
        "x-terminal-emulator",
        "xterm",
    ] {
        if let (Some(bin), Some(args)) = (
            find_binary(terminal, &[]),
            terminal_launch_args(terminal, title, cwd, &script),
        ) {
            candidates.push((bin, args));
        }
    }

    for (bin, args) in candidates {
        if Command::new(&bin).args(&args).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("No supported terminal emulator was found to run the provider login command".to_string())
}

#[cfg(windows)]
fn spawn_terminal(
    _title: &str,
    cwd: &Path,
    program: &Path,
    arguments: &[OsString],
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    // CREATE_NEW_CONSOLE keeps the interactive vendor login visible while
    // passing the executable and every argument directly to std::process.
    // Rust's Windows launcher also handles .cmd/.bat shims using its escaped
    // batch-file path; Consilium never constructs a command line for a shell.
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to launch provider login: {error}"))
}

#[cfg(not(any(unix, windows)))]
fn spawn_terminal(
    _title: &str,
    _cwd: &Path,
    _program: &Path,
    _arguments: &[OsString],
) -> Result<(), String> {
    Err("Provider login launching is not available on this platform".to_string())
}

#[tauri::command]
fn login_provider(provider_id: String) -> Result<String, String> {
    let cwd = user_home_dir().unwrap_or_else(std::env::temp_dir);
    match provider_id.as_str() {
        "gemini" => {
            let antigravity = gemini::find_gemini().ok_or_else(|| {
                "Antigravity CLI is not installed. Install it from https://antigravity.google/docs/cli-overview."
                    .to_string()
            })?;
            #[cfg(unix)]
            {
                let helper = std::env::current_exe()
                    .map_err(|e| format!("cannot locate the CONSILIUM executable: {e}"))?;
                spawn_terminal(
                    "Gemini Login",
                    &cwd,
                    &helper,
                    &[
                        OsString::from("--gemini-login"),
                        antigravity.into_os_string(),
                    ],
                )?;
                Ok("Gemini login launched through Google's Antigravity CLI. Complete verification in the browser while the terminal stays open.".to_string())
            }
            #[cfg(not(unix))]
            {
                spawn_terminal("Gemini Login", &cwd, &antigravity, &[])?;
                Ok("Gemini login launched through Google's Antigravity CLI.".to_string())
            }
        }
        "grok" => {
            let grok = cli::find_grok().ok_or_else(|| "Grok CLI is not installed".to_string())?;
            spawn_terminal("Grok Login", &cwd, &grok, &[OsString::from("login")])?;
            Ok("Grok login launched.".to_string())
        }
        "claude" => {
            let claude =
                claude::find_claude().ok_or_else(|| "Claude Code is not installed".to_string())?;
            spawn_terminal(
                "Claude Code Login",
                &cwd,
                &claude,
                &[OsString::from("auth"), OsString::from("login")],
            )?;
            Ok("Claude Code login launched.".to_string())
        }
        "codex" => {
            let codex =
                codex::find_codex().ok_or_else(|| "Codex CLI is not installed".to_string())?;
            spawn_terminal(
                "Codex Login",
                &cwd,
                &codex,
                &[OsString::from("login"), OsString::from("--device-auth")],
            )?;
            Ok("Codex device login launched.".to_string())
        }
        other => Err(format!("{other} does not have a login launcher yet")),
    }
}

#[tauri::command]
fn list_sessions(state: State<AppState>) -> Result<Vec<SessionMeta>, String> {
    state.store.list()
}

#[tauri::command]
fn load_session(id: String, state: State<AppState>) -> Result<StoredSession, String> {
    state.store.load(&id)
}

#[tauri::command]
fn save_session(session: StoredSession, state: State<AppState>) -> Result<(), String> {
    state.store.save(session)
}

#[tauri::command]
fn delete_session(id: String, state: State<AppState>) -> Result<(), String> {
    state.store.delete(&id)
}

#[tauri::command]
fn session_store_health(state: State<AppState>) -> SessionStoreHealth {
    state.store.health()
}

#[tauri::command]
fn restore_sessions_backup(state: State<AppState>) -> Result<usize, String> {
    state.with_idle(|| state.store.restore_backup())
}

#[tauri::command]
fn app_info(state: State<AppState>) -> AppInfo {
    AppInfo {
        name: "Consilium",
        version: env!("CARGO_PKG_VERSION"),
        publisher: "Flintglade",
        license: "Apache-2.0",
        repository: "https://github.com/flintglade/consilium",
        config_path: grok_chat_core::user_config_env_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No per-user configuration path is available".to_string()),
        session_path: state.store.health().path,
    }
}

/// Test instrumentation, active only with GROK_CHAT_SMOKE=1: the frontend
/// auto-sends one message on load and reports the streamed reply back so
/// an external harness can verify the full JS->Rust->backend->JS loop.
#[tauri::command]
fn smoke_mode() -> bool {
    std::env::var("GROK_CHAT_SMOKE").is_ok_and(|v| v == "1")
}

#[tauri::command]
fn smoke_config() -> SmokeConfig {
    SmokeConfig {
        enabled: smoke_mode(),
        prompt: std::env::var("GROK_CHAT_SMOKE_PROMPT")
            .unwrap_or_else(|_| "smoke test hello".to_string()),
        provider: std::env::var("GROK_CHAT_SMOKE_PROVIDER").unwrap_or_default(),
        agent: std::env::var("GROK_CHAT_SMOKE_AGENT").is_ok_and(|v| v == "1"),
        effort: std::env::var("GROK_CHAT_SMOKE_EFFORT").unwrap_or_default(),
        session_id: std::env::var("GROK_CHAT_SMOKE_SESSION_ID").unwrap_or_default(),
    }
}

#[tauri::command]
fn smoke_report(text: String, app: AppHandle) {
    println!("SMOKE_OK len={} text={:?}", text.chars().count(), text);
    app.exit(0);
}

#[derive(Clone, Copy)]
struct StartupRoute<'a> {
    id: &'a str,
    active: bool,
    available: bool,
    configured: bool,
    supports_chat: bool,
    local: bool,
}

impl<'a> From<&'a ProviderInfo> for StartupRoute<'a> {
    fn from(provider: &'a ProviderInfo) -> Self {
        Self {
            id: &provider.id,
            active: provider.active,
            available: provider.available,
            configured: provider.configured,
            supports_chat: provider.supports_chat,
            local: provider.local,
        }
    }
}

fn choose_manual_startup_provider(
    candidates: &[StartupRoute<'_>],
    requested: Option<&str>,
) -> Option<String> {
    let usable_for_chat = |candidate: &&StartupRoute<'_>| {
        candidate.active && candidate.available && candidate.supports_chat
    };

    if let Some(requested) = requested {
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.id == requested && usable_for_chat(candidate))
        {
            return Some(candidate.id.to_string());
        }
    }

    candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| usable_for_chat(candidate))
        .min_by_key(|(catalog_index, candidate)| {
            let readiness_rank = if candidate.configured && candidate.local {
                0
            } else if candidate.configured {
                1
            } else {
                2
            };
            (readiness_rank, *catalog_index)
        })
        .map(|(_, candidate)| candidate.id.to_string())
}

fn routing_profile_starts_in_agent_mode(profile: &str) -> bool {
    profile == "agent-tools"
}

fn no_usable_route_message() -> String {
    "No configured Chat route is available. Open Setup to configure a local OpenAI-compatible endpoint, sign in to an installed provider CLI, or add an optional direct API."
        .to_string()
}

fn initial_route() -> (String, String, HashMap<String, Backend>, Option<String>) {
    // Loading any route also performs the project's safe dotenv discovery. Do
    // this before inspecting provider availability so API-only installations
    // configured in an ignored .env file are represented accurately.
    let grok_backend = load_backend("grok");
    let catalog = provider_catalog();
    let requested_profile = std::env::var("CONSILIUM_ROUTING_PROFILE")
        .ok()
        .filter(|id| routing_profiles().iter().any(|profile| profile.id == *id));
    let requested_provider = std::env::var("CONSILIUM_DEFAULT_PROVIDER").ok();
    let startup_routes = catalog.iter().map(StartupRoute::from).collect::<Vec<_>>();

    let mut profile = requested_profile.unwrap_or_else(|| "manual".to_string());
    let mut provider = if profile != "manual" {
        decide(
            &profile,
            &catalog,
            routing_profile_starts_in_agent_mode(&profile),
        )
        .selected_provider
    } else {
        choose_manual_startup_provider(&startup_routes, requested_provider.as_deref())
    };
    if provider.is_none() {
        // An automatic profile with no usable Chat route degrades to the same
        // conservative manual startup choice. Never open in an Agent-only CLI
        // while the conversation toggle is still in Chat mode.
        profile = "manual".to_string();
        provider = choose_manual_startup_provider(&startup_routes, requested_provider.as_deref());
    }
    let no_usable_route = provider.is_none();
    let provider = provider.unwrap_or_else(|| {
        profile = "manual".to_string();
        "grok".to_string()
    });

    let loaded = if provider == "grok" {
        grok_backend
    } else {
        load_backend(&provider)
    };
    let mut backends = HashMap::new();
    let config_error = if no_usable_route {
        Some(no_usable_route_message())
    } else {
        match loaded {
            Ok(backend) => {
                backends.insert(provider.clone(), backend);
                None
            }
            Err(error) => Some(error.to_string()),
        }
    };
    (provider, profile, backends, config_error)
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(OsStr::new("--gemini-login")) {
        #[cfg(unix)]
        {
            let Some(program) = args.next().map(PathBuf::from) else {
                eprintln!("Gemini login helper requires the Antigravity CLI path");
                std::process::exit(2);
            };
            if let Err(error) = run_gemini_login_helper(&program) {
                eprintln!("\nGemini login failed: {error}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(unix))]
        {
            eprintln!("The Gemini login helper is only used by Unix desktop launches");
            std::process::exit(2);
        }
    }

    let (selected_provider, routing_profile, backends, config_error) = initial_route();

    tauri::Builder::default()
        .setup(|app| {
            // window icon for X11 taskbars/alt-tab; GNOME Wayland instead
            // matches app_id -> grok-chat-desktop.desktop for the dock icon
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(icon) =
                    tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
                {
                    let _ = window.set_icon(icon);
                }
            }
            Ok(())
        })
        .manage(AppState {
            backends: Arc::new(Mutex::new(backends)),
            selected_provider: Arc::new(Mutex::new(selected_provider)),
            session_owner_provider: Arc::new(Mutex::new(None)),
            routing_profile: Arc::new(Mutex::new(routing_profile)),
            config_error: Arc::new(Mutex::new(config_error)),
            active_request: Arc::new(Mutex::new(None)),
            store: SessionStore::at_default_location(),
        })
        .invoke_handler(tauri::generate_handler![
            status,
            send,
            interrupt,
            new_session,
            resume_session,
            set_model,
            restore_model_for_session,
            set_effort,
            list_models,
            list_providers,
            list_routing_profiles,
            routing_decision,
            set_routing_profile,
            list_open_models,
            set_provider,
            login_provider,
            list_sessions,
            load_session,
            save_session,
            delete_session,
            session_store_health,
            restore_sessions_backup,
            app_info,
            smoke_mode,
            smoke_config,
            smoke_report
        ])
        .run(tauri::generate_context!())
        .expect("failed to start Consilium");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shell_quote_handles_provider_paths_with_apostrophes() {
        assert_eq!(
            shell_quote("/tmp/provider's bin"),
            "'/tmp/provider'\\''s bin'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_launch_arguments_cover_common_linux_desktops() {
        let cwd = Path::new("/home/Ada Lovelace");
        for terminal in [
            "gnome-terminal",
            "mate-terminal",
            "konsole",
            "xfce4-terminal",
            "x-terminal-emulator",
            "xterm",
        ] {
            let args = terminal_launch_args(terminal, "Provider Login", cwd, "provider login")
                .unwrap_or_else(|| panic!("missing arguments for {terminal}"));
            assert!(args.iter().any(|arg| arg == "bash"));
            assert!(args.iter().any(|arg| arg == "provider login"));
        }
        let konsole = terminal_launch_args("konsole", "Provider Login", cwd, "login").unwrap();
        assert!(konsole.iter().any(|arg| arg == "/home/Ada Lovelace"));
        assert!(terminal_launch_args("unknown-terminal", "Title", cwd, "login").is_none());
    }

    #[test]
    fn user_home_resolution_uses_native_platform_variables() {
        let windows = resolve_user_home(true, |key| match key {
            "USERPROFILE" => Some(OsString::from("C:/Users/Ada")),
            "HOME" => Some(OsString::from("/wrong/home")),
            _ => None,
        });
        assert_eq!(windows, Some(PathBuf::from("C:/Users/Ada")));

        let home_drive = resolve_user_home(true, |key| match key {
            "HOMEDRIVE" => Some(OsString::from("D:")),
            "HOMEPATH" => Some(OsString::from("\\People\\Ada")),
            _ => None,
        });
        assert_eq!(home_drive, Some(PathBuf::from("D:\\People\\Ada")));

        let linux = resolve_user_home(false, |key| {
            (key == "HOME").then(|| OsString::from("/home/ada"))
        });
        assert_eq!(linux, Some(PathBuf::from("/home/ada")));
    }

    #[test]
    fn fallback_is_allowed_only_before_answer_text_and_with_a_remaining_route() {
        assert!(can_fallback(false, false, false, 0, 2));
        assert!(!can_fallback(true, false, false, 0, 2));
        assert!(!can_fallback(false, false, false, 1, 2));
        assert!(!can_fallback(false, false, true, 0, 2));
        assert!(!can_fallback(false, true, true, 0, 2));
        assert!(can_fallback(false, true, false, 0, 2));
    }

    #[test]
    fn automatic_route_rebuilds_context_when_transcript_owner_changes() {
        let primary = "claude";
        let fallback = "grok";

        // Primary owns the original transcript and may resume its own native
        // session on the next turn.
        assert!(!route_requires_context_rebuild(primary, Some(primary)));

        // Once fallback answers, the old primary native session predates that
        // answer and must be discarded before primary is tried again.
        assert!(route_requires_context_rebuild(primary, Some(fallback)));
        assert!(!route_requires_context_rebuild(fallback, Some(fallback)));

        // Fresh and restored-with-warning transcripts never trust a cached
        // provider-native session.
        assert!(route_requires_context_rebuild(primary, None));
    }

    #[test]
    fn manual_startup_prefers_configured_chat_routes_over_installed_clis() {
        let candidates = [
            StartupRoute {
                id: "grok",
                active: true,
                available: true,
                configured: false,
                supports_chat: true,
                local: false,
            },
            StartupRoute {
                id: "openai-api",
                active: true,
                available: true,
                configured: true,
                supports_chat: true,
                local: false,
            },
            StartupRoute {
                id: "openai-compatible",
                active: true,
                available: true,
                configured: true,
                supports_chat: true,
                local: true,
            },
        ];

        assert_eq!(
            choose_manual_startup_provider(&candidates, None).as_deref(),
            Some("openai-compatible")
        );
        assert_eq!(
            choose_manual_startup_provider(&candidates, Some("openai-api")).as_deref(),
            Some("openai-api")
        );
    }

    #[test]
    fn manual_startup_never_selects_an_agent_only_or_unusable_override() {
        let candidates = [
            StartupRoute {
                id: "codex",
                active: true,
                available: true,
                configured: false,
                supports_chat: false,
                local: false,
            },
            StartupRoute {
                id: "gemini",
                active: true,
                available: true,
                configured: false,
                supports_chat: false,
                local: false,
            },
            StartupRoute {
                id: "claude",
                active: true,
                available: true,
                configured: false,
                supports_chat: true,
                local: false,
            },
        ];

        assert_eq!(
            choose_manual_startup_provider(&candidates, Some("codex")).as_deref(),
            Some("claude")
        );
        assert_eq!(
            choose_manual_startup_provider(&candidates[..2], Some("gemini")),
            None
        );
    }

    #[test]
    fn agent_tools_profile_starts_in_agent_mode() {
        assert!(routing_profile_starts_in_agent_mode("agent-tools"));
        assert!(!routing_profile_starts_in_agent_mode("balanced"));
        assert!(!routing_profile_starts_in_agent_mode("manual"));
    }

    #[test]
    fn empty_startup_message_leads_with_every_supported_setup_path() {
        let message = no_usable_route_message();
        assert!(message.contains("local OpenAI-compatible endpoint"));
        assert!(message.contains("provider CLI"));
        assert!(message.contains("direct API"));
        assert!(!message.contains("required"));
    }

    #[test]
    fn open_checkpoint_ids_are_suggested_only_for_compatible_endpoints() {
        assert!(!open_model_suggestions("openai-compatible").is_empty());
        assert!(open_model_suggestions("mistral-api").is_empty());
        assert!(open_model_suggestions("deepseek-api").is_empty());
        assert!(open_model_suggestions("openai-api").is_empty());
    }

    #[test]
    fn request_validation_accepts_normal_input_and_rejects_invalid_boundaries() {
        let message = Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        };
        assert!(validate_send_input("request_123", std::slice::from_ref(&message), &[]).is_ok());
        assert!(validate_send_input("bad request", std::slice::from_ref(&message), &[]).is_err());
        assert!(validate_send_input("request", &[], &[]).is_err());

        let unsupported = Message {
            role: "tool".to_string(),
            content: "output".to_string(),
        };
        assert!(validate_send_input("request", &[unsupported], &[]).is_err());

        let oversized = Message {
            role: "user".to_string(),
            content: "x".repeat(MAX_USER_MESSAGE_BYTES + 1),
        };
        assert!(validate_send_input("request", &[oversized], &[]).is_err());

        let too_many_images = vec![
            ImageAttachment {
                data: String::new(),
                mime: "image/png".to_string(),
            };
            MAX_ATTACHMENTS + 1
        ];
        assert!(validate_send_input("request", &[message], &too_many_images).is_err());
    }

    #[test]
    fn request_cleanup_cannot_clear_a_newer_request() {
        let active = Arc::new(Mutex::new(Some(ActiveRequest {
            request_id: "newer".to_string(),
            cancel: CancellationToken::new(),
        })));

        clear_active_request(&active, "older");
        assert_eq!(
            active
                .lock()
                .unwrap()
                .as_ref()
                .map(|item| item.request_id.as_str()),
            Some("newer")
        );

        clear_active_request(&active, "newer");
        assert!(active.lock().unwrap().is_none());
    }

    #[test]
    fn restoring_a_chat_clears_every_cached_native_session_first() {
        let grok = Backend::Cli(cli::CliBackend::new(PathBuf::from("grok"), None));
        let claude = Backend::Claude(claude::ClaudeBackend::new(PathBuf::from("claude"), None));
        let codex = Backend::Codex(codex::CodexBackend::new(PathBuf::from("codex"), None));

        grok.set_session(Some("old-grok".to_string()), Some(false), None);
        claude.set_session(Some("old-claude".to_string()), Some(false), None);
        codex.set_session(Some("old-codex".to_string()), Some(true), None);

        let backends = HashMap::from([
            ("grok".to_string(), grok.clone()),
            ("claude".to_string(), claude.clone()),
            ("codex".to_string(), codex.clone()),
        ]);
        restore_selected_native_session(
            &backends,
            &claude,
            Some("restored-claude".to_string()),
            Some(true),
            Some("high".to_string()),
        );

        assert_eq!(grok.session_id(), None);
        assert_eq!(codex.session_id(), None);
        assert_eq!(claude.session_id().as_deref(), Some("restored-claude"));
    }

    #[test]
    fn automatic_status_copy_is_mode_neutral() {
        let explanation = status_route_explanation("balanced");
        assert!(explanation.contains("balanced"));
        assert!(explanation.contains("mode-specific"));
        assert!(!explanation.contains("Chat"));
        assert!(!explanation.contains("Agent"));
    }

    #[cfg(unix)]
    #[test]
    fn gemini_login_helper_uses_a_browser_override_when_available_and_selects_google_oauth() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-antigravity-login-helper-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("fake-agy.sh");
        let capture = dir.join("capture.txt");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nIFS= read -r selection\nprintf '%s|%s' \"$BROWSER\" \"$selection\" > {}\n",
                shell_quote(&capture.display().to_string())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        run_gemini_login_helper(&program).unwrap();
        let expected_browser = preferred_browser_override().unwrap_or_default();
        assert_eq!(
            std::fs::read_to_string(&capture).unwrap(),
            format!("{}|", PathBuf::from(expected_browser).display())
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn grok_default_model_gets_full_cli_effort_ladder() {
        let raw = serde_json::json!({
            "id": GROK_DEFAULT_MODEL,
            "name": "Grok 4.5",
            "description": "cached model",
            "context_window": 500000,
            "supports_reasoning_effort": true,
            "reasoning_efforts": [
                { "id": "low", "label": "Low Effort" },
                { "id": "medium", "label": "Medium Effort" },
                { "id": "high", "label": "High Effort", "default": true }
            ]
        });

        let model = model_from_map(raw.as_object().unwrap()).unwrap();
        let ids = model
            .efforts
            .iter()
            .map(|effort| effort.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["low", "medium", "high", "xhigh", "max"]);
        assert!(model.supports_effort);
        assert_eq!(model.context_window, Some(500_000));
    }
}
