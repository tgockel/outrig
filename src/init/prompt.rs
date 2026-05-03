//! Interactive prompts with `[default: ...]` rendering and `?`-help.
//!
//! `PromptSource` is a trait so terminal, AI/LLM, and scripted-test answer
//! sources are interchangeable. `TerminalPrompt` is the production impl
//! used by `outrig init` / `config init` / `container add`.

use std::io;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines, Stderr, Stdin,
};

use crate::error::{OutrigError, Result};

/// Static metadata for one interactive question.
///
/// Constants live next to the call sites (e.g. `init/container.rs`) and are
/// passed by reference into `PromptSource` methods.
pub struct Field {
    /// User-facing question, e.g. `"Pick a provider style"`.
    pub name: &'static str,
    /// Long-form explanation shown when the user types `?`.
    pub description: &'static str,
    /// Discrete choices as `(value, blurb)` pairs. Empty for free-text fields.
    pub options: &'static [(&'static str, &'static str)],
    /// Path under the repo root, e.g. `"doc/concepts/llm-providers.md"`.
    pub doc_link: &'static str,
}

/// Source of answers to interactive prompts.
///
/// Validation failures retry internally; only I/O errors (including
/// EOF on stdin) surface as `Err`.
//
// The trait is consumed `&mut self` from a single-threaded init flow and is
// never spawned across threads, so the lack of a `Send` bound on the
// returned future is intentional.
#[allow(async_fn_in_trait)]
pub trait PromptSource {
    async fn ask_string(&mut self, field: &Field, default: &str) -> Result<String>;
    async fn ask_bool(&mut self, field: &Field, default: bool) -> Result<bool>;
    async fn ask_select(&mut self, field: &Field, default_idx: usize) -> Result<usize>;
    async fn ask_multiselect(
        &mut self,
        field: &Field,
        default_indices: &[usize],
    ) -> Result<Vec<usize>>;
}

/// `PromptSource` impl that renders to `stderr` and reads from `stdin`.
///
/// Generic over the streams so tests can drive it through `tokio::io::duplex`.
pub struct TerminalPrompt<R, W> {
    lines: Lines<R>,
    stderr: W,
}

impl<R, W> TerminalPrompt<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(stdin: R, stderr: W) -> Self {
        Self {
            lines: stdin.lines(),
            stderr,
        }
    }
}

impl TerminalPrompt<BufReader<Stdin>, Stderr> {
    /// Build a `TerminalPrompt` over the process's real stdin/stderr.
    pub fn from_real_io() -> Self {
        Self::new(BufReader::new(tokio::io::stdin()), tokio::io::stderr())
    }
}

/// One read-loop iteration result, before any answer-type-specific parsing.
enum RawLine {
    /// User pressed Enter on an empty line: caller returns the default.
    Default,
    /// User typed `?`: help has been printed and the loop should re-prompt.
    Help,
    /// A non-empty, non-`?` line that the caller must validate.
    Value(String),
}

impl<R, W> TerminalPrompt<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    async fn write_prompt(&mut self, field: &Field, default_render: &str) -> Result<()> {
        // An empty `default_render` means the caller has no useful default to
        // suggest; drop the `[...]` suffix so the prompt reads cleanly.
        let line = if default_render.is_empty() {
            format!("? {}: ", field.name)
        } else {
            format!("? {} [{}]: ", field.name, default_render)
        };
        self.stderr.write_all(line.as_bytes()).await?;
        self.stderr.flush().await?;
        Ok(())
    }

    async fn write_help(&mut self, field: &Field) -> Result<()> {
        let mut buf = String::new();
        buf.push('\n');
        buf.push_str("  ");
        buf.push_str(field.description);
        buf.push('\n');
        for (value, blurb) in field.options {
            buf.push_str("  ");
            buf.push_str(value);
            buf.push_str("  ");
            buf.push_str(blurb);
            buf.push('\n');
        }
        buf.push('\n');
        buf.push_str("  See: ");
        buf.push_str(field.doc_link);
        buf.push_str("\n\n");
        self.stderr.write_all(buf.as_bytes()).await?;
        self.stderr.flush().await?;
        Ok(())
    }

    async fn write_error(&mut self, msg: &str) -> Result<()> {
        let line = format!("[outrig] {msg}\n");
        self.stderr.write_all(line.as_bytes()).await?;
        self.stderr.flush().await?;
        Ok(())
    }

    /// Render the prompt, read one line, and dispatch on `?` / empty / value.
    async fn read_one(&mut self, field: &Field, default_render: &str) -> Result<RawLine> {
        self.write_prompt(field, default_render).await?;
        let Some(line) = self.lines.next_line().await? else {
            return Err(OutrigError::Io(io::Error::from(
                io::ErrorKind::UnexpectedEof,
            )));
        };
        if line == "?" {
            self.write_help(field).await?;
            Ok(RawLine::Help)
        } else if line.is_empty() {
            Ok(RawLine::Default)
        } else {
            Ok(RawLine::Value(line))
        }
    }
}

