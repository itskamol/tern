//! OSC 7 (`ESC ] 7 ; file://host/path BEL`): the shell says which folder it is in. The
//! emulator drops it, so the OSC 133 scanner finds it in the bytes beside the prompt
//! marks. The host is kept, not checked: a nested `ssh` reports folders on another machine,
//! and only the app knows which host the session is on.

/// The folder a shell reported, with the host it named (empty for `file:///path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFolder {
    pub host: String,
    pub path: String,
}

/// Longest `7;file://…` body kept: the longest path macOS and Linux accept (`PATH_MAX`), with
/// room for the scheme and a host name.
pub(crate) const BODY_MAX: usize = 4096 + 256;

/// The folder in an OSC body (`7;file://host/path`), percent-decoded; `None` for anything
/// else, or a path that is not absolute UTF-8.
pub(crate) fn parse(body: &[u8]) -> Option<ShellFolder> {
    let url = body.strip_prefix(b"7;file://")?;
    let slash = url.iter().position(|&b| b == b'/')?;
    let (host, path) = url.split_at(slash);
    let host = String::from_utf8(decode(host)?).ok()?;
    let path = String::from_utf8(decode(path)?).ok()?;
    if path.chars().any(char::is_control) {
        return None;
    }
    Some(ShellFolder { host, path })
}

/// `%2F` and the like turned back into bytes; a broken escape makes the whole thing invalid.
fn decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&b) = bytes.get(at) {
        if b == b'%' {
            let hex = bytes.get(at + 1..at + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            at += 3;
        } else {
            out.push(b);
            at += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "osc7_tests.rs"]
mod tests;
