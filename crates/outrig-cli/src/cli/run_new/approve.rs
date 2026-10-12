//! The terminal's answer to whether a binding a repository declared may start.
//!
//! A binding's factory runs on this host, as the user, before the agent has done anything, and a
//! repository's config is content a cloned project brings and the agent can write. So `run-new`
//! shows the operator each such declaration whole, once, and starts it only on a yes; the library
//! remembers the yes, and asks again after any edit. With no terminal to ask on -- a script, CI --
//! the binding does not start, and the session runs without it.

use std::io::{BufRead, IsTerminal, Write};

use outrig::harness::{Approval, ApprovalRequest, Approver};

/// Asks on the terminal: the declaration on stderr, the answer from stdin.
pub(super) struct TerminalApprover;

impl Approver for TerminalApprover {
    fn approve(&self, request: &ApprovalRequest) -> Approval {
        // The std handle `converse` reads later too, so a line typed ahead of this prompt is
        // either the answer or still there for the agent.
        let stdin = std::io::stdin();
        let interactive = stdin.is_terminal();
        ask(request, interactive, &mut stdin.lock(), &mut std::io::stderr())
    }
}

/// Ask about `request` on `output`, reading the answer from `input` when `interactive`; with no
/// terminal, say how to approve it instead.
fn ask(
    request: &ApprovalRequest,
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Approval {
    let name = &request.name;
    let file = request
        .declared_in
        .as_ref()
        .map_or_else(|| "the repository's config".to_string(), |f| f.display().to_string());
    if !interactive {
        let _ = writeln!(
            output,
            "[outrig] the binding `{name}`, declared in {file}, is not approved and there is no \
             terminal to ask on, so it does not start; to approve it, run `outrig run-new` at a \
             terminal in {}, or move [bindings.{name}] to the global config",
            request.repo_root.display()
        );
        return Approval::CannotAsk;
    }
    let declaration: String = request
        .declaration
        .lines()
        .map(|line| format!("    {line}\n"))
        .collect();
    let _ = write!(
        output,
        "[outrig] {file} declares a binding. Its factory runs on this host, as you, with your \
         credentials, before the agent starts, and the agent can use what it builds:\n\n\
         {declaration}\n\
         [outrig] start `{name}`, and remember the answer for this declaration in {}? [y/N] ",
        request.repo_root.display()
    );
    let _ = output.flush();
    let mut line = String::new();
    let answer = match input.read_line(&mut line) {
        Ok(_) => line.trim().to_ascii_lowercase(),
        Err(_) => String::new(),
    };
    if answer == "y" || answer == "yes" {
        return Approval::Approve;
    }
    let _ = writeln!(
        output,
        "[outrig] the binding `{name}` does not start; the next session asks again"
    );
    Approval::Refuse
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest::new(
            "repo",
            Some(PathBuf::from("/home/a/p/.agents/outrig/config.toml")),
            "/home/a/p",
            "[bindings.repo]\ndescription = \"The project's repository.\"\nfactory = \"git:Repo\"\n",
        )
    }

    fn asked(interactive: bool, typed: &str) -> (Approval, String) {
        let mut output = Vec::new();
        let answer = ask(&request(), interactive, &mut Cursor::new(typed), &mut output);
        (answer, String::from_utf8(output).expect("utf-8"))
    }

    #[test]
    fn yes_at_the_terminal_approves_after_showing_the_declaration() {
        let (answer, shown) = asked(true, "y\n");
        assert_eq!(answer, Approval::Approve);
        assert!(shown.contains("    [bindings.repo]\n"), "{shown}");
        assert!(shown.contains("factory = \"git:Repo\""), "{shown}");
        assert!(shown.contains("start `repo`"), "{shown}");
        assert!(shown.contains("/home/a/p"), "{shown}");
    }

    #[test]
    fn anything_else_at_the_terminal_refuses() {
        for typed in ["n\n", "\n", "", "sure\n"] {
            let (answer, shown) = asked(true, typed);
            assert_eq!(answer, Approval::Refuse, "{typed:?}");
            assert!(shown.contains("`repo` does not start"), "{shown}");
        }
    }

    #[test]
    fn with_no_terminal_it_says_how_to_approve_and_asks_nothing() {
        let (answer, shown) = asked(false, "y\n");
        assert_eq!(answer, Approval::CannotAsk);
        assert!(shown.contains("no terminal to ask on"), "{shown}");
        assert!(shown.contains("move [bindings.repo] to the global config"), "{shown}");
        assert!(!shown.contains("[y/N]"), "{shown}");
    }
}
