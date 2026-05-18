//! Integration tests for `outrig_cli::init::prompt::TerminalPrompt`.
//!
//! Drives the prompt loop through `tokio::io::duplex` halves, matching the
//! style of `tests/repl_io.rs`. Each test scripts stdin, drains stderr, and
//! asserts on the parsed answer plus any expected re-prompt or help output.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, duplex};
use tokio::time::timeout;

use outrig_cli::init::prompt::{Field, PromptSource, TerminalPrompt};

const BUF: usize = 4096;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn provider_field() -> Field {
    Field {
        name: "Pick a provider style",
        description: "Which OpenAI-compatible API shape this provider speaks.",
        options: &[
            ("openai", "OpenAI's chat-completions API."),
            ("anthropic", "Anthropic Messages API."),
        ],
        doc_link: "doc/concepts/llm-providers.md",
    }
}

fn workspace_field() -> Field {
    Field {
        name: "Workspace host-path",
        description: "Path on the host that maps into the container.",
        options: &[],
        doc_link: "doc/concepts/workspace.md",
    }
}

fn define_model_field() -> Field {
    Field {
        name: "Define a model now?",
        description: "Whether to add a model definition to the new config.",
        options: &[],
        doc_link: "doc/usage/init.md",
    }
}

fn extras_field() -> Field {
    Field {
        name: "Extras",
        description: "Optional extras to enable.",
        options: &[
            ("foo", "Enable foo."),
            ("bar", "Enable bar."),
            ("baz", "Enable baz."),
        ],
        doc_link: "doc/usage/init.md",
    }
}

#[tokio::test]
async fn string_returns_default_on_empty() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);
    stdin_w.write_all(b"\n").await.unwrap();
    drop(stdin_w);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = workspace_field();

    let got = timeout(TEST_TIMEOUT, prompt.ask_string(&field, "."))
        .await
        .expect("must not hang")
        .expect("ask_string must succeed");
    drop(prompt);

    assert_eq!(got, ".");

    let mut stderr_buf = Vec::new();
    stderr_r.read_to_end(&mut stderr_buf).await.unwrap();
    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("? Workspace host-path [default: .]: "),
        "stderr was: {stderr:?}"
    );
}

#[tokio::test]
async fn string_help_then_value() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);
    stdin_w.write_all(b"?\n/srv/work\n").await.unwrap();
    drop(stdin_w);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = workspace_field();

    let got = timeout(TEST_TIMEOUT, prompt.ask_string(&field, "."))
        .await
        .expect("must not hang")
        .expect("ask_string must succeed");
    drop(prompt);

    assert_eq!(got, "/srv/work");

    let mut stderr_buf = Vec::new();
    stderr_r.read_to_end(&mut stderr_buf).await.unwrap();
    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("Path on the host that maps into the container."),
        "expected description in help output: {stderr:?}"
    );
    assert!(
        stderr.contains("See: doc/concepts/workspace.md"),
        "expected doc_link in help output: {stderr:?}"
    );
    let prompt_count = stderr.matches("? Workspace host-path").count();
    assert_eq!(
        prompt_count, 2,
        "expected two prompt renders (initial + after help), got: {stderr:?}"
    );
}

#[tokio::test]
async fn bool_y_n_aliases_all_parse() {
    for (input, expected) in [
        ("y\n", true),
        ("Y\n", true),
        ("yes\n", true),
        ("n\n", false),
        ("N\n", false),
        ("no\n", false),
    ] {
        let (mut stdin_w, stdin_r) = duplex(BUF);
        let (stderr_w, _stderr_r) = duplex(BUF);
        stdin_w.write_all(input.as_bytes()).await.unwrap();
        drop(stdin_w);

        let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
        let field = define_model_field();

        let got = timeout(TEST_TIMEOUT, prompt.ask_bool(&field, true))
            .await
            .expect("must not hang")
            .expect("ask_bool must succeed");
        assert_eq!(got, expected, "input {input:?}");
    }
}

#[tokio::test]
async fn bool_default_render_reflects_default() {
    for (default, expected_marker) in [(true, "[Y/n]"), (false, "[y/N]")] {
        let (mut stdin_w, stdin_r) = duplex(BUF);
        let (stderr_w, mut stderr_r) = duplex(BUF);
        stdin_w.write_all(b"\n").await.unwrap();
        drop(stdin_w);

        let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
        let field = define_model_field();

        let got = timeout(TEST_TIMEOUT, prompt.ask_bool(&field, default))
            .await
            .expect("must not hang")
            .expect("ask_bool must succeed");
        assert_eq!(got, default);
        drop(prompt);

        let mut stderr_buf = Vec::new();
        stderr_r.read_to_end(&mut stderr_buf).await.unwrap();
        let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
        assert!(
            stderr.contains(expected_marker),
            "expected `{expected_marker}` in stderr, got: {stderr:?}"
        );
    }
}

#[tokio::test]
async fn select_invalid_then_valid() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);
    stdin_w.write_all(b"acme\nanthropic\n").await.unwrap();
    drop(stdin_w);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = provider_field();

    let got = timeout(TEST_TIMEOUT, prompt.ask_select(&field, 0))
        .await
        .expect("must not hang")
        .expect("ask_select must succeed");
    drop(prompt);

    assert_eq!(got, 1);

    let mut stderr_buf = Vec::new();
    stderr_r.read_to_end(&mut stderr_buf).await.unwrap();
    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] expected one of: openai, anthropic"),
        "expected validation error in stderr: {stderr:?}"
    );
}

#[tokio::test]
async fn multiselect_comma_with_whitespace() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);
    stdin_w.write_all(b"foo, bar ,baz\n").await.unwrap();
    drop(stdin_w);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = extras_field();

    let got = timeout(TEST_TIMEOUT, prompt.ask_multiselect(&field, &[]))
        .await
        .expect("must not hang")
        .expect("ask_multiselect must succeed");
    assert_eq!(got, vec![0, 1, 2]);
}

#[tokio::test]
async fn multiselect_unknown_token_reprompts() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);
    stdin_w.write_all(b"foo,quux\nfoo,bar\n").await.unwrap();
    drop(stdin_w);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = extras_field();

    let got = timeout(TEST_TIMEOUT, prompt.ask_multiselect(&field, &[0]))
        .await
        .expect("must not hang")
        .expect("ask_multiselect must succeed");
    drop(prompt);

    assert_eq!(got, vec![0, 1]);

    let mut stderr_buf = Vec::new();
    stderr_r.read_to_end(&mut stderr_buf).await.unwrap();
    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("unknown value `quux`"),
        "expected validation error naming `quux`: {stderr:?}"
    );
}

#[tokio::test]
async fn eof_returns_io_error() {
    let (stdin_w, stdin_r) = duplex(BUF);
    drop(stdin_w);
    let (stderr_w, _stderr_r) = duplex(BUF);

    let mut prompt = TerminalPrompt::new(BufReader::new(stdin_r), stderr_w);
    let field = workspace_field();

    let err = timeout(TEST_TIMEOUT, prompt.ask_string(&field, "."))
        .await
        .expect("must not hang")
        .expect_err("ask_string must fail on EOF");

    let msg = format!("{err}");
    assert!(
        msg.contains("unexpected end of file") || msg.contains("UnexpectedEof"),
        "expected UnexpectedEof, got: {msg}"
    );
}
