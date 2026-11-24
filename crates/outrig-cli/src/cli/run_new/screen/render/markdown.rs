//! A small CommonMark presentation, shared by both transcript views.
//!
//! Parsing never changes an entry. HTML is literal text; links include their
//! destinations. Code soft-wraps without dropping whitespace, with `↪` rather
//! than `│` on continuation rows. No highlighting or terminal escapes are used.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::grapheme_cells;

const CODE: Style = Style::new().add_modifier(Modifier::REVERSED);
const QUIET: Style = Style::new().add_modifier(Modifier::DIM);

// This parser runs on the drawing thread, including on detail-view resizes.
// Bound input and potential container depth *before* Parser::new, whose eager
// block pass can otherwise take quadratic time on lazy nested continuations.
const MARKDOWN_BYTES: usize = 256 * 1024;
const CONTAINER_PREFIX_CELLS: usize = 128;

fn within_markdown_budget(source: &str) -> bool {
    if source.len() > MARKDOWN_BYTES {
        return false;
    }
    // CommonMark treats LF, CRLF, and bare CR as line endings. str::lines()
    // does not split bare CR, which could hide nesting after ordinary prose.
    source.split(['\r', '\n']).all(|line| {
        let mut cells = 0;
        for byte in line.bytes() {
            cells += match byte {
                b'\t' => super::super::editor::TAB_STOP,
                b'>' | b'-' | b'+' | b'*' | b'0'..=b'9' | b'.' | b')' => 1,
                byte if byte.is_ascii_whitespace() => 1,
                _ => break,
            };
            if cells > CONTAINER_PREFIX_CELLS {
                return false;
            }
        }
        true
    })
}

/// Styled logical lines first, terminal-cell wrapping second. Keeping those
/// steps separate also keeps graphemes whole across parser event boundaries.
pub(super) fn lines(source: &str, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for line in parse(source, width).lines {
        wrap(line, width, &mut rows);
    }
    rows
}

fn parse(source: &str, width: usize) -> Document {
    let mut document = Document {
        width,
        ..Document::default()
    };
    if !within_markdown_budget(source) {
        // A deliberately conservative check: code or punctuation-heavy prose
        // may also exceed it. Show the source literally, with the same safe,
        // cell-aware wrapping, rather than attempting to parse a prefix of it.
        document.text(source, Style::new());
        document.flush();
        return document;
    }
    let mut styles = vec![Style::new()];
    let mut lists = Vec::new();
    let mut links = Vec::new();
    let mut code = None;
    for event in Parser::new(source) {
        let style = *styles.last().expect("base style");
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                document.flush();
                document.separate();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((language, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((language, text)) = code.take() {
                    // The parser supplies a final line terminator, not an
                    // additional empty source line. Remove exactly that one.
                    document.code(&language, text.strip_suffix('\n').unwrap_or(&text));
                }
                document.gap = true;
            }
            Event::Text(text) if code.is_some() => {
                code.as_mut().expect("in code").1.push_str(&text);
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Heading { .. }) => {
                let modifier = match event {
                    Event::Start(Tag::Emphasis) => Modifier::ITALIC,
                    _ => Modifier::BOLD,
                };
                styles.push(style.add_modifier(modifier));
            }
            Event::End(TagEnd::Emphasis | TagEnd::Strong) => {
                styles.pop();
            }
            Event::End(TagEnd::Heading(_)) => {
                document.flush();
                document.gap = true;
                styles.pop();
            }
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                links.push(dest_url.into_string());
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(destination) = links.pop() {
                    document.text(&format!(" ({destination})"), style);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                document.flush();
                document.prefix.push(Prefix::Quote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                document.flush();
                document.prefix.pop();
                document.gap = true;
            }
            Event::Start(Tag::List(start)) => {
                document.flush();
                document.separate();
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                document.flush();
                lists.pop();
                document.gap = true;
            }
            Event::Start(Tag::Item) => {
                document.flush();
                document.gap = false;
                let marker = match lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number = number.saturating_add(1);
                        marker
                    }
                    _ => "- ".to_string(),
                };
                document.prefix.push(Prefix::Item {
                    marker,
                    before: document.emitted,
                });
            }
            Event::End(TagEnd::Item) => {
                document.flush();
                if matches!(document.prefix.last(), Some(Prefix::Item { before, .. })
                    if *before == document.emitted)
                {
                    // Only an item that emitted no content needs a marker-only
                    // row. Ordinary empty flushes must not manufacture rows.
                    document.text("", style);
                    document.flush();
                }
                document.prefix.pop();
                document.gap = false;
            }
            Event::End(TagEnd::Paragraph) => {
                document.flush();
                document.gap = true;
            }
            Event::Start(Tag::HtmlBlock) => {
                document.flush();
                document.separate();
            }
            Event::Html(text) => {
                // CRLF normalization may split content and its newline into
                // separate Html events. Only an actual newline ends a row.
                document.html(&text, style);
            }
            Event::End(TagEnd::HtmlBlock) => {
                document.flush();
                document.gap = true;
            }
            Event::Text(text) | Event::InlineHtml(text) => document.text(&text, style),
            Event::Code(text) => document.text(&text, style.patch(CODE)),
            Event::SoftBreak => document.text(" ", style),
            Event::HardBreak => document.flush(),
            Event::Rule => {
                document.text("---", QUIET);
                document.flush();
                document.gap = true;
            }
            // Extensions are deliberately disabled: tables, task markers,
            // math, etc. remain ordinary readable source, not hidden nodes.
            _ => {}
        }
    }
    document.flush();
    document
}

