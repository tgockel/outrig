//! The input box: what has been typed, where the cursor is, and what each key
//! does to them. Pure state, so every binding is tested without a terminal.
//!
//! The keys are readline's where readline has one, since that is what a
//! terminal user's fingers already know. What differs is what a line is: a
//! message may span lines, so Enter sends the whole box, and a newline is
//! Alt-Enter, or Enter on a line ending in `\` for terminals where Alt does not
//! reach the program. Ctrl-J sends too, whatever the box ends in: a line typed
//! before the screen took the terminal was ended by the terminal's own Enter,
//! which raw mode then reads as Ctrl-J, and each such line is its own message,
//! as it is without the screen.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::cli::log_divert::strip_escapes;
use crate::repl::HISTORY_SIZE;

/// Columns between tab stops, for a Tab typed or pasted.
pub(super) const TAB_STOP: usize = 4;

/// What a key did, for the screen to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Edit {
    /// Nothing.
    None,
    /// The box changed and is drawn again.
    Changed,
    /// The box was sent, and is empty again.
    Submit(String),
    /// Ctrl-D at an empty box: the end of input.
    Eof,
    /// Ctrl-C at an empty box.
    Interrupt,
    /// Ctrl-Z.
    Suspend,
    /// Ctrl-O: the detail view, in or out.
    ToggleDetail,
    /// Ctrl-L: everything drawn again.
    Redraw,
}

#[derive(Debug, Default)]
pub(super) struct Editor {
    text: String,
    /// A byte offset into `text`, always at a grapheme boundary.
    cursor: usize,
    history: Vec<String>,
    /// The entry Up has recalled, and what was being typed before it.
    recalled: Option<(usize, String)>,
}

impl Editor {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Edit {
        if key.kind == KeyEventKind::Release {
            return Edit::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.text.is_empty() {
                    return Edit::Interrupt;
                }
                self.replace(String::new());
            }
            KeyCode::Char('d') if ctrl => {
                if self.text.is_empty() {
                    return Edit::Eof;
                }
                self.delete_forward();
            }
            KeyCode::Char('o') if ctrl => return Edit::ToggleDetail,
            KeyCode::Char('l') if ctrl => return Edit::Redraw,
            KeyCode::Char('z') if ctrl => return Edit::Suspend,
            KeyCode::Char('j') if ctrl => return self.submit(),
            KeyCode::Enter if alt => self.insert("\n"),
            KeyCode::Enter => return self.enter(),
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('b') if ctrl => self.cursor = self.previous(self.cursor),
            KeyCode::Char('f') if ctrl => self.cursor = self.next(self.cursor),
            KeyCode::Char('b') if alt => self.cursor = self.word_start(),
            KeyCode::Char('f') if alt => self.cursor = self.word_end(),
            KeyCode::Char('w') if ctrl => self.delete_to(self.word_start()),
            KeyCode::Backspace if alt => self.delete_to(self.word_start()),
            KeyCode::Char('u') if ctrl => self.delete_to(self.line_start()),
            KeyCode::Char('k') if ctrl => self.kill_to_line_end(),
            KeyCode::Char('h') if ctrl => self.delete_to(self.previous(self.cursor)),
            KeyCode::Backspace => self.delete_to(self.previous(self.cursor)),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Char('p') if ctrl => self.recall_older(),
            KeyCode::Char('n') if ctrl => self.recall_newer(),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_start(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_end(),
            KeyCode::Left => self.cursor = self.previous(self.cursor),
            KeyCode::Right => self.cursor = self.next(self.cursor),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up if self.line_start() == 0 => self.recall_older(),
            KeyCode::Up => self.line_up(),
            KeyCode::Down if self.line_end() == self.text.len() => self.recall_newer(),
            KeyCode::Down => self.line_down(),
            KeyCode::Tab => return self.paste("\t"),
            KeyCode::Char(c) if !ctrl && !alt => self.insert(c.encode_utf8(&mut [0; 4])),
            _ => return Edit::None,
        }
        Edit::Changed
    }

