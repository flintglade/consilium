//! Local chat-history persistence: one JSON file holding every session
//! started from this app (and only this app). No secrets are stored —
//! just transcripts, titles, timestamps, and grok session ids for resume.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_SESSION_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SESSIONS: usize = 2_000;
const MAX_MESSAGES_PER_SESSION: usize = 1_024;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = 1_024;
const MAX_PROVIDER_SESSION_BYTES: usize = 4 * 1024;
const MAX_RUNTIME_FIELD_BYTES: usize = 512;
const MAX_MESSAGE_CONTENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_MESSAGE_THINKING_BYTES: usize = 16 * 1024 * 1024;
const MAX_ATTACHMENTS_PER_MESSAGE: usize = 12;
const MAX_ATTACHMENT_NAME_BYTES: usize = 512;
const MAX_ATTACHMENT_MIME_BYTES: usize = 128;
const MAX_ATTACHMENT_SIZE_BYTES: u64 = 32 * 1024 * 1024;

/// A persisted transcript message. Distinct from the API `Message` so it can
/// also carry Grok's reasoning ("thinking") for display on reload without
/// ever sending that field to the model. `thinking` is optional and omitted
/// when absent, so older session files keep loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Display-only metadata. File/image bytes and transient text-file bodies
    /// are intentionally absent from the durable session format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<StoredAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAttachment {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub updated_ms: u64,
    pub grok_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<StoredRuntime>,
    pub messages: Vec<StoredMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRuntime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub agent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub updated_ms: u64,
}

pub struct SessionStore {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStoreHealth {
    pub path: String,
    pub backup_path: String,
    pub backup_available: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn resolve_data_dir<F>(is_windows: bool, mut env: F, temporary: PathBuf) -> PathBuf
where
    F: FnMut(&str) -> Option<OsString>,
{
    if let Some(path) = env("GROK_CHAT_DATA_DIR") {
        return PathBuf::from(path);
    }

    if is_windows {
        if let Some(path) = env("LOCALAPPDATA").or_else(|| env("APPDATA")) {
            return PathBuf::from(path).join("Flintglade").join("Consilium");
        }
        if let Some(profile) = env("USERPROFILE") {
            return PathBuf::from(profile)
                .join("AppData")
                .join("Local")
                .join("Flintglade")
                .join("Consilium");
        }
    } else {
        if let Some(path) = env("XDG_DATA_HOME") {
            return PathBuf::from(path).join("grok-chat");
        }
        if let Some(home) = env("HOME") {
            return PathBuf::from(home).join(".local/share/grok-chat");
        }
    }

    temporary.join("grok-chat")
}

impl SessionStore {
    pub fn at_default_location() -> Self {
        let base = resolve_data_dir(
            cfg!(target_os = "windows"),
            |key| std::env::var_os(key),
            std::env::temp_dir(),
        );
        Self {
            path: base.join("sessions.json"),
            lock: Mutex::new(()),
        }
    }

    fn backup_path(&self) -> PathBuf {
        self.path.with_extension("json.bak")
    }

