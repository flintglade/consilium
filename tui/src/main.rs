mod app;
mod ui;

use std::ffi::OsString;
use std::io::{stdout, IsTerminal, Stdout};
use std::time::Duration;

use app::{App, TaggedStreamEvent};
use color_eyre::eyre::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::event::{Event, EventStream};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use grok_chat_core::api::STREAM_EVENT_CHANNEL_CAPACITY;
use grok_chat_core::{load_config, Backend};

const HELP: &str = concat!(
    "Consilium ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Flintglade's free, open-source terminal workspace for Grok CLI and the optional xAI API.\n\n",
    "Usage: grok-chat [OPTIONS]\n\n",
    "Options:\n",
    "  -h, --help     Print help\n",
    "  -V, --version  Print version\n\n",
    "Once Consilium is open, type /help for conversation commands and keyboard shortcuts.\n",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAction {
    Run,
    Help,
    Version,
}

fn parse_startup_args(args: impl IntoIterator<Item = OsString>) -> Result<StartupAction, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(StartupAction::Run),
        [arg] if arg == "-h" || arg == "--help" => Ok(StartupAction::Help),
        [arg] if arg == "-V" || arg == "--version" => Ok(StartupAction::Version),
        [arg] => Err(format!("unexpected argument '{}'", arg.to_string_lossy())),
        _ => Err("Consilium accepts one option at a time".to_string()),
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        stdout()
            .execute(EnterAlternateScreen)
            .context("failed to enter alternate screen")?
            .execute(EnableMouseCapture)
            .context("failed to enable mouse capture")?;
        let backend = CrosstermBackend::new(stdout());
        let terminal = Terminal::new(backend).context("failed to create terminal")?;
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = stdout()
            .execute(DisableMouseCapture)
            .and_then(|s| s.execute(LeaveAlternateScreen));
        let _ = self.terminal.show_cursor();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install().context("failed to install color-eyre")?;

    match parse_startup_args(std::env::args_os().skip(1)) {
        Ok(StartupAction::Run) => {}
        Ok(StartupAction::Help) => {
            print!("{HELP}");
            return Ok(());
        }
        Ok(StartupAction::Version) => {
            println!("Consilium {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!("Try 'grok-chat --help' for more information.");
            std::process::exit(2);
        }
    }

    let backend = match load_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            // A desktop-grid launch closes its terminal window the moment
            // we exit — hold it open so the message is actually readable.
            if std::io::stdin().is_terminal() {
                eprintln!(
                    "\nInstall and sign in to the Grok CLI, or set GROK_BACKEND=api with XAI_API_KEY for the optional direct API route."
                );
                eprintln!("Press Enter to close...");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            std::process::exit(1);
        }
    };

    run_tui(backend).await
}

async fn run_tui(backend: Backend) -> Result<()> {
    let mut terminal = TerminalGuard::new()?;
    let mut app = App::new();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let (stream_tx, mut stream_rx) =
        mpsc::channel::<TaggedStreamEvent>(STREAM_EVENT_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        let mut reader = EventStream::new();
        while let Some(Ok(event)) = reader.next().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));
    let mut should_quit = false;

    while !should_quit {
        if app.force_redraw {
            app.force_redraw = false;
            terminal
                .terminal_mut()
                .clear()
                .context("failed to clear terminal")?;
        }
        terminal
            .terminal_mut()
            .draw(|frame| ui::draw(frame, &mut app, &backend))
            .context("failed to draw terminal")?;

        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        should_quit = app.handle_key(key, &backend, &stream_tx)?;
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse.kind);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            Some(stream_event) = stream_rx.recv() => {
                if app.handle_stream_event(stream_event.request_id, stream_event.event) {
                    backend.reset_session();
                }
            }
            _ = tick_interval.tick() => {
                app.tick();
            }
        }
    }

    app.cancel_stream();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn startup_options_do_not_require_provider_configuration() {
        assert_eq!(
            parse_startup_args(args(&["--help"])).unwrap(),
            StartupAction::Help
        );
        assert_eq!(
            parse_startup_args(args(&["-h"])).unwrap(),
            StartupAction::Help
        );
        assert_eq!(
            parse_startup_args(args(&["--version"])).unwrap(),
            StartupAction::Version
        );
        assert_eq!(
            parse_startup_args(args(&["-V"])).unwrap(),
            StartupAction::Version
        );
        assert_eq!(parse_startup_args(args(&[])).unwrap(), StartupAction::Run);
    }

    #[test]
    fn startup_rejects_unknown_or_multiple_arguments_with_a_clear_reason() {
        assert_eq!(
            parse_startup_args(args(&["--verbose"])).unwrap_err(),
            "unexpected argument '--verbose'"
        );
        assert_eq!(
            parse_startup_args(args(&["--help", "--version"])).unwrap_err(),
            "Consilium accepts one option at a time"
        );
    }

    #[test]
    fn help_identifies_the_product_version_and_terminal_commands() {
        assert!(HELP.starts_with("Consilium 0.1.0\n"));
        assert!(HELP.contains("Usage: grok-chat [OPTIONS]"));
        assert!(HELP.contains("-V, --version"));
        assert!(HELP.contains("type /help"));
    }
}
