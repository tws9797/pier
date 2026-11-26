//! The memory limit of the WSL2 virtual machine.
//!
//! WSL runs every distro in one VM, and by default lets it grow to half of
//! the PC's memory. The limit is the `memory` line under `[wsl2]` in
//! `%UserProfile%\.wslconfig`, a file the user may have written other things
//! into: Pier changes that one line and leaves the rest as it found it.
//!
//! WSL reads the file when the VM starts, so a change applies once every
//! distro has stopped and one starts again. Pier does not shut WSL down to
//! make that happen: it would stop everything running in every distro.

use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    /// The limit as written in the file ("8GB"), if there is one.
    limit: Option<String>,
    /// The same in whole gigabytes, when it is that.
    limit_gb: Option<u32>,
    /// The PC's memory, in gigabytes, rounded.
    total_gb: u32,
}

fn path() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join(".wslconfig"))
        .ok_or_else(|| "Windows did not say where your user folder is".to_string())
}

/// The file's text, or "" if there is no file. A file that is not text is
/// an error: it must not be rewritten from a guess at what it said.
fn read() -> Result<String, String> {
    let path = path()?;
    match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| format!("{} is not a UTF-8 text file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("Could not read {}: {e}", path.display())),
    }
}

pub fn memory() -> Result<Memory, String> {
    let limit = limit_in(&read()?);
    Ok(Memory { limit_gb: limit.as_deref().and_then(whole_gb), limit, total_gb: total_gb() })
}

/// Sets the limit to `gb` gigabytes, or removes it.
pub fn set_memory(gb: Option<u32>) -> Result<Memory, String> {
    if gb.is_some_and(|gb| gb == 0 || gb > total_gb().max(1)) {
        return Err("That is not an amount of memory this PC has".into());
    }
    let before = read()?;
    let after = with_limit(&before, gb.map(|gb| format!("{gb}GB")).as_deref());
    if after != before {
        let path = path()?;
        std::fs::write(&path, after).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    }
    memory()
}

/// `[wsl2]` -> `wsl2`, for a line that opens a section.
fn section(line: &str) -> Option<String> {
    let line = line.trim();
    line.strip_prefix('[')?.strip_suffix(']').map(|name| name.trim().to_ascii_lowercase())
}

/// The value of a `memory = ...` line, if this is one (and not a comment).
fn memory_value(line: &str) -> Option<&str> {
    let (key, value) = line.split_once('=')?;
    key.trim().eq_ignore_ascii_case("memory").then(|| value.trim())
}

/// The limit a `.wslconfig` sets, as written. With several, WSL takes the last.
fn limit_in(text: &str) -> Option<String> {
    let mut inside = false;
    let mut found = None;
    for line in text.trim_start_matches('\u{feff}').lines() {
        if let Some(name) = section(line) {
            inside = name == "wsl2";
        } else if inside {
            if let Some(value) = memory_value(line).filter(|v| !v.is_empty()) {
                found = Some(value.to_string());
            }
        }
    }
    found
}

/// "8GB" -> 8. Anything else (megabytes, a bare number of bytes) is not one
/// of the amounts Pier offers and is shown as written instead.
fn whole_gb(value: &str) -> Option<u32> {
    let value = value.trim();
    let digits = value.len().checked_sub(2)?;
    if !value.is_char_boundary(digits) || !value[digits..].eq_ignore_ascii_case("GB") {
        return None;
    }
    value[..digits].trim().parse().ok()
}

