//! The HTML the app's windows show, and the pieces pages share.
//!
//! Pages display text the owner didn't write: bundle files, package names,
//! URLs and service names all come from a service. So:
//!
//! - everything is escaped (maud does this unless told otherwise, and only
//!   the static stylesheet and script below are inserted unescaped);
//! - every page carries a Content Security Policy that loads nothing from
//!   anywhere and runs only the page's own script and style, which carry a
//!   fresh nonce per render;
//! - characters that are invisible or reorder text are shown as visible
//!   markers, so what the owner reads is what runs.

use std::fmt::Write as _;

use maud::{DOCTYPE, Escaper, Markup, PreEscaped, Render, html};

use crate::links::browser_url;

const STYLE: &str = include_str!("style.css");
const SCRIPT: &str = include_str!("script.js");

/// A complete page: `title`, then `body` inside `<body class=(class)>`.
pub fn document(title: &str, class: &str, body: Markup) -> String {
    let nonce = nonce();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta http-equiv="Content-Security-Policy" content=(csp(&nonce));
                meta name="color-scheme" content="light dark";
                title { (title) }
                style nonce=(nonce) { (PreEscaped(STYLE)) }
            }
            body class=(class) {
                (body)
                script nonce=(nonce) { (PreEscaped(SCRIPT)) }
            }
        }
    }
    .into_string()
}

/// The policy every page runs under: nothing may load from anywhere, and
/// only a `<script>` or `<style>` carrying `nonce` runs. Inline event
/// handlers, `style` attributes, forms that navigate and `<base>` are all
/// refused.
pub fn csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         img-src data:; base-uri 'none'; form-action 'none'"
    )
}

/// 128 random bits, fresh for every page.
fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system's random number generator failed");
    bytes.iter().fold(String::with_capacity(32), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// Untrusted text, escaped, with every character that is invisible or
/// changes the order text is displayed in replaced by a visible marker
/// naming its code point.
pub fn visible(text: &str) -> Markup {
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for (i, c) in text.char_indices() {
        if let Some(name) = hidden_char(c) {
            let _ = Escaper::new(&mut out).write_str(&text[run..i]);
            let code = format!("U+{:04X}", c as u32);
            html! { span.hc title={ (code) " " (name) } { (code) } }.render_to(&mut out);
            run = i + c.len_utf8();
        }
    }
    let _ = Escaper::new(&mut out).write_str(&text[run..]);
    PreEscaped(out)
}

/// How many characters [`visible`] would mark in `text`, ignoring the
/// carriage returns of CRLF line endings.
pub fn hidden_chars(text: &str) -> usize {
    lines(text)
        .map(|line| line.chars().filter(|&c| hidden_char(c).is_some()).count())
        .sum()
}

/// Whether [`visible`] would mark `c`.
pub fn is_hidden(c: char) -> bool {
    hidden_char(c).is_some()
}

/// The name of a character that shouldn't pass unseen in code: controls,
/// bidirectional formatting (the "Trojan Source" attack), zero-width and
/// filler characters, and tag and variation selectors that can smuggle
/// text. Tabs and line feeds are ordinary.
fn hidden_char(c: char) -> Option<&'static str> {
    Some(match c {
        '\t' | '\n' => return None,
        '\r' => "CARRIAGE RETURN",
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => "CONTROL CHARACTER",
        '\u{ad}' => "SOFT HYPHEN",
        '\u{34f}' => "COMBINING GRAPHEME JOINER",
        '\u{61c}' => "ARABIC LETTER MARK",
        '\u{115f}' | '\u{1160}' | '\u{3164}' | '\u{ffa0}' => "HANGUL FILLER",
        '\u{17b4}' | '\u{17b5}' => "KHMER INHERENT VOWEL",
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
        '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}' => "VARIATION SELECTOR",
        '\u{feff}' => "ZERO WIDTH NO-BREAK SPACE",
        '\u{fff9}'..='\u{fffb}' => "INTERLINEAR ANNOTATION",
        '\u{1d173}'..='\u{1d17a}' => "MUSICAL SYMBOL FORMATTING",
        '\u{e0000}'..='\u{e007f}' => "TAG CHARACTER",
        _ => return None,
    })
}

