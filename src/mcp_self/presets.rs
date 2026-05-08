use serde::Serialize;

use crate::container::render::{BaseImage, Family, McpServer};

pub const SUGGESTIONS_NOTE: &str =
    "suggestions only -- not exhaustive; pick any base image or MCP server that fits";

#[derive(Debug, Clone, Serialize)]
pub struct SuggestionList<T> {
    pub note: &'static str,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaseImageSuggestion {
    pub image: &'static str,
    pub family: &'static str,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpPresetSuggestion {
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static [&'static str],
    pub install: &'static str,
    pub host_env: &'static [&'static str],
}

pub fn list_base_images() -> SuggestionList<BaseImageSuggestion> {
    SuggestionList {
        note: SUGGESTIONS_NOTE,
        items: BaseImage::ALL
            .iter()
            .copied()
            .map(|base| BaseImageSuggestion {
                image: base.as_str(),
                family: family_name(base.family()),
                reason: base.description(),
            })
            .collect(),
    }
}

pub fn list_mcp_presets() -> SuggestionList<McpPresetSuggestion> {
    SuggestionList {
        note: SUGGESTIONS_NOTE,
        items: McpServer::ALL
            .iter()
            .copied()
            .map(|server| McpPresetSuggestion {
                name: server.as_str(),
                description: server.description(),
                command: server.command_args(),
                install: server.install_cmd(),
                host_env: &[],
            })
            .collect(),
    }
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Debian => "debian",
        Family::Alpine => "alpine",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_carry_suggestions_note() {
        assert_eq!(list_base_images().note, SUGGESTIONS_NOTE);
        assert_eq!(list_mcp_presets().note, SUGGESTIONS_NOTE);
    }

    #[test]
    fn mcp_presets_include_command_and_install() {
        let presets = list_mcp_presets();
        let fs = presets
            .items
            .iter()
            .find(|preset| preset.name == "fs")
            .expect("fs preset");
        assert_eq!(fs.command[0], "mcp-server-filesystem");
        assert!(
            fs.install
                .contains("@modelcontextprotocol/server-filesystem")
        );
    }
}
