//! `claude-session-sync` (`claude-session-sync.exe` on Windows): the command line. On macOS
//! and Linux it is the daemon as well.

fn main() {
    claude_session_sync::cli::process_main(false)
}