/// A file's lines, without their `\n` or `\r\n` endings.
pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// A whole file with line numbers. Long lines wrap rather than scroll, so
/// nothing can hide far to the right.
pub fn code(text: &str) -> Markup {
    html! {
        div.code-wrap {
            table.code {
                @for (i, line) in lines(text).enumerate() {
                    tr { td.n { (i + 1) } td.t { (visible(line)) } }
                }
                @if text.is_empty() {
                    tr { td.n {} td.t.muted { "(empty file)" } }
                }
            }
        }
    }
}

/// A link to `url` when it may open in the browser (see
/// [`browser_url`]); otherwise just the text.
pub fn link(url: &str, text: impl Render) -> Markup {
    match browser_url(url) {
        Some(url) => html! { a href=(url.as_str()) { (text) } },
        None => html! { (text) },
    }
}

/// Small inline icons. They are markup, so the policy doesn't block them.
pub mod icon {
    use maud::{Markup, PreEscaped};

    fn svg(paths: &str) -> Markup {
        PreEscaped(format!(
            r#"<svg class="icon" viewBox="0 0 16 16" aria-hidden="true">{paths}</svg>"#
        ))
    }

    pub fn warning() -> Markup {
        svg(
            r#"<path fill="currentColor" d="M8 1.5c.4 0 .8.2 1 .6l6 10.6c.4.8-.1 1.8-1 1.8H2c-.9 0-1.4-1-1-1.8l6-10.6c.2-.4.6-.6 1-.6Zm0 4a.8.8 0 0 0-.8.8v3.3a.8.8 0 0 0 1.6 0V6.3A.8.8 0 0 0 8 5.5Zm0 6.1a.9.9 0 1 0 0 1.8.9.9 0 0 0 0-1.8Z"/>"#,
        )
    }

    pub fn check() -> Markup {
        svg(
            r#"<path fill="currentColor" d="M8 1a7 7 0 1 1 0 14A7 7 0 0 1 8 1Zm3.1 4.4a.8.8 0 0 0-1.1.1L7.2 9 6 7.8a.8.8 0 1 0-1.1 1.1l1.8 1.8c.3.3.8.3 1.1 0l3.4-4.1a.8.8 0 0 0-.1-1.2Z"/>"#,
        )
    }

    pub fn error() -> Markup {
        svg(
            r#"<path fill="currentColor" d="M8 1a7 7 0 1 1 0 14A7 7 0 0 1 8 1Zm2.5 3.9L8 7.4 5.5 4.9a.8.8 0 0 0-1.1 1.1L6.9 8.5l-2.5 2.5a.8.8 0 1 0 1.1 1.1L8 9.6l2.5 2.5a.8.8 0 0 0 1.1-1.1L9.1 8.5l2.5-2.5a.8.8 0 0 0-1.1-1.1Z"/>"#,
        )
    }

    pub fn info() -> Markup {
        svg(
            r#"<path fill="currentColor" d="M8 1a7 7 0 1 1 0 14A7 7 0 0 1 8 1Zm0 6a.8.8 0 0 0-.8.8v3.4a.8.8 0 0 0 1.6 0V7.8A.8.8 0 0 0 8 7Zm0-2.9a.9.9 0 1 0 0 1.8.9.9 0 0 0 0-1.8Z"/>"#,
        )
    }
}