    /// Insert pasted text at the cursor, as one edit: its line endings as
    /// newlines, its tabs as spaces, and nothing else a terminal would act on.
    pub(super) fn paste(&mut self, pasted: &str) -> Edit {
        let pasted = strip_escapes(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        let mut column = self.text[self.line_start()..self.cursor].chars().count();
        let mut clean = String::with_capacity(pasted.len());
        for c in pasted.chars() {
            match c {
                '\n' => {
                    clean.push('\n');
                    column = 0;
                }
                '\t' => {
                    let spaces = TAB_STOP - column % TAB_STOP;
                    clean.extend(std::iter::repeat_n(' ', spaces));
                    column += spaces;
                }
                c if c.is_control() => {}
                c => {
                    clean.push(c);
                    column += 1;
                }
            }
        }
        if clean.is_empty() {
            return Edit::None;
        }
        self.insert(&clean);
        Edit::Changed
    }

    /// Enter: a newline in place of a `\` that ends the box, or the box sent.
    fn enter(&mut self) -> Edit {
        if self.cursor == self.text.len() && self.text.ends_with('\\') {
            self.text.pop();
            self.cursor = self.text.len();
            self.insert("\n");
            return Edit::Changed;
        }
        self.submit()
    }

    /// The box sent. An empty box sends nothing; one holding only spaces is
    /// sent, as a line of them is without the screen.
    fn submit(&mut self) -> Edit {
        if self.text.is_empty() {
            return Edit::None;
        }
        let sent = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.recalled = None;
        if self.history.last() != Some(&sent) {
            if self.history.len() == HISTORY_SIZE {
                self.history.remove(0);
            }
            self.history.push(sent.clone());
        }
        Edit::Submit(sent)
    }

    fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.recalled = None;
    }

