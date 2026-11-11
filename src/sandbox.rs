//! Is this process inside the Claude desktop app's sandbox?
//!
//! This tool is often run from a terminal inside the Claude desktop app, which is a packaged
//! (MSIX) app. For a process started there, Windows quietly redirects what it writes under
//! `%LOCALAPPDATA%` and `%APPDATA%` to a private copy inside the package, and keeps what it
//! writes to `HKEY_CURRENT_USER` in a private registry that no process outside the app sees.
//! (The package APIs do not tell: `GetCurrentPackageFullName` says "no package" there.) So it
//! is found out by trying: a file made in `%LOCALAPPDATA%` that ends up somewhere else.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// Where a file or folder that exists really is (links and redirection followed).
pub fn final_path(path: &Path) -> io::Result<PathBuf> {
    let fh: File = OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: the handle is open, and `buf` is valid for the length given.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                fh.as_raw_handle() as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                0,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            return Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

/// A path as text for comparing: without the `\\?\` prefix, lower case, no trailing `\`.
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{}", rest),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    };
    text.trim_end_matches('\\').to_lowercase()
}

/// Is what is written to `dir` redirected somewhere else? A temporary file is made there, in
/// a new folder of its own (and both are removed again); if the file really is anywhere but
/// in that place under the folder `dir` really is, the answer is yes. If that cannot be told,
/// the answer is no. `resolve` says where an existing file or folder really is (`final_path`,
/// outside tests).
pub fn redirected(dir: &Path, resolve: &dyn Fn(&Path) -> io::Result<PathBuf>) -> bool {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        ".claude-session-sync-probe-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let folder = dir.join(&name);
    if fs::create_dir(&folder).is_err() {
        return false;
    }
    let probe = folder.join("probe.tmp");
    let made = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    let verdict = made
        && match (resolve(dir), resolve(&probe)) {
            (Ok(base), Ok(file)) => plain(&file) != plain(&base.join(&name).join("probe.tmp")),
            _ => false,
        };
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_dir(&folder);
    verdict
}

/// Is this process inside the app's sandbox, going by `%LOCALAPPDATA%`?
pub fn in_app_sandbox(localappdata: Option<&Path>) -> bool {
    localappdata.is_some_and(|dir| redirected(dir, &final_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn left_behind(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn folder_that_is_what_it_seems_is_not_redirected() {
        let dir = crate::tests::temp_dir("css-sandbox");
        assert!(!redirected(&dir, &final_path));
        assert!(!in_app_sandbox(Some(&dir)));
        assert!(!in_app_sandbox(None));
        assert_eq!(left_behind(&dir), 0); // the probe is removed
                                          // the real location is told for a file as for a folder, in the same spelling
        fs::write(dir.join("x"), b"").unwrap();
        assert_eq!(
            plain(&final_path(&dir.join("x")).unwrap()),
            plain(&final_path(&dir).unwrap().join("x"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_that_ends_up_elsewhere_means_redirected() {
        let dir = crate::tests::temp_dir("css-sandbox");
        let seen = dir.clone();
        // as inside the app: the folder is where it is, a new file in it lands in the package
        let packaged = move |p: &Path| -> io::Result<PathBuf> {
            if p == seen {
                return Ok(PathBuf::from(r"\\?\C:\Users\someone\AppData\Local"));
            }
            let inside = p.strip_prefix(&seen).unwrap();
            Ok(Path::new(
                r"\\?\C:\Users\someone\AppData\Local\Packages\Claude_abc\LocalCache\Local",
            )
            .join(inside))
        };
        assert!(redirected(&dir, &packaged));
        assert_eq!(left_behind(&dir), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spelling_alone_does_not_mean_redirected() {
        let dir = crate::tests::temp_dir("css-sandbox");
        let seen = dir.clone();
        // the same place, written with the prefix and in other case for the file only
        let respelled = move |p: &Path| -> io::Result<PathBuf> {
            if p == seen {
                return Ok(PathBuf::from(r"C:\Users\Someone\AppData\Local\"));
            }
            let inside = p
                .strip_prefix(&seen)
                .unwrap()
                .to_string_lossy()
                .to_uppercase();
            Ok(Path::new(r"\\?\c:\users\SOMEONE\appdata\local").join(inside))
        };
        assert!(!redirected(&dir, &respelled));
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\Dir\")),
            r"\\server\share\dir"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cannot_be_told_is_not_redirected() {
        let dir = crate::tests::temp_dir("css-sandbox");
        let unknown = |_: &Path| -> io::Result<PathBuf> { Err(io::Error::other("no telling")) };
        assert!(!redirected(&dir, &unknown));
        assert_eq!(left_behind(&dir), 0);
        // a folder that is not there (no probe can be made)
        assert!(!redirected(&dir.join("missing"), &final_path));
        assert!(!dir.join("missing").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
