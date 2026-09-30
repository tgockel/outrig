use rig::OneOrMany;
use rig::completion::Message;
use rig::completion::message::{AssistantContent, ToolResultContent, UserContent};
use serde_json::json;

use super::{History, Store, Window, split_turns};
use crate::agent::tool;
use crate::python::testing::{Fake, round_trip};

fn call(id: &str, source: &str) -> Message {
    Message::Assistant {
        id: None,
        content: OneOrMany::one(AssistantContent::tool_call(
            id,
            tool::NAME,
            json!({"source": source}),
        )),
    }
}

/// The text of `message`'s first part, whatever its role.
fn text(message: &Message) -> String {
    match message {
        Message::User { content } => match content.first() {
            UserContent::Text(text) => text.text,
            other => panic!("not text: {other:?}"),
        },
        Message::Assistant { content, .. } => match content.first() {
            AssistantContent::Text(text) => text.text,
            other => panic!("not text: {other:?}"),
        },
        Message::System { content } => content.clone(),
    }
}

/// A store whose rounds each hold `turns_per_round` turns, each a reply
/// naming it `r<round>t<turn>`.
fn rounds_of(window: Window, rounds: u32, turns_per_round: u32) -> Store {
    let mut store = Store {
        window,
        ..Store::default()
    };
    for round in 1..=rounds {
        store.begin_round(Message::user(format!("round {round}")));
        for turn in 1..=turns_per_round {
            store.commit(vec![Message::assistant(format!("r{round}t{turn}"))]);
        }
    }
    store
}

/// The replies of the turns among the first `before` that are sent.
fn selected(store: &Store, before: usize) -> Vec<String> {
    store
        .select(before)
        .map(|turn| text(turn.messages.last().expect("a reply")))
        .collect()
}

#[test]
fn a_turn_begins_at_each_reply() {
    let round = vec![
        call("a", "1"),
        Message::tool_result("a", "one"),
        call("b", "2"),
        Message::tool_result("b", "two"),
        Message::assistant("done"),
    ];
    assert_eq!(
        split_turns(round.clone()),
        [
            round[0..2].to_vec(),
            round[2..4].to_vec(),
            round[4..].to_vec()
        ]
    );
    assert!(split_turns(Vec::new()).is_empty());
}

/// A round's first turn begins with the line that opened it, which the store
/// held from the start; the round's later turns do not.
#[test]
fn a_rounds_first_turn_begins_with_its_opening() {
    let store = rounds_of(Window::DEFAULT, 1, 2);
    let texts = |id: usize| -> Vec<String> { store.turns[id].messages.iter().map(text).collect() };
    assert_eq!(texts(0), ["round 1", "r1t1"]);
    assert_eq!(texts(1), ["r1t2"]);
}

#[test]
fn the_view_is_the_first_rounds_and_the_recent_ones_in_order() {
    let store = rounds_of(
        Window {
            first: 1,
            recent: 2,
        },
        5,
        2,
    );
    assert_eq!(
        selected(&store, 10),
        ["r1t1", "r1t2", "r4t1", "r4t2", "r5t1", "r5t2"],
        "round 1, and the two rounds before the one in progress"
    );
    assert_eq!(
        selected(&store, 6),
        ["r1t1", "r1t2", "r2t1", "r2t2", "r3t1", "r3t2"],
        "recent is counted back from the last turn before the round"
    );
}

#[test]
fn a_short_conversation_is_sent_whole() {
    let store = rounds_of(Window::DEFAULT, 8, 1);
    assert_eq!(
        selected(&store, 8).len(),
        8,
        "two first rounds and six recent cover eight"
    );
    let store = rounds_of(Window::DEFAULT, 9, 1);
    assert_eq!(
        selected(&store, 9),
        [
            "r1t1", "r2t1", "r4t1", "r5t1", "r6t1", "r7t1", "r8t1", "r9t1"
        ],
        "the ninth round leaves the third out"
    );
}

#[test]
fn a_promoted_turn_is_sent_once_in_its_place() {
    let mut store = rounds_of(
        Window {
            first: 1,
            recent: 1,
        },
        4,
        2,
    );
    // Round 2's second turn, round 4's first -- which the window sends anyway
    // -- twice over, and a turn that is not there.
    store.promote(vec![3, 6, 6, 99]);
    assert_eq!(
        selected(&store, 8),
        ["r1t1", "r1t2", "r2t2", "r4t1", "r4t2"],
    );
}