    fn replace(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    /// Delete from `to` up to the cursor, or from the cursor up to `to`.
    fn delete_to(&mut self, to: usize) {
        let range = to.min(self.cursor)..to.max(self.cursor);
        if !range.is_empty() {
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
            self.recalled = None;
        }
    }

    fn delete_forward(&mut self) {
        self.delete_to(self.next(self.cursor));
    }

    /// Ctrl-K: the rest of the line, or the newline ending it when the cursor
    /// is at its end, joining the next.
    fn kill_to_line_end(&mut self) {
        let end = self.line_end();
        if end == self.cursor {
            self.delete_forward();
        } else {
            self.delete_to(end);
        }
    }

    /// The grapheme boundary before `at`.
    fn previous(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// The grapheme boundary after `at`.
    fn next(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |grapheme| at + grapheme.len())
    }

    /// Where the word before the cursor starts, past any space before it.
    fn word_start(&self) -> usize {
        let mut at = self.cursor;
        let mut seen_word = false;
        for (i, grapheme) in self.text[..self.cursor].grapheme_indices(true).rev() {
            let space = grapheme.chars().all(char::is_whitespace);
            if space && seen_word {
                break;
            }
            seen_word |= !space;
            at = i;
        }
        at
    }

    /// Where the word after the cursor ends, past any space before it.
    fn word_end(&self) -> usize {
        let mut at = self.cursor;
        let mut seen_word = false;
        for grapheme in self.text[self.cursor..].graphemes(true) {
            let space = grapheme.chars().all(char::is_whitespace);
            if space && seen_word {
                break;
            }
            seen_word |= !space;
            at += grapheme.len();
        }
        at
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    /// How many graphemes into its line the cursor is.
    fn column(&self) -> usize {
        self.text[self.line_start()..self.cursor]
            .graphemes(true)
            .count()
    }

    /// The boundary `column` graphemes into the line starting at `start`, or
    /// that line's end.
    fn at_column(&self, start: usize, column: usize) -> usize {
        let line = &self.text[start..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        line.grapheme_indices(true)
            .nth(column)
            .map_or(start + line.len(), |(i, _)| start + i)
    }

    fn line_up(&mut self) {
        let column = self.column();
        let above = self.line_start() - 1;
        let start = self.text[..above].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = self.at_column(start, column);
    }

    fn line_down(&mut self) {
        let column = self.column();
        let start = self.line_end() + 1;
        self.cursor = self.at_column(start, column);
    }

    /// Up at the first line: the message sent before the one shown, keeping
    /// what was being typed for Down to bring back.
    fn recall_older(&mut self) {
        let index = match &self.recalled {
            Some((0, _)) => return,
            Some((index, _)) => index - 1,
            None if self.history.is_empty() => return,
            None => self.history.len() - 1,
        };
        let draft = match self.recalled.take() {
            Some((_, draft)) => draft,
            None => self.text.clone(),
        };
        self.replace(self.history[index].clone());
        self.recalled = Some((index, draft));
    }

    /// Down at the last line: the message sent after the one shown, or what
    /// was being typed before Up.
    fn recall_newer(&mut self) {
        let Some((index, draft)) = self.recalled.take() else {
            return;
        };
        if index + 1 < self.history.len() {
            self.replace(self.history[index + 1].clone());
            self.recalled = Some((index + 1, draft));
        } else {
            self.replace(draft);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            assert_eq!(editor.key(press(KeyCode::Char(c))), Edit::Changed);
        }
    }

    /// The box as text with `|` at the cursor.
    fn shown(editor: &Editor) -> String {
        let mut text = editor.text.clone();
        text.insert(editor.cursor, '|');
        text
    }

    #[test]
    fn enter_and_ctrl_j_send_the_box_and_empty_it() {
        let mut editor = Editor::default();
        typed(&mut editor, "hello");
        assert_eq!(
            editor.key(press(KeyCode::Enter)),
            Edit::Submit("hello".into())
        );
        assert_eq!(shown(&editor), "|");
        typed(&mut editor, "typed ahead");
        assert_eq!(editor.key(ctrl('j')), Edit::Submit("typed ahead".into()));
        // A line typed ahead is sent whole, as it would be without the
        // screen: its `\` is not a newline's.
        typed(&mut editor, "C:\\");
        assert_eq!(editor.key(ctrl('j')), Edit::Submit("C:\\".into()));
    }

    /// Only an empty box sends nothing: spaces are sent, as a line of them is
    /// without the screen.
    #[test]
    fn an_empty_box_sends_nothing_and_spaces_are_sent() {
        let mut editor = Editor::default();
        assert_eq!(editor.key(press(KeyCode::Enter)), Edit::None);
        typed(&mut editor, "  ");
        assert_eq!(editor.key(press(KeyCode::Enter)), Edit::Submit("  ".into()));
    }

    #[test]
    fn alt_enter_and_a_trailing_backslash_make_a_newline() {
        let mut editor = Editor::default();
        typed(&mut editor, "one");
        assert_eq!(editor.key(alt(KeyCode::Enter)), Edit::Changed);
        typed(&mut editor, "two\\");
        assert_eq!(editor.key(press(KeyCode::Enter)), Edit::Changed);
        typed(&mut editor, "three");
        assert_eq!(shown(&editor), "one\ntwo\nthree|");
        assert_eq!(
            editor.key(press(KeyCode::Enter)),
            Edit::Submit("one\ntwo\nthree".into())
        );

        // A backslash with text after the cursor is not the end of the box.
        typed(&mut editor, "a\\b");
        editor.key(press(KeyCode::Left));
        assert_eq!(
            editor.key(press(KeyCode::Enter)),
            Edit::Submit("a\\b".into())
        );
    }

    /// Ctrl-C clears what was typed, and only at an empty box is it the
    /// interrupt; Ctrl-D ends input only at an empty box.
    #[test]
    fn ctrl_c_clears_first_and_ctrl_d_deletes_first() {
        let mut editor = Editor::default();
        typed(&mut editor, "draft");
        assert_eq!(editor.key(ctrl('c')), Edit::Changed);
        assert_eq!(shown(&editor), "|");
        assert_eq!(editor.key(ctrl('c')), Edit::Interrupt);

        typed(&mut editor, "ab");
        editor.key(ctrl('a'));
        assert_eq!(editor.key(ctrl('d')), Edit::Changed);
        assert_eq!(shown(&editor), "|b");
        editor.key(ctrl('d'));
        assert_eq!(editor.key(ctrl('d')), Edit::Eof);
    }

    #[test]
    fn other_control_keys_reach_the_screen() {
        let mut editor = Editor::default();
        assert_eq!(editor.key(ctrl('o')), Edit::ToggleDetail);
        assert_eq!(editor.key(ctrl('l')), Edit::Redraw);
        assert_eq!(editor.key(ctrl('z')), Edit::Suspend);
        assert_eq!(editor.key(ctrl('x')), Edit::None);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char('x'))
        };
        assert_eq!(editor.key(release), Edit::None);
    }

    /// The cursor moves and deletes by grapheme, so an accented letter built
    /// from two code points, or a wide one, goes in one step.
    #[test]
    fn the_cursor_moves_by_grapheme() {
        let mut editor = Editor::default();
        editor.paste("e\u{301}x日");
        editor.key(press(KeyCode::Left));
        assert_eq!(shown(&editor), "e\u{301}x|日");
        editor.key(press(KeyCode::Left));
        editor.key(press(KeyCode::Left));
        assert_eq!(shown(&editor), "|e\u{301}x日");
        editor.key(press(KeyCode::Delete));
        assert_eq!(shown(&editor), "|x日");
        editor.key(press(KeyCode::End));
        editor.key(press(KeyCode::Backspace));
        assert_eq!(shown(&editor), "x|");
        editor.key(ctrl('h'));
        assert_eq!(shown(&editor), "|");
    }

    #[test]
    fn word_and_line_keys_follow_readline() {
        let mut editor = Editor::default();
        typed(&mut editor, "run the  tests");
        editor.key(alt(KeyCode::Char('b')));
        assert_eq!(shown(&editor), "run the  |tests");
        editor.key(alt(KeyCode::Char('b')));
        assert_eq!(shown(&editor), "run |the  tests");
        editor.key(alt(KeyCode::Char('f')));
        assert_eq!(shown(&editor), "run the|  tests");
        editor.key(ctrl('w'));
        assert_eq!(shown(&editor), "run |  tests");
        editor.key(ctrl('k'));
        assert_eq!(shown(&editor), "run |");
        editor.key(ctrl('u'));
        assert_eq!(shown(&editor), "|");

        typed(&mut editor, "a b");
        editor.key(alt(KeyCode::Backspace));
        assert_eq!(shown(&editor), "a |");
        editor.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(shown(&editor), "|a ");
    }

    /// In a box of several lines, Home, End, Ctrl-U and Ctrl-K keep to the
    /// cursor's line, and Ctrl-K at a line's end joins the next.
    #[test]
    fn line_keys_keep_to_the_cursors_line() {
        let mut editor = Editor::default();
        editor.paste("first\nsecond\nthird");
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "first\nsecon|d\nthird");
        editor.key(press(KeyCode::Home));
        assert_eq!(shown(&editor), "first\n|second\nthird");
        editor.key(press(KeyCode::End));
        editor.key(ctrl('k'));
        assert_eq!(shown(&editor), "first\nsecond|third");
        editor.key(ctrl('u'));
        assert_eq!(shown(&editor), "first\n|third");
    }

