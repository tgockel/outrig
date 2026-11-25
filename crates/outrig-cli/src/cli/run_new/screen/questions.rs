//! Modal question state shared by the keyboard and drawing threads.
//! No terminal I/O, and no answer encoded as a command or ordinary user text.
use outrig::harness::{Answer, Question, Settle};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};

use super::chooser::List;
use super::editor::Editor;
use super::render;
use crate::cli::log_divert::strip_escapes;

#[derive(Default)]
pub(super) struct Dialogs {
    open: Vec<Pending>,
    active: Option<u64>,
    aside: bool,
    /// Last rendered width; paging has no visible position before the first layout.
    width: Option<u16>,
}
struct Pending {
    id: u64,
    asker: String,
    question: Question,
    focus: usize,
    selected: Vec<bool>,
    other: Option<String>,
    field: Option<Field>,
    submitting: bool,
    browse: isize,
}
struct Field {
    decline: bool,
    editor: Editor,
}
impl Pending {
    fn new(id: u64, asker: String, question: Question) -> Self {
        let field = question.choices.is_empty().then(|| Field {
            decline: false,
            editor: Editor::default(),
        });
        Self {
            id,
            asker,
            selected: vec![false; question.choices.len()],
            question,
            focus: 0,
            other: None,
            field,
            submitting: false,
            browse: 0,
        }
    }
    fn reply(&self) -> Settle {
        Settle::Reply(
            serde_json::to_value(Answer {
                choices: self
                    .question
                    .choices
                    .iter()
                    .zip(&self.selected)
                    .filter(|(_, selected)| **selected)
                    .map(|(c, _)| c.label.clone())
                    .collect(),
                text: self.other.clone(),
                by: "user".into(),
                chain: Vec::new(),
            })
            .expect("answer JSON"),
        )
    }
    fn choose(&mut self, n: usize) -> Option<Settle> {
        if n >= self.selected.len() {
            return None;
        }
        self.focus = n;
        if self.question.multiple {
            self.selected[n] = !self.selected[n];
            None
        } else {
            self.selected.fill(false);
            self.selected[n] = true;
            Some(self.reply())
        }
    }
}
impl Dialogs {
    pub fn asked(&mut self, id: u64, asker: String, q: Question, draft_empty: bool) {
        if self.open.iter().any(|p| p.id == id) {
            return;
        }
        self.open.push(Pending::new(id, asker, q));
        self.present(draft_empty);
    }
    pub fn closed(&mut self, id: u64, draft_empty: bool) {
        let was_active = self.active == Some(id);
        self.open.retain(|p| p.id != id);
        if was_active {
            self.active = None;
        }
        if self.open.is_empty() {
            self.aside = false;
        }
        self.present(was_active || draft_empty);
    }
    pub fn retry(&mut self, id: u64, draft_empty: bool) {
        if let Some(p) = self.open.iter_mut().find(|p| p.id == id) {
            p.submitting = false;
        }
        self.present(draft_empty);
    }
    pub fn sent(&mut self) {
        self.aside = false;
        self.present(true);
    }
    fn present(&mut self, draft_empty: bool) {
        if self.active.is_none() && draft_empty && !self.aside {
            self.active = self.open.iter().find(|p| !p.submitting).map(|p| p.id);
        }
    }
    pub fn visible(&self) -> bool {
        self.active.is_some()
    }
    pub fn hint(&self) -> Option<String> {
        let p = self.open.iter().find(|p| !p.submitting)?;
        Some(format!(
            "{} has asked {} question{} · Ctrl-Q answer",
            safe(&p.asker),
            self.open.len(),
            if self.open.len() == 1 { "" } else { "s" }
        ))
    }
    pub fn paste(&mut self, text: &str) {
        if let Some(p) = self.open.iter_mut().find(|p| Some(p.id) == self.active)
            && let Some(field) = &mut p.field
        {
            p.browse = 0;
            field.editor.paste(text);
        }
    }
    /// Global control keys are dispatched by the keyboard before this method.
    pub fn key(&mut self, mut key: KeyEvent) -> Option<(u64, Settle)> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // Newline bytes (including typed-ahead input) arrive as Ctrl-J. Confirm
        // before Editor::submit can take the text out of the field.
        if key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL) {
            key.code = KeyCode::Enter;
            key.modifiers
                .remove(KeyModifiers::CONTROL | KeyModifiers::ALT);
        }
        let index = self.open.iter().position(|p| Some(p.id) == self.active)?;
        if key.code == KeyCode::Esc {
            self.active = None;
            self.aside = true;
            return None;
        }
        // Switching works in a field too; literal tab belongs to neither field.
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let step =
                if key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.open.len() - 1
                } else {
                    1
                };
            for n in 1..=self.open.len() {
                let next = (index + step * n) % self.open.len();
                if !self.open[next].submitting {
                    self.active = Some(self.open[next].id);
                    break;
                }
            }
            return None;
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            // Reconcile against the latest wrapped bounds before *each* key,
            // including repeat bursts that arrive between drawing frames.
            let view = self.view(self.width?)?;
            let row = view.list.focus;
            let next = if key.code == KeyCode::PageUp {
                row.saturating_sub(1)
            } else {
                (row + 1).min(view.list.rows.len().saturating_sub(1))
            };
            self.open[index].browse += next as isize - row as isize;
            return None;
        }
        let p = &mut self.open[index];
        // Any field edit or cursor movement returns to its caret. Keep paging
        // deliberate, but never accept text into an invisible field.
        p.browse = 0;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n') => {
                    let id = p.id;
                    p.submitting = true;
                    self.active = None;
                    self.present(true);
                    return Some((id, Settle::Fail("not relevant".into())));
                }
                KeyCode::Char('r') => {
                    p.field = Some(Field {
                        decline: true,
                        editor: Editor::default(),
                    });
                    return None;
                }
                _ => {}
            }
        }
        let result = if let Some(field) = &mut p.field {
            if key.code == KeyCode::Char('b')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && (field.decline || !p.question.choices.is_empty())
            {
                p.field = p.question.choices.is_empty().then(|| Field {
                    decline: false,
                    editor: Editor::default(),
                });
                return None;
            }
            if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::ALT) {
                let text = field.editor.text().to_string();
                if field.decline {
                    Some(Settle::Fail(text))
                } else if p.question.multiple && !p.question.choices.is_empty() {
                    // An empty edit removes previously committed Other text;
                    // the offered choices (including none) are still valid.
                    p.other = (!text.trim().is_empty()).then_some(text);
                    p.field = None;
                    p.focus = 0;
                    None
                } else if text.trim().is_empty() {
                    None
                } else {
                    p.other = Some(text);
                    p.selected.fill(false);
                    Some(p.reply())
                }
            } else {
                // Ctrl-D/C in a field must not end input or clear a draft.
                let _ = field.editor.key(key);
                None
            }
        } else {
            let count = p.question.choices.len() + usize::from(p.question.other);
            match key.code {
                KeyCode::Up => {
                    p.focus = p.focus.saturating_sub(1);
                    None
                }
                KeyCode::Down => {
                    p.focus = (p.focus + 1).min(count.saturating_sub(1));
                    None
                }
                KeyCode::Char(c @ '1'..='9') => p.choose((c as u8 - b'1') as usize),
                KeyCode::Char(' ') if p.focus < p.selected.len() => {
                    if p.question.multiple {
                        p.selected[p.focus] = !p.selected[p.focus];
                    }
                    None
                }
                KeyCode::Enter | KeyCode::Char(' ')
                    if p.focus == p.selected.len() && p.question.other =>
                {
                    let mut editor = Editor::default();
                    if let Some(text) = &p.other {
                        editor.paste(text);
                    }
                    p.field = Some(Field {
                        decline: false,
                        editor,
                    });
                    None
                }
                KeyCode::Enter if p.question.multiple => Some(p.reply()),
                KeyCode::Enter => p.choose(p.focus),
                _ => None,
            }
        };
        let id = p.id;
        if let Some(answer) = result {
            p.submitting = true;
            self.active = None;
            self.present(true);
            Some((id, answer))
        } else {
            None
        }
    }
    pub fn view(&mut self, width: u16) -> Option<View> {
        self.width = Some(width);
        let index = self.open.iter().position(|p| Some(p.id) == self.active)?;
        let outstanding = self.open.len();
        let p = &mut self.open[index];
        let width = usize::from(width).max(1);
        let mut list = List::default();
        list.rows.extend(render::wrap(&p.question.text, width));
        if let Some(context) = &p.question.context {
            list.rows.extend(render::wrap(&context.description, width));
            for r in &context.references {
                let at = r.line.map_or(String::new(), |n| format!(":{n}"));
                let end = r.end_line.map_or(String::new(), |n| format!("-{n}"));
                list.rows.extend(render::wrap(
                    &format!("{}{at}{end} {}", r.path, r.description),
                    width,
                ));
            }
        }
        for (n, c) in p.question.choices.iter().enumerate() {
            if n == p.focus {
                list.focus = list.rows.len();
            }
            let marker = if n == p.focus { ">" } else { " " };
            let selected = if p.selected[n] { "x" } else { " " };
            list.rows.extend(render::wrap(
                &format!(
                    "{marker} [{selected}] {}. {}{}",
                    n + 1,
                    c.label,
                    if c.description.is_empty() {
                        String::new()
                    } else {
                        format!(" -- {}", c.description)
                    }
                ),
                width,
            ));
        }
        if p.question.other && !p.question.choices.is_empty() {
            if p.focus == p.selected.len() {
                list.focus = list.rows.len();
            }
            list.rows.extend(render::wrap(
                &format!(
                    "{} Other{}",
                    if p.focus == p.selected.len() {
                        ">"
                    } else {
                        " "
                    },
                    p.other.as_ref().map_or(String::new(), |t| format!(": {t}"))
                ),
                width,
            ));
        }
        if let Some(field) = &p.field {
            list.rows.push(
                if field.decline {
                    "Decline reason:"
                } else {
                    "Your answer:"
                }
                .into(),
            );
            let layout = render::field_layout(&field.editor, width as u16);
            let start = list.rows.len();
            list.rows.extend(layout.rows);
            list.focus = start + layout.cursor.0;
            list.cursor = Some((list.focus, layout.cursor.1));
        }
        let anchor = list.focus;
        list.focus = anchor
            .saturating_add_signed(p.browse)
            .min(list.rows.len().saturating_sub(1));
        // Store the clamped offset as well. A resize can reduce the bounds even
        // with no paging key, and must not leave reverse-navigation debt.
        p.browse = list.focus as isize - anchor as isize;
        Some(View {
            title: format!(
                "Question {} of {} · {} · #{}",
                index + 1,
                outstanding,
                safe(&p.asker),
                p.id
            ),
            footer: render::wrap(
                if p.field.is_some() {
                    "Esc aside · Enter confirm · Tab next · Ctrl-N not relevant · Ctrl-R decline · Ctrl-B choices"
                } else {
                    "Esc aside · Enter confirm · Tab next · Ctrl-N not relevant · Ctrl-R decline · ↑↓/1-9 choose · Space toggle · PgUp/PgDn scroll"
                },
                width,
            ),
            list,
        })
    }
}
fn safe(text: &str) -> String {
    strip_escapes(text)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
pub(super) struct View {
    title: String,
    footer: Vec<String>,
    list: List,
}
impl View {
    pub fn height(&self) -> u16 {
        (self.list.rows.len() + 2 + self.footer.len().min(2)).min(render::BOX_ROWS + 1) as u16
    }
    /// The caller draws the status row; this replaces only the input box.
    pub fn draw(&self, buf: &mut Buffer) -> Option<Position> {
        let area = buf.area;
        if area.height <= 1 || area.width == 0 {
            return None;
        }
        let style = ratatui::style::Style::default();
        buf.set_stringn(
            area.x,
            area.y + 1,
            &self.title,
            usize::from(area.width),
            style,
        );
        if area.height <= 2 {
            return None;
        }
        // Always leave a body row when the terminal has room for one.
        let footer_rows = self
            .footer
            .len()
            .min(2)
            .min(usize::from(area.height.saturating_sub(3)));
        let footer_y = area.bottom() - footer_rows as u16;
        for (n, row) in self.footer.iter().take(footer_rows).enumerate() {
            buf.set_stringn(
                area.x,
                footer_y + n as u16,
                row,
                usize::from(area.width),
                style,
            );
        }
        self.list.draw(
            buf,
            Rect::new(
                area.x,
                area.y + 2,
                area.width,
                footer_y.saturating_sub(area.y + 2),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use outrig::harness::Choice;
    fn q(multiple: bool, other: bool) -> Question {
        Question {
            text: "Which?".into(),
            choices: vec![
                Choice {
                    label: "one".into(),
                    description: "first".into(),
                },
                Choice {
                    label: "two".into(),
                    description: "second".into(),
                },
            ],
            multiple,
            other,
            context: None,
        }
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn opened(question: Question) -> Dialogs {
        let mut d = Dialogs::default();
        d.asked(1, "main".into(), question, true);
        d
    }
    fn answer(result: Option<(u64, Settle)>) -> Answer {
        let (_, Settle::Reply(value)) = result.expect("answered") else {
            panic!("not a reply")
        };
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn arrival_respects_drafts_and_aside_until_submit_or_explicit_reopen() {
        let mut d = Dialogs::default();
        d.asked(1, "main".into(), q(false, false), false);
        assert!(!d.visible());
        assert!(d.hint().unwrap().contains("main has asked 1 question"));
        d.sent();
        assert!(d.visible());
        d.key(key(KeyCode::Esc));
        assert!(!d.visible());
        d.asked(2, "main".into(), q(false, false), true);
        assert!(
            !d.visible(),
            "another question must not steal the keyboard after Esc"
        );
        assert!(d.hint().unwrap().contains("2 questions"));
        d.sent();
        assert_eq!(d.active, Some(1));
        assert!(d.view(80).unwrap().title.contains("1 of 2"));
        d.key(key(KeyCode::Tab));
        assert_eq!(d.active, Some(2));
        d.key(key(KeyCode::BackTab));
        assert_eq!(d.active, Some(1));
        d.key(key(KeyCode::Enter));
        assert_eq!(d.active, Some(2));
        d.closed(1, true);
        assert!(d.view(80).unwrap().title.contains("1 of 1"));
    }
    #[test]
    fn empty_input_presents_at_once_and_single_choice_accepts_arrows_or_numbers() {
        let mut d = opened(q(false, false));
        assert!(d.visible());
        d.key(key(KeyCode::Down));
        let a = answer(d.key(key(KeyCode::Enter)));
        assert_eq!(a.choices, ["two"]);
        assert_eq!(a.by, "user");
        assert!(!d.visible(), "an in-flight answer cannot be sent twice");
        let mut d = opened(q(false, false));
        assert_eq!(answer(d.key(key(KeyCode::Char('1')))).choices, ["one"]);
    }
    #[test]
    fn multi_select_toggles_and_orders_choices_and_can_confirm_none() {
        let mut d = opened(q(true, false));
        assert!(d.key(key(KeyCode::Char('2'))).is_none());
        d.key(key(KeyCode::Up));
        d.key(key(KeyCode::Char(' ')));
        let a = answer(d.key(key(KeyCode::Enter)));
        assert_eq!(a.choices, ["one", "two"]);
        let mut d = opened(q(true, false));
        d.key(key(KeyCode::Char('1')));
        d.key(key(KeyCode::Char('1')));
        assert!(answer(d.key(key(KeyCode::Enter))).choices.is_empty());
    }
    #[test]
    fn free_text_digits_spaces_and_other_are_structured_not_parsed_as_commands() {
        let mut free = q(false, false);
        free.choices.clear();
        let mut d = opened(free);
        d.key(key(KeyCode::Char('1')));
        d.key(key(KeyCode::Char(' ')));
        d.paste("2 /answer --decline");
        d.key(ctrl('b')); // In a free-form answer this is the editor's cursor-left, not discard.
        let a = answer(d.key(key(KeyCode::Enter)));
        assert_eq!(a.text.as_deref(), Some("1 2 /answer --decline"));
        assert!(a.choices.is_empty());
        let mut d = opened(q(true, true));
        d.key(key(KeyCode::Char('1')));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Enter));
        d.paste("another 3");
        assert!(d.key(key(KeyCode::Enter)).is_none());
        let a = answer(d.key(key(KeyCode::Enter)));
        assert_eq!(a.choices, ["one"]);
        assert_eq!(a.text.as_deref(), Some("another 3"));
    }
    #[test]
    fn declines_work_even_inside_a_free_form_field_and_preserve_the_reason() {
        let mut free = q(false, false);
        free.choices.clear();
        let mut d = opened(free.clone());
        let (_, Settle::Fail(reason)) = d.key(ctrl('n')).unwrap() else {
            panic!("not declined")
        };
        assert_eq!(reason, "not relevant");
        let mut d = opened(free);
        d.key(ctrl('r'));
        d.paste(" I don't know  ");
        let (_, Settle::Fail(reason)) = d.key(key(KeyCode::Enter)).unwrap() else {
            panic!("not declined")
        };
        assert_eq!(reason, " I don't know  ");
    }
    #[test]
    fn cancellation_drops_a_half_answer_and_moves_on_but_does_not_reopen_aside() {
        let mut d = opened(q(true, true));
        d.asked(2, "child via main".into(), q(true, false), true);
        d.key(ctrl('r'));
        d.paste("half an answer");
        d.closed(1, true);
        assert_eq!(d.active, Some(2));
        assert!(d.open[0].field.is_none());
        d.key(key(KeyCode::Esc));
        d.closed(2, true);
        assert!(!d.visible());
        assert!(d.hint().is_none());
        assert!(d.key(key(KeyCode::Enter)).is_none());
    }
    #[test]
    fn cancelling_the_recalled_modal_advances_even_with_a_preserved_draft() {
        let mut d = Dialogs::default();
        d.asked(1, "main".into(), q(false, false), false);
        d.asked(2, "main".into(), q(false, false), false);
        d.sent();
        d.closed(1, false);
        assert_eq!(d.active, Some(2));
        d.key(key(KeyCode::Esc));
        d.closed(2, false);
        d.asked(3, "main".into(), q(false, false), true);
        assert_eq!(d.active, Some(3), "aside ends when its questions are gone");
    }

    #[test]
    fn failed_settlement_can_be_retried_without_losing_the_question() {
        let mut d = opened(q(true, false));
        d.key(key(KeyCode::Enter));
        assert!(!d.visible());
        d.retry(1, true);
        assert!(d.visible());
        assert_eq!(d.active, Some(1));
    }
    #[test]
    fn an_empty_other_edit_removes_committed_text_without_changing_selections() {
        let mut d = opened(q(true, true));
        d.key(key(KeyCode::Char('1')));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Enter));
        d.paste("old Other text");
        assert!(d.key(key(KeyCode::Enter)).is_none());
        assert_eq!(d.open[0].other.as_deref(), Some("old Other text"));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Enter));
        d.key(ctrl('u'));
        assert!(d.key(key(KeyCode::Enter)).is_none());
        assert!(d.open[0].other.is_none());
        assert!(d.open[0].field.is_none());
        let a = answer(d.key(key(KeyCode::Enter)));
        assert_eq!(a.choices, ["one"]);
        assert!(a.text.is_none());
        let mut free = q(true, true);
        free.choices.clear();
        let mut d = opened(free);
        assert!(
            d.key(key(KeyCode::Enter)).is_none(),
            "free-form still requires text"
        );
        assert!(d.visible());
    }

    #[test]
    fn ctrl_j_confirms_fields_without_discarding_their_text() {
        let mut free = q(false, false);
        free.choices.clear();
        let mut d = opened(free.clone());
        d.paste("typed ahead answer");
        assert_eq!(
            answer(d.key(ctrl('j'))).text.as_deref(),
            Some("typed ahead answer")
        );
        let mut d = opened(free);
        d.key(ctrl('r'));
        d.paste("ask later");
        let (_, Settle::Fail(reason)) = d.key(ctrl('j')).unwrap() else {
            panic!("not declined")
        };
        assert_eq!(reason, "ask later");
        let mut d = opened(q(true, true));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Down));
        d.key(key(KeyCode::Enter));
        d.paste("Other text");
        assert!(d.key(ctrl('j')).is_none());
        assert_eq!(answer(d.key(ctrl('j'))).text.as_deref(), Some("Other text"));
    }

    #[test]
    fn editing_or_moving_a_field_caret_returns_from_context_browsing() {
        for decline in [false, true] {
            for action in [
                Some(key(KeyCode::Char('a'))),
                Some(key(KeyCode::Left)),
                Some(key(KeyCode::Right)),
                None,
            ] {
                let mut free = q(false, false);
                free.choices.clear();
                free.text = (0..30)
                    .map(|n| format!("context-{n}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut d = opened(free);
                if decline {
                    d.key(ctrl('r'));
                }
                d.paste("text");
                d.view(80);
                for _ in 0..20 {
                    d.key(key(KeyCode::PageUp));
                }
                let mut buf = Buffer::empty(Rect::new(0, 0, 80, 9));
                assert!(
                    d.view(80).unwrap().draw(&mut buf).is_none(),
                    "field was scrolled out"
                );
                if let Some(action) = action {
                    d.key(action);
                } else {
                    d.paste("pasted");
                }
                assert_eq!(d.open[0].browse, 0);
                let mut buf = Buffer::empty(Rect::new(0, 0, 80, 9));
                let cursor = d
                    .view(80)
                    .unwrap()
                    .draw(&mut buf)
                    .expect("caret follows editing");
                assert!(buf.area.contains(cursor));
                assert!(
                    super::super::rows_of(&buf)
                        .iter()
                        .any(|row| row.contains("text"))
                );
            }
        }
    }

    #[test]
    fn every_context_row_is_reachable_with_one_or_two_body_rows_after_resize() {
        let mut free = q(false, false);
        free.choices.clear();
        let context: Vec<_> = (0..20).map(|n| format!("context-{n}")).collect();
        free.text = context.join("\n");
        let mut d = opened(free);
        // Resizing preserves deliberate browsing. Single-row movement works even
        // when the height budget leaves less than a traditional page step.
        for height in [5, 6, 5] {
            let mut visible = std::collections::HashSet::new();
            let count = d.view(80).unwrap().list.rows.len();
            for code in [KeyCode::PageUp, KeyCode::PageDown] {
                for _ in 0..count {
                    let mut buf = Buffer::empty(Rect::new(0, 0, 80, height));
                    d.view(80).unwrap().draw(&mut buf);
                    visible.extend(super::super::rows_of(&buf));
                    d.key(key(code));
                }
            }
            assert!(
                context.iter().all(|row| visible.contains(row)),
                "height {height}: {visible:?}"
            );
        }
    }

    #[test]
    fn repeated_boundary_keys_do_not_delay_reverse_browsing_even_between_frames() {
        let mut free = q(false, false);
        free.choices.clear();
        free.text = (0..12)
            .map(|n| format!("context-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut d = opened(free);
        d.view(80);
        for _ in 0..40 {
            d.key(key(KeyCode::PageUp));
        }
        assert_eq!(d.view(80).unwrap().list.focus, 0);
        d.key(key(KeyCode::PageDown));
        assert_eq!(
            d.view(80).unwrap().list.focus,
            1,
            "reverse immediately at the top"
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 5)); // one body row
        d.view(80).unwrap().draw(&mut buf);
        assert_eq!(super::super::rows_of(&buf)[2], "context-1");
        for _ in 0..40 {
            d.key(key(KeyCode::PageDown));
        }
        let view = d.view(80).unwrap();
        let last = view.list.rows.len() - 1;
        assert_eq!(view.list.focus, last);
        d.key(key(KeyCode::PageUp));
        assert_eq!(
            d.view(80).unwrap().list.focus,
            last - 1,
            "reverse immediately at the bottom"
        );
    }

    #[test]
    fn resizing_reconciles_stored_browse_offsets_at_both_wrapped_bounds() {
        for top in [false, true] {
            let mut question = q(true, true);
            question.text = "long question background with many wrapped rows ".repeat(4);
            let mut d = opened(question);
            d.view(4);
            for _ in 0..200 {
                d.key(key(if top {
                    KeyCode::PageUp
                } else {
                    KeyCode::PageDown
                }));
            }
            // Widening dramatically shortens the wrapped content. Clamp stored
            // state now, rather than only clamping what this frame displays.
            let view = d.view(80).unwrap();
            let last = view.list.rows.len() - 1;
            assert_eq!(view.list.focus, if top { 0 } else { last });
            d.key(key(if top {
                KeyCode::PageDown
            } else {
                KeyCode::PageUp
            }));
            assert_eq!(
                d.view(80).unwrap().list.focus,
                if top { 1 } else { last - 1 }
            );
        }
    }

    #[test]
    fn modal_layout_separates_title_body_and_hints_without_color() {
        let mut d = opened(q(true, false));
        d.key(key(KeyCode::Char('1')));
        let view = d.view(80).unwrap();
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, view.height()));
        view.draw(&mut buf);
        let rows = super::super::rows_of(&buf);
        assert_eq!(rows[0], "", "status is drawn separately");
        assert_eq!(rows[1], "Question 1 of 1 · main · #1");
        assert_eq!(rows[2], "Which?");
        assert!(rows[3].starts_with("> [x] 1. one"));
        assert!(rows[4].starts_with("  [ ] 2. two"));
        assert!(rows[5].starts_with("Esc aside"));
        assert!(rows[5..].join(" ").contains("Ctrl-N not relevant"));
    }

    #[test]
    fn field_cursor_tracks_unicode_and_wrapping_without_touching_the_status() {
        let mut question = q(false, false);
        question.choices.clear();
        let mut d = opened(question);
        d.paste("界e\u{301} 123456789");
        for width in [1, 2, 10, 80] {
            let view = d.view(width).unwrap();
            let mut buf = Buffer::empty(Rect::new(0, 0, width, view.height()));
            let cursor = view.draw(&mut buf).expect("field cursor");
            assert!(buf.area.contains(cursor));
            assert!(cursor.y >= 2);
        }
    }

    #[test]
    fn layout_is_bounded_wraps_and_scrolls_through_choices_and_background_on_resize() {
        let mut question = q(true, true);
        question.text = "Long question text. ".repeat(20);
        question.choices.extend((3..35).map(|n| Choice {
            label: format!("choice {n} 界 with a long label"),
            description: "description ".repeat(3),
        }));
        let mut d = opened(question);
        for _ in 0..30 {
            d.key(key(KeyCode::Down));
        }
        for width in [0, 1, 7, 30, 80, 140] {
            let view = d.view(width).unwrap();
            assert!(view.height() <= render::BOX_ROWS as u16 + 1);
            for height in 0..12 {
                let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
                if let Some(at) = view.draw(&mut buf) {
                    assert!(buf.area.contains(at));
                }
            }
        }
        // Paging can reveal the rest of a long label, or the question above the choices.
        let before = d.view(20).unwrap().list.focus;
        d.key(key(KeyCode::PageDown));
        assert!(d.view(20).unwrap().list.focus > before);
        for _ in 0..1000 {
            d.key(key(KeyCode::PageUp));
        }
        assert_eq!(d.view(20).unwrap().list.focus, 0);
        let mut d = opened(q(true, false));
        let view = d.view(80).unwrap();
        assert!(
            view.list
                .rows
                .iter()
                .any(|row| row.contains("> [ ] 1. one"))
        );
    }
}
