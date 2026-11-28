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

/// How the file's text is stored. WSL opens it as UTF-8 and, through the C
/// runtime, also reads UTF-16 that starts with a byte order mark, which is
/// what Windows PowerShell's `>` writes. It is saved the way it was found.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Encoding {
    Utf8,
    Utf16,
}

const UTF16_MARK: [u8; 2] = [0xFF, 0xFE];

/// The text in a file's bytes. Bytes that are not text are an error: the
/// file must not be rewritten from a guess at what it said.
fn decode(bytes: &[u8]) -> Option<(String, Encoding)> {
    match bytes.strip_prefix(&UTF16_MARK) {
        Some(rest) if rest.len() % 2 == 0 => {
            let units: Vec<u16> = rest.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            String::from_utf16(&units).ok().map(|text| (text, Encoding::Utf16))
        }
        Some(_) => None,
        None => String::from_utf8(bytes.to_vec()).ok().map(|text| (text, Encoding::Utf8)),
    }
}

fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf16 => UTF16_MARK.into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
    }
}

/// The file's text and how it is stored, or "" if there is no file.
fn read() -> Result<(String, Encoding), String> {
    let path = path()?;
    match std::fs::read(&path) {
        Ok(bytes) => decode(&bytes).ok_or_else(|| format!("{} is not a text file Pier can read (UTF-8, or UTF-16 as Windows writes it)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((String::new(), Encoding::Utf8)),
        Err(e) => Err(format!("Could not read {}: {e}", path.display())),
    }
}

pub fn memory() -> Result<Memory, String> {
    let limit = limit_in(&read()?.0);
    Ok(Memory { limit_gb: limit.as_deref().and_then(whole_gb), limit, total_gb: total_gb() })
}

/// Sets the limit to `gb` gigabytes, or removes it.
pub fn set_memory(gb: Option<u32>) -> Result<Memory, String> {
    if gb.is_some_and(|gb| gb == 0 || gb > total_gb().max(1)) {
        return Err("That is not an amount of memory this PC has".into());
    }
    let (before, encoding) = read()?;
    let path = path()?;
    // WSL takes a carriage return on its own for a line break. Pier does
    // not, and would put the limit where WSL does not look for it.
    if before.replace("\r\n", "\n").contains('\r') {
        return Err(format!("{} has line breaks of a kind Pier does not edit. Change the memory line under [wsl2] there by hand.", path.display()));
    }
    let after = with_limit(&before, gb.map(|gb| format!("{gb}GB")).as_deref());
    if after != before {
        // Written beside the file and moved over it, so that a failure half
        // way leaves the old file and not a cut-off one.
        let fresh = path.with_extension("wslconfig.pier-new");
        std::fs::write(&fresh, encode(&after, encoding))
            .and_then(|_| std::fs::rename(&fresh, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&fresh);
                format!("Could not write {}: {e}", path.display())
            })?;
    }
    memory()
}

// How WSL reads the file (src/shared/configfile/configfile.cpp and
// ParseMemorySize in the WSL repository), which this has to agree with or
// Pier shows a limit that WSL does not apply, or writes one where WSL does
// not look:
// - section and key names are matched without regard to case;
// - `#` starts a comment, also after a value or a section header;
// - a value may be in double quotes, which are not part of it;
// - a value whose line ends in a backslash goes on on the next line, so
//   that line is part of the value whatever it looks like;
// - the FIRST `memory` under any `[wsl2]` counts, later ones are ignored;
// - units are B, KB/K, MB/M, GB/G, TB/T in capitals, straight after the
//   digits; a bare number is bytes; anything else is ignored with a warning.

const MARK: char = '\u{feff}';

/// `[wsl2]` -> `wsl2`, for a line that opens a section.
fn section(line: &str) -> Option<String> {
    let line = line.split('#').next()?.trim();
    line.strip_prefix('[')?.strip_suffix(']').map(|name| name.trim().to_ascii_lowercase())
}

/// What follows the `=`, if WSL reads this line as `key = value`: a key is
/// a letter and then letters and digits.
fn value_part(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let key = key.trim_matches([' ', '\t']);
    let mut letters = key.chars();
    (letters.next()?.is_ascii_alphabetic() && letters.all(|c| c.is_ascii_alphanumeric())).then_some((key, value))
}

/// Whether a value that reads `text` on this line goes on on the next one.
/// `quoted` is whether the value is inside quotes, before and after.
fn continues(text: &str, quoted: &mut bool) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => *quoted = !*quoted,
            // An escape takes the next character with it; with none left on
            // the line, it is the line break that is escaped.
            '\\' => {
                if chars.next().is_none() {
                    return true;
                }
            }
            '#' if !*quoted => return false,
            _ => {}
        }
    }
    false
}

