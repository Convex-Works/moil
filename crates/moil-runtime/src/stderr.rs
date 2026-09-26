//! Keeping the tail of stderr for error reports (spec §8.3, §9).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// The most a reported tail may take (spec §10).
pub(crate) const MAX_TAIL_BYTES: usize = 8 << 10;
/// How much raw stderr is kept, so that the reported tail can start at a
/// line boundary and survive invalid UTF-8.
const KEPT_BYTES: usize = 2 * MAX_TAIL_BYTES;

/// The last bytes a process wrote to stderr.
#[derive(Debug, Default)]
pub(crate) struct Tail {
    bytes: VecDeque<u8>,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        let chunk = &chunk[chunk.len().saturating_sub(KEPT_BYTES)..];
        let overflow = (self.bytes.len() + chunk.len()).saturating_sub(KEPT_BYTES);
        self.bytes.drain(..overflow);
        self.bytes.extend(chunk);
    }

    /// The tail as text: at most 8 KiB, starting at the beginning of a line
    /// when there's more than one, without trailing whitespace.
    pub fn render(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim_end();
        if text.len() <= MAX_TAIL_BYTES {
            return text.to_owned();
        }
        let mut start = text.len() - MAX_TAIL_BYTES;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let tail = &text[start..];
        match tail.find('\n') {
            Some(newline) if newline + 1 < tail.len() => tail[newline + 1..].to_owned(),
            _ => tail.to_owned(),
        }
    }
}

/// Reads `stderr` to its end into `tail`, also copying it to this process's
/// stderr if `echo` is set.
pub(crate) async fn collect(
    mut stderr: impl AsyncRead + Unpin,
    echo: bool,
    tail: Arc<Mutex<Tail>>,
) {
    let mut buf = vec![0; 8 << 10];
    let mut own = tokio::io::stderr();
    loop {
        let n = match stderr.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(error) => {
                tracing::warn!(%error, "couldn't read a process's stderr");
                return;
            }
        };
        tail.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(&buf[..n]);
        if echo {
            let _ = own.write_all(&buf[..n]).await;
            let _ = own.flush().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail_of(chunks: &[&[u8]]) -> String {
        let mut tail = Tail::default();
        for chunk in chunks {
            tail.push(chunk);
        }
        tail.render()
    }

    #[test]
    fn short_output_is_kept_whole() {
        assert_eq!(
            tail_of(&[b"Traceback:\n", b"  boom\n"]),
            "Traceback:\n  boom"
        );
    }

    #[test]
    fn long_output_keeps_the_last_whole_lines() {
        let lines: Vec<String> = (0..2000).map(|i| format!("line {i:04}\n")).collect();
        let chunks: Vec<&[u8]> = lines.iter().map(|l| l.as_bytes()).collect();
        let tail = tail_of(&chunks);
        assert!(tail.len() <= MAX_TAIL_BYTES);
        assert!(
            tail.starts_with("line "),
            "starts at a line: {:?}",
            &tail[..20]
        );
        assert!(tail.ends_with("line 1999"));
    }

    #[test]
    fn one_huge_line_is_cut_on_a_character_boundary() {
        let line = "é".repeat(10_000);
        let tail = tail_of(&[line.as_bytes()]);
        assert!(tail.len() <= MAX_TAIL_BYTES && tail.len() > MAX_TAIL_BYTES - 4);
        assert!(tail.chars().all(|c| c == 'é'));
    }
}
