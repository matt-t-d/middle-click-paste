mod ax;
mod engine;
mod ffi;
mod input;
mod launchd;
mod pasteboard;

use std::process::ExitCode;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::{Config, Platform};

const USAGE: &str = "\
middle-click-paste: Linux-style select-to-copy and middle-click-to-paste for macOS.

Selecting text stores it in a private buffer; middle-clicking a text field pastes it.
The normal clipboard (Cmd+C / Cmd+V) keeps its own contents.

USAGE:
    middle-click-paste [run] [OPTIONS]    Run in the foreground (default)
    middle-click-paste install [OPTIONS]  Start at login (LaunchAgent), with these options
    middle-click-paste uninstall          Remove the LaunchAgent

OPTIONS:
    --always             Paste on every middle-click, not just over editable text
                         (breaks middle-click on links and tabs)
    --strict             Paste only where Accessibility reports editable text; by default
                         windows it can't see into (e.g. GPU terminals) get pastes too
    --no-copy-fallback   Never send Cmd+C to read a selection; only use Accessibility
    -v, --verbose        Log captures and pastes to stderr
    -h, --help           Show this help
    -V, --version        Show version

Requires Accessibility permission:
    System Settings > Privacy & Security > Accessibility
";

/// The real OS, as seen by the engine.
struct MacOs;

impl Platform for MacOs {
    type Snapshot = pasteboard::Snapshot;
    fn selected_text(&self) -> ax::Selection {
        ax::selected_text()
    }
    fn change_count(&self) -> i64 {
        pasteboard::change_count()
    }
    fn read_string(&self) -> Option<String> {
        pasteboard::read_string()
    }
    fn snapshot(&self) -> pasteboard::Snapshot {
        pasteboard::snapshot()
    }
    fn restore(&self, snap: &pasteboard::Snapshot) {
        pasteboard::restore(snap)
    }
    fn write_transient_string(&self, text: &str) {
        pasteboard::write_transient_string(text)
    }
    fn post_copy(&self) {
        input::post_copy()
    }
    fn post_paste(&self) {
        input::post_paste()
    }
    fn post_left_click(&self, x: f64, y: f64) {
        input::post_left_click(x, y)
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

enum Command {
    Run(Config),
    Install(Vec<String>),
    Uninstall,
    Help,
    Version,
}

fn parse_args(args: &[String]) -> Result<Command, String> {
    let (sub, rest) = match args.first().map(String::as_str) {
        Some(s @ ("run" | "install" | "uninstall")) => (s, &args[1..]),
        _ => ("run", args),
    };
    let mut cfg = Config { always: false, strict: false, copy_fallback: true, verbose: false };
    for arg in rest {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--always" => cfg.always = true,
            "--strict" => cfg.strict = true,
            "--no-copy-fallback" => cfg.copy_fallback = false,
            "-v" | "--verbose" => cfg.verbose = true,
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    Ok(match sub {
        "install" => Command::Install(rest.to_vec()),
        "uninstall" if rest.is_empty() => Command::Uninstall,
        "uninstall" => return Err("uninstall takes no options".into()),
        _ => Command::Run(cfg),
    })
}

fn run(cfg: Config) -> Result<(), String> {
    if !ax::is_trusted(true) {
        eprintln!(
            "Waiting for Accessibility permission: enable middle-click-paste (or the terminal \
             running it) in System Settings > Privacy & Security > Accessibility."
        );
        while !ax::is_trusted(false) {
            std::thread::sleep(Duration::from_secs(2));
        }
        eprintln!("Accessibility permission granted.");
    }

    let buffer: engine::Buffer = Arc::new(Mutex::new(String::new()));
    let (jobs_tx, jobs_rx) = channel();

    let worker_buffer = buffer.clone();
    std::thread::Builder::new()
        .name("worker".into())
        .spawn(move || engine::run_worker(&MacOs, &cfg, &worker_buffer, jobs_rx))
        .map_err(|e| format!("can't start worker thread: {e}"))?;

    if cfg.verbose {
        eprintln!("running: {cfg:?}");
    }
    input::run_tap(Box::new(move |event| {
        engine::on_event(event, &cfg, &buffer, &jobs_tx, ax::target_at)
    }))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match parse_args(&args) {
        Ok(Command::Help) => {
            print!("{USAGE}");
            Ok(())
        }
        Ok(Command::Version) => {
            println!("middle-click-paste {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Ok(Command::Run(cfg)) => run(cfg),
        Ok(Command::Install(extra)) => launchd::install(&extra).map(|()| {
            println!("Installed: middle-click-paste will start now and at every login.");
            println!("Grant it Accessibility permission if macOS asks.");
        }),
        Ok(Command::Uninstall) => launchd::uninstall().map(|()| println!("Uninstalled.")),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_to_run() {
        match parse(&[]) {
            Ok(Command::Run(cfg)) => assert!(!cfg.always && cfg.copy_fallback && !cfg.verbose),
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn flags_with_and_without_subcommand() {
        for args in [&["--always", "-v"][..], &["run", "--always", "-v"][..]] {
            match parse(args) {
                Ok(Command::Run(cfg)) => assert!(cfg.always && cfg.verbose),
                _ => panic!("expected run"),
            }
        }
        match parse(&["--no-copy-fallback"]) {
            Ok(Command::Run(cfg)) => assert!(!cfg.copy_fallback),
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn install_forwards_validated_flags() {
        match parse(&["install", "--always"]) {
            Ok(Command::Install(extra)) => assert_eq!(extra, ["--always"]),
            _ => panic!("expected install"),
        }
        assert!(parse(&["install", "--bogus"]).is_err());
        assert!(parse(&["uninstall", "--always"]).is_err());
        assert!(matches!(parse(&["uninstall"]), Ok(Command::Uninstall)));
    }

    #[test]
    fn unknown_argument_is_an_error() {
        assert!(parse(&["--nope"]).is_err());
    }
}
