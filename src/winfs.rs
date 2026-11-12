//! File-system helpers with the exact behaviour the rules depend on: a file is created
//! without ever replacing one, replaced in one step, and keeps its times to the last digit
//! the file system stores.
//!
//! The module has its name from the first, Windows-only versions. What differs by system is
//! here for Windows and in `unixfs.rs` for macOS and Linux, under the same names.

use std::fs::{self, DirEntry};
#[cfg(windows)]
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
#[cfg(windows)]
use windows_sys::Win32::Globalization::LCMapStringEx;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, MoveFileExW};
#[cfg(windows)]
use windows_sys::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    TzSpecificLocalTimeToSystemTime,
};

#[cfg(unix)]
#[path = "unixfs.rs"]
mod unix;
#[cfg(unix)]
use unix::{create, from_metadata};
#[cfg(unix)]
pub use unix::{
    disk_free, local_fields, local_to_epoch, norm, open_shared, rename_noreplace, replace,
    set_times,
};

#[cfg(windows)]
const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000; // 1601 -> 1970, in 100 ns steps
#[cfg(windows)]
const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // read | write | delete
#[cfg(windows)]
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
#[cfg(windows)]
const LCMAP_LOWERCASE: u32 = 0x100;

/// What a file looked like: times in integer nanoseconds since 1970 (exactly as the file
/// system stores them: in 100 ns steps on Windows), and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub mtime_ns: i64,
    pub atime_ns: i64,
    pub size: u64,
    pub is_dir: bool,
}

impl Stat {
    /// The modification time in seconds, as a float.
    pub fn mtime(&self) -> f64 {
        ns_to_secs(self.mtime_ns)
    }
}

pub fn ns_to_secs(ns: i64) -> f64 {
    ns.div_euclid(1_000_000_000) as f64 + 1e-9 * ns.rem_euclid(1_000_000_000) as f64
}

#[cfg(windows)]
fn filetime_to_ns(ft: u64) -> i64 {
    (ft as i64)
        .saturating_sub(FILETIME_UNIX_EPOCH)
        .saturating_mul(100)
}

#[cfg(windows)]
fn from_metadata(md: &fs::Metadata) -> Stat {
    Stat {
        mtime_ns: filetime_to_ns(md.last_write_time()),
        atime_ns: filetime_to_ns(md.last_access_time()),
        size: md.file_size(),
        is_dir: md.is_dir(),
    }
}

/// The state of the file a path leads to (links are followed).
pub fn stat(path: &Path) -> io::Result<Stat> {
    fs::metadata(path).map(|md| from_metadata(&md))
}

/// The state of a directory entry as the listing reported it; a link is followed.
pub fn stat_entry(entry: &DirEntry) -> io::Result<Stat> {
    if entry.file_type()?.is_symlink() {
        return stat(&entry.path());
    }
    entry.metadata().map(|md| from_metadata(&md))
}

/// True for a directory that is not a link of any kind (the app refuses to write through links).
pub fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|md| md.file_type().is_dir())
        .unwrap_or(false)
}

/// Does a directory entry lead to a directory (links followed)?
pub fn entry_is_dir(entry: &DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) if t.is_dir() => true,
        Ok(t) if t.is_symlink() => fs::metadata(entry.path())
            .map(|m| m.is_dir())
            .unwrap_or(false),
        _ => false,
    }
}

/// Open for reading with read, write and delete sharing, so nothing the app does to the
/// file is refused because of us. (Windows still refuses a classic rename onto an open
/// file; the app handles that itself by writing in place, which write sharing permits.)
#[cfg(windows)]
pub fn open_shared(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(path)
}

pub fn read_shared(path: &Path) -> io::Result<Vec<u8>> {
    let mut fh = open_shared(path)?;
    let mut data = Vec::new();
    fh.read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(windows)]
fn ns_to_system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

