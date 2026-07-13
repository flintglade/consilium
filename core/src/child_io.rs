use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use color_eyre::eyre::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

const STDERR_TAIL_LINES: usize = 8;
const STDERR_LINE_LIMIT: usize = 4096;
pub(crate) const CLI_STDOUT_LINE_LIMIT: usize = 1024 * 1024;
pub(crate) const CLI_STDOUT_TOTAL_LIMIT: usize = 32 * 1024 * 1024;

/// CLI programs Consilium can discover and launch. Executable names,
/// overrides, and provider-specific locations live here so the provider
/// catalog and runtime adapters cannot disagree about availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliProvider {
    Grok,
    Claude,
    Codex,
    Gemini,
    Copilot,
    Kiro,
}

impl CliProvider {
    pub(crate) const fn binary(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "agy",
            Self::Copilot => "copilot",
            Self::Kiro => "kiro-cli",
        }
    }

    const fn override_variable(self) -> Option<&'static str> {
        match self {
            Self::Grok => Some("GROK_CLI_BIN"),
            Self::Claude => Some("CLAUDE_CLI_BIN"),
            Self::Codex => Some("CODEX_CLI_BIN"),
            Self::Gemini => Some("ANTIGRAVITY_CLI_BIN"),
            Self::Copilot | Self::Kiro => None,
        }
    }

    const fn home_relative_paths(self) -> &'static [&'static str] {
        match self {
            // ~/.local/bin is searched for every provider by the resolver.
            Self::Grok => &[".grok/bin/grok"],
            Self::Claude | Self::Codex | Self::Gemini | Self::Copilot | Self::Kiro => &[],
        }
    }
}

