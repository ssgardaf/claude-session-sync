//! What `winfs` needs from the system on macOS and Linux, under the names it has on Windows.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{on_calendar, Fields, Stat};

/// The mode of every file this tool writes, its own and the records it puts in the store:
/// the user's alone. Session titles and paths are in them, and they are nobody else's to
/// read; the app itself writes its records so.
pub const FILE_MODE: u32 = 0o600;
/// The mode of every folder this tool makes.
pub const DIR_MODE: u32 = 0o700;

pub(super) fn from_metadata(md: &fs::Metadata) -> Stat {
    Stat {
        mtime_ns: md
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(md.mtime_nsec()),
        atime_ns: md
            .atime()
            .saturating_mul(1_000_000_000)
            .saturating_add(md.atime_nsec()),
        size: md.size(),
        is_dir: md.is_dir(),
        is_file: md.is_file(),
    }
}

/// Open for reading. Nothing the app does to the file is refused because of that: here an
/// open file can be written, renamed over and deleted.
///
/// Only an ordinary file is opened. A pipe that nobody writes to would hold the open for
/// ever, and a device may have no end to read to; so the open does not wait (`O_NONBLOCK`,
/// which means nothing for an ordinary file), and what was opened is looked at before a
/// byte is read: anything else is an error.
pub fn open_shared(path: &Path) -> io::Result<File> {
    let fh = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !fh.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not an ordinary file: not read",
        ));
    }
    Ok(fh)
}

/// Open one of this tool's own files, or a file it puts in the store, with the options
/// given. A file that is made by that is the user's alone (mode 0600), whatever the
/// process's umask, and one that was there with a wider mode is narrowed.
pub fn open_own(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    let fh = options.mode(FILE_MODE).open(path)?;
    if fh.metadata()?.mode() & 0o7777 != FILE_MODE {
        fh.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(fh)
}

/// Make a folder of this tool's own, and the folders above it that are missing. The folders
/// made are the user's alone (mode 0700); one that is there already is left as it is.
pub fn make_dirs(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)?;
    // (whatever the umask took away or left)
    fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))
}

/// Make a file or folder that was moved into this tool's own folders the user's alone.
pub fn make_private(path: &Path) -> io::Result<()> {
    let found = fs::symlink_metadata(path)?;
    let mode = if found.is_dir() {
        DIR_MODE
    } else if found.is_file() {
        FILE_MODE | (found.mode() & 0o100) // (a program stays a program)
    } else {
        return Ok(()); // a link or something else: not ours to change
    };
    if found.mode() & 0o7777 != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL in it"))
}

fn timespec(ns: i64) -> libc::timespec {
    // SAFETY: timespec is plain data (on some systems with padding besides the two fields).
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    ts.tv_sec = ns.div_euclid(1_000_000_000) as _;
    ts.tv_nsec = ns.rem_euclid(1_000_000_000) as _;
    ts
}

/// Set a file's access and modification times (nanoseconds since 1970, kept to the nanosecond
/// where the file system stores them so).
pub fn set_times(path: &Path, atime_ns: i64, mtime_ns: i64) -> io::Result<()> {
    let p = c_path(path)?;
    let times = [timespec(atime_ns), timespec(mtime_ns)];
    // SAFETY: `p` is a NUL-terminated string and `times` an array of two, as the call wants.
    if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), times.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The system's own rename that refuses to replace. None where there is none, or where the
