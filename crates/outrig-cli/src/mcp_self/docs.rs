use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct Doc {
    pub page: &'static str,
    pub markdown: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DocSummary {
    pub page: &'static str,
    pub title: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocList {
    pub docs: Vec<DocSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocContent {
    pub page: &'static str,
    pub markdown: &'static str,
}

pub const DOCS: &[Doc] = &[
    Doc {
        page: "concepts/containers",
        markdown: include_str!("docs/concepts/containers.md"),
    },
    Doc {
        page: "concepts/mcp-servers",
        markdown: include_str!("docs/concepts/mcp-servers.md"),
    },
    Doc {
        page: "concepts/mcp-trust-model",
        markdown: include_str!("docs/concepts/mcp-trust-model.md"),
    },
    Doc {
        page: "concepts/workspace",
        markdown: include_str!("docs/concepts/workspace.md"),
    },
    Doc {
        page: "concepts/subagents",
        markdown: include_str!("docs/concepts/subagents.md"),
    },
    Doc {
        page: "reference/config",
        markdown: include_str!("docs/reference/config.md"),
    },
    Doc {
        page: "usage/ai-assisted-design",
        markdown: include_str!("docs/usage/ai-assisted-design.md"),
    },
    Doc {
        page: "usage/image",
        markdown: include_str!("docs/usage/image.md"),
    },
];

pub fn list_docs() -> DocList {
    DocList {
        docs: DOCS.iter().map(summarize).collect(),
    }
}

pub fn get_doc(page: &str) -> Option<DocContent> {
    DOCS.iter()
        .find(|doc| doc.page == page)
        .map(|doc| DocContent {
            page: doc.page,
            markdown: doc.markdown,
        })
}

fn summarize(doc: &Doc) -> DocSummary {
    let title = doc
        .markdown
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or(doc.page)
        .trim()
        .to_string();
    let summary = doc
        .markdown
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with('#')
                && !line.starts_with('>')
                && !line.starts_with("```")
        })
        .find(|line| *line != title)
        .unwrap_or("")
        .to_string();

    DocSummary {
        page: doc.page,
        title,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_includes_trust_model() {
        let list = list_docs();
        assert!(
            list.docs
                .iter()
                .any(|doc| doc.page == "concepts/mcp-trust-model"),
            "doc list should include the trust model: {:?}",
            list.docs,
        );
    }

    /// The bundle is served by `outrig mcp self`, where a link to a page that
    /// was never registered is a dead end for whatever AI tool is reading it.
    /// This caught `concepts/subagents` being written into `doc/` as a real
    /// file while two bundle pages already linked to it.
    #[test]
    fn every_internal_link_resolves_to_a_registered_page() {
        fn resolve(from_page: &str, link: &str) -> Option<String> {
            let mut parts: Vec<&str> = from_page.split('/').collect();
            parts.pop(); // the page's own name; links are relative to its dir
            for segment in link.trim_end_matches(".md").split('/') {
                match segment {
                    "." => {}
                    ".." => {
                        parts.pop()?;
                    }
                    other => parts.push(other),
                }
            }
            Some(parts.join("/"))
        }

        for doc in DOCS {
            // Bind the offset, not the matched text: `match_indices` yields the
            // needle itself as the second element, so slicing *that* past its
            // own length gave an empty string, no `)`, and a `continue` before
            // every assertion -- the test inspected nothing at all.
            for (idx, _) in doc.markdown.match_indices("](") {
                let rest = &doc.markdown[idx + 2..];
                let Some(end) = rest.find(')') else {
                    continue;
                };
                let link = &rest[..end];
                if link.contains("://") {
                    continue;
                }
                // Strip the anchor before the extension test: `foo.md#bar` is
                // just as dead a link as `foo.md`, and testing `ends_with`
                // first let every anchored link through unchecked.
                let path = link.split('#').next().unwrap_or(link);
                if !path.ends_with(".md") {
                    continue;
                }
                let Some(target) = resolve(doc.page, path) else {
                    panic!("{}: link {link:?} escapes the bundle", doc.page);
                };
                assert!(
                    DOCS.iter().any(|d| d.page == target),
                    "{}: links to {link:?} -> {target:?}, which is not a \
                     registered page. Either add it to DOCS or drop the link.",
                    doc.page,
                );
            }
        }
    }

    #[test]
    fn get_doc_returns_markdown() {
        let doc = get_doc("concepts/containers").expect("containers doc exists");
        assert!(doc.markdown.starts_with("# Containers"));
    }

    /// Both pages promised the model was inherited, which stopped being true
    /// when `outrig__subagent` grew a `model` argument. The bundle is what an AI
    /// tool reads to decide how to call the tools, so a stale promise here is
    /// worse than no documentation.
    #[test]
    fn docs_do_not_claim_model_is_inherited() {
        let stale = [
            ("concepts/subagents", "The model, provider, and limits"),
            ("reference/config", "MCP tools, model and"),
        ];
        for (page, phrasing) in stale {
            let doc = get_doc(page).unwrap_or_else(|| panic!("{page} is a registered page"));
            assert!(
                !doc.markdown.contains(phrasing),
                "{page} still claims the model is inherited: {phrasing:?}"
            );
        }
        assert!(
            get_doc("concepts/subagents")
                .expect("page")
                .markdown
                .contains("Choosing the subagent's model"),
            "the argument that replaced the promise has to be documented"
        );
    }
}
