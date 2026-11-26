//! "Start when I sign in to Windows".
//!
//! This is one value under the current user's `Run` key: the path of this
//! executable followed by `--hidden`, which makes the app start in the tray
//! with no window. Nothing is written unless the user ticks the box, and
//! unticking removes it again.

/// The argument that starts the app in the tray, with no window.
pub const HIDDEN: &str = "--hidden";

#[cfg(windows)]
mod registry {
    use std::ffi::c_void;

    const CURRENT_USER: *mut c_void = 0x8000_0001_u32 as i32 as isize as *mut c_void;
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const STRING: u32 = 1; // REG_SZ
    const ONLY_STRINGS: u32 = 0x2; // RRF_RT_REG_SZ

    #[link(name = "advapi32")]
    extern "system" {
        fn RegSetKeyValueW(key: *mut c_void, sub: *const u16, name: *const u16, kind: u32, data: *const c_void, bytes: u32) -> i32;
        fn RegDeleteKeyValueW(key: *mut c_void, sub: *const u16, name: *const u16) -> i32;
        fn RegGetValueW(key: *mut c_void, sub: *const u16, name: *const u16, flags: u32, kind: *mut u32, data: *mut c_void, bytes: *mut u32) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read(name: &str) -> Option<String> {
        let (run, name) = (wide(RUN), wide(name));
        let mut bytes = 0u32;
        unsafe {
            if RegGetValueW(CURRENT_USER, run.as_ptr(), name.as_ptr(), ONLY_STRINGS, std::ptr::null_mut(), std::ptr::null_mut(), &mut bytes) != 0 {
                return None;
            }
            let mut text = vec![0u16; bytes as usize / 2 + 1];
            let mut room = (text.len() * 2) as u32;
            if RegGetValueW(CURRENT_USER, run.as_ptr(), name.as_ptr(), ONLY_STRINGS, std::ptr::null_mut(), text.as_mut_ptr() as *mut c_void, &mut room) != 0 {
                return None;
            }
            let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
            Some(String::from_utf16_lossy(&text[..end]))
        }
    }

    pub fn write(name: &str, value: &str) -> Result<(), i32> {
        let (run, name, value) = (wide(RUN), wide(name), wide(value));
        let code = unsafe {
            RegSetKeyValueW(CURRENT_USER, run.as_ptr(), name.as_ptr(), STRING, value.as_ptr() as *const c_void, (value.len() * 2) as u32)
        };
        if code == 0 { Ok(()) } else { Err(code) }
    }

    pub fn remove(name: &str) -> Result<(), i32> {
        let (run, name) = (wide(RUN), wide(name));
        match unsafe { RegDeleteKeyValueW(CURRENT_USER, run.as_ptr(), name.as_ptr()) } {
            0 | 2 => Ok(()), // 2: it was not there
            code => Err(code),
        }
    }
}

/// What the `Run` value should say for this executable.
fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Could not find this program's path: {e}"))?;
    Ok(format!("\"{}\" {HIDDEN}", exe.display()))
}

/// Whether the app is set to start at sign-in, and to start this very
/// executable. An entry left by a copy that has since moved does not count:
/// ticking the box again then points it at the right place.
pub fn enabled(name: &str) -> bool {
    #[cfg(windows)]
    {
        matches!((registry::read(name), command()), (Some(set), Ok(wanted)) if set.eq_ignore_ascii_case(&wanted))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

pub fn set(name: &str, on: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let done = if on { registry::write(name, &command()?) } else { registry::remove(name) };
        done.map_err(|code| format!("Could not change the start-up setting (Windows error {code})"))
    }
    #[cfg(not(windows))]
    {
        let _ = (name, on);
        Err("Starting at sign-in is only available on Windows".into())
    }
}
