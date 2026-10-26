//! Where the CLI's log records go: stderr, unless something holding the
//! terminal has asked for them.
//!
//! `run-new`'s screen draws on the terminal for as long as it is up, and a
//! record written to stderr meanwhile -- a retry the library warns of, say --
//! would land on top of what it drew. So the screen diverts records to itself
//! while it is up. With nothing diverted, a record goes to stderr exactly as it
//! always has, for every command.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::writer::EitherWriter;

/// One record as the subscriber formatted it, without its ANSI styling or
/// its line ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) level: Level,
    pub(crate) text: String,
}

type Sink = Arc<dyn Fn(Record) + Send + Sync>;

/// The subscriber's writer: stderr, or whatever a divert installed.
#[derive(Clone, Default)]
pub(crate) struct LogWriter(Arc<Mutex<Option<Sink>>>);

impl LogWriter {
    /// The one the process's subscriber writes through.
    pub(crate) fn global() -> &'static LogWriter {
        static GLOBAL: OnceLock<LogWriter> = OnceLock::new();
        GLOBAL.get_or_init(LogWriter::default)
    }

    /// Send every record to `sink` until the returned guard is dropped.
    pub(crate) fn divert(&self, sink: impl Fn(Record) + Send + Sync + 'static) -> Diverted {
        let before = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Arc::new(sink));
        Diverted(self.clone(), before)
    }

    fn writer(&self, level: Level) -> EitherWriter<io::Stderr, Collector> {
        // Cloned under the lock and called after it is released, so a sink
        // that logs does not take it again.
        let sink = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match sink {
            Some(sink) => EitherWriter::B(Collector {
                sink,
                level,
                text: Vec::new(),
            }),
            None => EitherWriter::A(io::stderr()),
        }
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = EitherWriter<io::Stderr, Collector>;

    fn make_writer(&'a self) -> Self::Writer {
        self.writer(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        self.writer(*meta.level())
    }
}

/// Records go back to where they went before the divert -- stderr, or an
/// earlier divert's sink -- once this is dropped.
pub(crate) struct Diverted(LogWriter, Option<Sink>);

impl Drop for Diverted {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap_or_else(PoisonError::into_inner) = self.1.take();
    }
}

/// Where one diverted record is written. The subscriber makes a writer for
/// each record and writes the record whole, so it is handed over as the writer
/// is dropped.
pub(crate) struct Collector {
    sink: Sink,
    level: Level,
    text: Vec<u8>,
}

impl Write for Collector {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.text.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        let mut text = strip_escapes(&String::from_utf8_lossy(&self.text));
        text.truncate(text.trim_end().len());
        (self.sink)(Record {
            level: self.level,
            text,
        });
    }
}

/// `text` without its terminal escape sequences: each CSI sequence -- the
/// ANSI styling the subscriber adds, say -- whole, and any other escape as its
/// ESC and the one character after it.
pub(crate) fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // A CSI's parameters and intermediates run until a final byte, `@` to
        // `~`; any other escape is the one character after it.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(logs: &LogWriter) -> (Diverted, Arc<Mutex<Vec<Record>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let diverted = logs.divert(move |record| sink.lock().expect("unpoisoned").push(record));
        (diverted, seen)
    }

    fn subscriber(logs: &LogWriter) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(true)
            .with_max_level(Level::TRACE)
            .finish()
    }

    /// A diverted record arrives whole and with its level, its styling and
    /// line ending gone.
    #[test]
    fn a_diverted_record_arrives_without_its_styling() {
        let logs = LogWriter::default();
        let (_diverted, seen) = collect(&logs);
        tracing::subscriber::with_default(subscriber(&logs), || {
            tracing::warn!(target: "outrig::agent", "model a failed; trying b");
            tracing::error!("broken");
        });
        let seen = seen.lock().expect("unpoisoned");
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].level, Level::WARN);
        assert!(seen[0].text.contains("WARN"), "{:?}", seen[0].text);
        assert!(
            seen[0].text.ends_with("model a failed; trying b"),
            "{:?}",
            seen[0].text
        );
        assert!(!seen[0].text.contains('\x1b'), "{:?}", seen[0].text);
        assert_eq!(seen[1].level, Level::ERROR);
    }

    /// Once the divert is dropped, records are written to stderr again and
    /// the sink sees no more of them.
    #[test]
    fn dropping_the_divert_sends_records_back_to_stderr() {
        let logs = LogWriter::default();
        let (diverted, seen) = collect(&logs);
        drop(diverted);
        assert!(matches!(logs.writer(Level::WARN), EitherWriter::A(_)));
        tracing::subscriber::with_default(subscriber(&logs), || tracing::warn!("after"));
        assert!(seen.lock().expect("unpoisoned").is_empty());
    }

    /// A divert dropped under another goes back to that one's sink, not to
    /// stderr, so records stay queued with whoever held the log first (#644).
    #[test]
    fn dropping_a_divert_restores_the_one_it_replaced() {
        let logs = LogWriter::default();
        let (_outer, outer_seen) = collect(&logs);
        let (inner, inner_seen) = collect(&logs);
        drop(inner);
        tracing::subscriber::with_default(subscriber(&logs), || tracing::warn!("after"));
        assert_eq!(outer_seen.lock().expect("unpoisoned").len(), 1);
        assert!(inner_seen.lock().expect("unpoisoned").is_empty());
    }

    #[test]
    fn escapes_are_stripped_and_text_is_kept() {
        assert_eq!(
            strip_escapes("\x1b[2m2026\x1b[0m \x1b[33m WARN\x1b[0m x"),
            "2026  WARN x"
        );
        assert_eq!(strip_escapes("a\x1b[?2004lb\x1b7c"), "abc");
        assert_eq!(strip_escapes("plain"), "plain");
        assert_eq!(strip_escapes("cut \x1b["), "cut ");
    }
}
