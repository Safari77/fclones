//! Command line argument parsing and quoting utilities.
//!
//! Provides lossless OsString conversions to and from String by shell-like escaping and quoting.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{Debug, Display, Formatter};
use std::mem;

use itertools::Itertools;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use stfu8::DecodeError;

/// Argument passed to the app
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Arg(OsString);

impl Arg {
    pub fn from_escaped_string(s: &str) -> Result<Self, DecodeError> {
        Ok(Arg(from_stfu8(s)?))
    }

    pub fn to_escaped_string(&self) -> String {
        to_stfu8(self.0.clone())
    }

    pub fn quote(&self) -> String {
        quote(self.0.to_os_string())
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl AsRef<OsStr> for Arg {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl From<OsString> for Arg {
    fn from(s: OsString) -> Self {
        Arg(s)
    }
}

impl From<&OsStr> for Arg {
    fn from(s: &OsStr) -> Self {
        Arg(OsString::from(s))
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg(OsString::from(s))
    }
}

struct ArgVisitor;

impl Visitor<'_> for ArgVisitor {
    type Value = Arg;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an STFU encoded string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let arg = Arg::from_escaped_string(v).map_err(|e| E::custom(e.to_string()))?;
        Ok(arg)
    }
}

impl Serialize for Arg {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_escaped_string().as_str())
    }
}

impl<'de> Deserialize<'de> for Arg {
    fn deserialize<D>(deserializer: D) -> Result<Arg, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ArgVisitor)
    }
}

/// Returns a lossless string representation in [STFU8 format](https://crates.io/crates/stfu8).
#[cfg(unix)]
pub fn to_stfu8(s: OsString) -> String {
    use std::os::unix::ffi::OsStringExt;
    let raw_path_bytes = s.into_vec();
    stfu8::encode_u8(&raw_path_bytes)
}

/// Returns a lossless string representation in [STFU8 format](https://crates.io/crates/stfu8).
#[cfg(windows)]
pub fn to_stfu8(s: OsString) -> String {
    use std::os::windows::ffi::OsStrExt;
    let raw_path_bytes: Vec<u16> = s.encode_wide().collect();
    stfu8::encode_u16(&raw_path_bytes)
}

/// Decodes the path from the string encoded with [`to_stfu8`](OsString::to_stfu8).
#[cfg(unix)]
pub fn from_stfu8(encoded: &str) -> Result<OsString, DecodeError> {
    use std::os::unix::ffi::OsStringExt;
    let raw_bytes = stfu8::decode_u8(encoded)?;
    Ok(OsString::from_vec(raw_bytes))
}

/// Decodes the path from the string encoded with [`to_stfu8`](OsString::to_stfu8).
#[cfg(windows)]
pub fn from_stfu8(encoded: &str) -> Result<OsString, DecodeError> {
    use std::os::windows::ffi::OsStringExt;
    let raw_bytes = stfu8::decode_u16(encoded)?;
    Ok(OsString::from_wide(&raw_bytes))
}

/// Returns true if the character is guaranteed safe to leave unquoted in a Unix shell.
#[inline]
fn is_safe_shell_char(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | '_' | '.' | '-')
}