#[derive(Default)]
struct Document {
    lines: Vec<Logical>,
    spans: Vec<Span<'static>>,
    prefix: Vec<Prefix>,
    gap: bool,
    width: usize,
    /// Content rows emitted, excluding paragraph separators. Items remember
    /// this count rather than updating every ancestor on each emission.
    emitted: usize,
}

enum Prefix {
    Quote,
    Item { marker: String, before: usize },
}

struct Logical {
    spans: Vec<Span<'static>>,
    prefix: String,
    continuation: String,
    code: bool,
}

impl Document {
    fn separate(&mut self) {
        if self.gap && !self.lines.is_empty() {
            self.lines.push(Logical {
                spans: Vec::new(),
                prefix: String::new(),
                continuation: String::new(),
                code: false,
            });
        }
        self.gap = false;
    }

    fn text(&mut self, text: &str, style: Style) {
        self.separate();
        // Raw HTML may carry newlines. They are not terminal commands and
        // must not be passed to a single ratatui span as control characters.
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.flush();
            }
            self.spans.push(Span::styled(part.to_string(), style));
        }
    }

    fn html(&mut self, text: &str, style: Style) {
        self.separate();
        for part in text.split_inclusive('\n') {
            self.spans.push(Span::styled(
                part.strip_suffix('\n').unwrap_or(part).to_string(),
                style,
            ));
            if part.ends_with('\n') {
                self.flush();
            }
        }
    }

    fn prefixes(&self) -> (String, String) {
        let mut first = String::new();
        let mut rest = String::new();
        for prefix in &self.prefix {
            // Invisible nesting must cost neither retained prefix bytes nor
            // a full-depth walk on every lazy continuation or hard break.
            if first.len() == self.width {
                break;
            }
            let remaining = self.width - first.len();
            let (marker, initial) = match prefix {
                Prefix::Quote => ("> ", true),
                Prefix::Item { marker, before } => (marker.as_str(), *before == self.emitted),
            };
            // All container markers are ASCII. Both strings have equal cell
            // counts and are bounded before they enter the logical-line list.
            let marker = &marker[..marker.len().min(remaining)];
            if initial {
                first.push_str(marker);
            } else {
                first.extend(std::iter::repeat_n(' ', marker.len()));
            }
            if matches!(prefix, Prefix::Quote) {
                rest.push_str(marker);
            } else {
                rest.extend(std::iter::repeat_n(' ', marker.len()));
            }
        }
        (first, rest)
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let (prefix, continuation) = self.prefixes();
        self.emitted += 1;
        self.lines.push(Logical {
            spans: std::mem::take(&mut self.spans),
            prefix,
            continuation,
            code: false,
        });
    }

    fn code(&mut self, language: &str, text: &str) {
        if !language.is_empty() {
            self.text(&format!("│ {language}"), QUIET);
            self.flush();
        }
        for line in text.split('\n') {
            let (mut prefix, mut continuation) = self.prefixes();
            // The gutters add only a constant number of bytes to prefixes
            // already bounded by the viewport; wrap clips the final cue.
            prefix.push_str("│ ");
            continuation.push_str("↪ ");
            self.emitted += 1;
            self.lines.push(Logical {
                spans: vec![Span::raw(line.to_string())],
                prefix,
                continuation,
                code: true,
            });
        }
    }
}