impl<R, W> PromptSource for TerminalPrompt<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    async fn ask_string(&mut self, field: &Field, default: &str) -> Result<String> {
        let render = if default.is_empty() {
            String::new()
        } else {
            format!("default: {default}")
        };
        loop {
            match self.read_one(field, &render).await? {
                RawLine::Help => continue,
                RawLine::Default => return Ok(default.to_string()),
                RawLine::Value(s) => return Ok(s),
            }
        }
    }

    async fn ask_bool(&mut self, field: &Field, default: bool) -> Result<bool> {
        let render = if default { "Y/n" } else { "y/N" };
        loop {
            match self.read_one(field, render).await? {
                RawLine::Help => continue,
                RawLine::Default => return Ok(default),
                RawLine::Value(s) => match parse_bool(&s) {
                    Some(b) => return Ok(b),
                    None => {
                        self.write_error("expected y/yes or n/no").await?;
                        continue;
                    }
                },
            }
        }
    }

    async fn ask_select(&mut self, field: &Field, default_idx: usize) -> Result<usize> {
        let default_value = field.options[default_idx].0;
        loop {
            match self
                .read_one(field, &format!("default: {default_value}"))
                .await?
            {
                RawLine::Help => continue,
                RawLine::Default => return Ok(default_idx),
                RawLine::Value(s) => match index_of(field.options, s.trim()) {
                    Some(i) => return Ok(i),
                    None => {
                        let values = join_values(field.options);
                        self.write_error(&format!("expected one of: {values}"))
                            .await?;
                        continue;
                    }
                },
            }
        }
    }

    async fn ask_multiselect(
        &mut self,
        field: &Field,
        default_indices: &[usize],
    ) -> Result<Vec<usize>> {
        let default_render = {
            let joined: Vec<&str> = default_indices
                .iter()
                .map(|&i| field.options[i].0)
                .collect();
            format!("default: {}", joined.join(","))
        };
        loop {
            match self.read_one(field, &default_render).await? {
                RawLine::Help => continue,
                RawLine::Default => return Ok(default_indices.to_vec()),
                RawLine::Value(s) => match parse_multiselect(field.options, &s) {
                    Ok(indices) => return Ok(indices),
                    Err(bad) => {
                        let values = join_values(field.options);
                        self.write_error(&format!(
                            "unknown value `{bad}`; expected any of: {values}"
                        ))
                        .await?;
                        continue;
                    }
                },
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "y" | "Y" | "yes" | "Yes" | "YES" => Some(true),
        "n" | "N" | "no" | "No" | "NO" => Some(false),
        _ => None,
    }
}

fn index_of(options: &[(&str, &str)], needle: &str) -> Option<usize> {
    options.iter().position(|(value, _)| *value == needle)
}

fn join_values(options: &[(&str, &str)]) -> String {
    options
        .iter()
        .map(|(value, _)| *value)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_multiselect(
    options: &[(&str, &str)],
    input: &str,
) -> std::result::Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        match index_of(options, trimmed) {
            Some(i) => out.push(i),
            None => return Err(trimmed.to_string()),
        }
    }
    Ok(out)
}