    /// Up and Down move between lines of the box, keeping the column, and
    /// recall history only from its first and last lines.
    #[test]
    fn up_and_down_move_between_lines_before_recalling() {
        let mut editor = Editor::default();
        typed(&mut editor, "sent");
        editor.key(press(KeyCode::Enter));
        editor.paste("long line\nab\nlonger line");
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "long line\nab|\nlonger line");
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "lo|ng line\nab\nlonger line");
        editor.key(press(KeyCode::Down));
        editor.key(press(KeyCode::Down));
        assert_eq!(shown(&editor), "long line\nab\nlo|nger line");

        editor.key(press(KeyCode::Up));
        editor.key(press(KeyCode::Up));
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "sent|");
    }

    /// Up recalls what was sent, newest first, and Down past the newest brings
    /// back what was being typed. Ctrl-P and Ctrl-N do the same from any line.
    #[test]
    fn history_recall_keeps_the_draft() {
        let mut editor = Editor::default();
        for line in ["one", "two"] {
            typed(&mut editor, line);
            editor.key(press(KeyCode::Enter));
        }
        typed(&mut editor, "dra");
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "two|");
        editor.key(ctrl('p'));
        assert_eq!(shown(&editor), "one|");
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "one|", "the oldest stays");
        editor.key(ctrl('n'));
        assert_eq!(shown(&editor), "two|");
        editor.key(press(KeyCode::Down));
        assert_eq!(shown(&editor), "dra|");

        // Sending the same message twice keeps one entry for it.
        typed(&mut editor, "w");
        editor.key(press(KeyCode::Enter));
        typed(&mut editor, "draw");
        editor.key(press(KeyCode::Enter));
        editor.key(press(KeyCode::Up));
        editor.key(press(KeyCode::Up));
        assert_eq!(shown(&editor), "two|");
    }

    /// A paste is one edit: its line endings become newlines, its tabs
    /// spaces to the next stop, and escapes and other control characters go.
    #[test]
    fn a_paste_is_cleaned_and_inserted_whole() {
        let mut editor = Editor::default();
        typed(&mut editor, "x");
        assert_eq!(editor.paste("a\tb\r\nc\rd\x1b[31m!\x07"), Edit::Changed);
        assert_eq!(shown(&editor), "xa  b\nc\nd!|");
        assert_eq!(editor.paste("\x1b[0m"), Edit::None);
    }

    #[test]
    fn tab_inserts_spaces_to_the_next_stop() {
        let mut editor = Editor::default();
        typed(&mut editor, "ab");
        editor.key(press(KeyCode::Tab));
        assert_eq!(shown(&editor), "ab  |");
        editor.key(press(KeyCode::Tab));
        assert_eq!(shown(&editor), "ab      |");
    }
}