/// Wrap whole graphemes, retaining their styles. Code wraps at the cell edge;
/// prose prefers a space. Spaces are never discarded, even at a wrap boundary.
fn wrap(line: Logical, width: usize, rows: &mut Vec<Line<'static>>) {
    if width == 0 {
        return;
    }
    // Reassemble text before segmentation: an entity or emphasis boundary can
    // split a combining sequence in the parser. A grapheme takes its base's style.
    let mut text = String::new();
    let mut ranges = Vec::new();
    for span in line.spans {
        let start = text.len();
        text.push_str(&span.content);
        ranges.push((start..text.len(), span.style));
    }
    let mut range = 0;
    let mut column = 0;
    let mut glyphs = Vec::new();
    for (offset, grapheme) in text.grapheme_indices(true) {
        while range + 1 < ranges.len() && offset >= ranges[range].0.end {
            range += 1;
        }
        let style = ranges[range].1;
        if grapheme == "\t" {
            let spaces = super::super::editor::TAB_STOP - column % super::super::editor::TAB_STOP;
            for _ in 0..spaces {
                glyphs.push((" ", 1, style));
            }
            column += spaces;
        } else {
            let cells = grapheme_cells(grapheme);
            if cells > 0 {
                glyphs.push((grapheme, cells, style));
                column += cells;
            }
        }
    }
    let mut start = 0;
    let mut first = true;
    loop {
        let prefix = if first {
            &line.prefix
        } else {
            &line.continuation
        };
        // Leave room for a wide grapheme if possible, even inside nested lists.
        let next_cells = glyphs.get(start).map_or(0, |glyph| glyph.1);
        let budget = width.saturating_sub(next_cells);
        // Prefixes contain only ASCII and the one-cell code gutter symbols.
        let prefix: String = prefix.chars().take(budget).collect();
        let available = width - prefix.chars().count();
        let mut end = start;
        let mut used = 0;
        let mut space = None;
        while end < glyphs.len() {
            let (_, cells, _) = glyphs[end];
            if used + cells > available {
                break;
            }
            used += cells;
            if glyphs[end].0 == " " {
                space = Some(end + 1);
            }
            end += 1;
        }
        if !line.code
            && end < glyphs.len()
            && let Some(at) = space
        {
            end = at;
        }
        let mut spans = vec![Span::styled(prefix, QUIET)];
        if end == start && start < glyphs.len() {
            // A two-cell grapheme cannot fit on a one-cell terminal. Keep its
            // place with a replacement glyph; the stored source is untouched.
            spans.push(Span::styled("�", glyphs[start].2));
            end += 1;
        } else {
            for &(text, _, style) in &glyphs[start..end] {
                append(&mut spans, text, style);
            }
        }
        rows.push(Line::from(spans));
        if end == glyphs.len() {
            break;
        }
        start = end;
        first = false;
    }
}

