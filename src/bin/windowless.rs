//! `claude-session-syncw.exe`: the same program without a console window, for the daemon on
//! Windows. (On macOS and Linux a daemon has no window to hide: there this is the same program
//! as the other one, and it is neither installed nor released.)
#![windows_subsystem = "windows"]

fn main() {
    claude_session_sync::cli::process_main(true)
}
