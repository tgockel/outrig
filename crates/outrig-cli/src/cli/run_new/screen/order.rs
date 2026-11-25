//! Join the event subscription and the user outbox without comparing text.
//!
//! `python::host` publishes the primary agent's `MessageSent` before putting
//! that message in its FIFO outbox. The conversation polls the two separately,
//! so either can reach the screen first. Place the delivered message at its
//! event's boundary, holding its outbox permit until the screen draws it.
//! Child events never supply these boundaries (nor messages to this outbox).

use std::collections::VecDeque;

use tokio::sync::mpsc;

use super::{Fact, Kind, ToScreen};

#[derive(Default)]
pub(super) struct Order {
    events: VecDeque<ToScreen>,
    messages: VecDeque<ToScreen>,
    /// After a subscription gap, FIFO positions cannot be matched reliably.
    /// Keep arrival order instead; the gap itself is retained in the detail.
    unaligned: bool,
}

impl Order {
    pub(super) fn push(&mut self, item: ToScreen, queue: &mpsc::UnboundedSender<ToScreen>) {
        if matches!(item, ToScreen::Fact(Fact::Missed(_))) {
            self.release(queue);
        }
        if self.unaligned {
            let _ = queue.send(item);
            return;
        }
        if matches!(&item, ToScreen::Entry(entry, _) if entry.kind == Kind::Agent) {
            self.messages.push_back(item);
        } else {
            self.events.push_back(item);
        }
        while let Some(item) = self.events.front() {
            let sent = matches!(item, ToScreen::Fact(Fact::AgentSent));
            if sent && self.messages.is_empty() {
                break;
            }
            let _ = queue.send(self.events.pop_front().expect("front exists"));
            if sent {
                let _ = queue.send(self.messages.pop_front().expect("message exists"));
            }
        }
    }

