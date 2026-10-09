# Consilium

<p align="center">
  <img src="desktop/icons/consilium.svg" width="192" alt="Flintglade Consilium dragon mark">
</p>

Consilium is a free and open-source multi-provider AI harness. Its desktop app
routes among local models, provider CLIs, and direct APIs; a focused terminal
interface supports Grok CLI and the optional xAI API through the shared Rust
streaming backend. It is a
[Flintglade](https://flintglade.com/) project, developed in public at
[github.com/flintglade/consilium](https://github.com/flintglade/consilium).

Every Consilium feature is available in this repository under the
[Apache License 2.0](LICENSE). There are no paid Consilium tiers, feature
gates, license checks, advertisements, or first-party telemetry. Consilium
does not require a proprietary Consilium service: it is fully usable with a
local OpenAI-compatible server such as Ollama, LM Studio, llama.cpp, or vLLM.

Some optional connectors call third-party services. Their vendors may require
a subscription, account, API key, or usage payment. Those charges are between
you and that provider; they do not unlock anything in Consilium. You can use
the complete application without them by running a compatible model locally.

## What it includes

- **Consilium Desktop** (`grok-chat-desktop`) — a true-black Tauri interface
  with streaming Markdown, session history, provider/model/reasoning controls,
  a visible routing explanation, agent mode, file and image attachments, and
  recoverable local persistence.
- **Consilium Terminal** (`grok-chat`) — a focused ratatui interface for Grok
  CLI or the explicitly configured xAI API route, useful over SSH and in
  terminal-first workflows.
- **Shared Rust core** (`grok-chat-core`) — provider adapters, normalized
  streaming events, cancellation, model state, deterministic routing, and
  configuration used by the frontends; the desktop exposes the full provider
  and routing catalog.

## Downloads

The release workflow produces native packages on Windows and Linux. A release
stays in draft until its installers have been exercised on clean machines and
the downloadable files have passed the repository's package inspection step.

| Platform | Release downloads | Notes |
| --- | --- | --- |
| Windows x86_64 | NSIS setup `.exe`, MSI, portable ZIP | Includes the locked Rust dependency notices; the installers can bootstrap WebView2, while the portable build requires WebView2 to be present already |
| Linux x86_64 | AppImage, `.deb`, `.rpm` | Direct downloads built on Ubuntu 22.04; each package includes the Apache-2.0 license and locked Rust dependency notices |
| Linux from source | Local Flatpak recipe | Supports local compatible endpoints and direct APIs; host provider CLIs are outside its sandbox |

The AppImageHub catalog submission was [accepted](https://github.com/AppImage/appimage.github.io/pull/3790) on September 26, 2026; release files remain hosted on GitHub. A Snap needs its own tested
recipe and Store review, with the supported provider routes determined by its
confinement model. The local Flatpak recipe is not a Flathub listing. See the
[distribution plan](docs/DISTRIBUTION.md) for the current status and exact
release gates.

[`THIRD_PARTY_NOTICES.txt`](THIRD_PARTY_NOTICES.txt) records the package,
version, declared license expression, Cargo source, and any authors or
repository supplied in locked metadata for every third-party crate in the
graph. For crates used by the Linux and Windows desktop release binaries, it
also carries exact license,
copyright, attribution, and notice materials. A checksum-pinned fallback set
covers the twelve release crates whose archives omitted those files, and a
reviewed supplement set supplies concrete upstream attribution for nineteen
generic MIT materials. `packaging/generate-third-party-notices.sh --check`
fails on stale metadata, changed material, incomplete reviewed coverage, or an
unresolved package.

The desktop currently supports these active routes:

| Route | Authentication | Notes |
| --- | --- | --- |
| Local / OpenAI-compatible | Optional bearer token | Ollama, LM Studio, llama.cpp, vLLM, or another compatible local/self-hosted endpoint |
| Grok Build CLI | `grok login` | Subscription OAuth stays in the official CLI cache |
| Claude Code CLI | `claude auth login` | Claude subscription credentials stay in the official CLI cache |
| Codex CLI | `codex login --device-auth` | ChatGPT sign-in stays in the official CLI cache |
| Gemini via Antigravity CLI | Google sign-in through `agy` | Credentials remain in Google's keyring |
| xAI API | API key | Optional metered direct route |
| OpenAI API | API key | Optional metered direct route |
| Anthropic API | API key | Optional metered Messages API route |
| Gemini Developer API | API key | Optional metered direct route |
| Mistral API | API key | Optional metered compatible route |
| DeepSeek API | API key | Optional metered compatible route |

The connector catalog can also show planned adapters without presenting them
as usable. Availability is computed from installed CLI binaries and required
configuration; Consilium does not claim that a model is installed merely
because it appears in the open-weight suggestion catalog.

## Quick start: fully local

Install a server that exposes an OpenAI-compatible chat-completions endpoint,
download a model using that server, and copy the example configuration:

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/consilium"
cp .env.example "${XDG_CONFIG_HOME:-$HOME/.config}/consilium/.env"
```

For Ollama, a minimal `.env` configuration is:

```dotenv
OPENAI_COMPAT_BASE_URL=http://127.0.0.1:11434/v1
OPENAI_COMPAT_MODEL=qwen3:8b
```

Use the exact model ID reported by your server. Other common local base URLs
are `http://127.0.0.1:1234/v1` for LM Studio,
`http://127.0.0.1:8080/v1` for llama.cpp, and
`http://127.0.0.1:8000/v1` for vLLM. These are examples, not network probes;
Consilium connects only when you select or route to the configured provider.

Then build and start the desktop app:

```bash
make run-desktop
```

No cloud account or Consilium account is needed.

## Provider configuration

Consilium reads an optional `.env` without requiring a shell profile. Existing
process variables take precedence. It reads the installed-app location for the
current user, or the exact file selected with `CONSILIUM_ENV_FILE`:

| Platform | Per-user `.env` location |
| --- | --- |
| Linux | `${XDG_CONFIG_HOME:-$HOME/.config}/consilium/.env` |
| Windows | `%APPDATA%\Flintglade\Consilium\.env` (falling back to `%LOCALAPPDATA%`) |

To use a trusted source-tree configuration, set `CONSILIUM_ENV_FILE` to its
absolute path. Launch-directory and ancestor `.env` files are never discovered
automatically: they can change executable paths and credential recipients.
Consilium only reads these files: it never
creates, modifies, or copies them, and it never saves provider keys into its
session data. `.env` is gitignored. Never commit credentials; `.env.example`
contains names and non-secret examples only.

At a normal Chat-mode startup, `CONSILIUM_DEFAULT_PROVIDER=<provider-id>` is
honored only when that route is active, available, and Chat-capable. Without a
usable override, Consilium prefers a configured local endpoint, then another
configured direct Chat route, then an installed Chat-capable CLI. It never
auto-selects Agent-only Codex or Gemini while the Agent toggle is off. An
automatic `CONSILIUM_ROUTING_PROFILE` still uses that profile's eligible Chat
decision; if none exists, startup falls back to the conservative manual rule.

### Local and compatible endpoints

| Variable | Required | Description |
| --- | --- | --- |
| `OPENAI_COMPAT_BASE_URL` | Yes | Base ending in `/v1`, or a full `/chat/completions` URL |
| `OPENAI_COMPAT_MODEL` | Yes | Exact model ID exposed by the endpoint |
| `OPENAI_COMPAT_API_KEY` | No | Bearer token when the endpoint requires one |
| `OPENAI_COMPAT_NAME` | No | Friendly label shown for the endpoint |

The model catalog includes endpoint-agnostic suggestions from the Llama, Qwen,
Mistral, DeepSeek, Gemma, Phi, GLM, and Kimi families. Model weights have their
own licenses; Apache-2.0 licensing of Consilium does not relicense a model.
Your server remains the source of truth for exact IDs and capabilities.

### Subscription CLI routes

Install the desired official CLI, use its always-visible **Sign in** action in
the desktop connector list, then select it in the provider control. Binary
discovery is labeled **Installed**, not **Ready**, because Consilium cannot
reliably infer every provider's interactive account state without launching
that provider:

```bash
grok login
claude auth login
codex login --device-auth
agy
```

Optional binary overrides are `GROK_CLI_BIN`, `CLAUDE_CLI_BIN`,
`CODEX_CLI_BIN`, and `ANTIGRAVITY_CLI_BIN`. Optional model overrides are
`GROK_MODEL`, `CLAUDE_MODEL`, `CODEX_MODEL`, and `GEMINI_MODEL`. Use a full
binary path for overrides; Consilium does not expand shell expressions in
these values. On Windows, `.exe`, `.cmd`, and `.bat` provider launchers are
recognized both on `PATH` and in the common per-user CLI installation folders.

Consilium treats each CLI as an authentication boundary. It starts the CLI as
a child process and normalizes its output; it does not read, copy, or persist
the CLI's OAuth tokens. Chat and agent command construction is isolated by
provider so instruction files and permission modes do not leak between
adapters.

The current compatibility smoke matrix was run on Linux with Grok CLI 0.2.99,
Claude Code 2.1.197, Codex CLI 0.144.1, and Antigravity CLI 1.1.1. These vendor
tools update independently and are not bundled or version-locked by
Consilium; an incompatible or missing binary stays unavailable with an
actionable install/login message. Antigravity 1.1.1 accepts its prompt only as a
command-line argument, which local process inspectors may read while it runs.
Use the Google AI API connector for confidential Gemini conversations; the
Antigravity Agent route is disabled until you set `CONSILIUM_ALLOW_VISIBLE_PROMPTS=1` in trusted configuration to accept this limitation. It is unsuitable for confidential text on shared hosts.
Optional vendor CLIs may implement their own
diagnostics or telemetry under their own settings and terms. The fully local
compatible route does not require any of those CLIs.

Antigravity 1.1.1 accepts a non-interactive prompt only as the value of its
`--print` flag. Consilium therefore checks that prompt before launch: up to
96 KiB on Linux, 24 KiB for a native Windows executable, or 6 KiB for a Windows
`.cmd`/`.bat` shim. The smaller shim limit stays below the Windows command
interpreter ceiling. Use the Google AI API connector when a conversation needs
more context than the installed Antigravity launcher can accept.

### Direct API routes

Direct routes are enabled only when all required values are non-empty:

| Route | Required variables | Optional endpoint override |
| --- | --- | --- |
| xAI | `XAI_API_KEY` | `GROK_API_URL` |
| OpenAI | `OPENAI_API_KEY`, `OPENAI_MODEL` | `OPENAI_API_URL` |
| Anthropic | `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL` | `ANTHROPIC_API_URL` |
| Gemini | `GEMINI_API_KEY`, `GEMINI_API_MODEL` | `GEMINI_API_BASE_URL` |
| Mistral | `MISTRAL_API_KEY`, `MISTRAL_MODEL` | `MISTRAL_API_URL` |
| DeepSeek | `DEEPSEEK_API_KEY`, `DEEPSEEK_MODEL` | `DEEPSEEK_API_URL` |

`CONSILIUM_API_TIMEOUT_SECS` controls direct-API connection/read timeout
configuration and defaults to 120 seconds. `GROK_TIMEOUT_SECS` remains a
backward-compatible fallback. Direct API use can incur provider charges.

Direct keys are read from the process environment (or the ignored local
`.env`) into the selected in-process client. They are never written to the
session store or emitted by Consilium logs. Protect `.env` with `chmod 600`,
rotate keys at the provider, update the environment, and restart Consilium to
invalidate a cached client. Error bodies are bounded before display.

The terminal frontend retains `GROK_BACKEND=cli` (default) and the explicit
`GROK_BACKEND=api` opt-in for xAI API use. It never silently changes a CLI
session into metered API usage.

## Explainable routing and fallback

Manual provider selection is available alongside eight deterministic routing
profiles: Balanced, Local first, Maximum privacy, Lowest cost, Lowest latency,
Deep reasoning, Agent and tools, and Long context.

Routing uses provider metadata exposed in the UI: ordinal privacy, cost,
latency, reasoning, and tool scores; documented context-window buckets; and a
profile-specific local bonus. Unavailable or unconfigured routes are excluded.
Agent mode additionally excludes adapters without a real tool runtime. The UI
shows the scored candidate order and the reason for the selected route.
Choosing **Agent and tools** enables Agent mode automatically; turning Agent
off while that profile is active moves routing to Balanced so Chat mode never
silently presents a tool-driven profile.

Fallbacks use that same displayed order. A fallback is attempted only when a
provider fails **before any answer text has been emitted**, and only for a Chat
request. Once Chat output begins, Consilium reports that provider's error
instead of silently appending a second model's answer to a partial response.
Agent-mode runs never switch providers automatically after launch, whether or
not the provider reported progress; retrying an Agent run is always an explicit
user action.

If a provider fails after output begins, Consilium preserves the partial text,
marks the stored answer as interrupted, and shows the provider error plus
retry guidance. It never records a truncated answer as silently complete.

## Local sessions and recovery

Desktop transcripts and runtime selections are stored under:

```text
Linux:  ${GROK_CHAT_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/grok-chat}/sessions.json
Windows: %GROK_CHAT_DATA_DIR% or %LOCALAPPDATA%\Flintglade\Consilium\sessions.json
```

Windows falls back to `%APPDATA%` and then
`%USERPROFILE%\AppData\Local\Flintglade\Consilium` when local app data is not
defined.

Consilium does not store API keys in session history. New attachment records
persist only display metadata (kind, name, media type, and size), never image
bytes or transient text-file bodies. Text-file content is sent only in the
live request that attached it; after reload, the attachment chip remains but
the file must be attached again if a later turn needs its contents. Typed chat
text is still stored, so protect the data directory as you would any private
conversation archive.

Session updates use a same-directory temporary file, file synchronization,
atomic rename, and directory synchronization. Before replacing an existing
valid primary file, Consilium preserves it as `sessions.json.bak`. If the
primary JSON is corrupt, it is reported and not overwritten. The desktop
recovery action can restore the last-known-good backup after showing the paths
and availability status. `GROK_CHAT_DATA_DIR` can relocate the store for
testing or a custom backup policy.

The current file is a JSON array with additive optional runtime fields. Serde
defaults keep sessions written by older Consilium builds readable; changes
that cannot remain additive must ship with an explicit migration and fixtures.

## Architecture

```text
desktop/ui (HTML, CSS, JavaScript)       tui (ratatui)
                 |                           |
                 +------ Tauri commands -----+
                              |
                    grok-chat-core::Backend
                              |
        +---------------------+----------------------+
        |                     |                      |
 official CLI adapters   direct API adapters   compatible endpoint
        |                     |                      |
        +------ normalized StreamEvent channel -----+
                              |
                  transcript / thinking / errors
```

Provider-specific parsers and command builders stay in separate core modules.
All adapters emit the same text, thought, session, completion, and error event
types, which keeps streaming and cancellation behavior consistent. Desktop
session storage lives in the Tauri shell and never forms a required remote
service.

## Build, test, and run

The repository pins the Rust toolchain in `rust-toolchain.toml` and tracks
`Cargo.lock`. On Debian/Ubuntu, install the Rust-independent desktop build
dependencies:

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf
```

CI compiles and tests the full workspace and dependency-free frontend modules
on both Ubuntu 24.04 and GitHub's current Windows runner. The tag workflow
builds Linux packages on Ubuntu 22.04 and Windows packages on the Windows
runner, so each desktop bundle is produced on its target operating system.
Windows contributors should install the current Rust, Microsoft C++ Build
Tools, and WebView2 prerequisites from the
[Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#windows);
the package workflow is the source of truth for the supported installer
formats.

Build every workspace member from the locked dependency graph:

```bash
cargo build --locked --release --workspace
```

Or use the project shortcuts:

```bash
make build        # release build
make test         # Rust workspace and frontend tests
make run          # Grok CLI / optional xAI API terminal app
make run-desktop  # desktop app
make install      # local desktop launchers
```

Release binaries are `target/release/grok-chat` and
`target/release/grok-chat-desktop` on Linux, with `.exe` suffixes on Windows.
See [CONTRIBUTING.md](CONTRIBUTING.md) for the complete clean-build, package
inspection, and release checklist.

## Terminal controls

- `grok-chat --help` lists startup options; `grok-chat --version` prints the
  installed Consilium version without requiring provider configuration.
- **Enter** sends; **Alt+Enter** or **Ctrl+J** inserts a newline.
- `/help` lists conversation commands and keys. `/clear` clears the visible
  transcript while preserving context; `/new` starts with empty context.
- `/model <exact-id>` changes the model and starts a fresh conversation so
  models never inherit one another's context.
- **Ctrl+C** interrupts an active response and exits while idle. **Page Up**,
  **Page Down**, and the mouse wheel scroll the transcript.

## Desktop controls

- **Enter** sends; **Shift+Enter**, **Alt+Enter**, or **Ctrl+J** inserts a
  newline where supported.
- **Stop** cancels the active provider request.
- The provider, model, thinking, routing, and agent controls are fixed for the
  lifetime of an in-flight request so its transcript records the runtime that
  actually produced it.
- Changing the model starts a fresh conversation in both desktop and terminal
  interfaces; context is never silently carried from one model to another.
- Agent mode uses only connectors with a native tool-enabled runtime. Chat
  mode sends conversation text without enabling provider tools.
- `/help`, `/clear`, `/new`, and `/model <exact-id>` are available from the
  composer command menu.
- Images can be pasted, dropped, or selected. Text files are transiently
  inlined into that request; provider image support depends on the selected
  adapter. Durable history stores attachment metadata, not file contents.

## Contributing and support

Public contributions are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md), keep provider behavior covered by parser or
fake-CLI tests, and do not add telemetry, proprietary service requirements, or
paid Consilium feature gates.

If Consilium is useful to you, optional project support is available at
[Patreon](https://www.patreon.com/c/zach457). Sponsorship does not provide a
different build or unlock features.

## License

Consilium is licensed under the [Apache License 2.0](LICENSE). Third-party
provider software, services, and model weights remain under their respective
licenses and terms.
