//! Standalone `qmux-cli` binary. Locally the qmux app binary doubles as the
//! CLI, so this exists for hosts that get the CLI without the app — it is what
//! a remote launch target ships and points its hooks and shell wrappers at.

fn main() {
    match qmux_cli::run_cli_if_requested() {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "usage: qmux-cli [ping|notify|pane-write|cwd|agent-exec|agent-detach|claude|codex|grok|mcp|fork|open]"
            );
            std::process::exit(2);
        }
        Err(err) => {
            let (message, exit_code) = qmux_cli::error_report(&err);
            eprintln!("qmux-cli: {message}");
            std::process::exit(exit_code);
        }
    }
}
