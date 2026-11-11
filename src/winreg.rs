//! The one registry value this tool touches: the user's own `Path`
//! (`HKEY_CURRENT_USER\Environment`), which `install` adds the programs' folder to.
//!
//! The value is read and written as it is: its text unexpanded (`%USERPROFILE%\bin` stays
//! that), and its type (`REG_EXPAND_SZ` or `REG_SZ`) kept.

use std::io;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW;

const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const KEY_QUERY_VALUE: u32 = 0x1;
const KEY_SET_VALUE: u32 = 0x2;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_MORE_DATA: u32 = 234;
const WM_SETTINGCHANGE: u32 = 0x001A;
const SMTO_ABORTIFHUNG: u32 = 0x2;

/// A text value and whether it is of the kind in which `%NAME%` is expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub value: String,
    pub expand: bool,
}

/// An open registry key, closed when dropped.
pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn failed(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// The current user's environment variables, as kept in the registry.
pub fn user_environment(write: bool) -> io::Result<Key> {
    let name = wide("Environment");
    let mut key: HKEY = std::ptr::null_mut();
    let access = if write {
        KEY_QUERY_VALUE | KEY_SET_VALUE
    } else {
        KEY_QUERY_VALUE
    };
    // SAFETY: `name` is NUL-terminated and `key` is a valid out pointer.
    let code = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, access, &mut key) };
    if code != 0 {
        return Err(failed(code));
    }
    Ok(Key(key))
}

/// A text value of a key. None if there is no such value; an error if it is not text.
pub fn read_text(key: &Key, name: &str) -> io::Result<Option<Text>> {
    let name = wide(name);
    for _ in 0..4 {
        let (mut kind, mut size) = (0u32, 0u32);
        // SAFETY: with no buffer, only the type and the size needed are returned.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code != 0 {
            return Err(failed(code));
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            return Err(io::Error::other("the value is not text"));
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut got = (buf.len() * 2) as u32;
        // SAFETY: `buf` is valid for `got` bytes.
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut got,
            )
        };
        if code == ERROR_MORE_DATA {
            continue; // it grew between the two calls: ask again
        }
        if code != 0 {
            return Err(failed(code));
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            return Err(io::Error::other("the value is not text"));
        }
        buf.truncate(got as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        let value = String::from_utf16(&buf)
            .map_err(|_| io::Error::other("the value is not valid text"))?;
        return Ok(Some(Text {
            value,
            expand: kind == REG_EXPAND_SZ,
        }));
    }
    Err(io::Error::other(
        "the value kept changing while it was read",
    ))
}

/// Set a text value of a key.
pub fn write_text(key: &Key, name: &str, text: &Text) -> io::Result<()> {
    if text.value.contains('\0') {
        return Err(io::Error::other("the text has a NUL in it"));
    }
    let name = wide(name);
    let data = wide(&text.value);
    let kind = if text.expand { REG_EXPAND_SZ } else { REG_SZ };
    // SAFETY: `data` is valid for the byte count given, which includes its terminating NUL.
    let code = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            kind,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if code != 0 {
        return Err(failed(code));
    }
    Ok(())
}

/// Tell running programs (Explorer, above all) that the environment has changed, so that
/// terminals opened from now on see it.
pub fn announce_environment_change() {
    let what = wide("Environment");
    let mut result: usize = 0;
    // SAFETY: a broadcast of a string that outlives the call; it gives up on windows that hang.
    unsafe {
        SendMessageTimeoutW(
            0xffff as _, // HWND_BROADCAST
            WM_SETTINGCHANGE,
            0,
            what.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
}

/// A registry hive of our own in a file, for tests: nothing in the real registry is touched.
#[cfg(test)]
pub(crate) fn private_hive(file: &std::path::Path) -> io::Result<Key> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::RegLoadAppKeyW;
    const KEY_ALL_ACCESS: u32 = 0xF003F;
    let path: Vec<u16> = file
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and `key` is a valid out pointer.
    let code = unsafe { RegLoadAppKeyW(path.as_ptr(), &mut key, KEY_ALL_ACCESS, 0, 0) };
    if code != 0 {
        return Err(failed(code));
    }
    Ok(Key(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_values_keep_their_text_and_their_kind() {
        let dir = crate::tests::temp_dir("css-hive");
        {
            let key = private_hive(&dir.join("hive.dat")).unwrap();
            assert_eq!(read_text(&key, "Path").unwrap(), None);
            let path = Text {
                value: String::from(r"%USERPROFILE%\bin;C:\Tools;C:\Dossier d’été\bin;"),
                expand: true,
            };
            write_text(&key, "Path", &path).unwrap();
            assert_eq!(read_text(&key, "Path").unwrap(), Some(path)); // not expanded, nothing lost
            let plain = Text {
                value: String::from(r"C:\One;C:\Two"),
                expand: false,
            };
            write_text(&key, "Path", &plain).unwrap();
            assert_eq!(read_text(&key, "Path").unwrap(), Some(plain));
            let empty = Text {
                value: String::new(),
                expand: true,
            };
            write_text(&key, "Path", &empty).unwrap();
            assert_eq!(read_text(&key, "Path").unwrap(), Some(empty));
            let long = Text {
                value: r"C:\a-rather-long-folder-name\bin;".repeat(400),
                expand: true,
            };
            write_text(&key, "Path", &long).unwrap();
            assert_eq!(read_text(&key, "Path").unwrap(), Some(long));
            // a value that is not text is not read as text (and so never rewritten)
            let name = wide("Number");
            let four = 4u32.to_le_bytes();
            assert_eq!(
                unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, 4, four.as_ptr(), 4) },
                0
            );
            assert!(read_text(&key, "Number").is_err());
            assert!(write_text(
                &key,
                "Path",
                &Text {
                    value: String::from("a\0b"),
                    expand: true
                }
            )
            .is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
