//! Turning the script's stdout into events (spec §8.3).
//!
//! The runtime is lenient: a stray `print` or a chatty library never breaks
//! a job. Anything that isn't a well-formed event becomes a `log` event
//! instead, so the author can see what happened.

use std::sync::{Arc, Mutex};

use moil_proto::{JobEvent, LogLevel, ScriptEvent};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};

use crate::EventSink;

/// The longest stdout line the runtime reads; longer lines are dropped with
/// a warning (spec §10).
pub(crate) const MAX_LINE_BYTES: usize = 1 << 20;
/// How much of a line that isn't an event is forwarded as a log message.
const MAX_PLAIN_LINE_BYTES: usize = 4 << 10;
/// The most `data.payload` and `result.meta` may take when serialized
/// (spec §10).
pub(crate) const MAX_PAYLOAD_BYTES: usize = 256 << 10;
/// How deeply `data.payload` and `result.meta` may nest arrays and objects
/// (spec §10).
const MAX_PAYLOAD_DEPTH: usize = 64;
/// The longest message the runtime passes on, in events and failures.
/// Keeps every message to the service well under the 1 MiB limit of a
/// control message.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 << 10;
/// How much of an offending line a warning quotes.
const QUOTE_BYTES: usize = 200;

/// What the script reported besides the events forwarded as they came.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The first `result` accepted.
    pub result: Option<ScriptResult>,
    /// The first `error`.
    pub error: Option<ScriptError>,
    /// Why the runtime refused a `result` the script sent, if it did.
    pub refused_result: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptResult {
    pub files: Vec<String>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptError {
    pub message: String,
    pub retryable: bool,
}

/// Reads `stdout` to its end, forwarding events to `events` as they arrive
/// and recording `result` and `error` in `report`. The report is shared so
/// that it survives if the caller stops reading early.
pub(crate) async fn forward(
    stdout: impl AsyncRead + Unpin,
    events: EventSink,
    report: Arc<Mutex<Report>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut number = 0u64;
    loop {
        let read = match read_line(&mut reader, &mut line, MAX_LINE_BYTES).await {
            Ok(Some(read)) => read,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "couldn't read the script's stdout");
                return;
            }
        };
        number += 1;
        let event = {
            let mut report = report.lock().unwrap_or_else(|e| e.into_inner());
            match read {
                Read::Line => interpret(&line, number, &mut report),
                Read::TooLong => Some(warn(format!(
                    "dropped stdout line {number}: it is longer than the 1 MiB limit"
                ))),
            }
        };
        if let Some(event) = event {
            let _ = events.send(event);
        }
    }
}

enum Read {
    /// A whole line, without its newline, is in the buffer.
    Line,
    /// The line was longer than the limit and has been skipped.
    TooLong,
}

/// Reads one line into `buf`, keeping at most `max` bytes of it in memory.
/// A final line without a newline counts; `None` means end of input.
async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<Read>> {
    buf.clear();
    let mut too_long = false;
    let mut any = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(match (any, too_long) {
                (false, _) => None,
                (true, true) => Some(Read::TooLong),
                (true, false) => Some(Read::Line),
            });
        }
        any = true;
        let newline = chunk.iter().position(|&b| b == b'\n');
        let part = &chunk[..newline.unwrap_or(chunk.len())];
        if !too_long {
            if buf.len() + part.len() > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(part);
            }
        }
        let used = newline.map_or(chunk.len(), |i| i + 1);
        reader.consume(used);
        if newline.is_some() {
            return Ok(Some(if too_long { Read::TooLong } else { Read::Line }));
        }
    }
}

