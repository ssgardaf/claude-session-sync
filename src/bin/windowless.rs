//! `claude-session-syncw.exe`: the same program without a console window, for the daemon.
#![windows_subsystem = "windows"]

fn main() {
    claude_session_sync::cli::process_main()
}
