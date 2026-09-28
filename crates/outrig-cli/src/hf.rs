//! Minimal HuggingFace tree-listing helper used by `outrig config init`.
//!
//! Returns each file's path *and* size so the picker can display
//! human-readable sizes alongside filenames -- mistralrs users tell
//! quantizations apart by file size as much as by name. Hits HF's
//! model-info endpoint directly via `reqwest`, asking for `blobs` so each
//! file carries its size: the same listing `hf-hub::Api::info()` reads for
//! the loader, which keeps only the filenames. It names every file in the
//! repo at its full path in one answer, so a quantization kept in a
//! directory of its own is listed with the rest.
//!
//! The real implementation is behind the `local-llm` feature. Builds
//! without it still get the trait plus an `Unavailable` impl that always
//! errors -- so the init flow can prompt for `model-file` as free-form text.

use crate::error::{OutrigError, Result};

/// One file in a HuggingFace repo, as the model-info endpoint lists it,
/// trimmed to the fields the picker needs. `size` is the file's byte count
/// when known (HF reports it for every file when asked for blobs; the field
/// stays `Option` so future API quirks don't break the picker).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct HfFile {
    #[serde(rename = "rfilename")]
    pub path: String,
    #[serde(default)]
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
struct ModelInfo {
    siblings: Vec<HfFile>,
}

#[cfg(feature = "local-llm")]
impl HfTreeFetcher for ApiHfTreeFetcher {
    async fn list_files(&mut self, model_id: &str, revision: Option<&str>) -> Result<Vec<HfFile>> {
        let url = info_url(model_id, revision.unwrap_or("main"));
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
        let info: ModelInfo = resp
            .json()
            .await
            .map_err(|e| OutrigError::Configuration(format!("hf list {model_id:?}: {e}")))?;
        Ok(info.siblings)
    }
}

/// HF's model-info URL for `model_id` at `revision`, asking for each file's
/// size. hf-hub builds the path, as it does for the loader's own listing, so
/// the revision is escaped the way the download escapes it: left bare, HF
/// reads `refs/pr/1` as revision `refs`.
#[cfg(feature = "local-llm")]
fn info_url(model_id: &str, revision: &str) -> String {
    let repo = hf_hub::Repo::with_revision(
        model_id.to_string(),
        hf_hub::RepoType::Model,
        revision.to_string(),
    );
    format!("https://huggingface.co/api/{}?blobs=true", repo.api_url())
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

/// Whether `path` is one shard of a split quantization, which llama.cpp's
/// gguf-split names `<name>-00001-of-00003.gguf`. A shard is not a model by
/// itself: mistralrs refuses a set with any of its shards missing.
pub fn is_split_shard(path: &str) -> bool {
    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    let Some((head, count)) = stem.rsplit_once("-of-") else {
        return false;
    };
    let index = head.rsplit_once('-').map_or("", |(_, index)| index);
    [index, count]
        .iter()
        .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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
    fn is_split_shard_reads_gguf_split_names() {
        for shard in [
            "model-00001-of-00002.gguf",
            "BF16/gemma-3-27b-it-BF16-00002-of-00002.gguf",
            "DeepSeek-R1-BF16/DeepSeek-R1.BF16-00001-of-00030.GGUF",
        ] {
            assert!(is_split_shard(shard), "{shard} is a shard");
        }
        for whole in [
            "gemma-3-27b-it-IQ4_NL.gguf",
            "Q4_K_M/model-Q4_K_M.gguf",
            "state-of-the-art-q4.gguf",
            "model-of-00002.gguf",
        ] {
            assert!(!is_split_shard(whole), "{whole} is not a shard");
        }
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

    /// hf-hub builds the path, so the revision is escaped as the download
    /// escapes it: a bare `refs/pr/1` would read as revision `refs`.
    #[cfg(feature = "local-llm")]
    #[test]
    fn info_url_asks_for_sizes_and_escapes_the_revision() {
        assert_eq!(
            info_url("owner/repo", "main"),
            "https://huggingface.co/api/models/owner/repo/revision/main?blobs=true"
        );
        assert_eq!(
            info_url("owner/repo", "refs/pr/1"),
            "https://huggingface.co/api/models/owner/repo/revision/refs%2Fpr%2F1?blobs=true"
        );
    }

    /// Every file comes back at its full path in the repo, so a quantization
    /// kept in a directory of its own is listed with the rest.
    #[cfg(feature = "local-llm")]
    #[test]
    fn model_info_lists_files_at_their_full_paths() {
        let info: ModelInfo = serde_json::from_value(serde_json::json!({
            "id": "owner/repo",
            "siblings": [
                {"rfilename": "README.md", "size": 10, "blobId": "a1"},
                {
                    "rfilename": "Q4_K_M/model-Q4_K_M-00001-of-00002.gguf",
                    "size": 5_000,
                    "blobId": "b2",
                },
                {"rfilename": "Q4_K_M/model-Q4_K_M-00002-of-00002.gguf", "blobId": "c3"},
            ],
        }))
        .expect("a model-info answer parses");

        assert_eq!(
            info.siblings,
            vec![
                f("README.md", Some(10)),
                f("Q4_K_M/model-Q4_K_M-00001-of-00002.gguf", Some(5_000)),
                f("Q4_K_M/model-Q4_K_M-00002-of-00002.gguf", None),
            ]
        );
    }
}