/// The file as WSL sees it: one entry for each setting, section header,
/// comment or blank line, holding the line it starts on and the lines its
/// value continues on.
fn entries(text: &str) -> Vec<Vec<&str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    let mut open = false;
    let mut quoted = false;
    for line in text.lines() {
        if open {
            open = continues(line, &mut quoted);
            out.last_mut().expect("a line is being continued").push(line);
            continue;
        }
        quoted = false;
        open = value_part(line.trim_start_matches(MARK)).is_some_and(|(_, value)| continues(value, &mut quoted));
        out.push(vec![line]);
    }
    out
}

/// The value of a `memory = ...` line, if this is one (and not a comment):
/// without a comment after it and without quotes around it.
fn memory_value(line: &str) -> Option<String> {
    let (key, value) = value_part(line)?;
    if !key.eq_ignore_ascii_case("memory") {
        return None;
    }
    let mut out = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => break,
            c => out.push(c),
        }
    }
    Some(if value.contains('"') { out } else { out.trim().to_string() })
}

/// The limit a `.wslconfig` sets, as written. With several, WSL takes the
/// first and ignores the rest.
fn limit_in(text: &str) -> Option<String> {
    let mut inside = false;
    for entry in entries(text.trim_start_matches(MARK)) {
        if let Some(name) = section(entry[0]) {
            inside = name == "wsl2";
        } else if inside {
            if let Some(value) = memory_value(entry[0]) {
                return Some(value).filter(|v| !v.is_empty());
            }
        }
    }
    None
}

