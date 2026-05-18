//! Minimal HuggingFace tree-listing helper used by `outrig config init`.
//!
//! Returns each file's path *and* size so the picker can display
//! human-readable sizes alongside filenames -- mistralrs users tell
//! quantizations apart by file size as much as by name. Hits HF's
//! `/api/models/{id}/tree/{revision}` directly via `reqwest` (rather
//! than `hf-hub::Api::info()`, which only exposes filenames).
//!
//! The `local-llm` feature pulls in `reqwest` for the real implementation.
//! Builds without the feature still get the trait plus an `Unavailable`
//! impl that always errors -- so the init flow can prompt for `model-file`
//! as free-form text without compiling against `reqwest`.

use crate::error::{OutrigError, Result};

/// One file in a HuggingFace repo, projected into the fields the picker
/// needs. `size` is the file's byte count when known (HF's tree endpoint
/// reports it for every regular file; the field stays `Option` so future
/// API quirks don't break the picker).
#[derive(Debug, Clone, PartialEq)]
pub struct HfFile {
    pub path: String,
    pub size: Option<u64>,
}

#[allow(async_fn_in_trait)]
pub trait HfTreeFetcher {
    async fn list_files(&mut self, model_id: &str, revision: Option<&str>) -> Result<Vec<HfFile>>;
}

#[cfg(feature = "local-llm")]
pub struct ApiHfTreeFetcher;

#[cfg(feature = "local-llm")]
#[derive(serde::Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: Option<u64>,
}

#[cfg(feature = "local-llm")]
impl HfTreeFetcher for ApiHfTreeFetcher {
    async fn list_files(&mut self, model_id: &str, revision: Option<&str>) -> Result<Vec<HfFile>> {
        let revision = revision.unwrap_or("main");
        let url = format!("https://huggingface.co/api/models/{model_id}/tree/{revision}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| OutrigError::Configuration(format!("hf client: {e}")))?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| OutrigError::Configuration(format!("hf list {model_id:?}: {e}")))?;
        if !resp.status().is_success() {
            return Err(OutrigError::Configuration(format!(
                "hf list {model_id:?}: HTTP {}",
                resp.status()
            ))
            .into());
        }
        let entries: Vec<TreeEntry> = resp
            .json()
            .await
            .map_err(|e| OutrigError::Configuration(format!("hf list {model_id:?}: {e}")))?;
        Ok(entries
            .into_iter()
            .filter(|e| e.kind == "file")
            .map(|e| HfFile {
                path: e.path,
                size: e.size,
            })
            .collect())
    }
}

/// Always-fails fetcher used when the `local-llm` feature is off (or by
/// callers that explicitly want to bypass the network). Returns a
/// configuration error the prompt flow recognizes as "fall back to the
/// free-form text prompt".
pub struct UnavailableHfTreeFetcher;

impl HfTreeFetcher for UnavailableHfTreeFetcher {
    async fn list_files(
        &mut self,
        _model_id: &str,
        _revision: Option<&str>,
    ) -> Result<Vec<HfFile>> {
        Err(OutrigError::Configuration(
            "HuggingFace tree-listing not available in this build".to_string(),
        )
        .into())
    }
}

/// Pick a fetcher appropriate for the current build. Mirrors the
/// `init::prompt::auto` factory.
pub fn auto() -> AutoHfTreeFetcher {
    #[cfg(feature = "local-llm")]
    {
        AutoHfTreeFetcher::Api(ApiHfTreeFetcher)
    }
    #[cfg(not(feature = "local-llm"))]
    {
        AutoHfTreeFetcher::Unavailable(UnavailableHfTreeFetcher)
    }
}

pub enum AutoHfTreeFetcher {
    #[cfg(feature = "local-llm")]
    Api(ApiHfTreeFetcher),
    Unavailable(UnavailableHfTreeFetcher),
}

impl HfTreeFetcher for AutoHfTreeFetcher {
    async fn list_files(&mut self, model_id: &str, revision: Option<&str>) -> Result<Vec<HfFile>> {
        match self {
            #[cfg(feature = "local-llm")]
            Self::Api(f) => f.list_files(model_id, revision).await,
            Self::Unavailable(f) => f.list_files(model_id, revision).await,
        }
    }
}

/// Filter to `.gguf` files only, sorted by path.
pub fn filter_gguf(files: Vec<HfFile>) -> Vec<HfFile> {
    let mut out: Vec<HfFile> = files
        .into_iter()
        .filter(|f| f.path.to_ascii_lowercase().ends_with(".gguf"))
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Render a byte count as a human-readable string (e.g. "1.4 GiB").
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, size: Option<u64>) -> HfFile {
        HfFile {
            path: path.to_string(),
            size,
        }
    }

    #[test]
    fn filter_gguf_keeps_only_gguf_and_sorts() {
        let files = vec![
            f("README.md", Some(1_024)),
            f("config.json", Some(512)),
            f(
                "qwen2.5-coder-1.5b-instruct-q5_k_m.gguf",
                Some(1_500_000_000),
            ),
            f(
                "qwen2.5-coder-1.5b-instruct-q4_k_m.GGUF",
                Some(1_000_000_000),
            ),
            f("qwen2.5-coder-1.5b-instruct-q8_0.gguf", Some(2_000_000_000)),
            f("tokenizer.json", Some(512)),
        ];
        let out = filter_gguf(files);
        let names: Vec<&str> = out.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "qwen2.5-coder-1.5b-instruct-q4_k_m.GGUF",
                "qwen2.5-coder-1.5b-instruct-q5_k_m.gguf",
                "qwen2.5-coder-1.5b-instruct-q8_0.gguf",
            ]
        );
    }

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2 * 1024), "2.0 KiB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(format_size(1_500_000_000), "1.4 GiB");
    }

    #[tokio::test]
    async fn unavailable_fetcher_always_errors() {
        let mut x = UnavailableHfTreeFetcher;
        assert!(x.list_files("anything", None).await.is_err());
    }
}
