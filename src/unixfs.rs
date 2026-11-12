//! What `winfs` needs from the system on macOS and Linux, under the names it has on Windows.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{on_calendar, Fields, Stat};

/// The mode of every file this tool writes: what the app gives its own records.
const FILE_MODE: u32 = 0o644;

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
    }
}

/// Open for reading. Nothing the app does to the file is refused because of that: here an
/// open file can be written, renamed over and deleted.
pub fn open_shared(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// A new, empty file for writing, readable by all and writable by its owner whatever the
/// process's umask is (whatever was there is emptied).
pub(super) fn create(path: &Path) -> io::Result<File> {
    let fh = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)?;
    fh.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
    Ok(fh)
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

    #[test]
    fn what_is_written_is_readable_by_all_whatever_the_umask() {
        let dir = scratch("mode");
        let p = dir.join("local_x.json");
        // (the umask is the process's, and other tests write files meanwhile: it is not
        // changed here; the file's mode is set after it is made, which is what is checked)
        atomic_write(&p, b"{}", None, false, Expect::Absent).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o644);
        // also when a file made with another mode is in the way of the temp file
        let tmp = dir.join(".sync-local_x.json.tmp");
        fs::write(&tmp, b"left over").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write(&p, b"{\"a\":1}", None, false, Expect::Any).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o7777, 0o644);
        assert!(!tmp.exists());
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
