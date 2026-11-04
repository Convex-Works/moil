//! How the owner's commands print results (spec/machine.md §3.1).

use std::borrow::Cow;

use moil_agent::display::reveal;
use moil_agent::report::json_line;
use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::Value;

/// A result as `Label: value` lines, or, with `--json`, as one object
/// whose keys are the labels in lowercase with underscores for spaces:
/// `Open at login: true` is `{"open_at_login": true}`. Building both from
/// one list keeps them saying the same.
#[derive(Debug, Default)]
pub struct Labels(Vec<(&'static str, Value)>);

impl Labels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, label: &'static str, value: impl Into<Value>) -> Self {
        self.0.push((label, value.into()));
        self
    }

    /// Prints the lines, or the object as one line of JSON. Strings are
    /// shown with hidden characters revealed.
    pub fn print(&self, json: bool) {
        if json {
            println!("{}", json_line(self));
            return;
        }
        for (label, value) in &self.0 {
            println!("{label}: {}", text(value));
        }
    }
}

impl Serialize for Labels {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (label, value) in &self.0 {
            map.serialize_entry(&key(label), value)?;
        }
        map.end()
    }
}

/// The JSON key for `label`.
fn key(label: &str) -> String {
    label.to_lowercase().replace(' ', "_")
}

fn text(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(text) => reveal(text),
        other => Cow::Owned(other.to_string()),
    }
}

/// Prints `value` as one line of JSON.
pub fn print_json(value: &impl Serialize) {
    println!("{}", json_line(value));
}

/// How a report's enum reads: as in its JSON, such as `running`.
pub fn word(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(word)) => word,
        _ => "unknown".into(),
    }
}

/// Prints aligned columns. Only the last column may contain spaces.
pub fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        println!("{}", out.trim_end());
    };
    line(headers.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_keys_in_lowercase_with_underscores() {
        let labels = Labels::new()
            .with("Open at login", true)
            .with("URL", "https://tide.example.com/pair\u{202e}");
        assert_eq!(
            json_line(&labels),
            r#"{"open_at_login":true,"url":"https://tide.example.com/pair\u202e"}"#
        );
    }
}
