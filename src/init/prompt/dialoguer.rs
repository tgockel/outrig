//! Rich-TUI `PromptSource` impl backed by `dialoguer`.
//!
//! Handles the case where stdin is a real TTY: arrow-key fuzzy selects,
//! interactive confirms, and inline-edited string input. The trait surface
//! is identical to `TerminalPrompt`, so callers reach this through the
//! `auto()` factory in the parent module.
//!
//! Dialoguer is sync and reads `/dev/tty` directly, so every call is
//! wrapped in `tokio::task::spawn_blocking`.

use std::io;

use dialoguer::{Confirm, FuzzySelect, Input, MultiSelect};
use tokio::io::AsyncWriteExt;
use tokio::task;

use crate::error::{OutrigError, Result};
use crate::init::prompt::{Field, PromptSource};

#[derive(Debug, Default)]
pub struct DialoguerPrompt;

impl DialoguerPrompt {
    pub fn new() -> Self {
        Self
    }
}

/// Print the field's `description` to stderr so the user has the same
/// "what is this question asking?" context the line impl exposes via `?`.
async fn write_description(description: &str) -> Result<()> {
    let line = format!("\n  {description}\n");
    let mut stderr = tokio::io::stderr();
    stderr.write_all(line.as_bytes()).await?;
    stderr.flush().await?;
    Ok(())
}

fn map_dialoguer_err(e: dialoguer::Error) -> OutrigError {
    match e {
        dialoguer::Error::IO(io_err) => OutrigError::Io(io_err),
    }
}

fn map_join_err(e: task::JoinError) -> OutrigError {
    OutrigError::Io(io::Error::other(e))
}

impl PromptSource for DialoguerPrompt {
    async fn ask_string(&mut self, field: &Field, default: &str) -> Result<String> {
        write_description(field.description).await?;
        let prompt = field.name.to_owned();
        let default = default.to_owned();
        task::spawn_blocking(move || {
            Input::<String>::new()
                .with_prompt(prompt)
                .default(default)
                // Without this, dialoguer rejects an empty default ("") on
                // Enter; matches `TerminalPrompt`'s "Enter accepts default"
                // semantics regardless of whether the default is empty.
                .allow_empty(true)
                .interact_text()
        })
        .await
        .map_err(map_join_err)?
        .map_err(map_dialoguer_err)
    }

    async fn ask_bool(&mut self, field: &Field, default: bool) -> Result<bool> {
        write_description(field.description).await?;
        let prompt = field.name.to_owned();
        task::spawn_blocking(move || {
            Confirm::new()
                .with_prompt(prompt)
                .default(default)
                .interact()
        })
        .await
        .map_err(map_join_err)?
        .map_err(map_dialoguer_err)
    }

    async fn ask_select(&mut self, field: &Field, default_idx: usize) -> Result<usize> {
        write_description(field.description).await?;
        let prompt = field.name.to_owned();
        let items: Vec<&'static str> = field.options.iter().map(|(v, _)| *v).collect();
        task::spawn_blocking(move || {
            FuzzySelect::new()
                .with_prompt(prompt)
                .items(&items)
                .default(default_idx)
                .interact()
        })
        .await
        .map_err(map_join_err)?
        .map_err(map_dialoguer_err)
    }

    async fn ask_multiselect(
        &mut self,
        field: &Field,
        default_indices: &[usize],
    ) -> Result<Vec<usize>> {
        write_description(field.description).await?;
        let prompt = field.name.to_owned();
        let items: Vec<&'static str> = field.options.iter().map(|(v, _)| *v).collect();
        let mut mask = vec![false; items.len()];
        for &i in default_indices {
            if let Some(slot) = mask.get_mut(i) {
                *slot = true;
            }
        }
        task::spawn_blocking(move || {
            MultiSelect::new()
                .with_prompt(prompt)
                .items(&items)
                .defaults(&mask)
                .interact()
        })
        .await
        .map_err(map_join_err)?
        .map_err(map_dialoguer_err)
    }
}