/// Escapes special characters in a string, so that it will retain its literal meaning when used as
/// a part of command in Unix shell.
///
/// It tries to avoid introducing any unnecessary quotes or escape characters, but specifics
/// regarding quoting style are left unspecified.
pub fn quote(s: OsString) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let is_invalid_utf8 = s.to_str().is_none();
    let lossy = s.to_string_lossy();
    if is_invalid_utf8
        || lossy
            .chars()
            .any(|c| c < '\u{20}' || c == '\u{7f}' || c == '\'')
    {
        format!("$'{}'", to_stfu8(s).replace('\'', "\\'"))
    } else if !lossy.chars().all(is_safe_shell_char) {
        format!("'{lossy}'")
    } else {
        lossy.into_owned()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ParseError {
    pub msg: String,
}

impl ParseError {
    pub fn new(msg: &str) -> ParseError {
        ParseError {
            msg: msg.to_string(),
        }
    }
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl Error for ParseError {}

enum State {
    /// Within a delimiter.
    Delimiter,
    /// After backslash, but before starting word.
    Backslash,
    /// Within an unquoted word.
    Unquoted,
    /// After backslash in an unquoted word.
    UnquotedBackslash,
    /// Within a single quoted word.
    SingleQuoted,
    /// Within a double quoted word.
    DoubleQuoted,
    /// After backslash inside a double quoted word.
    DoubleQuotedBackslash,
    /// After dollar in an unquoted word.
    Dollar,
    /// Within a quoted word preceded by a dollar sign.
    DollarQuoted,
    /// After backslash in a dollar-quoted word.
    DollarQuotedBackslash,
    /// Inside a comment.
    Comment,
}

/// Appends a character to OsString
fn append(s: &mut OsString, c: char) {
    let mut buf = [0; 4];
    let c = c.encode_utf8(&mut buf);
    s.push(c)
}

/// Unescapes `\'` and `\"` inside a `$'...'` slice in a single pass so escaped backslashes (`\\`)
/// are preserved for STFU-8 decoding.
fn unescape_dollar_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\'') => out.push('\''),
                Some('"') => out.push('"'),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Splits command line into separate arguments, in much the same way Unix shell would, but without
/// many of expansion the shell would perform.
///
/// The split functionality is compatible with behaviour of Unix shell, but with word expansions
/// limited to quote removal, and without special token recognition rules for operators.
///
/// The result is exactly the same as one obtained from Unix shell as long as those unsupported
/// features are not present in input: no operators, no variable assignments, no tilde expansion,
/// no parameter expansion, no command substitution, no arithmetic expansion, no pathname
/// expansion.
///
/// In case those unsupported shell features are present, the syntax that introduce them is
/// interpreted literally.
///
/// # Errors
///
/// When input contains unmatched quote, an error is returned.
///
/// # Compatibility with other implementations
///
/// It should be fully compatible with g_shell_parse_argv from GLib, except that in GLib
/// it is an error not to have any words after tokenization.
///
/// It is also very close to shlex.split available in Python standard library, when used in POSIX
/// mode with support for comments. Though, shlex implementation diverges from POSIX, and from
/// implementation contained herein in three aspects. First, it doesn't support line continuations.
/// Second, inside double quotes, the backslash characters retains its special meaning as an escape
/// character only when followed by \\ or \", whereas POSIX specifies that it should retain its
/// special meaning when followed by: $, \`, \", \\, or a newline. Third, it treats carriage return
/// as one of delimiters.
/// ```
pub fn split(s: &str) -> Result<Vec<Arg>, ParseError> {
    // Based on shell-words crate by Tomasz Miąsko
    // Handling of dollar quotes added by Piotr Kołaczkowski

    use State::*;

    let mut words = Vec::new();
    let mut word = OsString::new();

    let mut dollar_quote_start = 0;

    let mut chars = s.char_indices();
    let mut state = Delimiter;

    loop {
        let (pos, c) = match chars.next() {
            Some((p, ch)) => (p, Some(ch)),
            None => (s.len(), None),
        };
        state = match state {
            Delimiter => match c {
                None => break,
                Some('\'') => SingleQuoted,
                Some('\"') => DoubleQuoted,
                Some('\\') => Backslash,
                Some('\t') | Some(' ') | Some('\n') => Delimiter,
                Some('$') => Dollar,
                Some('#') => Comment,
                Some(c) => {
                    append(&mut word, c);
                    Unquoted
                }
            },
            Backslash => match c {
                None => {
                    append(&mut word, '\\');
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    break;
                }
                Some('\n') => Delimiter,
                Some(c) => {
                    append(&mut word, c);
                    Unquoted
                }
            },
            Unquoted => match c {
                None => {
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    break;
                }
                Some('\'') => SingleQuoted,
                Some('\"') => DoubleQuoted,
                Some('\\') => UnquotedBackslash,
                Some('$') => Dollar,
                Some('\t') | Some(' ') | Some('\n') => {
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    Delimiter
                }
                Some(c) => {
                    append(&mut word, c);
                    Unquoted
                }
            },
            UnquotedBackslash => match c {
                None => {
                    append(&mut word, '\\');
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    break;
                }
                Some('\n') => Unquoted,
                Some(c) => {
                    append(&mut word, c);
                    Unquoted
                }
            },
            SingleQuoted => match c {
                None => return Err(ParseError::new("Unclosed single quote")),
                Some('\'') => Unquoted,
                Some(c) => {
                    append(&mut word, c);
                    SingleQuoted
                }
            },
            DoubleQuoted => match c {
                None => return Err(ParseError::new("Unclosed double quote")),
                Some('\"') => Unquoted,
                Some('\\') => DoubleQuotedBackslash,
                Some(c) => {
                    append(&mut word, c);
                    DoubleQuoted
                }
            },
            DoubleQuotedBackslash => match c {
                None => return Err(ParseError::new("Unexpected end of input")),
                Some('\n') => DoubleQuoted,
                Some(c @ '$') | Some(c @ '`') | Some(c @ '"') | Some(c @ '\\') => {
                    append(&mut word, c);
                    DoubleQuoted
                }
                Some(c) => {
                    append(&mut word, '\\');
                    append(&mut word, c);
                    DoubleQuoted
                }
            },
            Dollar => match c {
                None => {
                    append(&mut word, '$');
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    break;
                }
                Some('\'') => {
                    dollar_quote_start = pos + 1;
                    DollarQuoted
                }
                Some('\"') => {
                    append(&mut word, '$');
                    DoubleQuoted
                }
                Some('\\') => {
                    append(&mut word, '$');
                    UnquotedBackslash
                }
                Some('$') => {
                    append(&mut word, '$');
                    Dollar
                }
                Some('\t') | Some(' ') | Some('\n') => {
                    append(&mut word, '$');
                    words.push(Arg(mem::replace(&mut word, OsString::new())));
                    Delimiter
                }
                Some(c) => {
                    append(&mut word, '$');
                    append(&mut word, c);
                    Unquoted
                }
            },
            DollarQuoted => match c {
                None => return Err(ParseError::new("Unclosed single quote")),
                Some('\\') => DollarQuotedBackslash,
                Some('\'') => {
                    let quoted_slice = unescape_dollar_quoted(&s[dollar_quote_start..pos]);
                    let decoded = from_stfu8(&quoted_slice).map_err(|e| {
                        ParseError::new(format!("Failed to decode STFU-8 chunk: {e}").as_str())
                    })?;
                    word.push(decoded.as_os_str());
                    Unquoted
                }
                Some(_) => DollarQuoted,
            },
            DollarQuotedBackslash => match c {
                None => return Err(ParseError::new("Unexpected end of input")),
                Some(_) => DollarQuoted,
            },
            Comment => match c {
                None => break,
                Some('\n') => Delimiter,
                Some(_) => Comment,
            },
        };
    }

    Ok(words)
}

/// Joins multiple command line args into a single-line escaped representation
pub fn join(args: &[Arg]) -> String {
    args.iter().map(|arg| arg.quote()).join(" ")
}

#[cfg(test)]
mod test {
    use std::ffi::OsString;

    use crate::arg::{join, quote, split, Arg};

    #[test]
    fn quote_no_special_chars() {
        assert_eq!(quote(OsString::from("abc/def_123.txt")), "abc/def_123.txt");
    }

    #[test]
    fn quote_empty_string() {
        assert_eq!(quote(OsString::from("")), "''");
        assert_eq!(
            split(&quote(OsString::from(""))).unwrap(),
            vec![Arg::from("")]
        );
    }

    #[test]
    fn quote_path_with_control_chars() {
        assert_eq!(quote(OsString::from("a\nb")), "$'a\\nb'");
        assert_eq!(quote(OsString::from("a\tb")), "$'a\\tb'");
        assert_eq!(quote(OsString::from("a\rb")), "$'a\\rb'");
        assert_eq!(quote(OsString::from("a\x00b")), "$'a\\x00b'");
        assert_eq!(quote(OsString::from("a\x7fb")), "$'a\\x7Fb'");
    }

    #[test]
    fn quote_path_with_special_chars() {
        assert_eq!(quote(OsString::from("a b")), "'a b'");
        assert_eq!(quote(OsString::from("a*b")), "'a*b'");
        assert_eq!(quote(OsString::from("a?b")), "'a?b'");
        assert_eq!(quote(OsString::from("$ab")), "'$ab'");
        assert_eq!(quote(OsString::from("a(b)")), "'a(b)'");
        assert_eq!(quote(OsString::from("a\\b")), "'a\\b'");
    }

    #[test]
    fn quote_malicious_command_injection_filenames() {
        assert_eq!(
            quote(OsString::from("hi.txt;echo pwned")),
            "'hi.txt;echo pwned'"
        );
        assert_eq!(quote(OsString::from("$(id)")), "'$(id)'");
        assert_eq!(quote(OsString::from("`uname -a`")), "'`uname -a`'");
        assert_eq!(
            quote(OsString::from("foo | sh && rm -rf /ohno")),
            "'foo | sh && rm -rf /ohno'"
        );
        assert_eq!(quote(OsString::from("a>b<c")), "'a>b<c'");
        assert_eq!(
            quote(OsString::from("file\necho pwned\n")),
            "$'file\\necho pwned\\n'"
        );
    }

    #[test]
    fn quote_tilde_history_and_glob_characters() {
        // Ensures ASCII tilde U+007E (not just U+02DC) is quoted
        assert_eq!(quote(OsString::from("~")), "'~'");
        assert_eq!(quote(OsString::from("~/secret")), "'~/secret'");
        assert_eq!(quote(OsString::from("~root/.ssh")), "'~root/.ssh'");

        // Ensures history expansion (!) and zsh extended globs (^) are quoted
        assert_eq!(quote(OsString::from("!-1")), "'!-1'");
        assert_eq!(quote(OsString::from("foo!bar")), "'foo!bar'");
        assert_eq!(quote(OsString::from("foo^bar")), "'foo^bar'");
    }

    #[test]
    fn quote_unicode_whitespace_and_control_chars() {
        // Non-breaking space (U+00A0) and C1 control character (U+0085) are non-ASCII,
        // so the allowlist places them in single quotes
        assert_eq!(quote(OsString::from("a\u{00a0}b")), "'a\u{00a0}b'");
        assert_eq!(quote(OsString::from("a\u{0085}b")), "'a\u{0085}b'");
        // Valid UTF-8 replacement character U+FFFD is single-quoted (not treated as invalid UTF-8)
        assert_eq!(quote(OsString::from("a\u{fffd}b")), "'a\u{fffd}b'");
    }

    #[test]
    fn quote_path_with_single_quotes() {
        assert_eq!(quote(OsString::from("a'b")), "$'a\\'b'");
        assert_eq!(quote(OsString::from("a'b'")), "$'a\\'b\\''");
        assert_eq!(quote(OsString::from("a\\'b")), "$'a\\\\\\'b'");
    }

    #[test]
    fn split_unquoted_args() {
        assert_eq!(
            split("arg1 arg2").unwrap(),
            vec![Arg::from("arg1"), Arg::from("arg2")]
        )
    }

    #[test]
    fn split_single_quoted_args() {
        assert_eq!(
            split("'arg1 with spaces' arg2").unwrap(),
            vec![Arg::from("arg1 with spaces"), Arg::from("arg2")]
        )
    }

    #[test]
    fn split_doubly_quoted_args() {
        assert_eq!(
            split("\"arg1 with spaces\" arg2").unwrap(),
            vec![Arg::from("arg1 with spaces"), Arg::from("arg2")]
        )
    }

    #[test]
    fn split_quotes_escaping() {
        assert_eq!(
            split("\"escaped \\\" quotes\"").unwrap(),
            vec![Arg::from("escaped \" quotes")]
        )
    }

    #[test]
    fn split_escaped_single_quote() {
        assert_eq!(
            split("$'single\\'quote'").unwrap(),
            vec![Arg::from("single'quote")]
        );
        assert_eq!(
            split("$'backslash\\\\\\'quote'").unwrap(),
            vec![Arg::from("backslash\\'quote")]
        );
        assert_eq!(
            split("$'escaped\\\"doublequote'").unwrap(),
            vec![Arg::from("escaped\"doublequote")]
        );
        assert_eq!(
            split("$'backslash\\\\\"doublequote'").unwrap(),
            vec![Arg::from("backslash\\\"doublequote")]
        );
    }

    #[test]
    fn split_spaces_escaping() {
        assert_eq!(
            split("escaped\\ space").unwrap(),
            vec![Arg::from("escaped space")]
        )
    }

    #[test]
    fn dollar_quoting() {
        assert_eq!(
            split("arg1 $'arg2-\\n\\t\\\\' arg3-$'\\x7f'").unwrap(),
            vec![
                Arg::from("arg1"),
                Arg::from("arg2-\n\t\\"),
                Arg::from("arg3-\x7f")
            ]
        )
    }

    #[test]
    fn split_multibyte_utf8_with_dollar_quoting() {
        // Multi-byte UTF-8 before, inside, and after $'...' must not panic or corrupt slices
        assert_eq!(
            split("ą $'foo\\nbar'").unwrap(),
            vec![Arg::from("ą"), Arg::from("foo\nbar")]
        );
        assert_eq!(
            split("🦀 $'zażółć\\ngęślą\\njaźń' 🌸").unwrap(),
            vec![
                Arg::from("🦀"),
                Arg::from("zażółć\ngęślą\njaźń"),
                Arg::from("🌸")
            ]
        );
        assert_eq!(split("$'中\\'文'").unwrap(), vec![Arg::from("中'文")]);
    }

    #[test]
    fn split_unquoted_dollar_signs() {
        assert_eq!(
            split("$HOME foo$bar price$ $ $$ $'a\\nb' $$$'c\\nd'").unwrap(),
            vec![
                Arg::from("$HOME"),
                Arg::from("foo$bar"),
                Arg::from("price$"),
                Arg::from("$"),
                Arg::from("$$"),
                Arg::from("a\nb"),
                Arg::from("$$c\nd"),
            ]
        );
        assert_eq!(
            split("$\"quoted\" $\\ space").unwrap(),
            vec![Arg::from("$quoted"), Arg::from("$ space")]
        );
    }

    #[test]
    fn roundtrip_quote_and_split_edge_cases() {
        let cases = vec![
            "",
            " ",
            "   ",
            "simple_file.txt",
            "-rf",
            "--no-preserve-root",
            "hi.txt;echo pwned",
            "$(reboot)",
            "`id`",
            "~",
            "~/Documents",
            "~root/file",
            "!history",
            "a'b",
            "a\\'b",
            "a\\\\'b",
            "a\"b",
            "a\\\"b",
            "it's a \"quoted\" ' nightmare \\ \n \t \r \x07 \x7f",
            "zażółć gęślą jaźń",
            "🦀'🌸\n日本語",
            "$VAR",
            "${VAR}",
            "$$",
            "#comment_lookalike",
            "mid#hash",
            "trailing_backslash\\",
        ];

        let args: Vec<Arg> = cases.into_iter().map(Arg::from).collect();
        let joined = join(&args);
        let parsed = split(&joined).unwrap();
        assert_eq!(parsed, args);
    }

    #[cfg(unix)]
    #[test]
    fn roundtrip_non_utf8_bytes_unix() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![
            b'f', b'o', b'o', 0x80, 0xfe, 0xff, b'\'', b'\\', b'\n',
        ]);
        let arg = Arg::from(raw);
        let quoted = arg.quote();
        assert_eq!(quoted, "$'foo\\x80\\xFE\\xFF\\'\\\\\\n'");
        let parsed = split(&quoted).unwrap();
        assert_eq!(parsed, vec![arg]);
    }

    #[test]
    fn split_error_cases() {
        assert!(split("'unclosed single").is_err());
        assert!(split("\"unclosed double").is_err());
        assert!(split("$'unclosed dollar").is_err());
        assert!(split("\"trailing backslash\\").is_err());
        assert!(split("$'trailing backslash\\").is_err());
    }
}
