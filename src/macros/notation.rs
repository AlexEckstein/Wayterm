//! The text form of a step's keys in the macro editor (doc 12
//! §Notation): text stands for itself, everything else is a `<Name>`
//! token — `<Enter>`, `<Up>`, `<Ctrl-C>`, `<0x9B>` — and `<lt>` is a
//! literal `<`. A space that would be invisible at either end of an entry
//! is `<Space>`. [`decode`] inverts [`encode`] for every byte string.
//! Unknown tokens are errors, never literal text, so a typo in the editor
//! cannot reach the remote shell.

use std::fmt;

/// Named multi-byte sequences: what VTE sends for these keys outside
/// application cursor/keypad mode (the same bytes as xterm).
const SEQUENCES: &[(&str, &[u8])] = &[
    ("Up", b"\x1b[A"),
    ("Down", b"\x1b[B"),
    ("Right", b"\x1b[C"),
    ("Left", b"\x1b[D"),
    ("Home", b"\x1b[H"),
    ("End", b"\x1b[F"),
    ("Insert", b"\x1b[2~"),
    ("Delete", b"\x1b[3~"),
    ("PageUp", b"\x1b[5~"),
    ("PageDown", b"\x1b[6~"),
    ("Shift-Tab", b"\x1b[Z"),
    ("F1", b"\x1bOP"),
    ("F2", b"\x1bOQ"),
    ("F3", b"\x1bOR"),
    ("F4", b"\x1bOS"),
    ("F5", b"\x1b[15~"),
    ("F6", b"\x1b[17~"),
    ("F7", b"\x1b[18~"),
    ("F8", b"\x1b[19~"),
    ("F9", b"\x1b[20~"),
    ("F10", b"\x1b[21~"),
    ("F11", b"\x1b[23~"),
    ("F12", b"\x1b[24~"),
];

/// Named single bytes; the other C0 controls are `<Ctrl-X>`.
const BYTES: &[(&str, u8)] = &[
    ("Enter", b'\r'),
    ("Tab", b'\t'),
    ("Esc", 0x1b),
    ("Backspace", 0x7f),
    ("lt", b'<'),
];

/// Why some editor text is not a key sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotationError {
    /// A `<` with no `>` after it.
    Unclosed,
    /// `<name>` names no key.
    UnknownKey(String),
}

impl fmt::Display for NotationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unclosed => write!(f, "\"<\" without a closing \">\""),
            Self::UnknownKey(name) => write!(f, "unknown key <{name}>"),
        }?;
        write!(f, " (write <lt> for a literal \"<\")")
    }
}

impl std::error::Error for NotationError {}

/// The editor text for `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut rest = bytes;
    while let Some(&first) = rest.first() {
        if let Some((name, seq)) = SEQUENCES.iter().find(|(_, seq)| rest.starts_with(seq)) {
            push_token(&mut text, name);
            rest = &rest[seq.len()..];
        } else if let Some(name) = byte_name(first) {
            push_token(&mut text, &name);
            rest = &rest[1..];
        } else if first == b' ' && (text.is_empty() || rest.len() == 1) {
            push_token(&mut text, "Space");
            rest = &rest[1..];
        } else if let Some(ch) = leading_char(rest).filter(|ch| !ch.is_control()) {
            text.push(ch);
            rest = &rest[ch.len_utf8()..];
        } else {
            // Invalid UTF-8, or a C1 control: one byte at a time.
            push_token(&mut text, &format!("0x{first:02X}"));
            rest = &rest[1..];
        }
    }
    text
}

/// The bytes `text` stands for.
pub fn decode(text: &str) -> Result<Vec<u8>, NotationError> {
    let mut bytes = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        bytes.extend_from_slice(&rest.as_bytes()[..open]);
        let after = &rest[open + 1..];
        let close = after.find('>').ok_or(NotationError::Unclosed)?;
        let name = &after[..close];
        let key = token_bytes(name).ok_or_else(|| NotationError::UnknownKey(name.into()))?;
        bytes.extend_from_slice(&key);
        rest = &after[close + 1..];
    }
    bytes.extend_from_slice(rest.as_bytes());
    Ok(bytes)
}

fn push_token(text: &mut String, name: &str) {
    text.push('<');
    text.push_str(name);
    text.push('>');
}

/// The name of a byte that never stands for itself: the C0 controls,
/// DEL, and `<`.
fn byte_name(byte: u8) -> Option<String> {
    if let Some((name, _)) = BYTES.iter().find(|(_, b)| *b == byte) {
        return Some((*name).into());
    }
    match byte {
        0x00..=0x1f => Some(format!("Ctrl-{}", char::from(byte + 0x40))),
        _ => None,
    }
}