fn resolve_user_home_with<F>(is_windows: bool, env: &mut F) -> Option<PathBuf>
where
    F: FnMut(&'static str) -> Option<OsString>,
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

pub(crate) fn user_home_dir() -> Option<PathBuf> {
    let mut env = |key| std::env::var_os(key);
    resolve_user_home_with(cfg!(windows), &mut env)
}

fn resolve_data_dir_with<F>(is_windows: bool, env: &mut F, temporary: PathBuf) -> PathBuf
where
    F: FnMut(&'static str) -> Option<OsString>,
{
    if let Some(path) = env("GROK_CHAT_DATA_DIR") {
        return PathBuf::from(path);
    }
    if is_windows {
        if let Some(path) = env("LOCALAPPDATA").or_else(|| env("APPDATA")) {
            return PathBuf::from(path).join("Flintglade").join("Consilium");
        }
        if let Some(home) = resolve_user_home_with(true, env) {
            return home
                .join("AppData")
                .join("Local")
                .join("Flintglade")
                .join("Consilium");
        }
    } else {
        if let Some(path) = env("XDG_DATA_HOME") {
            return PathBuf::from(path).join("grok-chat");
        }
        if let Some(home) = resolve_user_home_with(false, env) {
            return home.join(".local").join("share").join("grok-chat");
        }
    }
    temporary.join("grok-chat")
}

pub(crate) fn application_data_dir() -> PathBuf {
    let mut env = |key| std::env::var_os(key);
    resolve_data_dir_with(cfg!(windows), &mut env, std::env::temp_dir())
}

fn resolve_user_config_path_with<F>(is_windows: bool, env: &mut F) -> Option<PathBuf>
where
    F: FnMut(&'static str) -> Option<OsString>,
{
    let directory = if is_windows {
        env("APPDATA")
            .or_else(|| env("LOCALAPPDATA"))
            .map(PathBuf::from)
            .or_else(|| {
                resolve_user_home_with(true, env).map(|home| home.join("AppData").join("Roaming"))
            })?
            .join("Flintglade")
            .join("Consilium")
    } else {
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| resolve_user_home_with(false, env).map(|home| home.join(".config")))?
            .join("consilium")
    };
    Some(directory.join(".env"))
}

pub(crate) fn user_config_env_path() -> Option<PathBuf> {
    let mut env = |key| std::env::var_os(key);
    resolve_user_config_path_with(cfg!(windows), &mut env)
}

fn executable_candidates(path: PathBuf, is_windows: bool) -> Vec<PathBuf> {
    if !is_windows || path.extension().is_some() {
        return vec![path];
    }
    let mut candidates = ["exe", "cmd", "bat"]
        .into_iter()
        .map(|extension| path.with_extension(extension))
        .collect::<Vec<_>>();
    candidates.push(path);
    candidates
}

fn push_unique_directory(directories: &mut Vec<PathBuf>, directory: PathBuf) {
    if !directory.as_os_str().is_empty() && !directories.contains(&directory) {
        directories.push(directory);
    }
}

pub(crate) fn find_program_with<F, G>(
    provider: CliProvider,
    is_windows: bool,
    mut env: F,
    mut is_file: G,
) -> Option<PathBuf>
where
    F: FnMut(&'static str) -> Option<OsString>,
    G: FnMut(&Path) -> bool,
{
    let find_candidate = |candidate: PathBuf, is_file: &mut G| {
        executable_candidates(candidate, is_windows)
            .into_iter()
            .find(|candidate| is_file(candidate))
    };

    let override_path = provider.override_variable().and_then(&mut env);
    if override_path.as_ref().is_some_and(|path| path.is_empty()) {
        return None;
    }
    if let Some(path) = override_path.as_ref().map(PathBuf::from) {
        if path.is_absolute() || path.components().count() > 1 {
            return find_candidate(path, &mut is_file);
        }
    }

    let requested_binary = override_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(provider.binary()));
    let mut directories = Vec::new();

    if let Some(path) = env("PATH") {
        for directory in std::env::split_paths(&path) {
            push_unique_directory(&mut directories, directory);
        }
    }

    if let Some(path) = env("PNPM_HOME") {
        push_unique_directory(&mut directories, PathBuf::from(path));
    }
    if let Some(path) = env("CARGO_HOME") {
        push_unique_directory(&mut directories, PathBuf::from(path).join("bin"));
    }

    if is_windows {
        let app_data = env("APPDATA").map(PathBuf::from);
        let local_app_data = env("LOCALAPPDATA").map(PathBuf::from);
        if let Some(path) = app_data {
            push_unique_directory(&mut directories, path.join("npm"));
            push_unique_directory(&mut directories, path.join("pnpm"));
        }
        if let Some(path) = local_app_data {
            push_unique_directory(&mut directories, path.join("Microsoft").join("WindowsApps"));
            push_unique_directory(&mut directories, path.join("pnpm"));
        }
        if let Some(path) = env("SCOOP") {
            push_unique_directory(&mut directories, PathBuf::from(path).join("shims"));
        }
    }

    let home = resolve_user_home_with(is_windows, &mut env);
    if let Some(home) = &home {
        push_unique_directory(&mut directories, home.join(".local").join("bin"));
        push_unique_directory(&mut directories, home.join(".cargo").join("bin"));
        if is_windows {
            for directory in [
                home.join("scoop").join("shims"),
                home.join("AppData").join("Roaming").join("npm"),
                home.join("AppData").join("Roaming").join("pnpm"),
                home.join("AppData")
                    .join("Local")
                    .join("Microsoft")
                    .join("WindowsApps"),
                home.join("AppData").join("Local").join("pnpm"),
            ] {
                push_unique_directory(&mut directories, directory);
            }
        }
    }

    for directory in directories {
        if let Some(candidate) = find_candidate(directory.join(&requested_binary), &mut is_file) {
            return Some(candidate);
        }
    }

    // An override is authoritative. If it cannot be resolved, do not silently
    // launch the provider's default binary.
    if override_path.is_some() {
        return None;
    }

    if let Some(home) = home {
        for relative in provider.home_relative_paths() {
            if let Some(candidate) = find_candidate(home.join(relative), &mut is_file) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_launchable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().ok().is_some_and(|metadata| {
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        })
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub(crate) fn find_program(provider: CliProvider) -> Option<PathBuf> {
    find_program_with(
        provider,
        cfg!(windows),
        std::env::var_os,
        is_launchable_file,
    )
}

/// A provider process started in its own process group. Cancelling a request
/// must stop helpers launched by the provider as well as the immediate child.
pub(crate) struct ManagedChild {
    child: Child,
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let child = command
            .spawn()
            .context("failed to start provider process")?;
        Ok(Self { child })
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Stop the complete provider process tree and reap the direct child.
    pub(crate) async fn terminate(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.child.id() {
                // The child was placed in a new process group whose id equals
                // its pid. A negative pid addresses the complete group.
                let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                if result != 0 {
                    let _ = self.child.start_kill();
                }
            } else {
                let _ = self.child.start_kill();
            }
        }

        #[cfg(windows)]
        {
            if let Some(pid) = self.child.id() {
                // Windows does not expose process groups through
                // std::process. `taskkill /T` provides the platform's normal
                // tree-aware termination path; start_kill is the fallback if
                // taskkill is unavailable on a stripped-down installation.
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
            let _ = self.child.start_kill();
        }

        let _ = self.child.wait().await;
    }
}

fn finish_stderr_line(bytes: &mut VecDeque<u8>, truncated: &mut bool, tail: &mut VecDeque<String>) {
    while matches!(bytes.back(), Some(b'\r' | b' ' | b'\t')) {
        bytes.pop_back();
    }
    while matches!(bytes.front(), Some(b' ' | b'\t')) {
        bytes.pop_front();
    }
    if bytes.is_empty() {
        *truncated = false;
        return;
    }

    let contiguous = bytes.make_contiguous();
    let text = String::from_utf8_lossy(contiguous);
    let text = if *truncated {
        format!("…{text}")
    } else {
        text.into_owned()
    };
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(text);
    bytes.clear();
    *truncated = false;
}

/// Drain a child's stderr concurrently so a full pipe cannot block the
/// provider. Input is handled as raw bytes: malformed UTF-8 and newline-free
/// output are both fully consumed, while only a small diagnostic tail remains.
pub(crate) fn drain_stderr_tail<R>(reader: R) -> JoinHandle<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut read_buffer = [0_u8; 8192];
        let mut line_tail = VecDeque::with_capacity(STDERR_LINE_LIMIT);
        let mut line_truncated = false;
        let mut tail = VecDeque::with_capacity(STDERR_TAIL_LINES);

        loop {
            match reader.read(&mut read_buffer).await {
                Ok(0) => {
                    finish_stderr_line(&mut line_tail, &mut line_truncated, &mut tail);
                    break;
                }
                Ok(read) => {
                    for &byte in &read_buffer[..read] {
                        if byte == b'\n' {
                            finish_stderr_line(&mut line_tail, &mut line_truncated, &mut tail);
                            continue;
                        }
                        if line_tail.len() == STDERR_LINE_LIMIT {
                            line_tail.pop_front();
                            line_truncated = true;
                        }
                        line_tail.push_back(byte);
                    }
                }
                Err(error) => {
                    finish_stderr_line(&mut line_tail, &mut line_truncated, &mut tail);
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(format!("failed reading provider stderr: {error}"));
                    break;
                }
            }
        }
        tail.into_iter().collect::<Vec<_>>().join(" ")
    })
}

/// Read one line without allowing `read_line`/`read_until` to allocate an
/// arbitrarily large buffer. The returned bytes include the newline, if any.
pub(crate) async fn read_bounded_line<R>(
    reader: &mut R,
    output: &mut Vec<u8>,
    limit: usize,
) -> io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    output.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(output.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(available.len());
        if output.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("provider output line exceeded {limit} bytes"),
            ));
        }
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        if output.last() == Some(&b'\n') {
            return Ok(output.len());
        }
    }
}

