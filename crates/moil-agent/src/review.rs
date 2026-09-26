//! What the owner looks at before approving a bundle.

use std::borrow::Cow;
use std::fmt::Write as _;

use moil_bundle::{Bundle, FileDiff};

use crate::PairedService;

/// A bundle fetched from a service and verified against the hash the owner
/// is asked to approve, with the version it would replace.
#[derive(Debug, Clone)]
pub struct Review {
    pub service: PairedService,
    pub bundle: Bundle,
    /// The most recently approved bundle with the same name from the same
    /// service, if any.
    pub previous: Option<Bundle>,
    /// Whether the owner already approved this exact bundle.
    pub approved: bool,
}

impl Review {
    /// Recomputes the bundle hash from the three files (spec §5.2), for
    /// owners who want to check it with standard tools.
    pub const VERIFY_COMMAND: &str =
        "shasum -a 256 job.py job.py.lock manifest.json | shasum -a 256";

    /// How each file changed since `previous`.
    pub fn diff(&self) -> Option<Vec<FileDiff>> {
        self.previous
            .as_ref()
            .map(|previous| moil_bundle::diff(previous, &self.bundle))
    }
}

/// `text` with every character that could hide code from someone reading
/// it made visible: control characters (a carriage return can overwrite a
/// line on a terminal, escape sequences can hide text), bidirectional
/// overrides that reorder what's shown ("Trojan Source"), and invisible
/// characters. Tabs and newlines stay as they are. Show bundle files
/// through this.
pub fn reveal(text: &str) -> Cow<'_, str> {
    if !text.chars().any(hides_something) {
        return Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            c if !hides_something(c) => shown.push(c),
            // Control pictures: ␀ … ␟ for C0, ␡ for DEL.
            '\0'..='\x1f' => shown.push(char::from_u32(0x2400 + c as u32).unwrap_or('�')),
            '\x7f' => shown.push('\u{2421}'),
            c => {
                let _ = write!(shown, "\\u{{{:04x}}}", c as u32);
            }
        }
    }
    Cow::Owned(shown)
}

fn hides_something(c: char) -> bool {
    matches!(c,
        '\0'..='\x08' | '\x0b'..='\x1f' | '\x7f'..='\u{9f}'
        // Soft hyphen, Arabic letter mark, zero-width and direction marks.
        | '\u{ad}' | '\u{61c}' | '\u{200b}'..='\u{200f}'
        // Bidirectional embeddings, overrides and isolates.
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}'
        | '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_code_is_shown_as_is() {
        let code = "def f(x):\n\treturn x  # déjà vu, 日本語\n";
        assert!(matches!(reveal(code), Cow::Borrowed(s) if s == code));
    }

    #[test]
    fn characters_that_hide_code_are_made_visible() {
        // A carriage return would let the second half overwrite the first.
        assert_eq!(reveal("rm -rf ~\rprint('hi')"), "rm -rf ~␍print('hi')");
        assert_eq!(reveal("a\x1b[8mhidden\x1b[0m"), "a␛[8mhidden␛[0m");
        assert_eq!(reveal("x\x7f\u{85}"), "x␡\\u{0085}");
        // "Trojan Source": the override makes the check read backwards.
        assert_eq!(
            reveal("if access_level != \"user\u{202e} \u{2066}// admin\u{2069} \u{2066}\""),
            "if access_level != \"user\\u{202e} \\u{2066}// admin\\u{2069} \\u{2066}\""
        );
        assert_eq!(
            reveal("pass\u{200b}word\u{feff}"),
            "pass\\u{200b}word\\u{feff}"
        );
    }
}
