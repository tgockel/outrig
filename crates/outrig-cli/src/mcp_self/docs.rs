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

    #[test]
    fn get_doc_returns_markdown() {
        let doc = get_doc("concepts/containers").expect("containers doc exists");
        assert!(doc.markdown.starts_with("# Containers"));
    }
}