/// Set a file's access and modification times (nanoseconds since 1970; kept to 100 ns).
#[cfg(windows)]
pub fn set_times(path: &Path, atime_ns: i64, mtime_ns: i64) -> io::Result<()> {
    let fh = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let times = fs::FileTimes::new()
        .set_accessed(ns_to_system_time(atime_ns))
        .set_modified(ns_to_system_time(mtime_ns));
    fh.set_times(times)
}

/// A path for a direct Win32 call: NUL-terminated UTF-16. A path too long for the classic
/// API is given in its `\\?\` form, as the standard library does for its own calls.
#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    fn text(units: &[u16], prefix: &str) -> bool {
        units
            .iter()
            .copied()
            .take(prefix.len())
            .eq(prefix.encode_utf16())
    }
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.len() >= 248 && !text(&units, r"\\?\") {
        if let Ok(abs) = std::path::absolute(path) {
            let abs: Vec<u16> = abs.as_os_str().encode_wide().collect();
            if text(&abs, r"\\?\") || text(&abs, r"\\.\") {
                units = abs;
            } else if text(&abs, r"\\") {
                units = r"\\?\UNC\"
                    .encode_utf16()
                    .chain(abs[2..].iter().copied())
                    .collect();
            } else {
                units = r"\\?\".encode_utf16().chain(abs).collect();
            }
        }
    }
    units.push(0);
    units
}

/// Rename that fails when something is already at `dst` (it never replaces).
#[cfg(windows)]
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (wide(src), wide(dst));
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the call.
    if unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rename that replaces whatever file is at `dst`.
#[cfg(windows)]
pub fn replace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (wide(src), wide(dst));
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the call.
    if unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Move a file: a rename where that works, otherwise (another volume) copy and remove.
pub fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    if rename_noreplace(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst)?;
    fs::remove_file(src)
}

/// What must be at the destination in the last instant before a file is put in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// Whatever is there is replaced.
    Any,
    /// There must be no file there: it is created, never replaced.
    Absent,
    /// The file there must still be the one with this (mtime_ns, size).
    Is(i64, u64),
}

#[derive(Debug)]
pub enum WriteError {
    /// The destination is not what it was a moment ago: leave it and look again later.
    Changed,
    Io(io::Error),
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        WriteError::Io(e)
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Changed => write!(f, "the destination changed"),
            WriteError::Io(e) => write!(f, "{}", e),
        }
    }
}

/// A new, empty file for writing (on Windows: whatever was there is emptied).
#[cfg(windows)]
fn create(path: &Path) -> io::Result<File> {
    File::create(path)
}

fn temp_name(path: &Path) -> PathBuf {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".sync-{}.tmp", base))
}