/// Provider session ids are persisted and later inserted as positional CLI
/// arguments. Accept only the canonical lowercase, hyphenated UUID shape used
/// by Grok, Claude, and Codex.
pub(crate) fn is_canonical_session_id(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use tokio::io::AsyncWriteExt;

    fn variables(values: &[(&str, &str)]) -> HashMap<String, OsString> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(value)))
            .collect()
    }

    #[test]
    fn windows_program_discovery_supports_exe_and_command_shims() {
        let vars = variables(&[("PATH", "/tools"), ("APPDATA", "/profile/roaming")]);
        let files = HashSet::from([PathBuf::from("/tools/codex.cmd")]);
        assert_eq!(
            find_program_with(
                CliProvider::Codex,
                true,
                |key| vars.get(key).cloned(),
                |path| files.contains(path),
            ),
            Some(PathBuf::from("/tools/codex.cmd"))
        );

        let vars = variables(&[("APPDATA", "/profile/roaming")]);
        let files = HashSet::from([PathBuf::from("/profile/roaming/npm/claude.exe")]);
        assert_eq!(
            find_program_with(
                CliProvider::Claude,
                true,
                |key| vars.get(key).cloned(),
                |path| files.contains(path),
            ),
            Some(PathBuf::from("/profile/roaming/npm/claude.exe"))
        );
    }

    #[test]
    fn windows_program_discovery_covers_user_install_layouts() {
        let vars = variables(&[
            ("LOCALAPPDATA", "/profile/local"),
            ("PNPM_HOME", "/profile/pnpm"),
            ("CARGO_HOME", "/profile/cargo"),
            ("SCOOP", "/profile/scoop-root"),
            ("USERPROFILE", "/profile"),
        ]);
        let files = HashSet::from([
            PathBuf::from("/profile/local/Microsoft/WindowsApps/agy.bat"),
            PathBuf::from("/profile/pnpm/copilot.cmd"),
            PathBuf::from("/profile/cargo/bin/kiro-cli.exe"),
            PathBuf::from("/profile/scoop-root/shims/codex.cmd"),
            PathBuf::from("/profile/.local/bin/claude.exe"),
            PathBuf::from("/profile/.grok/bin/grok.cmd"),
        ]);
        let expected = [
            (
                CliProvider::Gemini,
                "/profile/local/Microsoft/WindowsApps/agy.bat",
            ),
            (CliProvider::Copilot, "/profile/pnpm/copilot.cmd"),
            (CliProvider::Kiro, "/profile/cargo/bin/kiro-cli.exe"),
            (CliProvider::Codex, "/profile/scoop-root/shims/codex.cmd"),
            (CliProvider::Claude, "/profile/.local/bin/claude.exe"),
            (CliProvider::Grok, "/profile/.grok/bin/grok.cmd"),
        ];

        for (provider, expected_path) in expected {
            assert_eq!(
                find_program_with(
                    provider,
                    true,
                    |key| vars.get(key).cloned(),
                    |path| files.contains(path),
                ),
                Some(PathBuf::from(expected_path)),
                "failed to resolve {provider:?}",
            );
        }
    }

    #[test]
    fn explicit_command_shim_override_wins_without_path_search() {
        let vars = variables(&[("GROK_CLI_BIN", "/custom/grok.cmd"), ("PATH", "/tools")]);
        let files = HashSet::from([
            PathBuf::from("/custom/grok.cmd"),
            PathBuf::from("/tools/grok.exe"),
        ]);
        assert_eq!(
            find_program_with(
                CliProvider::Grok,
                true,
                |key| vars.get(key).cloned(),
                |path| files.contains(path),
            ),
            Some(PathBuf::from("/custom/grok.cmd"))
        );
    }

    #[test]
    fn linux_program_discovery_uses_path_and_user_locations() {
        let vars = variables(&[("HOME", "/home/ada"), ("CARGO_HOME", "/opt/cargo")]);
        let files = HashSet::from([
            PathBuf::from("/home/ada/.local/bin/claude"),
            PathBuf::from("/opt/cargo/bin/codex"),
        ]);
        assert_eq!(
            find_program_with(
                CliProvider::Claude,
                false,
                |key| vars.get(key).cloned(),
                |path| files.contains(path),
            ),
            Some(PathBuf::from("/home/ada/.local/bin/claude"))
        );
        assert_eq!(
            find_program_with(
                CliProvider::Codex,
                false,
                |key| vars.get(key).cloned(),
                |path| files.contains(path),
            ),
            Some(PathBuf::from("/opt/cargo/bin/codex"))
        );
    }

    #[test]
    fn installed_app_directories_follow_each_platform() {
        let windows = variables(&[("LOCALAPPDATA", "C:/Users/Ada/AppData/Local")]);
        let mut windows_env = |key| windows.get(key).cloned();
        assert_eq!(
            resolve_data_dir_with(true, &mut windows_env, PathBuf::from("C:/Temp")),
            PathBuf::from("C:/Users/Ada/AppData/Local/Flintglade/Consilium")
        );

        let windows = variables(&[("APPDATA", "C:/Users/Ada/AppData/Roaming")]);
        let mut windows_env = |key| windows.get(key).cloned();
        assert_eq!(
            resolve_user_config_path_with(true, &mut windows_env),
            Some(PathBuf::from(
                "C:/Users/Ada/AppData/Roaming/Flintglade/Consilium/.env"
            ))
        );

        let linux = variables(&[
            ("XDG_DATA_HOME", "/home/ada/.local/share"),
            ("XDG_CONFIG_HOME", "/home/ada/.config"),
        ]);
        let mut linux_data = |key| linux.get(key).cloned();
        assert_eq!(
            resolve_data_dir_with(false, &mut linux_data, PathBuf::from("/tmp")),
            PathBuf::from("/home/ada/.local/share/grok-chat")
        );
        let mut linux_config = |key| linux.get(key).cloned();
        assert_eq!(
            resolve_user_config_path_with(false, &mut linux_config),
            Some(PathBuf::from("/home/ada/.config/consilium/.env"))
        );
    }

    #[tokio::test]
    async fn drains_input_and_keeps_a_bounded_tail() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let drain = drain_stderr_tail(reader);
        for index in 0..12 {
            writer
                .write_all(format!("line-{index}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(writer);

        let tail = drain.await.unwrap();
        assert!(!tail.contains("line-3 "));
        assert!(tail.starts_with("line-4 "));
        assert!(tail.ends_with("line-11"));
    }

    #[tokio::test]
    async fn drains_huge_and_invalid_utf8_lines_without_growing_unbounded() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let drain = drain_stderr_tail(reader);
        let writer_task = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 128 * 1024]).await.unwrap();
            writer.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();
            writer.write_all(b"final-line\n").await.unwrap();
        });
        writer_task.await.unwrap();

        let tail = drain.await.unwrap();
        assert!(tail.contains('…'));
        assert!(tail.contains('\u{fffd}'));
        assert!(tail.ends_with("final-line"));
        assert!(tail.len() <= STDERR_LINE_LIMIT + 64);
    }

    #[tokio::test]
    async fn bounded_line_rejects_a_newline_free_record_before_allocating_it() {
        let input = vec![b'a'; 8192];
        let mut reader = BufReader::new(input.as_slice());
        let mut output = Vec::new();
        let error = read_bounded_line(&mut reader, &mut output, 1024)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(output.len() <= 1024);
    }

    #[test]
    fn session_ids_require_canonical_uuid_text() {
        assert!(is_canonical_session_id(
            "018f59d6-bb05-78a2-a275-c81444e65f42"
        ));
        for invalid in [
            "",
            "thread-1",
            "018F59D6-BB05-78A2-A275-C81444E65F42",
            "018f59d6-bb05-78a2-a275-c81444e65f42 --last",
            "018f59d6bb0578a2a275c81444e65f42",
        ] {
            assert!(!is_canonical_session_id(invalid), "accepted {invalid:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn termination_stops_descendants_in_the_provider_group() {
        let dir =
            std::env::temp_dir().join(format!("consilium-child-tree-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("descendant.pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > '{}'; wait",
                pid_file.display()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = ManagedChild::spawn(&mut command).unwrap();

        let descendant_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake provider did not launch its descendant");

        child.terminate().await;
        let stat_path = format!("/proc/{descendant_pid}/stat");
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match std::fs::read_to_string(&stat_path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break Ok(()),
                    Err(error) => break Err(error),
                    Ok(stat)
                        if stat
                            .split_once(") ")
                            .and_then(|(_, rest)| rest.chars().next())
                            == Some('Z') =>
                    {
                        break Ok(());
                    }
                    Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                }
            }
        })
        .await;
        match stopped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("could not inspect descendant state: {error}"),
            Err(_) => {
                let stat = std::fs::read_to_string(&stat_path)
                    .unwrap_or_else(|error| format!("could not read descendant state: {error}"));
                panic!("descendant did not stop within 2 seconds: {stat}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
