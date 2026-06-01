use serde::Serialize;

use crate::image_setup::render::{BaseImage, Family, McpServer};

pub const SUGGESTIONS_NOTE: &str = concat!(
    "suggestions only -- incomplete starting points; ",
    "pick any base image or MCP server command that fits",
);

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
pub struct McpServerSuggestion {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<&'static [&'static str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<&'static str>,
    pub curated_recipe: bool,
    pub guidance: &'static str,
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

pub fn list_mcp_server_suggestions() -> SuggestionList<McpServerSuggestion> {
    let mut items: Vec<McpServerSuggestion> = McpServer::ALL
        .iter()
        .copied()
        .map(|server| McpServerSuggestion {
            name: server.as_str(),
            description: server.description(),
            command: Some(server.command_args()),
            install: Some(server.install_cmd()),
            curated_recipe: true,
            guidance: "This is a maintained package recipe used by `outrig image add`; edit \
                       it freely when the container needs different paths or package versions.",
            host_env: &[],
        })
        .collect();
    items.push(McpServerSuggestion {
        name: "shell",
        description: "Shell execution MCP server for running commands inside the OutRig container.",
        command: None,
        install: None,
        curated_recipe: false,
        guidance: "OutRig supports arbitrary MCP server commands. Coding containers usually need \
                   shell execution; choose and install a real shell MCP package, then declare its \
                   command in `[images.<name>.mcp]`.",
        host_env: &[],
    });
    SuggestionList {
        note: SUGGESTIONS_NOTE,
        items,
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
        assert_eq!(list_mcp_server_suggestions().note, SUGGESTIONS_NOTE);
    }

    #[test]
    fn mcp_suggestions_include_command_and_install() {
        let suggestions = list_mcp_server_suggestions();
        let fs = suggestions
            .items
            .iter()
            .find(|suggestion| suggestion.name == "fs")
            .expect("fs suggestion");
        assert_eq!(fs.command.expect("fs command")[0], "mcp-server-filesystem");
        assert!(
            fs.install
                .expect("fs install")
                .contains("@modelcontextprotocol/server-filesystem")
        );
    }

    #[test]
    fn mcp_suggestions_include_shell_guidance() {
        let suggestions = list_mcp_server_suggestions();
        let shell = suggestions
            .items
            .iter()
            .find(|suggestion| suggestion.name == "shell")
            .expect("shell suggestion");
        assert!(!shell.curated_recipe);
        assert!(shell.guidance.contains("arbitrary MCP server commands"));
    }
}
