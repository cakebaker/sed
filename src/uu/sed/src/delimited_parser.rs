// Parse delimited character sequences
//
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Diomidis Spinellis
//
// This file is part of the uutils sed package.
// It is licensed under the MIT License.
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use crate::script_char_provider::ScriptCharProvider;
use crate::script_line_provider::ScriptLineProvider;
use std::char;
use uucore::error::{UResult, USimpleError};

// Fail with msg as a compile error at the current location
pub fn compilation_error<T>(
    lines: &ScriptLineProvider,
    line: &ScriptCharProvider,
    msg: impl ToString,
) -> UResult<T> {
    Err(USimpleError::new(
        1,
        format!(
            "{}:{}:{}: error: {}",
            lines.get_input_name(),
            lines.get_line_number(),
            line.get_pos(),
            msg.to_string()
        ),
    ))
}

/// Return true if c is a valid octal digit
fn is_ascii_octal_digit(c: char) -> bool {
    matches!(c, '0'..='7')
}

/// Parse a numeric character escape and return the corresponding char.
/// Advance line to the first character not part of the escape.
/// ndigits is the number of allowed digits and radix is the value's
/// radix (e.g. 8, 10, 16 for octal, decimal, and hex escapes).
/// For values up to 3 ndigits is the maximum number of allowed digits,
/// for values above 3 ndigits is the exact number of allowed digits.
/// Return `None` if no valid character has been specified.
fn parse_numeric_escape(
    line: &mut ScriptCharProvider,
    is_allowed_char: fn(char) -> bool,
    ndigits: usize,
    radix: u32,
) -> Option<char> {
    let mut valid_chars = Vec::new();

    for _ in 0..ndigits {
        if !line.eol() && is_allowed_char(line.current()) {
            valid_chars.push(line.current());
            line.advance();
        } else {
            break;
        }
    }

    if valid_chars.is_empty() {
        return None;
    }

    if ndigits > 3 && valid_chars.len() != ndigits {
        line.retreat(valid_chars.len());
        return None;
    }

    let char_string: String = valid_chars.into_iter().collect();
    match u32::from_str_radix(&char_string, radix)
        .ok()
        .and_then(char::from_u32)
    {
        Some(decoded) => Some(decoded),
        None => panic!("Unable to decode numeric character escape."),
    }
}

/// Transforms the specified character into the corresponding ASCII
/// control character as follows.
/// - Convert lowercase letters to uppercase
/// - XOR the ASCII value with 0x40 (inverts bit 6)
///
/// Return `None` if the result is not a valid Unicode scalar.
fn create_control_char(x: char) -> Option<char> {
    if !x.is_ascii() {
        return None;
    }

    let c = x.to_ascii_uppercase();

    let transformed = (c as u8) ^ 0x40;
    char::from_u32(transformed as u32)
}

/// Parse a character escape valid in all contexts (RE pattern, substitution,
/// transliterarion) and return the corresponding char.
/// At entry line.current() must have advanced after the `\\`.
/// Advance line to the first character not part of the escape.
/// Return `None` if an invalid escape has been specified.
fn parse_char_escape(line: &mut ScriptCharProvider) -> Option<char> {
    match line.current() {
        'a' => {
            line.advance();
            Some('\x07')
        }
        'f' => {
            line.advance();
            Some('\x0c')
        }
        'n' => {
            line.advance();
            Some('\n')
        }
        'r' => {
            line.advance();
            Some('\r')
        }
        't' => {
            line.advance();
            Some('\t')
        }
        'v' => {
            line.advance();
            Some('\x0b')
        }

        'c' => {
            // Control character escape: \cC
            line.advance(); // move past 'c'
            match create_control_char(line.current()) {
                Some(decoded) => {
                    line.advance();
                    Some(decoded)
                }
                None => Some('c'),
            }
        }

        'd' => {
            // Decimal escape: \dnnn
            line.advance(); // move past 'd'
            match parse_numeric_escape(line, |c| c.is_ascii_digit(), 3, 10) {
                Some(decoded) => Some(decoded),
                None => Some('d'),
            }
        }

        'o' => {
            // Octal escape: \onnn
            line.advance(); // move past 'o'
            match parse_numeric_escape(line, is_ascii_octal_digit, 3, 8) {
                Some(decoded) => Some(decoded),
                None => Some('o'),
            }
        }

        'u' => {
            // Short Unicode escape \uXXXX (exactly four hex digits)
            line.advance(); // move past 'x'
            match parse_numeric_escape(line, |c| c.is_ascii_hexdigit(), 4, 16) {
                Some(decoded) => Some(decoded),
                None => Some('u'),
            }
        }

        'U' => {
            // Short Unicode escape \UXXXXXXXX (exactly eight heax digits)
            line.advance(); // move past 'x'
            match parse_numeric_escape(line, |c| c.is_ascii_hexdigit(), 8, 16) {
                Some(decoded) => Some(decoded),
                None => Some('U'),
            }
        }

        'x' => {
            // Hexadecimal escape: \xnn
            line.advance(); // move past 'x'
            match parse_numeric_escape(line, |c| c.is_ascii_hexdigit(), 2, 16) {
                Some(decoded) => Some(decoded),
                None => Some('x'),
            }
        }
        _ => None,
    }
}

