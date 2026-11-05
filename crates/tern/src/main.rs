//! tern: a fast, low-memory SSH terminal.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod alloc;
mod chrome;
mod connections;
#[cfg(debug_assertions)]
mod devkeys;
mod empty_state;
mod forward_spec;
mod host_history;
mod hover;
mod icons;
mod indicator;
mod keeper;
mod keeper_gate;
mod keymap;
mod login;
mod password_command;
mod picker;
mod platform;
mod reconnect;
mod session;
mod session_log;
mod settings;
mod settings_widgets;
mod sftp_paths;
mod shell;
mod sidebar;
mod snippets;
mod split;
mod statusline;
mod tabs;
mod tabs_store;
mod text_input;
mod theme;
mod theme_tint;
mod vault_pin;
mod wallpaper;
mod wallpaper_fx;
mod wallpaper_gallery;
mod wallpaper_panel;

use gpui::App;
use tracing_subscriber::EnvFilter;

/// What the command line asks for. Flags are answered before any window opens, and nothing
/// starting with `-` is ever taken as a host to dial.
#[derive(Debug, PartialEq, Eq)]
enum Cli {
    Open(Option<String>),
    Print(String),
    Refuse(String),
    VaultInit,
}

fn parse_cli(arg: Option<String>) -> Cli {
    let usage = "usage: tern [alias | user@host[:port]]\n       tern vault init   (passphrase, then PIN, on standard input)";
    match arg.as_deref() {
        None => Cli::Open(None),
        Some("-V" | "--version") => Cli::Print(format!("tern {}", env!("CARGO_PKG_VERSION"))),
        Some("-h" | "--help") => Cli::Print(usage.to_owned()),
        Some("vault") if std::env::args().nth(2).as_deref() == Some("init") => Cli::VaultInit,
        Some(flag) if flag.starts_with('-') => {
            Cli::Refuse(format!("unknown option {flag}\n{usage}"))
        }
        Some(_) => Cli::Open(arg),
    }
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
fn main() {
    alloc::init();
    let target = match parse_cli(std::env::args().nth(1)) {
        Cli::Open(target) => target,
        Cli::Print(text) => {
            println!("{text}");
            return;
        }
        Cli::Refuse(text) => {
            eprintln!("{text}");
            std::process::exit(2);
        }
        Cli::VaultInit => match vault_pin::init(std::io::stdin().lock()) {
            Ok(text) => {
                println!("{text}");
                return;
            }
            Err(text) => {
                eprintln!("{text}");
                std::process::exit(1);
            }
        },
    };
    let _log_guard = init_logging();
    tern_term::frame_stats::init();
    log_panics();
    let _app = tracing::info_span!("app", service = "tern", env = env()).entered();
    gpui_platform::application()
        .with_assets(icons::Assets)
        .run(move |cx: &mut App| {
            platform::register_fonts(cx);
            chrome::init_menus(cx);
            let keychain = settings::dir()
                .map(|d| settings::Settings::load(&d).vault_keychain)
                .unwrap_or(false);
            keeper::Keeper::install(keychain, cx);
            let keymap = settings::dir()
                .map(|d| keymap::Keymap::load(&d))
                .unwrap_or_default();
            cx.set_global(keymap);
            keymap::apply(cx);
            if let Err(e) = platform::SshRuntime::install(cx) {
                tracing::error!(error = %e, "ssh_runtime_start_failed");
                cx.quit();
                return;
            }
            let window = match shell::open_main_window(cx) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(error = %e, "window_open_failed");
                    cx.quit();
                    return;
                }
            };
            if let Some(target) = target {
                let _ = window.update(cx, |shell, window, cx| {
                    shell.connect_target(&target, window, cx)
                });
            }
            #[cfg(debug_assertions)]
            devkeys::replay(window, cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
            tracing::info!("app_started");
        });
}

/// A panic is also written to `panic.log` in the log directory (`settings::log_dir`) with a backtrace: launched from
/// Finder or the Dock, tern has no terminal, and a Rust panic leaves no macOS crash report.
fn log_panics() {
    let Some(dir) = settings::log_dir() else {
        return;
    };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let entry = format!(
            "--- {} tern {} panicked: {info}\n{backtrace}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            env!("CARGO_PKG_VERSION"),
        );
        if std::fs::create_dir_all(&dir).is_ok() {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
            {
                let _ = f.write_all(entry.as_bytes());
            }
        }
        tracing::error!(panic = %info, "panic");
        previous(info);
    }));
}

/// Structured JSON logs on stderr, filtered by `TERN_LOG` (default `info`).
/// Structured JSON logs on stderr and, so that a tern started from Finder or the Dock still
/// leaves a record, in `tern.log` in `settings::log_dir` (daily files, a week kept). Filtered by
/// `TERN_LOG` (default `info`). The returned guard flushes the file on exit.
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let filter = || EnvFilter::try_from_env("TERN_LOG").unwrap_or_else(|_| "info".into());
    let stderr = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_writer(std::io::stderr);
    let (file, guard) = match settings::log_dir().and_then(|dir| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("tern")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)
            .ok()
    }) {
        Some(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_ansi(false)
                        .with_writer(writer),
                ),
                Some(guard),
            )
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter())
        .with(stderr)
        .with(file)
        .init();
    guard
}

fn env() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "production"
    }
}

#[cfg(test)]
mod cli_tests {
    use super::{Cli, parse_cli};

    #[test]
    fn flags_never_become_hosts() {
        let arg = |s: &str| Some(s.to_owned());
        assert!(matches!(parse_cli(arg("--version")), Cli::Print(v) if v.starts_with("tern ")));
        assert!(matches!(parse_cli(arg("-h")), Cli::Print(_)));
        assert!(matches!(parse_cli(arg("--verbose")), Cli::Refuse(_)));
        assert_eq!(
            parse_cli(arg("deploy@10.0.0.5:2222")),
            Cli::Open(arg("deploy@10.0.0.5:2222"))
        );
        assert_eq!(parse_cli(None), Cli::Open(None));
    }
}
