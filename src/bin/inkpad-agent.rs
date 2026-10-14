//! Headless Inkpad access for AI assistants and local command-line automation.
//! Each MCP client launches its own stdio process; no desktop window or port.
//! Keep the Windows console subsystem so MCP stdin/stdout redirection works.
//! MCP clients should spawn this binary with CREATE_NO_WINDOW where needed.

#[path = "../store.rs"]
mod store;
#[path = "../settings.rs"]
mod settings;
#[path = "../media.rs"]
mod media;
#[path = "../canvas.rs"]
mod canvas;
#[path = "../agent.rs"]
mod agent;

fn main() {
    if let Err(error) = agent::run() {
        eprintln!("inkpad-agent: {error}");
        std::process::exit(1);
    }
}