/// `text` with the limit set to `limit` (or taken out), and nothing else
/// changed: other settings, comments, blank lines and line endings stay.
fn with_limit(text: &str, limit: Option<&str>) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut header = None; // where the first [wsl2] line ended up
    let mut written = false;
    for line in text.lines() {
        if let Some(name) = section(line.trim_start_matches('\u{feff}')) {
            inside = name == "wsl2";
            if inside && header.is_none() {
                header = Some(out.len());
            }
        } else if inside && memory_value(line).is_some() {
            // The first one becomes the new limit; any others go, so there
            // is no second line to disagree with it.
            if let (Some(limit), false) = (limit, written) {
                out.push(format!("memory={limit}"));
                written = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if let (Some(limit), false) = (limit, written) {
        match header {
            Some(at) => out.insert(at + 1, format!("memory={limit}")),
            None => {
                if out.last().is_some_and(|last| !last.trim().is_empty()) {
                    out.push(String::new());
                }
                out.push("[wsl2]".into());
                out.push(format!("memory={limit}"));
            }
        }
    }
    if out.is_empty() {
        return String::new();
    }
    out.join(newline) + newline
}

#[cfg(windows)]
fn total_gb() -> u32 {
    #[repr(C)]
    struct MemoryStatus {
        length: u32,
        load: u32,
        total_phys: u64,
        rest: [u64; 6],
    }
    extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
    }
    let mut status = MemoryStatus { length: std::mem::size_of::<MemoryStatus>() as u32, load: 0, total_phys: 0, rest: [0; 6] };
    // SAFETY: the structure is the size its first field says, and the call only fills it in.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return 0;
    }
    ((status.total_phys as f64) / (1u64 << 30) as f64).round() as u32
}

#[cfg(not(windows))]
fn total_gb() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_empty_file_gets_just_the_section_and_the_line() {
        assert_eq!(with_limit("", Some("8GB")), "[wsl2]\nmemory=8GB\n");
        assert_eq!(with_limit("", None), "");
        assert_eq!(limit_in(""), None);
    }

    #[test]
    fn the_rest_of_the_file_is_left_as_it_was() {
        let before = "# my settings\r\n[wsl2]\r\nprocessors=4\r\nMemory = 4GB   \r\nswap=0\r\n\r\n[experimental]\r\nautoMemoryReclaim=gradual\r\n";
        assert_eq!(limit_in(before).as_deref(), Some("4GB"));
        let after = with_limit(before, Some("8GB"));
        assert_eq!(after, before.replace("Memory = 4GB   ", "memory=8GB"));
        assert_eq!(limit_in(&after).as_deref(), Some("8GB"));
        // Taking it out again removes the line and nothing else.
        assert_eq!(with_limit(&after, None), before.replace("Memory = 4GB   \r\n", ""));
    }

    #[test]
    fn the_line_goes_into_an_existing_section_or_a_new_one_at_the_end() {
        assert_eq!(with_limit("[wsl2]\nswap=0\n", Some("6GB")), "[wsl2]\nmemory=6GB\nswap=0\n");
        assert_eq!(with_limit("[experimental]\nsparseVhd=true", Some("6GB")), "[experimental]\nsparseVhd=true\n\n[wsl2]\nmemory=6GB\n");
    }

    #[test]
    fn only_the_wsl2_section_counts_and_comments_do_not() {
        let text = "[other]\nmemory=1GB\n[wsl2]\n# memory=2GB\nmemory=3GB\nmemory=5GB\n";
        assert_eq!(limit_in(text).as_deref(), Some("5GB"), "WSL takes the last one");
        // One line is left, and the other section's is not touched.
        assert_eq!(with_limit(text, Some("8GB")), "[other]\nmemory=1GB\n[wsl2]\n# memory=2GB\nmemory=8GB\n");
        assert_eq!(with_limit(text, None), "[other]\nmemory=1GB\n[wsl2]\n# memory=2GB\n");
    }

    #[test]
    fn amounts_pier_does_not_offer_are_kept_as_written() {
        assert_eq!(whole_gb("8GB"), Some(8));
        assert_eq!(whole_gb(" 12 gb "), Some(12));
        assert_eq!(whole_gb("512MB"), None);
        assert_eq!(whole_gb("4294967296"), None);
        assert_eq!(whole_gb("GB"), None);
    }
}