fn append(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut().filter(|span| span.style == style) {
        last.content.to_mut().push_str(text);
    } else {
        spans.push(Span::styled(text.to_string(), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn texts(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(ToString::to_string).collect()
    }

    fn style_of(rows: &[Line<'_>], text: &str) -> Style {
        rows.iter()
            .flat_map(|row| &row.spans)
            .find(|span| span.content == text)
            .expect(text)
            .style
    }

    #[test]
    fn pathological_containers_and_oversized_messages_fall_back_before_parsing() {
        let depth = 64_000;
        let nested_list: String = (0..CONTAINER_PREFIX_CELLS)
            .map(|depth| format!("{}- **item**\n", "  ".repeat(depth)))
            .collect();
        let nested_ordered: String = (0..CONTAINER_PREFIX_CELLS)
            .map(|depth| format!("{}1. **item**\n", "   ".repeat(depth)))
            .collect();
        assert!(nested_list.len() < MARKDOWN_BYTES);
        assert!(nested_ordered.len() < MARKDOWN_BYTES);
        let sources = [
            "> ".repeat(depth) + &"a  \n".repeat(depth),
            "- ".repeat(depth) + &"a  \n".repeat(depth),
            // Below the byte budget: these must trip the prefix guard alone.
            ">".repeat(CONTAINER_PREFIX_CELLS + 1) + " **literal**",
            nested_list,
            nested_ordered,
            "**literal** ".repeat(MARKDOWN_BYTES / 12 + 1),
        ];
        for source in &sources {
            assert!(!within_markdown_budget(source));
            // The fallback keeps the entire source, with no parser-generated
            // markers or styles. This is deterministic, not a timing assertion.
            let document = parse(source, 80);
            let logical: Vec<String> = document
                .lines
                .iter()
                .map(|line| {
                    assert!(line.prefix.is_empty());
                    assert!(line.continuation.is_empty());
                    line.spans
                        .iter()
                        .map(|span| {
                            assert_eq!(span.style, Style::new());
                            span.content.as_ref()
                        })
                        .collect()
                })
                .collect();
            assert_eq!(logical.join("\n"), *source);
        }
        for source in &sources[..2] {
            for row in lines(source, 80) {
                assert!(row.width() <= 80);
                assert!(
                    row.spans
                        .iter()
                        .filter(|span| !span.content.is_empty())
                        .all(|span| span.style == Style::new())
                );
            }
        }
    }

    #[test]
    fn preflight_rejects_deep_containers_after_cr_and_mixed_line_endings() {
        let depth = 40_000;
        let attack = "hello\r".to_string() + &"> ".repeat(depth) + &"a  \r".repeat(depth);
        let mixed_attack = "hello\r\nordinary\nanother\r".to_string()
            + &"> ".repeat(depth)
            + &"a  \r\nb  \nc  \r".repeat(10_000);
        let mut sources = vec![attack, mixed_attack];
        for ending in ["\n", "\r\n", "\r"] {
            for continuation in ["\n", "\r\n", "\r"] {
                for marker in ["> ", "- ", "1. "] {
                    sources.push(format!(
                        "ordinary{ending}{}**literal**{continuation}",
                        marker.repeat(CONTAINER_PREFIX_CELLS + 1)
                    ));
                }
            }
        }
        for source in sources {
            // The byte cap must not mask a missed container-depth check.
            assert!(source.len() < MARKDOWN_BYTES);
            assert!(!within_markdown_budget(&source));
            let document = parse(&source, 80);
            let logical: Vec<String> = document
                .lines
                .iter()
                .map(|line| {
                    assert!(line.prefix.is_empty());
                    assert!(line.continuation.is_empty());
                    line.spans
                        .iter()
                        .map(|span| {
                            assert_eq!(span.style, Style::new());
                            span.content.as_ref()
                        })
                        .collect()
                })
                .collect();
            assert_eq!(logical.join("\n"), source);
        }
        // Ordinary messages using any of these endings remain eligible.
        assert!(within_markdown_budget(
            "hello\r> **normal**\r\n- item\nnext"
        ));
    }

    #[test]
    fn markdown_preflight_accepts_normal_prefixes_and_has_explicit_boundaries() {
        for prefix in [
            ">".repeat(CONTAINER_PREFIX_CELLS),
            "- ".repeat(CONTAINER_PREFIX_CELLS / 2),
            "\t".repeat(CONTAINER_PREFIX_CELLS / super::super::super::editor::TAB_STOP),
        ] {
            assert!(within_markdown_budget(&(prefix.clone() + "text")));
            assert!(!within_markdown_budget(&(prefix + " text")));
        }
        assert!(within_markdown_budget(&"a".repeat(MARKDOWN_BYTES)));
        assert!(!within_markdown_budget(&"a".repeat(MARKDOWN_BYTES + 1)));
        assert_eq!(texts(&lines("> - **normal**", 80)), ["> - normal"]);
    }

    #[test]
    fn deep_quote_prefix_storage_is_bounded_by_viewport_width() {
        // About 48 KiB of source used to retain roughly 256 MiB of prefixes.
        // Inspect logical lines, not just clipped output, to guard the bound.
        let depth = 8_000;
        for width in [0, 1, 80] {
            let mut document = Document {
                width,
                prefix: (0..depth).map(|_| Prefix::Quote).collect(),
                ..Document::default()
            };
            for _ in 0..depth {
                document.text("a", Style::new());
                document.flush();
            }
            assert_eq!(document.lines.len(), depth);
            let mut retained = 0;
            let mut rows = Vec::new();
            for line in document.lines {
                assert!(line.prefix.len() <= width);
                assert!(line.continuation.len() <= width);
                retained += line.prefix.capacity() + line.continuation.capacity();
                wrap(line, width, &mut rows);
            }
            assert!(retained <= depth * 2 * width.max(8).next_power_of_two());
            assert_eq!(rows.len(), if width == 0 { 0 } else { depth });
            for row in rows {
                assert!(row.width() <= width);
                assert!(row.to_string().ends_with('a'));
            }
        }
    }

    #[test]
    fn empty_items_keep_their_markers_without_adding_rows_to_nonempty_items() {
        for (source, expected) in [
            ("-", vec!["- "]),
            ("- a\n-\n- c", vec!["- a", "- ", "- c"]),
            ("1.", vec!["1. "]),
            ("3. a\n4.\n5. c", vec!["3. a", "4. ", "5. c"]),
            ("- a\n  - b\n  -\n- c", vec!["- a", "  - b", "  - ", "- c"]),
            ("- **bold**", vec!["- bold"]),
            ("-\n  - child", vec!["- - child"]),
        ] {
            assert_eq!(texts(&lines(source, 80)), expected, "{source:?}");
        }
        assert_eq!(style_of(&lines("-", 80), "- "), QUIET);
        assert_eq!(texts(&lines("-", 1)), ["-"]);
        assert_eq!(texts(&lines("1.", 2)), ["1."]);
    }

    #[test]
    fn literal_html_preserves_line_adjacency_and_existing_blank_lines() {
        for source in [
            "<div>\nfirst\nsecond\n</div>",
            "<pre>\nfirst\n\nsecond\n</pre>",
            "<!--\nfirst\n\nsecond\n-->",
        ] {
            let rows = lines(source, 80);
            assert_eq!(rows, lines(&source.replace('\n', "\r\n"), 80));
            assert_eq!(rows, lines(&(source.to_string() + "\n"), 80));
            assert_eq!(rows, lines(&(source.replace('\n', "\r\n") + "\r\n"), 80));
            assert_eq!(texts(&rows), source.lines().collect::<Vec<_>>());
            assert!(
                rows.iter()
                    .flat_map(|row| &row.spans)
                    .filter(|span| !span.content.is_empty())
                    .all(|span| span.style == Style::new())
            );
        }
        assert_eq!(
            texts(&lines("before\n\n<div>\nraw\n</div>\n\nafter", 80)),
            ["before", "", "<div>", "raw", "</div>", "", "after"]
        );
    }

    #[test]
    fn inline_styles_are_semantic_and_do_not_spill() {
        let rows = lines("**bold** _italic_ `a_b` plain **_both_**", 80);
        assert_eq!(texts(&rows), ["bold italic a_b plain both"]);
        assert_eq!(style_of(&rows, "bold").add_modifier, Modifier::BOLD);
        assert_eq!(style_of(&rows, "italic").add_modifier, Modifier::ITALIC);
        assert_eq!(style_of(&rows, "a_b").add_modifier, Modifier::REVERSED);
        assert_eq!(style_of(&rows, " plain "), Style::new());
        assert_eq!(
            style_of(&rows, "both").add_modifier,
            Modifier::BOLD | Modifier::ITALIC
        );
        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .all(|span| span.style.fg.is_none() && span.style.bg.is_none())
        );
    }

    #[test]
    fn escaped_unmatched_and_identifier_delimiters_stay_literal() {
        let source = r"\*literal\* foo_bar_baz **unfinished `unfinished";
        let rows = lines(source, 100);
        assert_eq!(
            texts(&rows),
            ["*literal* foo_bar_baz **unfinished `unfinished"]
        );
        assert!(
            rows[0]
                .spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .all(|span| span.style == Style::new())
        );
    }

    #[test]
    fn code_is_literal_and_keeps_indentation_blank_lines_and_trailing_spaces() {
        let rows = lines("```unknown\n  **x**  \n\n\t_y_\n```", 40);
        assert_eq!(
            texts(&rows),
            ["│ unknown", "│   **x**  ", "│ ", "│     _y_"]
        );
        assert_eq!(style_of(&rows, "│ unknown"), QUIET);
        assert_eq!(style_of(&rows, "  **x**  "), Style::new());
        assert_eq!(texts(&lines("```\nfirst\n\n```", 40)), ["│ first", "│ "]);
        assert_eq!(
            texts(&lines("```\nunclosed **code**", 40)),
            ["│ unclosed **code**"]
        );
        assert_eq!(
            texts(&lines("    indented _code_\n", 40)),
            ["│ indented _code_"]
        );
    }

    #[test]
    fn code_wraps_without_deleting_spaces_and_marks_only_continuations() {
        let rows = lines("```\n ab  cd  \nnext\n```", 6);
        assert_eq!(texts(&rows), ["│  ab ", "↪  cd ", "↪  ", "│ next"]);
        let reconstructed: String = rows[..3]
            .iter()
            .map(|row| row.to_string())
            .map(|row| row.chars().skip(2).collect::<String>())
            .collect();
        assert_eq!(reconstructed, " ab  cd  ");
    }

    #[test]
    fn blocks_links_and_unsupported_markup_remain_readable() {
        let rows = lines(
            "# Small heading\n\n> a **quote**\n\n3. first\n4. second\n   - nested\n\n[site](https://example.com) ![alt](pic.png)\n\n<div>raw</div>\n\n|a|b|\n|-|-|",
            100,
        );
        assert_eq!(
            texts(&rows),
            [
                "Small heading",
                "",
                "> a quote",
                "",
                "3. first",
                "4. second",
                "   - nested",
                "",
                "site (https://example.com) alt (pic.png)",
                "",
                "<div>raw</div>",
                "",
                "|a|b| |-|-|"
            ]
        );
        assert_eq!(
            style_of(&rows, "Small heading").add_modifier,
            Modifier::BOLD
        );
        assert_eq!(style_of(&rows, "quote").add_modifier, Modifier::BOLD);
        assert!(
            texts(&lines("[missing][ref] ~~unsupported~~", 80))[0]
                .contains("[missing][ref] ~~unsupported~~")
        );
    }

    #[test]
    fn wrapping_retains_styles_and_graphemes_even_across_parser_events() {
        let rows = lines("**abcd**ef", 3);
        assert_eq!(texts(&rows), ["abc", "def"]);
        assert_eq!(style_of(&rows, "abc").add_modifier, Modifier::BOLD);
        assert_eq!(style_of(&rows, "d").add_modifier, Modifier::BOLD);
        assert_eq!(style_of(&rows, "ef"), Style::new());
        // The combining mark arrives as a separate entity event.
        let rows = lines("**e**&#x301;👩‍💻日本", 3);
        assert_eq!(texts(&rows), ["e\u{301}👩‍💻", "日", "本"]);
        assert_eq!(style_of(&rows, "e\u{301}").add_modifier, Modifier::BOLD);
    }

    #[test]
    fn every_row_fits_the_buffer_at_narrow_widths() {
        for source in [
            "# **日本** e\u{301} 👩‍💻 `code` and a long_word",
            "> - **nested** [link](https://example.com)",
            "```x\n \t日本 e\u{301}👩‍💻  \n\nend\n```",
        ] {
            for width in 0..20u16 {
                let rows = lines(source, usize::from(width));
                let mut buffer = Buffer::empty(Rect::new(0, 0, width, rows.len() as u16));
                for (i, row) in rows.iter().enumerate() {
                    assert!(row.width() <= usize::from(width), "{row:?} at {width}");
                    let (end, _) = buffer.set_line(0, i as u16, row, width);
                    assert_eq!(usize::from(end), row.width(), "{row:?} at {width}");
                }
            }
        }
    }

    #[test]
    fn controls_never_reach_the_terminal_even_in_code_or_links() {
        let source = "**bold**\u{1b}[2J\u{1b}]52;c;clipboard\u{7}\n\n```\n\u{1b}[31mcode\u{9b}0m\n```\n\n[x](https://example.com/&#27;evil)";
        for row in lines(source, 80) {
            for span in row.spans {
                assert!(!span.content.contains(char::is_control), "{span:?}");
            }
        }
    }
}