#[test]
fn a_round_that_commits_nothing_leaves_no_gap_in_the_numbering() {
    let mut store = Store::default();
    store.begin_round(Message::user("one"));
    store.commit(vec![Message::assistant("one")]);
    store.begin_round(Message::user("failed"));
    store.begin_round(Message::user("two"));
    let (id, turn) = store.commit(vec![Message::assistant("two")]);
    assert_eq!((id, turn.round), (1, 2));
    assert_eq!(
        text(&turn.messages[0]),
        "two",
        "the failed round's opening is gone"
    );
    let (id, turn) = store.commit(vec![Message::assistant("still two")]);
    assert_eq!((id, turn.round), (2, 2), "one round, however many turns");
}

#[test]
fn a_turn_is_mirrored_as_its_prompt_text_and_calls() {
    let turn = [
        Message::user("[outrig] 1 message is waiting."),
        Message::Assistant {
            id: None,
            content: OneOrMany::many([
                AssistantContent::text("Reading it."),
                AssistantContent::tool_call("a", tool::NAME, json!({"source": "1 + 1"})),
                AssistantContent::tool_call("b", tool::NAME, json!({"code": "x"})),
            ])
            .expect("three parts"),
        },
        Message::User {
            content: OneOrMany::many([
                UserContent::tool_result("b", OneOrMany::one(ToolResultContent::text("refused"))),
                UserContent::tool_result("a", OneOrMany::one(ToolResultContent::text("2\n"))),
            ])
            .expect("two results"),
        },
    ];
    assert_eq!(
        super::mirror(3, &turn),
        json!({
            "round": 3,
            "prompt": "[outrig] 1 message is waiting.",
            "text": "Reading it.",
            "calls": [
                {"source": "1 + 1", "result": "2\n"},
                {"source": "{\"code\":\"x\"}", "result": "refused"},
            ],
        }),
        "each result is paired with its call by id; arguments without a source are kept whole"
    );
    assert_eq!(
        super::mirror(3, &turn[1..]),
        json!({
            "round": 3,
            "prompt": null,
            "text": "Reading it.",
            "calls": [
                {"source": "1 + 1", "result": "2\n"},
                {"source": "{\"code\":\"x\"}", "result": "refused"},
            ],
        }),
        "a turn after the first has no prompt"
    );
}

#[tokio::test]
async fn turns_are_pushed_in_order_and_a_promotion_comes_back_into_the_view() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(
        interpreter.clone(),
        Window {
            first: 1,
            recent: 1,
        },
    );
    for round in 1..=3 {
        history.begin_round(Message::user(format!("r{round}")));
        history.commit(vec![Message::assistant(format!("reply {round}"))]);
    }
    for id in 0..3 {
        let turn = fake.expect("turn").await;
        assert_eq!(turn["id"], id, "{turn}");
        assert_eq!(turn["agent"], "primary", "{turn}");
        assert_eq!(turn["round"], id + 1, "{turn}");
        assert_eq!(turn["prompt"], format!("r{}", id + 1), "{turn}");
    }
    assert_eq!(history.omitted(), 1, "round 2 is between the window's ends");
    history.begin_round(Message::user("r4"));
    let view: Vec<String> = history.view().iter().map(text).collect();
    assert_eq!(view, ["r1", "reply 1", "r3", "reply 3"]);

    // Turn 9 was never committed, so only turn 1 is taken.
    fake.send(json!({"t": "promote", "agent": "primary", "turns": [1, 9]}))
        .await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(history.omitted(), 0);
    let view: Vec<String> = history.view().iter().map(text).collect();
    assert_eq!(
        view,
        ["r1", "reply 1", "r2", "reply 2", "r3", "reply 3"],
        "the promoted turn, in its place"
    );
}

/// A round whose model call answered with nothing still sent its opening, so
/// ending it keeps the opening as a turn of its own. A round whose first turn
/// took the opening keeps nothing more.
#[tokio::test]
async fn an_opening_no_turn_took_is_kept_when_the_round_ends() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(interpreter, Window::DEFAULT);
    history.begin_round(Message::user("answered with nothing"));
    history.finish_round();
    let turn = fake.expect("turn").await;
    assert_eq!(
        (
            turn["id"].clone(),
            turn["prompt"].clone(),
            turn["calls"].clone()
        ),
        (json!(0), json!("answered with nothing"), json!([]))
    );

    history.begin_round(Message::user("answered"));
    history.commit(vec![Message::assistant("done")]);
    history.finish_round();
    assert_eq!(fake.expect("turn").await["id"], 1);
    assert_eq!(history.len(), 2, "nothing more");
}