/// Interprets stdout line `number`: returns the event to forward, if any,
/// and records a `result` or `error` in `report`.
fn interpret(line: &[u8], number: u64, report: &mut Report) -> Option<JobEvent> {
    let text = String::from_utf8_lossy(line);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    if text.trim().is_empty() {
        return None;
    }
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
        return Some(JobEvent::log(
            LogLevel::Info,
            truncate(text, MAX_PLAIN_LINE_BYTES),
        ));
    };
    let ignored = |why: String| Some(warn(format!("ignored stdout line {number}: {why}")));

    match object.get("v") {
        None => {}
        Some(v) if v.as_u64() == Some(1) => {}
        Some(v) => return ignored(format!("event version {v} isn't supported (expected 1)")),
    }
    let kind = match object.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        _ => {
            return ignored(format!(
                "a JSON object without a \"type\": {}",
                truncate(text, QUOTE_BYTES)
            ));
        }
    };
    let event = match kind.as_str() {
        "progress" | "log" | "data" | "result" | "error" => parse(object),
        "phase" => return ignored("scripts can't send phase events".into()),
        _ => return ignored(format!("unknown event type {kind:?}")),
    };
    let event = match event {
        Ok(event) => event,
        Err(error) => return ignored(format!("invalid {kind} event: {error}")),
    };

    match event {
        ScriptEvent::Progress { fraction, message } => {
            if fraction.is_none() && message.is_none() {
                return ignored("a progress event needs a fraction or a message".into());
            }
            if let Some(fraction) = fraction
                && !(0.0..=1.0).contains(&fraction)
            {
                return ignored(format!(
                    "progress fraction {fraction} isn't between 0 and 1"
                ));
            }
            Some(JobEvent::Progress {
                fraction,
                message: message.map(|m| truncate(&m, MAX_MESSAGE_BYTES)),
            })
        }
        ScriptEvent::Log { level, message } => Some(JobEvent::Log {
            level,
            message: truncate(&message, MAX_MESSAGE_BYTES),
        }),
        ScriptEvent::Data { payload } => match beyond_limits(&payload) {
            Some(why) => ignored(format!("the data payload {why}")),
            None => Some(JobEvent::Data { payload }),
        },
        ScriptEvent::Result { files, meta } => {
            if report.error.is_some() {
                return ignored("a result after an error".into());
            }
            // A refused result was the first one, and the first one counts.
            if report.result.is_some() || report.refused_result.is_some() {
                return ignored("a second result".into());
            }
            if let Some(why) = meta.as_ref().and_then(beyond_limits) {
                let why = format!("the result's meta {why}");
                report.refused_result = Some(format!("the script's result was refused: {why}"));
                return ignored(why);
            }
            report.result = Some(ScriptResult { files, meta });
            None
        }
        ScriptEvent::Error { message, retryable } => {
            if report.error.is_some() {
                return ignored("a second error".into());
            }
            report.error = Some(ScriptError {
                message: truncate(&message, MAX_MESSAGE_BYTES),
                retryable,
            });
            None
        }
    }
}

fn parse(object: Map<String, Value>) -> serde_json::Result<ScriptEvent> {
    serde_json::from_value(Value::Object(object))
}

/// How `value` goes beyond the limits of arbitrary JSON the service
/// accepts (spec §10), completing "the payload …", if it does.
fn beyond_limits(value: &Value) -> Option<String> {
    if depth(value) > MAX_PAYLOAD_DEPTH {
        return Some("is nested more than 64 levels deep".into());
    }
    let size = serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
    (size > MAX_PAYLOAD_BYTES).then(|| format!("is {size} bytes, over the 256 KiB limit"))
}

/// How deeply `value` nests arrays and objects: 0 for a scalar, 1 for
/// `[1]`.
fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(fields) => 1 + fields.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn warn(message: String) -> JobEvent {
    JobEvent::log(LogLevel::Warn, message)
}

/// `text` cut to at most `max` bytes, on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8], max: usize) -> Vec<Option<String>> {
        let mut reader = BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        while let Some(read) = read_line(&mut reader, &mut buf, max).await.unwrap() {
            out.push(match read {
                Read::Line => Some(String::from_utf8(buf.clone()).unwrap()),
                Read::TooLong => None,
            });
        }
        out
    }

    #[tokio::test]
    async fn long_lines_are_skipped_without_losing_the_next_one() {
        // The reader's buffer is smaller than the lines, so lines arrive in
        // pieces, as they do from a pipe.
        assert_eq!(
            lines(b"short\n0123456789abc\nafter\nlast", 10).await,
            [
                Some("short".into()),
                None,
                Some("after".into()),
                Some("last".into())
            ]
        );
        assert_eq!(
            lines(b"0123456789\n", 10).await,
            [Some("0123456789".into())]
        );
        assert_eq!(lines(b"", 10).await, Vec::<Option<String>>::new());
    }

    #[test]
    fn depth_counts_containers() {
        use serde_json::json;
        assert_eq!(depth(&json!(1)), 0);
        assert_eq!(depth(&json!([])), 1);
        assert_eq!(depth(&json!([1, [2]])), 2);
        assert_eq!(depth(&json!({"a": {"b": []}, "c": 1})), 3);
    }

    #[test]
    fn truncation_respects_characters() {
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("héllo", 3), "hé");
        assert_eq!(truncate("hi", 10), "hi");
    }
}
