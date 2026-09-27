//! Showing the owner what services and bundles say, and sizes for people.
//!
//! Service names, bundle files, package names, URLs and error messages all
//! come from a service. Any of them could carry characters that make text
//! read differently from what it is: a carriage return or a terminal escape
//! that overwrites what came before, a bidirectional override that reorders
//! it ("Trojan Source"), or an invisible character. Everything a service or
//! bundle controls is shown through [`reveal`], by the command line and the
//! app alike (spec §5.5).

use std::borrow::Cow;
use std::fmt::Write as _;

/// `text` with every [hidden](is_hidden) character made visible: C0
/// controls as control pictures (`␍`, `␛`), DEL as `␡`, and everything else
/// as `\u{202e}`. Tabs and line feeds stay as they are.
pub fn reveal(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_hidden) {
        return Cow::Borrowed(text);
    }
    let mut shown = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            c if !is_hidden(c) => shown.push(c),
            '\0'..='\x1f' => {
                shown.push(char::from_u32(0x2400 + c as u32).expect("control pictures exist"));
            }
            '\x7f' => shown.push('\u{2421}'),
            c => {
                let _ = write!(shown, "\\u{{{:04x}}}", c as u32);
            }
        }
    }
    Cow::Owned(shown)
}

/// Whether `c` could make text read differently from what it is (spec
/// §5.5): a control character other than tab and line feed, a
/// bidirectional formatting character, or an invisible or
/// default-ignorable one.
pub fn is_hidden(c: char) -> bool {
    hidden_name(c).is_some()
}

/// What a [hidden](is_hidden) character is, for people: `RIGHT-TO-LEFT
/// OVERRIDE`. `None` for every other character.
pub fn hidden_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\t' | '\n' => return None,
        '\r' => "CARRIAGE RETURN",
        '\u{1b}' => "ESCAPE",
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => "CONTROL CHARACTER",
        '\u{ad}' => "SOFT HYPHEN",
        '\u{34f}' => "COMBINING GRAPHEME JOINER",
        '\u{61c}' => "ARABIC LETTER MARK",
        '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}' => "HANGUL FILLER",
        '\u{17b4}' | '\u{17b5}' => "KHMER INHERENT VOWEL",
        '\u{180b}'..='\u{180d}' | '\u{180f}' => "MONGOLIAN FREE VARIATION SELECTOR",
        '\u{180e}' => "MONGOLIAN VOWEL SEPARATOR",
        '\u{200b}' => "ZERO WIDTH SPACE",
        '\u{200c}' => "ZERO WIDTH NON-JOINER",
        '\u{200d}' => "ZERO WIDTH JOINER",
        '\u{200e}' => "LEFT-TO-RIGHT MARK",
        '\u{200f}' => "RIGHT-TO-LEFT MARK",
        '\u{2028}' => "LINE SEPARATOR",
        '\u{2029}' => "PARAGRAPH SEPARATOR",
        '\u{202a}' => "LEFT-TO-RIGHT EMBEDDING",
        '\u{202b}' => "RIGHT-TO-LEFT EMBEDDING",
        '\u{202c}' => "POP DIRECTIONAL FORMATTING",
        '\u{202d}' => "LEFT-TO-RIGHT OVERRIDE",
        '\u{202e}' => "RIGHT-TO-LEFT OVERRIDE",
        '\u{2060}' => "WORD JOINER",
        '\u{2061}'..='\u{2064}' => "INVISIBLE OPERATOR",
        '\u{2066}' => "LEFT-TO-RIGHT ISOLATE",
        '\u{2067}' => "RIGHT-TO-LEFT ISOLATE",
        '\u{2068}' => "FIRST STRONG ISOLATE",
        '\u{2069}' => "POP DIRECTIONAL ISOLATE",
        '\u{206a}'..='\u{206f}' => "DEPRECATED FORMATTING CHARACTER",
        '\u{2800}' => "BRAILLE PATTERN BLANK",
        '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}' => "VARIATION SELECTOR",
        '\u{feff}' => "ZERO WIDTH NO-BREAK SPACE",
        '\u{fff9}'..='\u{fffb}' => "INTERLINEAR ANNOTATION",
        '\u{13430}'..='\u{1343f}' => "EGYPTIAN HIEROGLYPH FORMAT CONTROL",
        '\u{1bca0}'..='\u{1bca3}' => "SHORTHAND FORMAT CONTROL",
        '\u{1d173}'..='\u{1d17a}' => "MUSICAL SYMBOL FORMATTING",
        '\u{e0000}'..='\u{e007f}' => "TAG CHARACTER",
        // Reserved to be invisible whatever they become.
        '\u{2065}'
        | '\u{fff0}'..='\u{fff8}'
        | '\u{e0080}'..='\u{e00ff}'
        | '\u{e01f0}'..='\u{e0fff}' => "UNASSIGNED IGNORABLE CHARACTER",
        _ => return None,
    })
}