/// file system does not know it.
#[cfg(target_os = "macos")]
fn rename_exclusive(src: &CString, dst: &CString) -> Option<io::Result<()>> {
    // SAFETY: both are NUL-terminated strings that outlive the call.
    if unsafe { libc::renamex_np(src.as_ptr(), dst.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Some(Ok(()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::ENOSYS) => None,
        _ => Some(Err(e)),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_exclusive(src: &CString, dst: &CString) -> Option<io::Result<()>> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    // SAFETY: both are NUL-terminated strings that outlive the call. (The call is made by
    // number: older C libraries have no function for it.)
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Some(Ok(()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::ENOTSUP) => None,
        _ => Some(Err(e)),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn rename_exclusive(_src: &CString, _dst: &CString) -> Option<io::Result<()>> {
    None
}

/// Rename that fails when something is already at `dst` (it never replaces). Where the
/// system's own call for that is not to be had, the file is given its new name as a second
/// name (which fails when the name is taken) and the old name is removed.
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (c_path(src)?, c_path(dst)?);
    if let Some(done) = rename_exclusive(&s, &d) {
        return done;
    }
    fs::hard_link(src, dst)?;
    fs::remove_file(src)
}

/// Rename that replaces whatever file is at `dst`.
pub fn replace(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}

/// Bytes free for this user on the volume that holds `path`.
pub fn disk_free(path: &Path) -> io::Result<u64> {
    let p = c_path(path)?;
    // SAFETY: statvfs is plain data, and `p` a NUL-terminated string.
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(p.as_ptr(), &mut vfs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64))
}

/// The key a path has in the manifest: absolute, spelled as it is (names differ by case here).
pub fn norm(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// (year, month, day, hour, minute, second) in local time, for a time in seconds since 1970.
pub fn local_fields(epoch: f64) -> Option<Fields> {
    // the years 1601 to 9999, as on Windows
    if !epoch.is_finite() || !(-11_644_473_600.0..253_402_300_800.0).contains(&epoch) {
        return None;
    }
    let t = epoch.floor() as libc::time_t;
    // SAFETY: tm is plain data; both pointers are to values owned by this function.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    ))
}

/// Seconds since 1970 for a local calendar time. None if it is not a time on the calendar.
pub fn local_to_epoch(f: Fields) -> Option<f64> {
    let (year, month, day, hour, minute, second) = f;
    if !on_calendar(f) {
        return None;
    }
    // SAFETY: tm is plain data, owned by this function.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = month as i32 - 1;
    tm.tm_mday = day as i32;
    tm.tm_hour = hour as i32;
    tm.tm_min = minute as i32;
    tm.tm_isdst = -1; // summer time or not: as the calendar has it
    tm.tm_wday = -1; // (set by a call that worked: -1 is also a time it may return)
    let t = unsafe { libc::mktime(&mut tm) };
    if tm.tm_wday == -1 {
        return None;
    }
    Some(t as f64 + second as f64)
}

#[cfg(test)]
mod tests {
    use super::super::{atomic_write, stat, Expect};
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "css-unixfs-{}-{}-{}",
            name,
            std::process::id(),
            (super::super::real_now() * 1e6) as u64
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn fifo(path: &Path) {
        let c = c_path(path).unwrap();
        // SAFETY: a NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    }

    #[test]
    fn what_is_written_is_the_users_alone_whatever_the_umask() {
        use super::super::{append_file, create_file, create_new_file, write_file};
        let dir = scratch("mode");
        let p = dir.join("local_x.json");
        // (the umask is the process's, and other tests write files meanwhile: it is not
        // changed here. The mode asked for at creation is 0600, which no umask widens, and
        // it is set again on the open file, which no umask narrows. The programs are run
        // under several umasks in tests/processes.rs.)
        atomic_write(&p, b"{}", None, false, Expect::Absent).unwrap();
        assert_eq!(mode(&p), 0o600);
        // also when a file made with another mode is in the way of the temp file
        let tmp = dir.join(".sync-local_x.json.tmp");
        fs::write(&tmp, b"left over").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&p, b"{\"a\":1}", None, false, Expect::Any).unwrap();
        assert_eq!(mode(&p), 0o600);
        assert!(!tmp.exists());
        // a record the app left readable by all is the user's alone once it is replaced
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let st = stat(&p).unwrap();
        atomic_write(
            &p,
            b"{\"a\":2}",
            None,
            false,
            Expect::Is(st.mtime_ns, st.size),
        )
        .unwrap();
        assert_eq!(mode(&p), 0o600);
        // every way a file of this tool's own is made
        let made = dir.join("made");
        drop(create_file(&made).unwrap());
        let new = dir.join("new");
        drop(create_new_file(&new).unwrap());
        assert_eq!(
            create_new_file(&new).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let log = dir.join("log");
        drop(append_file(&log).unwrap());
        let small = dir.join("small");
        write_file(&small, b"x").unwrap();
        for file in [&made, &new, &log, &small] {
            assert_eq!(mode(file), 0o600, "{}", file.display());
        }
        // one that an earlier version left readable by all is narrowed when it is next written
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        drop(append_file(&log).unwrap());
        assert_eq!(mode(&log), 0o600);
        // folders: the ones made are the user's alone, one that was there is left as it is
        let deep = dir.join("a").join("b").join("c");
        make_dirs(&deep).unwrap();
        for folder in [&deep, &dir.join("a").join("b"), &dir.join("a")] {
            assert_eq!(mode(folder), 0o700, "{}", folder.display());
        }
        fs::set_permissions(&deep, fs::Permissions::from_mode(0o755)).unwrap();
        make_dirs(&deep).unwrap();
        assert_eq!(mode(&deep), 0o755);
        // what was moved in from elsewhere: a file 0600, a program still a program, a link left
        let moved = dir.join("moved");
        fs::write(&moved, b"x").unwrap();
        fs::set_permissions(&moved, fs::Permissions::from_mode(0o664)).unwrap();
        make_private(&moved).unwrap();
        assert_eq!(mode(&moved), 0o600);
        fs::set_permissions(&moved, fs::Permissions::from_mode(0o755)).unwrap();
        make_private(&moved).unwrap();
        assert_eq!(mode(&moved), 0o700);
        make_private(&deep).unwrap();
        assert_eq!(mode(&deep), 0o700);
        let link = dir.join("link");
        std::os::unix::fs::symlink(&small, &link).unwrap();
        fs::set_permissions(&small, fs::Permissions::from_mode(0o644)).unwrap();
        make_private(&link).unwrap();
        assert_eq!(mode(&small), 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A pipe nobody writes to would hold an ordinary open for ever, and a device may never
    /// end: neither is opened for reading, nor is a folder, nor a link to any of them.
    #[test]
    fn only_ordinary_files_are_read_and_none_without_end() {
        use super::super::{read_limited, read_shared};
        let dir = scratch("special");
        let file = dir.join("file");
        fs::write(&file, b"0123456789").unwrap();
        assert_eq!(read_shared(&file).unwrap(), b"0123456789");
        let link = dir.join("link-to-file");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(read_shared(&link).unwrap(), b"0123456789"); // a link to a file is the file
        let pipe = dir.join("pipe");
        fifo(&pipe);
        let link_to_pipe = dir.join("link-to-pipe");
        std::os::unix::fs::symlink(&pipe, &link_to_pipe).unwrap();
        let link_to_device = dir.join("link-to-device");
        std::os::unix::fs::symlink("/dev/zero", &link_to_device).unwrap();
        let began = std::time::Instant::now();
        for special in [
            pipe.as_path(),
            link_to_pipe.as_path(),
            link_to_device.as_path(),
            Path::new("/dev/zero"),
            Path::new("/dev/null"),
            dir.as_path(),
        ] {
            let refused = read_shared(special).unwrap_err();
            assert_eq!(
                refused.kind(),
                io::ErrorKind::InvalidInput,
                "{}",
                special.display()
            );
            assert!(!stat(special).unwrap().is_file);
        }
        assert!(began.elapsed().as_secs() < 5); // (nothing was waited for)
        assert!(stat(&file).unwrap().is_file && stat(&link).unwrap().is_file);
        // a file larger than is to be read is not read to its end
        assert_eq!(read_limited(&file, 10).unwrap().len(), 10);
        assert!(read_limited(&file, 9).is_err());
        assert!(read_limited(&file, 0).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn times_are_kept_to_the_nanosecond() {
        let dir = scratch("times");
        let p = dir.join("f");
        fs::write(&p, b"x").unwrap();
        set_times(&p, 1_700_000_000_123_456_789, 1_700_000_001_987_654_321).unwrap();
        let st = stat(&p).unwrap();
        // (a file system that stores less is not one the app's store is on; tmpfs, ext4, APFS do)
        assert_eq!(st.mtime_ns, 1_700_000_001_987_654_321);
        assert_eq!(st.atime_ns, 1_700_000_000_123_456_789);
        set_times(&p, -1_500_000_000, -1).unwrap(); // before 1970: the fraction counts upwards
        assert_eq!(stat(&p).unwrap().mtime_ns, -1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_fallback_for_a_rename_that_never_replaces_does_not_replace_either() {
        let dir = scratch("link");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, b"new").unwrap();
        fs::write(&b, b"there").unwrap();
        let refused = fs::hard_link(&a, &b).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"there");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_keep_their_case_in_a_key() {
        let key = norm(Path::new("/Some/Where/local_MiXed-Case.json"));
        assert_eq!(key, "/Some/Where/local_MiXed-Case.json");
        assert!(norm(Path::new("relative/File.json")).ends_with("/relative/File.json"));
        assert!(norm(Path::new("relative/File.json")).starts_with('/'));
    }

    #[test]
    fn free_space_is_told() {
        assert!(disk_free(&std::env::temp_dir()).unwrap() > 0);
        assert!(disk_free(Path::new("/no/such/folder/anywhere")).is_err());
    }
}