    /// On a gap or shutdown, never strand entries behind a lost boundary or
    /// an undelivered message. All available text is still retained once.
    pub(super) fn release(&mut self, queue: &mpsc::UnboundedSender<ToScreen>) {
        self.unaligned = true;
        for item in self.events.drain(..).chain(self.messages.drain(..)) {
            let _ = queue.send(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    use super::*;
    use crate::cli::run_new::screen::transcript::Entry;

    fn events() -> Vec<ToScreen> {
        vec![
            ToScreen::Fact(Fact::RoundStarted { round: 1 }),
            ToScreen::Fact(Fact::Calling {
                model: "model".into(),
                call: 1,
            }),
            ToScreen::Fact(Fact::Said("same".into())),
            ToScreen::Fact(Fact::Submitted {
                exec: 1,
                source: "send()".into(),
            }),
            ToScreen::Fact(Fact::AgentSent),
            ToScreen::Fact(Fact::Calling {
                model: "model".into(),
                call: 2,
            }),
            ToScreen::Fact(Fact::Said("same".into())),
            ToScreen::Fact(Fact::AgentSent),
            ToScreen::Fact(Fact::Calling {
                model: "model".into(),
                call: 3,
            }),
            ToScreen::Fact(Fact::Said("final reply".into())),
            ToScreen::Ended {
                entries: vec![],
                still_running: None,
            },
        ]
    }

    fn message() -> ToScreen {
        ToScreen::Entry(Entry::new(Kind::Agent, "same"), None)
    }

    fn entries(inbox: &mut mpsc::UnboundedReceiver<ToScreen>) -> Vec<Entry> {
        let mut entries = Vec::new();
        while let Ok(item) = inbox.try_recv() {
            match item {
                ToScreen::Entry(entry, _) => entries.push(entry),
                ToScreen::Fact(fact) => entries.extend(Entry::from_fact(&fact)),
                ToScreen::Ended {
                    entries: ending, ..
                } => entries.extend(ending),
                _ => panic!("unexpected item"),
            }
        }
        entries
    }

    #[test]
    fn multiple_calls_and_identical_user_sends_keep_event_order_in_either_poll_order() {
        for messages_first in [false, true] {
            let (queue, mut inbox) = mpsc::unbounded_channel();
            let mut order = Order::default();
            if messages_first {
                order.push(message(), &queue);
                order.push(message(), &queue);
            }
            for event in events() {
                order.push(event, &queue);
            }
            if !messages_first {
                order.push(message(), &queue);
                order.push(message(), &queue);
            }
            let all = entries(&mut inbox);
            let ordinary: Vec<_> = all.iter().filter(|entry| entry.inline(false)).collect();
            assert_eq!(
                ordinary
                    .iter()
                    .map(|entry| (entry.kind, entry.text.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (Kind::Thinking, "same"),
                    (Kind::Agent, "same"),
                    (Kind::Thinking, "same"),
                    (Kind::Agent, "same"),
                    (Kind::Thinking, "final reply"),
                ],
            );
            // The detail and verbose views share the same entries, not copies.
            assert_eq!(
                all.iter()
                    .filter(|entry| entry.kind == Kind::Thinking)
                    .count(),
                3
            );
            assert_eq!(
                all.iter().filter(|entry| entry.inline(true)).count(),
                all.len()
            );
            assert!(
                all.iter()
                    .any(|entry| entry.text == "send()" && !entry.inline(false))
            );
        }
    }

    #[test]
    fn each_delivered_message_releases_only_its_boundary_and_keeps_its_permit() {
        let (queue, mut inbox) = mpsc::unbounded_channel();
        let mut order = Order::default();
        let permits = Arc::new(Semaphore::new(1));
        for event in events() {
            order.push(event, &queue);
        }
        let before = entries(&mut inbox);
        assert_eq!(
            before
                .iter()
                .filter(|entry| entry.kind == Kind::Thinking)
                .count(),
            1
        );
        let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
        order.push(
            ToScreen::Entry(Entry::new(Kind::Agent, "same"), Some(permit)),
            &queue,
        );
        assert_eq!(permits.available_permits(), 0, "held until drawn");
        let middle = entries(&mut inbox);
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(
            middle.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            [Kind::Agent, Kind::Thinking]
        );
        order.push(message(), &queue);
        let after = entries(&mut inbox);
        assert_eq!(
            after.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            [Kind::Agent, Kind::Thinking]
        );
    }

    #[test]
    fn a_gap_releases_unmatched_boundaries_and_messages_without_text_deduplication() {
        for message_first in [false, true] {
            let (queue, mut inbox) = mpsc::unbounded_channel();
            let mut order = Order::default();
            order.push(
                if message_first {
                    message()
                } else {
                    ToScreen::Fact(Fact::AgentSent)
                },
                &queue,
            );
            order.push(ToScreen::Fact(Fact::Missed(3)), &queue);
            order.push(message(), &queue);
            order.push(ToScreen::Fact(Fact::Said("same".into())), &queue);
            let all = entries(&mut inbox);
            assert_eq!(
                all.iter().filter(|entry| entry.kind == Kind::Agent).count(),
                if message_first { 2 } else { 1 }
            );
            assert_eq!(
                all.iter()
                    .filter(|entry| entry.kind == Kind::Thinking)
                    .count(),
                1
            );
            assert!(
                all.iter()
                    .any(|entry| entry.text.contains("3 of the session's events"))
            );
        }
    }

    #[test]
    fn shutdown_releases_messages_even_when_their_events_were_not_polled() {
        let (queue, mut inbox) = mpsc::unbounded_channel();
        let mut order = Order::default();
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
        order.push(
            ToScreen::Entry(Entry::new(Kind::Agent, "last message"), Some(permit)),
            &queue,
        );
        assert!(inbox.try_recv().is_err(), "waiting for its event");
        order.release(&queue);
        assert_eq!(entries(&mut inbox)[0].text, "last message");
        assert_eq!(permits.available_permits(), 1);
        // The rest of a shutdown drain must not wait for any more events.
        order.push(message(), &queue);
        assert_eq!(entries(&mut inbox)[0].kind, Kind::Agent);
    }

    #[test]
    fn a_no_text_round_has_no_thinking_and_needs_no_outbox_message() {
        let (queue, mut inbox) = mpsc::unbounded_channel();
        let mut order = Order::default();
        order.push(ToScreen::Fact(Fact::RoundStarted { round: 1 }), &queue);
        order.push(
            ToScreen::Ended {
                entries: vec![],
                still_running: None,
            },
            &queue,
        );
        assert!(
            entries(&mut inbox)
                .iter()
                .all(|entry| entry.kind != Kind::Thinking)
        );
    }
}