/// Parse a POSIX RE character class returning it as a string.
/// This functionality is needed to avoid terminating delimited
/// sequences when a delimiter appears within a character class.
/// While at it, handle escaped characters for the sake of consistency.
fn parse_character_class(
    lines: &ScriptLineProvider,
    line: &mut ScriptCharProvider,
) -> UResult<String> {
    let mut result = String::new();

    if line.eol() || line.current() != '[' {
        panic!("Invalid character class.");
    }

    line.advance();
    result.push('[');

    // Optional negation
    if !line.eol() && line.current() == '^' {
        result.push('^');
        line.advance();
    }

    // Optional leading ']' inside the class
    if !line.eol() && line.current() == ']' {
        result.push(']');
        line.advance();
    }

    while !line.eol() {
        let ch = line.current();

        if ch == ']' {
            result.push(']');
            line.advance();
            return Ok(result);
        }

        if ch == '[' {
            line.advance();
            if !line.eol() {
                let marker = line.current();
                // POSIX character class, collating symbol, or equivalence
                if marker == ':' || marker == '.' || marker == '=' {
                    line.advance();

                    result.push('[');
                    result.push(marker);

                    let mut inner = String::new();
                    let mut terminated = false;

                    while !line.eol() {
                        let c = line.current();
                        if c == marker {
                            line.advance();
                            if !line.eol() && line.current() == ']' {
                                line.advance();
                                result.push_str(&inner);
                                result.push(marker);
                                result.push(']');
                                terminated = true;
                                break;
                            } else {
                                // False alarm, just part of the inner name
                                inner.push(marker);
                            }
                        } else {
                            inner.push(c);
                            line.advance();
                        }
                    }

                    if !terminated {
                        return compilation_error(
                            lines,
                            line,
                            "Unterminated POSIX character class, equivalence or collating symbol",
                        );
                    }

                    continue;
                } else {
                    // Not a POSIX construct — treat as literal
                    result.push('[');
                    result.push(marker);
                    line.advance();
                    continue;
                }
            } else {
                result.push('[');
                continue;
            }
        }

        if ch == '\\' {
            // Handle escape sequence
            line.advance();
            if line.eol() {
                break;
            }
            match parse_char_escape(line) {
                Some(decoded) => result.push(decoded),
                None => {
                    result.push('\\');
                    result.push(line.current());
                    line.advance();
                }
            }
        } else {
            result.push(ch);
            line.advance();
        }
    }

    compilation_error(lines, line, "Unterminated bracket expression")
}

/// Scan and return the opening delimiter of a delimited string
/// Advances the line past the opening delimiter
fn scan_delimiter(lines: &ScriptLineProvider, line: &mut ScriptCharProvider) -> UResult<char> {
    // Sanity check
    if line.eol() {
        return compilation_error(lines, line, "unexpected end of line".to_string());
    }

    let delimiter = line.current();
    if delimiter == '\\' {
        return compilation_error(lines, line, "\\ cannot be used as a string delimiter");
    }
    line.advance(); // skip the opening delimiter
    Ok(delimiter)
}