/// A size in decimal units, as Finder shows them: `75.5 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    // Rounded to one decimal, 999.95 would read `1000.0`.
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// An amount of memory in binary units, as manifests usually mean it:
/// `8 GiB`, `1.5 GiB`.
pub fn memory(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let rounded = format!("{value:.1}");
    let rounded = rounded.strip_suffix(".0").unwrap_or(&rounded);
    format!("{rounded} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default_Ignorable_Code_Point, from Unicode's DerivedCoreProperties.
    const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
        (0x00ad, 0x00ad),
        (0x034f, 0x034f),
        (0x061c, 0x061c),
        (0x115f, 0x1160),
        (0x17b4, 0x17b5),
        (0x180b, 0x180f),
        (0x200b, 0x200f),
        (0x202a, 0x202e),
        (0x2060, 0x206f),
        (0x3164, 0x3164),
        (0xfe00, 0xfe0f),
        (0xfeff, 0xfeff),
        (0xffa0, 0xffa0),
        (0xfff0, 0xfff8),
        (0x1bca0, 0x1bca3),
        (0x1d173, 0x1d17a),
        (0xe0000, 0xe0fff),
    ];

    /// The bidirectional formatting characters spec §5.3 lists.
    const BIDI: &[(u32, u32)] = &[
        (0x061c, 0x061c),
        (0x200e, 0x200f),
        (0x202a, 0x202e),
        (0x2066, 0x2069),
    ];

    fn chars(ranges: &[(u32, u32)]) -> impl Iterator<Item = char> + '_ {
        ranges
            .iter()
            .flat_map(|&(first, last)| (first..=last).filter_map(char::from_u32))
    }

    #[test]
    fn everything_spec_5_5_names_is_hidden() {
        let controls = ('\0'..='\u{9f}').filter(|c| c.is_control() && !matches!(c, '\t' | '\n'));
        for c in controls
            .chain(chars(BIDI))
            .chain(chars(DEFAULT_IGNORABLE))
            .chain(['\u{2028}', '\u{2029}'])
        {
            assert!(is_hidden(c), "U+{:04X}", c as u32);
        }
        for c in ['\t', '\n', ' ', 'a', 'é', '日', '\u{a0}', '\u{2003}', '✓'] {
            assert!(!is_hidden(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn reveal_changes_exactly_the_hidden_characters_into_visible_ones() {
        let mut buf = [0; 4];
        for c in char::MIN..=char::MAX {
            let text = c.encode_utf8(&mut buf);
            match reveal(text) {
                Cow::Borrowed(_) => assert!(!is_hidden(c), "U+{:04X}", c as u32),
                Cow::Owned(shown) => {
                    assert!(is_hidden(c), "U+{:04X}", c as u32);
                    assert!(!shown.chars().any(is_hidden), "{shown}");
                }
            }
        }
    }

    #[test]
    fn ordinary_code_is_shown_as_is() {
        let code = "def f(x):\n\treturn x  # déjà vu, 日本語\n";
        assert!(matches!(reveal(code), Cow::Borrowed(s) if s == code));
    }

    #[test]
    fn characters_that_hide_code_are_made_visible() {
        // A carriage return would let the second half overwrite the first.
        assert_eq!(reveal("rm -rf ~\rprint('hi')"), "rm -rf ~␍print('hi')");
        // An escape sequence could clear the screen and draw anything.
        assert_eq!(reveal("1.0\x1b[2J\x1b[H"), "1.0␛[2J␛[H");
        assert_eq!(reveal("x\x7f\u{85}"), "x␡\\u{0085}");
        // "Trojan Source": the override makes the check read backwards.
        assert_eq!(
            reveal("if access_level != \"user\u{202e} \u{2066}// admin\u{2069} \u{2066}\""),
            "if access_level != \"user\\u{202e} \\u{2066}// admin\\u{2069} \\u{2066}\""
        );
        assert_eq!(
            reveal("pass\u{200b}word\u{feff}\u{3164}\u{e0041}"),
            "pass\\u{200b}word\\u{feff}\\u{3164}\\u{e0041}"
        );
    }

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(size(5), "5 bytes");
        assert_eq!(size(2370), "2.4 KB");
        assert_eq!(size(75_538_270), "75.5 MB");
        assert_eq!(size(999_999), "1.0 MB", "never `1000.0 KB`");
        assert_eq!(size(8_589_934_592), "8.6 GB");
        assert_eq!(size(u64::MAX), "18446.7 PB");
    }

    #[test]
    fn memory_uses_binary_units_without_needless_decimals() {
        assert_eq!(memory(8 << 30), "8 GiB");
        assert_eq!(memory(6_442_450_944), "6 GiB");
        assert_eq!(memory(1536 << 20), "1.5 GiB");
        assert_eq!(memory(512 << 20), "512 MiB");
        assert_eq!(memory((1 << 30) - 1), "1 GiB", "never `1024 MiB`");
        assert_eq!(memory(100), "100 bytes");
    }
}