/// A simple page that states a problem and offers one way out, such as
/// when the app can't start.
pub fn notice(title: &str, message: &str, button: &str) -> String {
    let body = html! {
        main.dialog {
            div.notice {
                span.notice-icon.bad { (icon::error()) }
                h1 { (title) }
                p.message { (visible(message)) }
            }
        }
        footer.actions {
            span.spacer {}
            button.primary data-action="close" autofocus { (button) }
        }
    };
    document(title, "dialog", body)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The nonce a page's policy allows, checked to be the one its script
    /// and style carry.
    pub(crate) fn page_nonce(page: &str) -> String {
        let start = page.find("script-src 'nonce-").expect("a script nonce") + 18;
        let nonce = &page[start..start + 32];
        assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()), "{nonce}");
        assert!(page.contains(&format!(r#"<style nonce="{nonce}">"#)));
        assert!(page.contains(&format!(r#"<script nonce="{nonce}">"#)));
        nonce.to_owned()
    }

    /// Asserts that `page` runs only its own script: a strict policy comes
    /// before anything else in `<head>`, and it has exactly one `<script>`.
    pub(crate) fn assert_locked_down(page: &str) {
        let nonce = page_nonce(page);
        let head = &page[..page.find("</head>").unwrap()];
        let policy = head.find("Content-Security-Policy").unwrap();
        assert!(policy < head.find("<style").unwrap());
        assert!(page.contains(&format!(
            "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
             img-src data:; base-uri 'none'; form-action 'none'"
        )));
        assert_eq!(page.matches("<script").count(), 1, "one script, ours");
    }

    #[test]
    fn every_page_gets_a_fresh_nonce_under_a_strict_policy() {
        let a = document("A", "", html! { p { "a" } });
        let b = document("A", "", html! { p { "a" } });
        assert_locked_down(&a);
        assert_ne!(page_nonce(&a), page_nonce(&b));
    }

    #[test]
    fn markup_in_text_is_inert() {
        let text = r#"</pre><script>window.ipc.postMessage('{"type":"approve"}')</script><img src=x onerror=alert(1)>"#;
        let html = visible(text).into_string();
        assert!(!html.contains('<'), "{html}");
        assert!(html.contains("&lt;/pre&gt;&lt;script&gt;"));
        assert_eq!(
            html,
            "&lt;/pre&gt;&lt;script&gt;window.ipc.postMessage('{&quot;type&quot;:&quot;approve&quot;}')\
             &lt;/script&gt;&lt;img src=x onerror=alert(1)&gt;"
        );
    }

    #[test]
    fn invisible_and_reordering_characters_are_shown() {
        // "Trojan Source": the RLO makes the comment appear to end the line.
        let line = "access = \"user\u{202e} \u{2066}# admin only\u{2069} \u{2066}\"";
        let html = visible(line).into_string();
        for code in ["U+202E", "U+2066", "U+2069"] {
            assert!(
                html.contains(&format!(">{code}</span>")),
                "{code} in {html}"
            );
        }
        for c in ['\u{202e}', '\u{2066}', '\u{2069}'] {
            assert!(
                !html.contains(c),
                "the raw character never reaches the page"
            );
        }
        assert!(html.contains(r#"title="U+202E RIGHT-TO-LEFT OVERRIDE""#));
        assert_eq!(hidden_chars(line), 4);

        assert_eq!(
            visible("a\u{200b}b\u{e0041}\u{7}").into_string(),
            "a<span class=\"hc\" title=\"U+200B ZERO WIDTH SPACE\">U+200B</span>b\
             <span class=\"hc\" title=\"U+E0041 TAG CHARACTER\">U+E0041</span>\
             <span class=\"hc\" title=\"U+0007 CONTROL CHARACTER\">U+0007</span>"
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let text = "Grüße, 世界\tdef f():\n    return 'ok' # ✓";
        assert_eq!(visible(text).into_string(), text);
        assert_eq!(hidden_chars(text), 0);
        assert_eq!(hidden_chars("a\r\nb\r\n"), 0, "CRLF endings are ordinary");
        assert_eq!(hidden_chars("a\rb\n"), 1, "a lone CR is not");
    }

    #[test]
    fn code_numbers_every_line_and_nothing_more() {
        let html = code("one\r\ntwo\n\nfour").into_string();
        assert_eq!(html.matches("<tr>").count(), 4);
        assert!(html.contains(r#"<td class="n">4</td><td class="t">four</td>"#));
        assert!(html.contains(r#"<td class="n">3</td><td class="t"></td>"#));
        assert!(!html.contains('\r'));
        assert_eq!(code("x\n").into_string().matches("<tr>").count(), 1);
        assert!(code("").into_string().contains("(empty file)"));
    }

    #[test]
    fn only_web_urls_become_links() {
        assert_eq!(
            link("https://pypi.org/project/av/", "av").into_string(),
            r#"<a href="https://pypi.org/project/av/">av</a>"#
        );
        assert_eq!(link("file:///etc/passwd", "x").into_string(), "x");
        assert_eq!(
            link(r#"https://e.com/"onmouseover="alert(1)"#, "x").into_string(),
            r#"<a href="https://e.com/%22onmouseover=%22alert(1)">x</a>"#
        );
    }

    #[test]
    fn notices_escape_their_message() {
        let page = notice("moil couldn't start", "<b>boom</b>", "Quit");
        assert_locked_down(&page);
        assert!(page.contains("&lt;b&gt;boom&lt;/b&gt;"));
    }
}