/// Write through a temp file. `expect` guards the last instant before the file is put in place.
pub fn atomic_write(
    path: &Path,
    data: &[u8],
    times_ns: Option<(i64, i64)>,
    fsync: bool,
    expect: Expect,
) -> Result<(), WriteError> {
    let tmp = temp_name(path);
    let result = (|| -> Result<(), WriteError> {
        {
            let mut fh = create(&tmp)?;
            fh.write_all(data)?;
            if fsync {
                fh.flush()?;
                fh.sync_all()?;
            }
        }
        if let Some((atime, mtime)) = times_ns {
            set_times(&tmp, atime, mtime)?;
        }
        match expect {
            Expect::Absent => {
                return match rename_noreplace(&tmp, path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(WriteError::Changed),
                    Err(e) => Err(WriteError::Io(e)),
                };
            }
            Expect::Is(mtime_ns, size) => match stat(path) {
                Ok(st) if (st.mtime_ns, st.size) == (mtime_ns, size) => {}
                Ok(_) => return Err(WriteError::Changed),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WriteError::Changed),
                Err(e) => return Err(WriteError::Io(e)),
            },
            Expect::Any => {}
        }
        replace(&tmp, path)?;
        Ok(())
    })();
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Bytes free for this user on the volume that holds `path`.
#[cfg(windows)]
pub fn disk_free(path: &Path) -> io::Result<u64> {
    let p = wide(path);
    let mut free: u64 = 0;
    // SAFETY: `p` is a NUL-terminated UTF-16 string; `free` is a valid out pointer.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            p.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}

/// Lower-case the way Windows does for file names in the invariant locale (one code unit
/// in, one out), which is what the manifest keys written by the Python version use.
#[cfg(windows)]
fn lower_invariant(s: &str) -> String {
    let src: Vec<u16> = s.encode_utf16().collect();
    if src.is_empty() {
        return String::new();
    }
    let mut dst = vec![0u16; src.len()];
    let locale = [0u16; 1]; // LOCALE_NAME_INVARIANT
                            // SAFETY: the buffers are valid for the lengths passed.
    let n = unsafe {
        LCMapStringEx(
            locale.as_ptr(),
            LCMAP_LOWERCASE,
            src.as_ptr(),
            src.len() as i32,
            dst.as_mut_ptr(),
            dst.len() as i32,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if n <= 0 {
        return s.to_lowercase();
    }
    String::from_utf16_lossy(&dst[..n as usize])
}

/// The key a path has in the manifest: absolute, backslashes, lower case.
#[cfg(windows)]
pub fn norm(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    lower_invariant(&abs.to_string_lossy().replace('/', "\\"))
}

pub fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The current time in seconds since 1970.
pub fn real_now() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// A calendar time: (year, month, day, hour, minute, second).
pub type Fields = (i32, u32, u32, u32, u32, u32);

#[cfg(windows)]
fn blank_systemtime() -> SYSTEMTIME {
    SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    }
}

/// (year, month, day, hour, minute, second) in local time, for a time in seconds since 1970.
#[cfg(windows)]
pub fn local_fields(epoch: f64) -> Option<Fields> {
    if !epoch.is_finite() {
        return None;
    }
    let ticks = (epoch.floor() as i128) * 10_000_000 + FILETIME_UNIX_EPOCH as i128;
    if !(0..=i64::MAX as i128).contains(&ticks) {
        return None;
    }
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let (mut utc, mut local) = (blank_systemtime(), blank_systemtime());
    // SAFETY: all three pointers are to valid structures owned by this function.
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    if !ok {
        return None;
    }
    Some((
        local.wYear as i32,
        local.wMonth as u32,
        local.wDay as u32,
        local.wHour as u32,
        local.wMinute as u32,
        local.wSecond as u32,
    ))
}

/// Is this a time on the calendar (of the years 1601 to 9999)?
fn on_calendar(f: Fields) -> bool {
    let (year, month, day, hour, minute, second) = f;
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1601..=9999).contains(&year)
        && day >= 1
        && day <= days
        && hour <= 23
        && minute <= 59
        && second <= 61
}

/// Seconds since 1970 for a local calendar time. None if it is not a time on the calendar.
#[cfg(windows)]
pub fn local_to_epoch(f: Fields) -> Option<f64> {
    let (year, month, day, hour, minute, second) = f;
    if !on_calendar(f) {
        return None;
    }
    let mut local = blank_systemtime();
    local.wYear = year as u16;
    local.wMonth = month as u16;
    local.wDay = day as u16;
    local.wHour = hour as u16;
    local.wMinute = minute as u16;
    let mut utc = blank_systemtime();
    let mut ft = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: all pointers are to valid structures owned by this function.
    let ok = unsafe {
        TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) != 0
            && SystemTimeToFileTime(&utc, &mut ft) != 0
    };
    if !ok {
        return None;
    }
    let ticks = ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64;
    Some(((ticks - FILETIME_UNIX_EPOCH) / 10_000_000) as f64 + second as f64)
}