/// "8GB" or "8G" -> 8. Anything else is not one of the amounts Pier offers
/// and is shown as written instead: megabytes, a bare number of bytes, and
/// spellings WSL does not accept ("8gb", "8 GB") and so does not apply.
fn whole_gb(value: &str) -> Option<u32> {
    let digits = value.strip_suffix("GB").or_else(|| value.strip_suffix('G'))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `text` with the limit set to `limit` (or taken out), and nothing else
/// changed: other settings, comments, blank lines and line endings stay.
fn with_limit(text: &str, limit: Option<&str>) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    let mut header = None; // where the first [wsl2] line ended up
    let mut written = false;
    for entry in entries(text) {
        let first = entry[0].trim_start_matches(MARK);
        if let Some(name) = section(first) {
            inside = name == "wsl2";
            if inside && header.is_none() {
                header = Some(out.len());
            }
        } else if inside && memory_value(first).is_some() {
            // The first one becomes the new limit; any others go, so there
            // is no second one to disagree with it. Lines its value went on
            // on go with it.
            if let (Some(limit), false) = (limit, written) {
                out.push(format!("memory={limit}"));
                written = true;
            }
            continue;
        }
        out.extend(entry.into_iter().map(String::from));
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
        assert_eq!(limit_in(text).as_deref(), Some("3GB"), "WSL takes the first one");
        // One line is left, and the other section's is not touched.
        assert_eq!(with_limit(text, Some("8GB")), "[other]\nmemory=1GB\n[wsl2]\n# memory=2GB\nmemory=8GB\n");
        assert_eq!(with_limit(text, None), "[other]\nmemory=1GB\n[wsl2]\n# memory=2GB\n");
    }

    #[test]
    fn amounts_pier_does_not_offer_are_kept_as_written() {
        assert_eq!(whole_gb("8GB"), Some(8));
        assert_eq!(whole_gb("12G"), Some(12));
        // WSL rejects these, so they are not shown as a limit Pier offers.
        assert_eq!(whole_gb("12gb"), None);
        assert_eq!(whole_gb("12 GB"), None);
        assert_eq!(whole_gb("+8GB"), None);
        assert_eq!(whole_gb("512MB"), None);
        assert_eq!(whole_gb("4294967296"), None);
        assert_eq!(whole_gb("GB"), None);
    }

    #[test]
    fn the_file_is_read_the_way_wsl_reads_it() {
        // A comment after the value or the header, and quotes, are not part of it.
        assert_eq!(limit_in("[wsl2] # mine\nmemory = 8GB # for docker\n").as_deref(), Some("8GB"));
        assert_eq!(limit_in("[WSL2]\nMEMORY=\"6GB\"\n").as_deref(), Some("6GB"));
        // The first one counts, also across a second [wsl2].
        assert_eq!(limit_in("[wsl2]\nmemory=4GB\n[other]\nx=1\n[wsl2]\nmemory=8GB\n").as_deref(), Some("4GB"));
        // A header with a comment is still the section: no second one is added.
        assert_eq!(with_limit("[wsl2] # mine\nswap=0\n", Some("6GB")), "[wsl2] # mine\nmemory=6GB\nswap=0\n");
        // Setting a limit leaves exactly one line, so first or last is the same.
        let after = with_limit("[wsl2]\nmemory=4GB # old\n[wsl2]\nmemory=8GB\n", Some("6GB"));
        assert_eq!(after, "[wsl2]\nmemory=6GB\n[wsl2]\n");
        assert_eq!(limit_in(&after).as_deref(), Some("6GB"));
    }

    #[test]
    fn a_line_a_value_continues_on_is_part_of_that_value() {
        // WSL reads the third line as the rest of the kernel command line.
        let text = "[wsl2]\nkernelCommandLine = quiet \\\nmemory=4GB\nswap=0\n";
        assert_eq!(limit_in(text), None);
        assert_eq!(with_limit(text, Some("8GB")), "[wsl2]\nmemory=8GB\nkernelCommandLine = quiet \\\nmemory=4GB\nswap=0\n");
        assert_eq!(with_limit(text, None), text);
        // A real limit after it is still found and replaced.
        let text = "[wsl2]\r\nkernelCommandLine = \"a \\\r\nmemory=1GB\" \\\r\n  memory=2GB\r\nmemory=4GB\r\n";
        assert_eq!(limit_in(text).as_deref(), Some("4GB"));
        assert_eq!(with_limit(text, Some("8GB")), text.replace("\r\nmemory=4GB", "\r\nmemory=8GB"));
        // Not a continuation: an escaped backslash, a backslash in a comment,
        // one after a comment has begun, and one on a line that is no setting.
        for text in ["[wsl2]\nx=a\\\\\nmemory=4GB\n", "[wsl2]\n# x \\\nmemory=4GB\n", "[wsl2]\nx=a # b \\\nmemory=4GB\n", "[wsl2]\nnonsense \\\nmemory=4GB\n"] {
            assert_eq!(limit_in(text).as_deref(), Some("4GB"), "{text}");
        }
        // A limit whose own value continues goes as a whole.
        assert_eq!(with_limit("[wsl2]\nmemory=4GB\\\n\nswap=0\n", Some("8GB")), "[wsl2]\nmemory=8GB\nswap=0\n");
    }

    #[test]
    fn utf16_as_windows_writes_it_is_read_and_kept() {
        let text = "[wsl2]\r\nmemory=4GB\r\n";
        let bytes = encode(text, Encoding::Utf16);
        assert_eq!(&bytes[..4], [0xFF, 0xFE, b'[', 0]);
        assert_eq!(decode(&bytes), Some((text.to_string(), Encoding::Utf16)));
        assert_eq!(decode(text.as_bytes()), Some((text.to_string(), Encoding::Utf8)));
        // Half a character, or bytes that are no text at all.
        assert_eq!(decode(&[0xFF, 0xFE, b'[']), None);
        assert_eq!(decode(&[b'[', 0xC3]), None);
    }

    /// On real files, in a folder standing in for the user's. Ignored in
    /// the ordinary run because it changes USERPROFILE for the process:
    /// `cargo test --lib real_files -- --ignored`.
    #[test]
    #[ignore]
    fn real_files_are_saved_in_place_and_in_their_encoding() {
        let home = std::env::temp_dir().join(format!("pier-wslconfig-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("USERPROFILE", &home);
        let file = home.join(".wslconfig");

        // No file yet.
        assert_eq!(memory().unwrap().limit, None);
        assert_eq!(set_memory(Some(1)).unwrap().limit_gb, Some(1));
        assert_eq!(std::fs::read(&file).unwrap(), b"[wsl2]\nmemory=1GB\n");

        // UTF-16, as `>` in Windows PowerShell writes it, with a value that
        // continues on a line that looks like a limit.
        let before = "[wsl2]\r\nkernelCommandLine = quiet \\\r\nmemory=4GB\r\nswap=0\r\n";
        std::fs::write(&file, encode(before, Encoding::Utf16)).unwrap();
        assert_eq!(memory().unwrap().limit, None);
        assert_eq!(set_memory(Some(1)).unwrap().limit_gb, Some(1));
        let (after, encoding) = decode(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(encoding, Encoding::Utf16);
        assert_eq!(after, before.replace("[wsl2]\r\n", "[wsl2]\r\nmemory=1GB\r\n"));
        assert_eq!(set_memory(None).unwrap().limit, None);
        assert_eq!(decode(&std::fs::read(&file).unwrap()).unwrap().0, before);

        // Nothing is left beside it, and a file Pier cannot read stays as it is.
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
        std::fs::write(&file, [0xFF, 0xFE, b'[']).unwrap();
        assert!(memory().is_err() && set_memory(Some(1)).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), [0xFF, 0xFE, b'[']);
        std::fs::remove_dir_all(&home).unwrap();
    }
}