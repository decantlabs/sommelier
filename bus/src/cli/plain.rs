//! Text that came over the wire, made safe to print on a terminal.
//!
//! A hub, or a member whose message the hub passes on, may put control characters and escape
//! sequences into a string. On a terminal those move the cursor, clear the screen, set the title or
//! write to the clipboard. [`plain`] removes them. Newline and tab stay. Use it, or the macros `outln!`,
//! `out!` and `errln!` that call it, for every string that was not made by this program before it
//! goes to the terminal.

use std::borrow::Cow;

/// Like `println!` but the whole text goes through [`plain`] first.
#[macro_export]
macro_rules! outln {
    () => { ::std::println!() };
    ($($arg:tt)*) => {
        ::std::println!("{}", $crate::cli::plain::plain(&::std::format!($($arg)*)))
    };
}

/// Like `print!` but the whole text goes through [`plain`] first.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {
        ::std::print!("{}", $crate::cli::plain::plain(&::std::format!($($arg)*)))
    };
}

/// Like `eprintln!` but the whole text goes through [`plain`] first.
#[macro_export]
macro_rules! errln {
    () => { ::std::eprintln!() };
    ($($arg:tt)*) => {
        ::std::eprintln!("{}", $crate::cli::plain::plain(&::std::format!($($arg)*)))
    };
}

/// True for a character that must not reach a terminal as it is. Newline and tab are fine.
fn unsafe_char(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    c.is_control()
        || matches!(
            c,
            // Bidirectional overrides and isolates, zero width characters, line and paragraph
            // separators, the byte order mark.
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// The text without control characters, escape sequences and invisible direction marks. A whole
/// escape sequence is dropped, not only its first byte: CSI (`ESC [ ... final`), OSC and the string
/// forms (`ESC ] ... BEL` or `ESC \`), and the two byte forms. A lone C1 control character is dropped.
/// `\r` is dropped too, so a line cannot be overwritten. Newline and tab stay.
pub fn plain(s: &str) -> Cow<'_, str> {
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => skip_escape(&mut it),
            // C1 forms of the same introducers.
            '\u{9b}' => skip_csi(&mut it),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => skip_string(&mut it),
            c if unsafe_char(c) => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_escape(it: &mut Chars<'_>) {
    match it.peek().copied() {
        Some('[') => {
            it.next();
            skip_csi(it);
        }
        // OSC, DCS, SOS, PM and APC end with BEL or ESC backslash.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            it.next();
            skip_string(it);
        }
        // Intermediate bytes then one final byte, such as ESC ( B.
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            while it.next_if(|c| ('\u{20}'..='\u{2f}').contains(c)).is_some() {}
            it.next();
        }
        Some(_) => {
            it.next();
        }
        None => {}
    }
}

/// Parameter bytes, intermediate bytes, then one final byte `@` to `~`.
fn skip_csi(it: &mut Chars<'_>) {
    // A byte that is not part of a sequence stays in the iterator and is seen as text.
    while let Some(c) = it.next_if(|c| ('\u{20}'..='\u{7e}').contains(c)) {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

fn skip_string(it: &mut Chars<'_>) {
    while let Some(c) = it.next() {
        if c == '\u{7}' || c == '\u{9c}' {
            break;
        }
        if c == '\u{1b}' {
            it.next_if_eq(&'\\');
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn normal_text_is_borrowed_and_unchanged() {
        let s = "hello\tworld\nünï 日本";
        assert!(matches!(plain(s), std::borrow::Cow::Borrowed(_)));
        assert_eq!(plain(s), s);
    }

    #[test]
    fn escape_sequences_are_removed_whole() {
        assert_eq!(plain("a\u{1b}[2Jb"), "ab");
        assert_eq!(plain("a\u{1b}[31;1mred\u{1b}[0m"), "ared");
        assert_eq!(plain("x\u{1b}]0;title\u{7}y"), "xy");
        assert_eq!(plain("x\u{1b}]52;c;ZGF0YQ==\u{1b}\\y"), "xy");
        assert_eq!(plain("x\u{1b}(By"), "xy");
        assert_eq!(plain("x\u{9b}2Jy"), "xy");
        assert_eq!(plain("x\u{1b}Pq data\u{1b}\\y"), "xy");
    }

    #[test]
    fn controls_and_direction_marks_are_removed() {
        assert_eq!(plain("a\rb\u{8}c\u{7}d\u{0}e"), "abcde");
        assert_eq!(plain("a\u{202e}b\u{200b}c\u{feff}"), "abc");
        assert_eq!(plain("a\u{85}b\u{2028}c"), "abc");
    }

    #[test]
    fn a_cut_off_sequence_does_not_swallow_the_next_line() {
        assert_eq!(plain("a\u{1b}[\nnext"), "a\nnext");
        assert_eq!(plain("a\u{1b}"), "a");
    }
}