/// Local time for an epoch time, formatted. The pattern knows `%Y %m %d %H %M %S`.
pub fn local_format(epoch: f64, pattern: &str) -> String {
    let Some((year, month, day, hour, minute, second)) = local_fields(epoch) else {
        return String::from("-");
    };
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", year)),
            Some('m') => out.push_str(&format!("{:02}", month)),
            Some('d') => out.push_str(&format!("{:02}", day)),
            Some('H') => out.push_str(&format!("{:02}", hour)),
            Some('M') => out.push_str(&format!("{:02}", minute)),
            Some('S') => out.push_str(&format!("{:02}", second)),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// The fields of `YYYY-MM-DD[ HH:MM[:SS]]`, if the text is exactly that, and whether a time
/// of day and seconds were given.
fn parse_stamp(text: &str) -> Option<(Fields, bool, bool)> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:\s+([0-9]{1,2}):([0-9]{1,2})(?::([0-9]{1,2}))?)?$")
            .expect("valid pattern")
    });
    let m = re.captures(text)?;
    let n = |i: usize| m.get(i).map_or(Some(0), |v| v.as_str().parse::<u32>().ok());
    let fields = (n(1)? as i32, n(2)?, n(3)?, n(4)?, n(5)?, n(6)?);
    Some((fields, m.get(4).is_some(), m.get(6).is_some()))
}

/// Epoch seconds for a local `YYYY-MM-DD HH:MM:SS` stamp (the app log's format).
pub fn parse_local_stamp(text: &str) -> Option<f64> {
    match parse_stamp(text)? {
        (fields, true, true) => local_to_epoch(fields),
        _ => None,
    }
}

/// Epoch seconds for what the user typed after `undo --since`, in local time:
/// `YYYY-MM-DD HH:MM:SS`, `YYYY-MM-DD HH:MM` or `YYYY-MM-DD`.
pub fn parse_when(text: &str) -> Result<f64, String> {
    parse_stamp(text)
        .and_then(|(fields, _, _)| local_to_epoch(fields))
        .ok_or_else(|| String::from("use \"YYYY-MM-DD HH:MM\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_round_trips() {
        for epoch in [0.0, 315_705_600.0, 1_794_404_914.0, 4_354_560_000.0] {
            let f = local_fields(epoch).unwrap();
            assert_eq!(local_to_epoch(f), Some(epoch), "{:?}", f);
        }
        assert_eq!(local_fields(1_794_404_914.9), local_fields(1_794_404_914.0)); // fractions are dropped
        assert_eq!(local_fields(f64::NAN), None);
        let text = local_format(1_794_404_914.0, "%Y-%m-%d %H:%M:%S");
        assert_eq!(parse_local_stamp(&text), Some(1_794_404_914.0));
        assert_eq!(
            local_format(1_794_404_914.0, "%Y%m%d-%H%M%S"),
            text.replace(['-', ':'], "").replace(' ', "-")
        );
    }

    #[test]
    fn what_the_user_may_type() {
        let day = parse_when("2026-11-11").unwrap();
        assert_eq!(parse_when("2026-11-11 00:00"), Ok(day));
        assert_eq!(parse_when("2026-11-11 10:30"), Ok(day + 10.5 * 3600.0));
        assert_eq!(
            parse_when("2026-11-11 10:30:15"),
            Ok(day + 10.5 * 3600.0 + 15.0)
        );
        assert_eq!(parse_when("2026-1-5 9:03"), parse_when("2026-01-05 09:03"));
        for bad in [
            "last tuesday",
            "",
            "2026-13-01",
            "2026-02-30",
            "2026-11-11 24:00",
            "2026-11-11 10:61",
            "2026-11-11 10",
            "26-11-11",
            "2026-11-11x",
        ] {
            assert!(parse_when(bad).is_err(), "{:?}", bad);
        }
        assert_eq!(parse_local_stamp("2026-11-11 10:30"), None); // the log's stamps always have seconds
        assert_eq!(parse_local_stamp("2026-13-11 10:30:00"), None);
    }
}