/// Parse the regular expression delimited by the current line
/// character and return it as a string.
/// On return the line is on the closing delimiter.
pub fn parse_regex(lines: &ScriptLineProvider, line: &mut ScriptCharProvider) -> UResult<String> {
    let delimiter = scan_delimiter(lines, line)?;
    let mut result = String::new();

    while !line.eol() {
        match line.current() {
            '[' if delimiter != '[' => {
                let cc = parse_character_class(lines, line)?;
                result.push_str(&cc);
                continue;
            }
            '\\' => {
                line.advance();
                if line.eol() {
                    return compilation_error(lines, line, "unterminated regular expression");
                }
                if line.current() == delimiter {
                    // Push escaped delimiter
                    result.push(line.current());
                    line.advance();
                    continue;
                }
                match parse_char_escape(line) {
                    Some(decoded) => result.push(decoded),
                    None => {
                        // Pass through \<any> to RE engine for further treatment
                        result.push('\\');
                        result.push(line.current());
                        line.advance();
                    }
                }
                continue;
            }
            c if c == delimiter => return Ok(result),
            c => result.push(c),
        }
        line.advance();
    }
    compilation_error(lines, line, "unterminated regular expression")
}

/// Parse the transliteration string delimited by the current line
/// character and return it as a string.
/// On return the line is on the closing delimiter.
pub fn parse_transliteration(
    lines: &ScriptLineProvider,
    line: &mut ScriptCharProvider,
) -> UResult<String> {
    let delimiter = scan_delimiter(lines, line)?;
    let mut result = String::new();

    while !line.eol() {
        match line.current() {
            '\\' => {
                line.advance();
                if line.eol() {
                    return compilation_error(lines, line, "unterminated transliteration string");
                }
                if line.current() == delimiter || line.current() == '\\' {
                    // Push only the escaped character
                    result.push(line.current());
                    line.advance();
                    continue;
                }
                match parse_char_escape(line) {
                    Some(decoded) => result.push(decoded),
                    None => {
                        // Pass through \<any> to tr for literal use
                        result.push('\\');
                        result.push(line.current());
                        line.advance();
                    }
                }
                continue;
            }
            c if c == delimiter => return Ok(result),
            c => result.push(c),
        }
        line.advance();
    }
    compilation_error(lines, line, "unterminated transliteration string")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_providers(input: &str) -> (ScriptLineProvider, ScriptCharProvider) {
        let lines = ScriptLineProvider::new(vec![]); // Empty for tests
        let line = ScriptCharProvider::new(input);
        (lines, line)
    }

    // parse_numeric_escape
    #[test]
    fn test_compile_octal_escape() {
        let mut provider = ScriptCharProvider::new("141rest");
        let c = parse_numeric_escape(&mut provider, is_ascii_octal_digit, 3, 8);
        assert_eq!(c, Some('a'));
        assert_eq!(provider.current(), 'r'); // "141" was consumed
    }

    #[test]
    fn test_compile_octal_escape_eol() {
        let mut provider = ScriptCharProvider::new("141");
        let c = parse_numeric_escape(&mut provider, is_ascii_octal_digit, 3, 8);
        assert_eq!(c, Some('a'));
        assert!(provider.eol()); // "141" was consumed
    }

    #[test]
    fn test_compile_decimal_escape() {
        let mut provider = ScriptCharProvider::new("0659");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_digit(), 3, 10);
        assert_eq!(c, Some('A'));
        assert_eq!(provider.current(), '9'); // "65" was consumed
    }

    #[test]
    fn test_compile_decimal_invalid() {
        let mut provider = ScriptCharProvider::new("QR");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_digit(), 3, 10);
        assert_eq!(c, None);
        assert_eq!(provider.current(), 'Q');
    }

    #[test]
    fn test_compile_hex_escape() {
        let mut provider = ScriptCharProvider::new("3cZ");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 2, 16);
        assert_eq!(c, Some('<'));
        assert_eq!(provider.current(), 'Z'); // "41" was consumed
    }

    #[test]
    fn test_compile_hex_escape_truncated() {
        let mut provider = ScriptCharProvider::new("4G");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 2, 16);
        assert_eq!(c, Some('\u{4}')); // Only '4' is valid hex
        assert_eq!(provider.current(), 'G'); // "41" was consumed
    }

    #[test]
    fn test_compile_unicode_escape_short() {
        // U+2665 = '♥'
        let mut provider = ScriptCharProvider::new("26650");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 4, 16);
        assert_eq!(c, Some('♥'));
        assert_eq!(provider.current(), '0'); // "2665" was consumed
    }

    #[test]
    fn test_compile_unicode_escape_short_invalid() {
        let mut provider = ScriptCharProvider::new("123Q");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 4, 16);
        assert_eq!(c, None);
        assert_eq!(provider.current(), '1');
    }

    #[test]
    fn test_compile_unicode_escape_long_invalid() {
        // U+2665 = '♥'
        let mut provider = ScriptCharProvider::new("1234567Q");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 8, 16);
        assert_eq!(c, None);
        assert_eq!(provider.current(), '1');
    }

    #[test]
    fn test_compile_unicode_escape_long() {
        // U+1F600 = 😀
        let mut provider = ScriptCharProvider::new("0001F6009");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_hexdigit(), 8, 16);
        assert_eq!(c, Some('😀'));
        assert_eq!(provider.current(), '9'); // "0001F600" was consumed
    }

    #[test]
    fn test_no_valid_digits() {
        let mut provider = ScriptCharProvider::new("xyz");
        let c = parse_numeric_escape(&mut provider, |c| c.is_ascii_digit(), 3, 10);
        assert_eq!(c, None);
        assert_eq!(provider.current(), 'x'); // No advancement
    }

    // create_control_char
    #[test]
    fn test_lowercase_letter() {
        assert_eq!(create_control_char('z'), Some('\u{1a}')); // 0x5A ^ 0x40 = 0x1A
        assert_eq!(create_control_char('a'), Some('\u{01}')); // 0x41 ^ 0x40 = 0x01
    }

    #[test]
    fn test_uppercase_letter() {
        assert_eq!(create_control_char('Z'), Some('\u{1a}'));
        assert_eq!(create_control_char('A'), Some('\u{01}'));
    }

    #[test]
    fn test_symbol_characters() {
        assert_eq!(create_control_char('{'), Some(';')); // 0x7B ^ 0x40 = 0x3B
        assert_eq!(create_control_char(';'), Some('{')); // 0x3B ^ 0x40 = 0x7B
    }

    #[test]
    fn test_non_ascii_char() {
        // This will not match any transformation and may panic if it overflows
        // But the current function only handles ASCII-safe chars
        assert_eq!(create_control_char('é'), None); // outside ASCII
    }

    #[test]
    fn test_edge_ascii_values() {
        assert_eq!(create_control_char('@'), Some('\0')); // 0x40 ^ 0x40 = 0x00
        assert_eq!(create_control_char('\x7F'), Some('\x3F')); // 0x7F ^ 0x40 = 0x3F
    }

    // parse_char_escape
    fn escape_result_with_current(input: &str) -> (Option<char>, Option<char>) {
        let mut provider = ScriptCharProvider::new(input);
        let result = parse_char_escape(&mut provider);
        let current = if provider.eol() {
            None
        } else {
            Some(provider.current())
        };
        (result, current)
    }

    #[test]
    fn test_standard_escapes_eol() {
        assert_eq!(escape_result_with_current("a"), (Some('\x07'), None));
        assert_eq!(escape_result_with_current("f"), (Some('\x0c'), None));
        assert_eq!(escape_result_with_current("n"), (Some('\n'), None));
        assert_eq!(escape_result_with_current("r"), (Some('\r'), None));
        assert_eq!(escape_result_with_current("t"), (Some('\t'), None));
        assert_eq!(escape_result_with_current("v"), (Some('\x0b'), None));
    }

    #[test]
    fn test_standard_escapes_more() {
        assert_eq!(escape_result_with_current("a."), (Some('\x07'), Some('.')));
        assert_eq!(escape_result_with_current("f."), (Some('\x0c'), Some('.')));
        assert_eq!(escape_result_with_current("n."), (Some('\n'), Some('.')));
        assert_eq!(escape_result_with_current("r."), (Some('\r'), Some('.')));
        assert_eq!(escape_result_with_current("t."), (Some('\t'), Some('.')));
        assert_eq!(escape_result_with_current("v."), (Some('\x0b'), Some('.')));
    }

    #[test]
    fn test_escape_invalid() {
        assert_eq!(escape_result_with_current("zx"), (None, Some('z')));
    }

    #[test]
    fn test_control_escape_valid() {
        assert_eq!(escape_result_with_current("cZ"), (Some('\x1A'), None));
    }

    #[test]
    fn test_control_escape_invalid() {
        assert_eq!(escape_result_with_current("cé"), (Some('c'), Some('é')));
    }

    #[test]
    fn test_decimal_escape_valid() {
        assert_eq!(escape_result_with_current("d065r"), (Some('A'), Some('r')));
    }

    #[test]
    fn test_octal_escape_valid() {
        assert_eq!(escape_result_with_current("o141x"), (Some('a'), Some('x')));
    }

    #[test]
    fn test_hex_escape_valid() {
        assert_eq!(escape_result_with_current("x41;"), (Some('A'), Some(';')));
    }

    #[test]
    fn test_short_unicode_escape_valid() {
        assert_eq!(escape_result_with_current("u2665;"), (Some('♥'), Some(';')));
    }

    #[test]
    fn test_long_unicode_escape_valid() {
        assert_eq!(
            escape_result_with_current("U0001F600;"),
            (Some('😀'), Some(';'))
        );
    }

    #[test]
    fn test_decimal_escape_fallback() {
        assert_eq!(escape_result_with_current("d;."), (Some('d'), Some(';')));
    }

    #[test]
    fn test_octal_escape_fallback() {
        assert_eq!(escape_result_with_current("o9x"), (Some('o'), Some('9')));
    }

    #[test]
    fn test_hex_escape_fallback() {
        assert_eq!(escape_result_with_current("xyz"), (Some('x'), Some('y')));
    }

    #[test]
    fn test_unknown_escape() {
        assert_eq!(escape_result_with_current("q"), (None, Some('q')));
    }

    // parse_character_class
    fn char_provider_from(input: &str) -> ScriptCharProvider {
        ScriptCharProvider::new(input)
    }

    fn test_lines() -> ScriptLineProvider {
        ScriptLineProvider::with_active_state("test.sed", 3)
    }

    #[test]
    fn test_basic_character_class() {
        let mut line = char_provider_from("[qr]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[qr]");
    }

    #[test]
    fn test_negated_class() {
        let mut line = char_provider_from("[^abc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[^abc]");
    }

    #[test]
    fn test_leading_close_bracket() {
        let mut line = char_provider_from("[]abc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[]abc]");
    }

    #[test]
    fn test_leading_negated_close_bracket() {
        let mut line = char_provider_from("[^]abc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[^]abc]");
    }

    #[test]
    fn test_escaped_character_begin() {
        let mut line = char_provider_from("[\\nabc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[\nabc]");
    }

    #[test]
    fn test_escaped_character_middle() {
        let mut line = char_provider_from("[a\\nbc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[a\nbc]");
    }

    #[test]
    fn test_escaped_character_end() {
        let mut line = char_provider_from("[abc\\n]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[abc\n]");
    }

    #[test]
    fn test_escaped_delimiter() {
        let mut line = char_provider_from("[a\\]bc]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[a\\]bc]");
    }

    #[test]
    fn test_posix_class() {
        let mut line = char_provider_from("[[:digit:]]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[[:digit:]]");
    }

    #[test]
    fn test_equivalence_class() {
        let mut line = char_provider_from("[[=a=]]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[[=a=]]");
    }

    #[test]
    fn test_collating_symbol() {
        let mut line = char_provider_from("[[.ch.]]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[[.ch.]]");
    }

    #[test]
    fn test_unterminated_class_error() {
        let mut line = char_provider_from("[abc"); // missing closing ]
        let lines = test_lines();
        let err = parse_character_class(&lines, &mut line);
        assert!(err.is_err());
    }

    #[test]
    fn test_unterminated_posix_class_error() {
        let mut line = char_provider_from("[[:digit:]");
        let lines = test_lines();
        let err = parse_character_class(&lines, &mut line);
        assert!(err.is_err());
    }

    #[test]
    fn test_unterminated_escape_error() {
        let mut line = char_provider_from("[abc\\"); // missing closing ]
        let lines = test_lines();
        let err = parse_character_class(&lines, &mut line);
        assert!(err.is_err());
    }

    #[test]
    fn test_malformed_posix_like_pattern_treated_as_literal() {
        let mut line = char_provider_from("[[x]yz]");
        let lines = test_lines();
        let result = parse_character_class(&lines, &mut line).unwrap();
        assert_eq!(result, "[[x]");
    }

    // parse_regex
    #[test]
    fn test_simple_regex() {
        let (lines, mut line) = make_providers("/abc/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "abc");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_regex_with_escaped_delimiter() {
        let (lines, mut line) = make_providers("/ab\\/c/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "ab/c");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_regex_with_escape_sequence() {
        let (lines, mut line) = make_providers("/ab\\n/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "ab\n");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn errors_on_unterminated_regex() {
        let (lines, mut line) = make_providers("/unterminated");
        let err = parse_regex(&lines, &mut line).unwrap_err();
        assert!(err.to_string().contains("unterminated regular expression"));
    }

    #[test]
    fn errors_on_esc_at_re_eol() {
        let (lines, mut line) = make_providers("/foo\\");
        let err = parse_regex(&lines, &mut line).unwrap_err();
        assert!(err.to_string().contains("unterminated regular expression"));
    }

    #[test]
    fn errors_on_backslash_delimiter() {
        let (lines, mut line) = make_providers("\\bad");
        let err = parse_regex(&lines, &mut line).unwrap_err();
        assert!(err
            .to_string()
            .contains("\\ cannot be used as a string delimiter"));
    }

    #[test]
    fn test_regex_with_character_class() {
        let (lines, mut line) = make_providers("/[a-z]/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "[a-z]");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_regex_with_bracket_delimiter() {
        let (lines, mut line) = make_providers("[abc[");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "abc");
        assert_eq!(line.current(), '[');
    }

    #[test]
    fn test_bracket_regex_with_bracket_delimiter() {
        let (lines, mut line) = make_providers("[a\\[0-9]bc[");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "a[0-9]bc");
        assert_eq!(line.current(), '[');
    }

    #[test]
    fn test_regex_with_escaped_bracket_in_character_class() {
        let (lines, mut line) = make_providers("/[a\\]z]/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "[a\\]z]");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_regex_with_delimiter_inside_character_class() {
        let (lines, mut line) = make_providers("/[a/c]/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "[a/c]");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_regex_with_escaped_paren_and_backslash() {
        let (lines, mut line) = make_providers("/\\(\\\\/");
        let parsed = parse_regex(&lines, &mut line).unwrap();
        assert_eq!(parsed, "\\(\\\\");
        assert_eq!(line.current(), '/');
    }

    // parse_transliteration
    #[test]
    fn test_simple_transliteration() {
        let (lines, mut line) = make_providers("/abc/");
        let parsed = parse_transliteration(&lines, &mut line).unwrap();
        assert_eq!(parsed, "abc");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_transliteration_with_escaped_delimiter() {
        let (lines, mut line) = make_providers("/ab\\/c/");
        let parsed = parse_transliteration(&lines, &mut line).unwrap();
        assert_eq!(parsed, "ab/c");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_transliteration_with_escaped_backslash() {
        let (lines, mut line) = make_providers("/ab\\\\c/");
        let parsed = parse_transliteration(&lines, &mut line).unwrap();
        assert_eq!(parsed, "ab\\c");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn test_transliteration_with_escape_sequence() {
        let (lines, mut line) = make_providers("/ab\\n/");
        let parsed = parse_transliteration(&lines, &mut line).unwrap();
        assert_eq!(parsed, "ab\n");
        assert_eq!(line.current(), '/');
    }

    #[test]
    fn errors_on_unterminated_transliteration() {
        let (lines, mut line) = make_providers("/unterminated");
        let err = parse_transliteration(&lines, &mut line).unwrap_err();
        assert!(err
            .to_string()
            .contains("unterminated transliteration string"));
    }

    #[test]
    fn errors_on_esc_at_tr_eol() {
        let (lines, mut line) = make_providers("/foo\\");
        let err = parse_transliteration(&lines, &mut line).unwrap_err();
        assert!(err
            .to_string()
            .contains("unterminated transliteration string"));
    }
}