    fn read_limited(path: &Path) -> Result<Option<Vec<u8>>, String> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "cannot inspect session history at {}: {error}",
                    path.display()
                ))
            }
        };
        if metadata.len() > MAX_SESSION_FILE_BYTES as u64 {
            return Err(format!(
                "session history at {} exceeds the {} MiB limit and was not loaded",
                path.display(),
                MAX_SESSION_FILE_BYTES / 1024 / 1024
            ));
        }
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "cannot read session history at {}: {error}",
                    path.display()
                ))
            }
        };
        if raw.len() > MAX_SESSION_FILE_BYTES {
            return Err(format!(
                "session history at {} grew beyond the {} MiB limit while it was being read",
                path.display(),
                MAX_SESSION_FILE_BYTES / 1024 / 1024
            ));
        }
        Ok(Some(raw))
    }

    fn validate_runtime(runtime: &StoredRuntime) -> Result<(), String> {
        for (label, value) in [
            ("provider", runtime.provider.as_deref()),
            ("model", runtime.model.as_deref()),
            ("effort", runtime.effort.as_deref()),
            ("routing profile", runtime.routing_profile.as_deref()),
        ] {
            if value.is_some_and(|value| value.len() > MAX_RUNTIME_FIELD_BYTES) {
                return Err(format!("a stored {label} value is too long"));
            }
        }
        Ok(())
    }

    fn validate_sessions(sessions: &[StoredSession]) -> Result<(), String> {
        if sessions.len() > MAX_SESSIONS {
            return Err(format!(
                "session history contains more than {MAX_SESSIONS} sessions"
            ));
        }

        let mut ids = HashSet::with_capacity(sessions.len());
        for session in sessions {
            if session.id.is_empty()
                || session.id.len() > MAX_SESSION_ID_BYTES
                || session.id.chars().any(char::is_control)
            {
                return Err("a stored session has an invalid identifier".to_string());
            }
            if !ids.insert(session.id.as_str()) {
                return Err(format!(
                    "session history contains duplicate identifier {}",
                    session.id
                ));
            }
            if session.title.len() > MAX_TITLE_BYTES {
                return Err(format!("session {} has an oversized title", session.id));
            }
            if session
                .grok_session
                .as_ref()
                .is_some_and(|value| value.len() > MAX_PROVIDER_SESSION_BYTES)
            {
                return Err(format!(
                    "session {} has an oversized provider session identifier",
                    session.id
                ));
            }
            if let Some(runtime) = &session.runtime {
                Self::validate_runtime(runtime)
                    .map_err(|error| format!("session {}: {error}", session.id))?;
            }
            if session.messages.len() > MAX_MESSAGES_PER_SESSION {
                return Err(format!(
                    "session {} contains more than {MAX_MESSAGES_PER_SESSION} messages",
                    session.id
                ));
            }
            for message in &session.messages {
                if !matches!(message.role.as_str(), "user" | "assistant" | "system") {
                    return Err(format!(
                        "session {} contains an unsupported message role",
                        session.id
                    ));
                }
                if message.content.len() > MAX_MESSAGE_CONTENT_BYTES {
                    return Err(format!(
                        "session {} contains a message larger than {} MiB",
                        session.id,
                        MAX_MESSAGE_CONTENT_BYTES / 1024 / 1024
                    ));
                }
                if message
                    .thinking
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_MESSAGE_THINKING_BYTES)
                {
                    return Err(format!(
                        "session {} contains a reasoning record larger than {} MiB",
                        session.id,
                        MAX_MESSAGE_THINKING_BYTES / 1024 / 1024
                    ));
                }
                if message.attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
                    return Err(format!(
                        "session {} contains more than {MAX_ATTACHMENTS_PER_MESSAGE} attachment records on one message",
                        session.id
                    ));
                }
                for attachment in &message.attachments {
                    if !matches!(attachment.kind.as_str(), "image" | "text") {
                        return Err(format!(
                            "session {} contains an unsupported attachment kind",
                            session.id
                        ));
                    }
                    if attachment.name.is_empty()
                        || attachment.name.len() > MAX_ATTACHMENT_NAME_BYTES
                        || attachment.name.chars().any(char::is_control)
                    {
                        return Err(format!(
                            "session {} contains an invalid attachment name",
                            session.id
                        ));
                    }
                    if attachment.mime.as_ref().is_some_and(|mime| {
                        mime.len() > MAX_ATTACHMENT_MIME_BYTES
                            || mime.chars().any(char::is_control)
                            || (attachment.kind == "image" && !mime.starts_with("image/"))
                    }) {
                        return Err(format!(
                            "session {} contains an invalid attachment media type",
                            session.id
                        ));
                    }
                    if attachment.size_bytes > MAX_ATTACHMENT_SIZE_BYTES {
                        return Err(format!(
                            "session {} contains an implausible attachment size",
                            session.id
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn decode_file(path: &Path, raw: &[u8]) -> Result<Vec<StoredSession>, String> {
        let sessions: Vec<StoredSession> = serde_json::from_slice(raw).map_err(|error| {
            format!(
                "session history at {} is corrupt and was not overwritten: {error}",
                path.display()
            )
        })?;
        Self::validate_sessions(&sessions).map_err(|error| {
            format!(
                "session history at {} is invalid and was not overwritten: {error}",
                path.display()
            )
        })?;
        Ok(sessions)
    }

    fn read_file(path: &Path) -> Result<Vec<StoredSession>, String> {
        let Some(raw) = Self::read_limited(path)? else {
            return Ok(Vec::new());
        };
        Self::decode_file(path, &raw)
    }

    fn read_all(&self) -> Result<Vec<StoredSession>, String> {
        let backup = self.backup_path();
        if !self.path.exists() && backup.is_file() {
            return Err(format!(
                "session history is missing at {}. A last-known-good backup is available at {}.",
                self.path.display(),
                backup.display()
            ));
        }
        Self::read_file(&self.path).map_err(|error| {
            if backup.is_file() {
                format!(
                    "{error}. A last-known-good backup is available at {}.",
                    backup.display()
                )
            } else {
                error
            }
        })
    }

    fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
        let dir = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sessions.json");
        let temporary = dir.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), stamp));

        let result = (|| -> Result<(), String> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .map_err(|error| format!("cannot create temporary session file: {error}"))?;
            file.write_all(contents)
                .map_err(|error| format!("cannot write temporary session file: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("cannot flush temporary session file: {error}"))?;
            drop(file);
            std::fs::rename(&temporary, path).map_err(|error| {
                format!("cannot atomically replace {}: {error}", path.display())
            })?;
            // Unix permits opening and syncing the containing directory so the
            // rename itself is durable. Windows does not expose the same
            // directory-handle operation through std::fs::File; the temporary
            // file has already been flushed before its atomic replacement.
            #[cfg(unix)]
            {
                let directory = std::fs::File::open(dir).map_err(|error| {
                    format!("cannot open {} for syncing: {error}", dir.display())
                })?;
                directory
                    .sync_all()
                    .map_err(|error| format!("cannot sync {}: {error}", dir.display()))?;
            }
            Ok(())
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn write_all(&self, sessions: &[StoredSession]) -> Result<(), String> {
        Self::validate_sessions(sessions)?;
        let json = serde_json::to_vec(sessions).map_err(|error| error.to_string())?;
        if json.len() > MAX_SESSION_FILE_BYTES {
            return Err(format!(
                "session history would exceed the {} MiB limit and was not saved",
                MAX_SESSION_FILE_BYTES / 1024 / 1024
            ));
        }

        if self.path.exists() {
            // Refuse to replace an unreadable/corrupt primary. This prevents a
            // later save from turning a recoverable failure into lost history.
            let current = Self::read_limited(&self.path)?.ok_or_else(|| {
                "the existing session history disappeared before it could be preserved".to_string()
            })?;
            Self::decode_file(&self.path, &current)?;
            Self::atomic_write(&self.backup_path(), &current)?;
        }
        Self::atomic_write(&self.path, &json)
    }

    pub fn list(&self) -> Result<Vec<SessionMeta>, String> {
        let _guard = self.lock.lock().expect("store lock poisoned");
        let mut metas: Vec<SessionMeta> = self
            .read_all()?
            .into_iter()
            .map(|s| SessionMeta {
                id: s.id,
                title: s.title,
                updated_ms: s.updated_ms,
            })
            .collect();
        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_ms));
        Ok(metas)
    }

    pub fn load(&self, id: &str) -> Result<StoredSession, String> {
        let _guard = self.lock.lock().expect("store lock poisoned");
        self.read_all()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("session {id} not found"))
    }

    pub fn save(&self, mut session: StoredSession) -> Result<(), String> {
        let _guard = self.lock.lock().expect("store lock poisoned");
        session.updated_ms = now_ms();
        let mut sessions = self.read_all()?;
        match sessions.iter_mut().find(|s| s.id == session.id) {
            Some(existing) => *existing = session,
            None => sessions.push(session),
        }
        self.write_all(&sessions)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let _guard = self.lock.lock().expect("store lock poisoned");
        let mut sessions = self.read_all()?;
        sessions.retain(|s| s.id != id);
        self.write_all(&sessions)
    }

    pub fn health(&self) -> SessionStoreHealth {
        let backup = self.backup_path();
        SessionStoreHealth {
            path: self.path.display().to_string(),
            backup_path: backup.display().to_string(),
            backup_available: backup.is_file(),
        }
    }

    pub fn restore_backup(&self) -> Result<usize, String> {
        let _guard = self.lock.lock().expect("store lock poisoned");
        let backup = self.backup_path();
        if !backup.is_file() {
            return Err("No last-known-good session backup is available.".to_string());
        }
        let raw = Self::read_limited(&backup)?
            .ok_or_else(|| "The session backup disappeared before it could be read.".to_string())?;
        let sessions = Self::decode_file(&backup, &raw)
            .map_err(|error| format!("session backup could not be restored: {error}"))?;
        Self::atomic_write(&self.path, &raw)?;
        Ok(sessions.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn store_in_temp() -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "grok-chat-store-test-{}-{}-{}",
            std::process::id(),
            now_ms(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SessionStore {
            path: dir.join("sessions.json"),
            lock: Mutex::new(()),
        };
        (store, dir)
    }

    fn sample(id: &str, title: &str) -> StoredSession {
        StoredSession {
            id: id.to_string(),
            title: title.to_string(),
            updated_ms: 0,
            grok_session: Some(format!("grok-{id}")),
            runtime: None,
            messages: vec![StoredMessage {
                role: "user".to_string(),
                content: title.to_string(),
                thinking: None,
                attachments: Vec::new(),
            }],
        }
    }

    fn resolved_for(is_windows: bool, variables: &[(&str, &str)], temporary: &str) -> PathBuf {
        let variables: HashMap<&str, OsString> = variables
            .iter()
            .map(|(key, value)| (*key, OsString::from(value)))
            .collect();
        resolve_data_dir(
            is_windows,
            |key| variables.get(key).cloned(),
            PathBuf::from(temporary),
        )
    }

    #[test]
    fn explicit_data_directory_overrides_every_platform_default() {
        let path = resolved_for(
            true,
            &[
                ("GROK_CHAT_DATA_DIR", "D:/Chats"),
                ("LOCALAPPDATA", "C:/Users/Test/AppData/Local"),
            ],
            "C:/Temp",
        );
        assert_eq!(path, PathBuf::from("D:/Chats"));
    }

    #[test]
    fn windows_history_uses_durable_local_app_data() {
        let path = resolved_for(
            true,
            &[("LOCALAPPDATA", "C:/Users/Test/AppData/Local")],
            "C:/Temp",
        );
        assert_eq!(
            path,
            PathBuf::from("C:/Users/Test/AppData/Local/Flintglade/Consilium")
        );

        let profile_fallback = resolved_for(true, &[("USERPROFILE", "C:/Users/Test")], "C:/Temp");
        assert_eq!(
            profile_fallback,
            PathBuf::from("C:/Users/Test/AppData/Local/Flintglade/Consilium")
        );
    }

    #[test]
    fn linux_history_keeps_existing_xdg_and_home_locations() {
        assert_eq!(
            resolved_for(false, &[("XDG_DATA_HOME", "/data")], "/tmp"),
            PathBuf::from("/data/grok-chat")
        );
        assert_eq!(
            resolved_for(false, &[("HOME", "/home/test")], "/tmp"),
            PathBuf::from("/home/test/.local/share/grok-chat")
        );
    }

    #[test]
    fn save_list_load_delete_roundtrip() {
        let (store, dir) = store_in_temp();

        store.save(sample("a", "first chat")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.save(sample("b", "second chat")).unwrap();

        // newest first
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "b");
        assert_eq!(list[1].id, "a");

        // load returns full transcript and grok session for resume
        let loaded = store.load("a").unwrap();
        assert_eq!(loaded.title, "first chat");
        assert_eq!(loaded.grok_session.as_deref(), Some("grok-a"));
        assert_eq!(loaded.messages.len(), 1);

        // upsert updates in place, not duplicates
        let mut updated = sample("a", "first chat renamed");
        updated.messages.push(StoredMessage {
            role: "assistant".to_string(),
            content: "reply".to_string(),
            thinking: Some("some reasoning".to_string()),
            attachments: Vec::new(),
        });
        store.save(updated).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
        let reloaded = store.load("a").unwrap();
        assert_eq!(reloaded.messages.len(), 2);
        // thinking round-trips through save/load
        assert_eq!(
            reloaded.messages[1].thinking.as_deref(),
            Some("some reasoning")
        );

        store.delete("a").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.load("a").is_err());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn attachment_records_keep_metadata_but_never_unknown_file_payloads() {
        let raw = serde_json::json!({
            "role": "user",
            "content": "summarize this",
            "attachments": [{
                "kind": "text",
                "name": "notes.txt",
                "mime": "text/plain",
                "size_bytes": 42,
                "text": "private file body",
                "data": "cHJpdmF0ZQ=="
            }]
        });
        let message: StoredMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(
            message.attachments,
            vec![StoredAttachment {
                kind: "text".to_string(),
                name: "notes.txt".to_string(),
                mime: Some("text/plain".to_string()),
                size_bytes: 42,
            }]
        );

        let encoded = serde_json::to_string(&message).unwrap();
        assert!(encoded.contains("notes.txt"));
        assert!(!encoded.contains("private file body"));
        assert!(!encoded.contains("cHJpdmF0ZQ"));
    }

    #[test]
    fn save_is_atomic_and_keeps_last_known_good_backup() {
        let (store, dir) = store_in_temp();
        store.save(sample("a", "first chat")).unwrap();
        let first = std::fs::read(&store.path).unwrap();
        store.save(sample("b", "second chat")).unwrap();

        assert_eq!(std::fs::read(store.backup_path()).unwrap(), first);
        assert_eq!(
            SessionStore::read_file(&store.backup_path()).unwrap().len(),
            1
        );
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(!std::fs::read_dir(&dir).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_primary_is_reported_and_never_overwritten() {
        let (store, dir) = store_in_temp();
        store.save(sample("a", "first chat")).unwrap();
        store.save(sample("b", "second chat")).unwrap();
        std::fs::write(&store.path, b"{truncated").unwrap();

        let error = store.list().unwrap_err();
        assert!(error.contains("is corrupt and was not overwritten"));
        assert!(error.contains("last-known-good backup"));
        assert!(store.save(sample("c", "third chat")).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), b"{truncated");

        let restored = store.restore_backup().unwrap();
        assert_eq!(restored, 1);
        assert_eq!(store.list().unwrap().len(), 1);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_primary_with_backup_is_recoverable_not_silently_empty() {
        let (store, dir) = store_in_temp();
        store.save(sample("a", "first chat")).unwrap();
        store.save(sample("b", "second chat")).unwrap();
        std::fs::remove_file(&store.path).unwrap();

        let error = store.list().unwrap_err();
        assert!(error.contains("session history is missing"));
        assert!(error.contains("last-known-good backup"));
        assert_eq!(store.restore_backup().unwrap(), 1);
        assert_eq!(store.list().unwrap().len(), 1);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_primary_is_rejected_without_being_replaced() {
        let (store, dir) = store_in_temp();
        std::fs::create_dir_all(&dir).unwrap();
        let file = std::fs::File::create(&store.path).unwrap();
        file.set_len((MAX_SESSION_FILE_BYTES + 1) as u64).unwrap();

        let error = store.list().unwrap_err();
        assert!(error.contains("exceeds the 64 MiB limit"));
        assert!(store.save(sample("new", "must not replace")).is_err());
        assert_eq!(
            std::fs::metadata(&store.path).unwrap().len(),
            64 * 1024 * 1024 + 1
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_candidate_does_not_change_good_history_or_backup() {
        let (store, dir) = store_in_temp();
        store.save(sample("a", "first chat")).unwrap();
        let original = std::fs::read(&store.path).unwrap();

        let mut too_large = sample("b", "too large");
        too_large.messages[0].content = "x".repeat(MAX_MESSAGE_CONTENT_BYTES + 1);
        let error = store.save(too_large).unwrap_err();
        assert!(error.contains("message larger than 32 MiB"));
        assert_eq!(std::fs::read(&store.path).unwrap(), original);
        assert!(!store.backup_path().exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_session_identifiers_are_rejected() {
        let (store, dir) = store_in_temp();
        let sessions = vec![sample("same", "one"), sample("same", "two")];
        let error = store.write_all(&sessions).unwrap_err();
        assert!(error.contains("duplicate identifier"));
        assert!(!store.path.exists());

        std::fs::remove_dir_all(dir).unwrap_or(());
    }
}