/// The char `bytes` starts with, if they start with valid UTF-8.
fn leading_char(bytes: &[u8]) -> Option<char> {
    (1..=bytes.len().min(4))
        .find_map(|len| std::str::from_utf8(&bytes[..len]).ok())
        .and_then(|s| s.chars().next())
}

/// The bytes a token's name stands for; names are case-insensitive.
fn token_bytes(name: &str) -> Option<Vec<u8>> {
    if let Some((_, seq)) = SEQUENCES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(seq.to_vec());
    }
    if let Some((_, byte)) = BYTES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(vec![*byte]);
    }
    if name.eq_ignore_ascii_case("Space") {
        return Some(b" ".to_vec());
    }
    if let Some(hex) = strip_prefix_ignore_case(name, "0x") {
        // `from_str_radix` alone would take a sign: `<0x+1>`.
        let digits = hex.len() == 2 && hex.bytes().all(|b| b.is_ascii_hexdigit());
        return digits.then(|| vec![u8::from_str_radix(hex, 16).unwrap_or_default()]);
    }
    let key = strip_prefix_ignore_case(name, "Ctrl-")?;
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch @ ('@'..='_' | 'a'..='z')), None) => {
            Some(vec![(ch.to_ascii_uppercase() as u8) & 0x1f])
        }
        _ => None,
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystrokes_read_as_their_names() {
        let cases: [(&[u8], &str); 15] = [
            (b" ", "<Space>"),
            (b" ls  -l ", "<Space>ls  -l<Space>"),
            (b"echo hi", "echo hi"),
            (b"ls -la\r", "ls -la<Enter>"),
            (b"\x1b[A", "<Up>"),
            (b"\x1bOA", "<Esc>OA"),
            (b"\x03", "<Ctrl-C>"),
            (b"\x7f", "<Backspace>"),
            (b"\t", "<Tab>"),
            (b"\x00\x1c\x1f", "<Ctrl-@><Ctrl-\\><Ctrl-_>"),
            (b"a<b>c", "a<lt>b>c"),
            (b"\x1b[24~\x1bOP", "<F12><F1>"),
            ("héllo ✓".as_bytes(), "héllo ✓"),
            (b"\xff\xc3", "<0xFF><0xC3>"),
            ("\u{9b}".as_bytes(), "<0xC2><0x9B>"),
        ];
        for (bytes, text) in cases {
            assert_eq!(encode(bytes), text, "{bytes:?}");
            assert_eq!(decode(text).unwrap(), bytes, "{text}");
        }
    }

    #[test]
    fn names_are_case_insensitive_and_ctrl_takes_letters() {
        assert_eq!(
            decode("<enter><ESC><ctrl-c><Ctrl-[><0x9b>").unwrap(),
            [13, 27, 3, 27, 0x9b]
        );
        assert_eq!(decode("<pageup>").unwrap(), b"\x1b[5~");
    }

    #[test]
    fn unknown_or_unclosed_tokens_are_errors() {
        assert_eq!(
            decode("echo a <Entr>"),
            Err(NotationError::UnknownKey("Entr".into()))
        );
        assert_eq!(decode("a < b"), Err(NotationError::Unclosed));
        for bad in [
            "<>",
            "<0x1>",
            "<0xZZ>",
            "<0x+1>",
            "<Ctrl-1>",
            "<Ctrl-ab>",
            "<0x123>",
        ] {
            assert!(decode(bad).is_err(), "{bad}");
        }
        assert_eq!(
            NotationError::Unclosed.to_string(),
            "\"<\" without a closing \">\" (write <lt> for a literal \"<\")"
        );
    }

    #[test]
    fn a_lone_closing_bracket_is_text() {
        assert_eq!(decode("a>b").unwrap(), b"a>b");
    }

    /// `decode(encode(b)) == b` for every byte, every byte pair, and every
    /// named sequence with a byte on either side.
    #[test]
    fn decode_inverts_encode() {
        let roundtrip = |bytes: &[u8]| {
            assert_eq!(decode(&encode(bytes)).as_deref(), Ok(bytes), "{bytes:?}");
        };
        for a in 0..=255u8 {
            roundtrip(&[a]);
            for b in 0..=255u8 {
                roundtrip(&[a, b]);
            }
        }
        for (_, seq) in SEQUENCES {
            for byte in 0..=255u8 {
                roundtrip(&[&[byte], *seq].concat());
                roundtrip(&[*seq, &[byte]].concat());
            }
        }
    }
}
