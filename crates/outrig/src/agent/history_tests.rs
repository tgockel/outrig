use rig::OneOrMany;
use rig::completion::Message;
use rig::completion::message::{AssistantContent, ToolResultContent, UserContent};
use serde_json::json;

use super::{Adjacent, History, Prompt, Role, Store, TooLarge, Why, Window, split_turns};
use crate::agent::budget::{Budget, candidate};
use crate::agent::tool;
use crate::python::host::ContextChange;
use crate::python::testing::{Fake, round_trip};

/// Room for anything a test here commits.
const ROOMY: u64 = 1_000_000;

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

/// The replies of the turns before `start` that a call of a round beginning
/// there is sent, with room for all of them.
fn selected(store: &Store, start: usize) -> Vec<String> {
    let selection = store
        .select(start, Prompt::Opening { tokens: 0 }, ROOMY)
        .expect("room for everything");
    selection
        .carried
        .iter()
        .filter(|(id, _)| *id < start)
        .map(|(id, _)| text(store.turns[*id].messages.last().expect("a reply")))
        .collect()
}

/// The ids a call answering the store's latest turn carries in `room`, or
/// the estimate of that turn when it does not fit.
fn carried(store: &Store, room: u64) -> Result<Vec<usize>, u64> {
    let prompt = Prompt::Turn(store.turns.len() - 1);
    let selection = store.select(store.start, prompt, room)?;
    Ok(selection.carried.iter().map(|(id, _)| *id).collect())
}

/// Every turn of `store` costs `tokens`.
fn priced(mut store: Store, tokens: u64) -> Store {
    for turn in &mut store.turns {
        turn.tokens = tokens;
    }
    store
}

/// Round 5 in progress with three turns committed, after four rounds of two;
/// the window is round 1 and round 4, round 2's first turn (id 2) is promoted,
/// and every turn costs 10.
///
/// | ids     | round | why                |
/// |---------|-------|--------------------|
/// | 0, 1    | 1     | first              |
/// | 2       | 2     | promoted           |
/// | 3, 4, 5 | 2, 3  | left out by window |
/// | 6, 7    | 4     | recent             |
/// | 8, 9    | 5     | this round         |
/// | 10      | 5     | latest             |
fn five_rounds() -> Store {
    let mut store = rounds_of(
        Window {
            first: 1,
            recent: 1,
        },
        4,
        2,
    );
    store.begin_round(Message::user("round 5"));
    for turn in 1..=3 {
        store.commit(vec![Message::assistant(format!("r5t{turn}"))]);
    }
    store.change(ContextChange::Promote(vec![2]));
    priced(store, 10)
}

fn promote(store: &mut Store, ids: &[u64]) {
    store.change(ContextChange::Promote(ids.to_vec()));
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
    promote(&mut store, &[3, 6, 6, 99]);
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
        super::mirror(3, &turn, false),
        json!({
            "round": 3,
            "prompt": "[outrig] 1 message is waiting.",
            "text": "Reading it.",
            "calls": [
                {"source": "1 + 1", "result": "2\n"},
                {"source": "{\"code\":\"x\"}", "result": "refused"},
            ],
            "incomplete": false,
        }),
        "each result is paired with its call by id; arguments without a source are kept whole"
    );
    assert_eq!(
        super::mirror(3, &turn[1..], true),
        json!({
            "round": 3,
            "prompt": null,
            "text": "Reading it.",
            "calls": [
                {"source": "1 + 1", "result": "2\n"},
                {"source": "{\"code\":\"x\"}", "result": "refused"},
            ],
            "incomplete": true,
        }),
        "a turn after the first has no prompt"
    );
}

/// The replies and openings a call is sent, in order, its prompt last.
fn sent(history: &History) -> Vec<String> {
    let (sent, _) = history
        .assemble(&Budget::with_room("m", ROOMY))
        .expect("room for everything");
    sent.iter().map(text).collect()
}

/// What `History::open_round` counts, opening a round on a line that is only
/// the count.
fn omitted(history: &History, room: u64) -> usize {
    let opening = history.open_round(&Budget::with_room("m", room), |n| n.to_string());
    text(&opening).parse().expect("a count")
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
        history.commit(vec![Message::assistant(format!("reply {round}"))], false);
    }
    for id in 0..3 {
        let turn = fake.expect("turn").await;
        assert_eq!(turn["id"], id, "{turn}");
        assert_eq!(turn["agent"], "primary", "{turn}");
        assert_eq!(turn["round"], id + 1, "{turn}");
        assert_eq!(turn["prompt"], format!("r{}", id + 1), "{turn}");
        assert_eq!(turn["incomplete"], false, "{turn}");
    }
    assert_eq!(
        omitted(&history, ROOMY),
        1,
        "round 2 is between the window's ends"
    );
    history.begin_round(Message::user("r4"));
    assert_eq!(sent(&history), ["r1", "reply 1", "r3", "reply 3", "r4"]);

    // Turn 9 was never committed, so only turn 1 is taken.
    fake.send(json!({"t": "promote", "agent": "primary", "turns": [1, 9]}))
        .await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(
        sent(&history),
        ["r1", "reply 1", "r2", "reply 2", "r3", "reply 3", "r4"],
        "the promoted turn, in its place"
    );

    // Demoted, it is left out again, and a second demotion changes nothing.
    for _ in 0..2 {
        fake.send(json!({"t": "demote", "agent": "primary", "turns": [1]}))
            .await;
    }
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(sent(&history), ["r1", "reply 1", "r3", "reply 3", "r4"]);
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
    history.commit(vec![Message::assistant("done")], false);
    history.finish_round();
    assert_eq!(fake.expect("turn").await["id"], 1);
    assert_eq!(history.len(), 2, "nothing more");
}

// ---------------------------------------------------------------------------- the budget

/// What does not fit goes in the order the maintainer settled: the recent
/// rounds from their oldest, then the first rounds from their newest, then
/// promotions, then this round's earlier turns, oldest first. The latest turn
/// never goes; when it alone does not fit, nothing is sent.
#[test]
fn the_budget_evicts_the_window_then_promotions_then_the_round() {
    let store = five_rounds();
    let everything = vec![0, 1, 2, 6, 7, 8, 9, 10];
    assert_eq!(carried(&store, 80), Ok(everything.clone()));
    let cases: [(u64, &[usize]); 7] = [
        (70, &[6]),
        (60, &[6, 7]),
        (50, &[6, 7, 1]),
        (40, &[6, 7, 1, 0]),
        (30, &[6, 7, 1, 0, 2]),
        (20, &[6, 7, 1, 0, 2, 8]),
        (10, &[6, 7, 1, 0, 2, 8, 9]),
    ];
    for (room, left_out) in cases {
        let expected: Vec<usize> = everything
            .iter()
            .copied()
            .filter(|id| !left_out.contains(id))
            .collect();
        assert_eq!(carried(&store, room), Ok(expected), "room {room}");
    }
    assert_eq!(carried(&store, 9), Err(10), "the latest turn alone is 10");
}

/// One turn too large for what is left costs only itself: a smaller turn
/// that comes after it in the order still goes.
#[test]
fn a_turn_too_large_to_fit_is_left_out_and_smaller_ones_still_go() {
    let mut store = five_rounds();
    // Round 4's second turn, the recent window's newest, is kept longest of
    // the two, and is now too large for what the rest leave.
    store.turns[7].tokens = 50;
    assert_eq!(carried(&store, 70), Ok(vec![0, 1, 2, 6, 8, 9, 10]));
}

/// A turn that is both promoted and in the window is carried once, as a
/// promotion, which the budget keeps longer.
#[test]
fn a_promoted_turn_in_the_window_is_carried_once_as_promoted() {
    let mut store = five_rounds();
    promote(&mut store, &[7, 7]);
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", 80))
        .expect("room for everything");
    assert_eq!(
        manifest.carried,
        [
            (0, Why::First),
            (1, Why::First),
            (2, Why::Promoted),
            (6, Why::Recent),
            (7, Why::Promoted),
            (8, Why::Round),
            (9, Why::Round),
            (10, Why::Latest),
        ]
    );
    // At 60, two turns go: the recent window's, then the first rounds'
    // newest -- not turn 7, which is promoted.
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", 60))
        .expect("the latest fits");
    assert_eq!(manifest.evicted, [(1, Why::First), (6, Why::Recent)]);
}

/// Promoting twice is promoting once; a promotion lasts until it is demoted;
/// demoting a turn that was not promoted changes nothing.
#[test]
fn promotions_and_demotions_settle_on_what_was_asked_last() {
    let mut store = rounds_of(
        Window {
            first: 1,
            recent: 1,
        },
        4,
        1,
    );
    promote(&mut store, &[1, 1]);
    assert_eq!(selected(&store, 4), ["r1t1", "r2t1", "r4t1"]);
    store.change(ContextChange::Demote(vec![1]));
    assert_eq!(selected(&store, 4), ["r1t1", "r4t1"]);
    store.change(ContextChange::Demote(vec![2, 99]));
    promote(&mut store, &[1]);
    assert_eq!(selected(&store, 4), ["r1t1", "r2t1", "r4t1"]);
    store.change(ContextChange::Promote(vec![2]));
    assert_eq!(
        selected(&store, 4),
        ["r1t1", "r2t1", "r3t1", "r4t1"],
        "promoted out of order, sent in order"
    );
}

/// A call that cannot fit its latest turn is not made, and the reason names
/// the turn, its round, and what the room was made of.
#[test]
fn the_latest_turn_is_never_evicted_and_too_large_names_it() {
    let mut store = five_rounds();
    store.turns[10].tokens = 500;
    let budget = Budget {
        model: "small".into(),
        window: 1_000,
        window_assumed: true,
        reserve: 250,
        overhead: 300,
    };
    let err = store.assemble(&budget).expect_err("450 of room");
    assert_eq!(
        (err.turn, err.round, err.estimate),
        (Some(10), 5, 500),
        "{err}"
    );
    let text = err.to_string();
    for needle in [
        "turn 10 of round 5",
        "about 500 tokens",
        "room for about 450",
        "1000-token context window (assumed: [models.small] sets no context-window)",
        "250 reserved for the reply",
        "about 300 for the system prompt",
        "stays in runtime.history",
        "[models.small].context-window",
    ] {
        assert!(text.contains(needle), "{needle:?} in {text}");
    }
    assert_eq!(store.calls, 0, "a call not made is not counted");

    // Alone, it fits exactly, and everything else goes.
    store.turns[10].tokens = 450;
    let (sent, manifest) = store.assemble(&budget).expect("fits alone");
    assert_eq!(manifest.carried, [(10, Why::Latest)]);
    assert_eq!(manifest.estimate, 450 + 300);
    assert_eq!(sent, store.turns[10].messages, "the latest turn alone");
}

/// Each candidate has its own allowance, and the same conversation assembled
/// for a smaller one leaves more out -- in the same order -- and names the
/// candidate it was held to.
#[test]
fn a_smaller_candidates_allowance_evicts_more_of_the_same_store() {
    // Priced so each allowance clears the least room a session starts with.
    let mut store = priced(five_rounds(), 1_000);
    let large = Budget::new(&candidate("large", Some(2_000 + 8_000)), Some(1_000), 1_000)
        .expect("8000 of room");
    let small = Budget::new(&candidate("small", Some(2_000 + 4_000)), Some(1_000), 1_000)
        .expect("4000 of room");
    let (_, manifest) = store.assemble(&large).expect("room for all");
    assert_eq!(
        (manifest.budget.model.as_str(), manifest.evicted.len()),
        ("large", 0)
    );
    let (_, manifest) = store.assemble(&small).expect("room for some");
    assert_eq!(manifest.budget.model, "small");
    assert_eq!(
        manifest.evicted,
        [
            (0, Why::First),
            (1, Why::First),
            (6, Why::Recent),
            (7, Why::Recent)
        ]
    );
    assert_eq!(manifest.call, 1, "calls are counted across candidates");

    // A latest turn that one candidate's window holds and the other's does not.
    store.turns[10].tokens = 6_000;
    assert!(store.assemble(&large).is_ok());
    let err: TooLarge = store.assemble(&small).expect_err("6000 of 4000");
    assert_eq!(err.budget.model, "small");
}

/// A cut can put one role after itself. The manifest says where, since that
/// is the request a strict provider refuses.
#[test]
fn a_cut_that_puts_one_role_after_itself_is_recorded() {
    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    // Round 1 ends in text; round 2 stops after a call, so it ends in a result.
    store.begin_round(Message::user("round 1"));
    store.commit(vec![Message::assistant("done")]);
    store.begin_round(Message::user("round 2"));
    store.commit(vec![call("a", "1"), Message::tool_result("a", "one")]);
    store.commit(vec![call("b", "2"), Message::tool_result("b", "two")]);
    // Round 3 opens, and its first call carries round 1 and 2 -- and the
    // opening after a result.
    store.begin_round(Message::user("round 3"));
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY))
        .expect("room");
    assert_eq!(
        manifest.adjacent,
        [Adjacent {
            turn: None,
            role: Role::User
        }]
    );
    // Round 2's second turn, promoted into a view without its first, follows
    // round 1's closing text with a tool call.
    store.window = Window {
        first: 1,
        recent: 0,
    };
    promote(&mut store, &[2]);
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY))
        .expect("room");
    assert_eq!(
        manifest.adjacent,
        [
            Adjacent {
                turn: Some(2),
                role: Role::Assistant
            },
            Adjacent {
                turn: None,
                role: Role::User
            }
        ]
    );
}

/// What a cut is called says what the conversation held, never which roles
/// the wire carried: a prompt after tool results is two user messages to
/// Anthropic and a `tool` message then a user one to OpenAI.
#[test]
fn a_cut_is_named_for_the_conversation_not_the_wire() {
    let replies = Adjacent {
        turn: Some(2),
        role: Role::Assistant,
    };
    assert_eq!(
        replies.to_string(),
        "two of the model's replies in a row, where turn 2 begins"
    );
    let prompt = Adjacent {
        turn: None,
        role: Role::User,
    };
    let text = prompt.to_string();
    assert_eq!(
        text,
        "a prompt right after a prompt or tool results the model never answered, where the \
         round's opening begins"
    );
    assert!(!text.contains("user message"), "{text}");
}

/// The count on a round's opening line includes what the budget leaves out
/// of its first call, and not only what the window does.
#[tokio::test]
async fn the_opening_count_includes_what_the_budget_leaves_out() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(interpreter, Window::DEFAULT);
    for round in 1..=3 {
        history.begin_round(Message::user(format!("r{round}")));
        history.commit(vec![Message::assistant("x".repeat(300))], false);
        fake.expect("turn").await;
    }
    assert_eq!(omitted(&history, ROOMY), 0);
    let each = history.tokens_of(1);
    // Room for the opening and two turns, not three.
    let opening = crate::agent::budget::tokens([&Message::user("1")]);
    assert_eq!(omitted(&history, opening + 2 * each + each / 2), 1);
    assert_eq!(omitted(&history, opening), 3);
}

/// The count an opening states is what its first call leaves out, however the
/// line's length moves what fits. Here the line grows with the count, and one
/// large turn sits among small ones, so a longer line can leave out the large
/// one and fit the small: costed at its own size, an opening would state a
/// count its call does not keep.
#[test]
fn the_opening_counts_exactly_what_its_first_call_leaves_out() {
    let mut store = rounds_of(
        Window {
            first: 1,
            recent: 2,
        },
        3,
        1,
    );
    for (turn, tokens) in store.turns.iter_mut().zip([2_000, 100, 100]) {
        turn.tokens = tokens;
    }
    let line = |n: usize| format!("{n} {}", "y".repeat(30 * n));
    for room in (0..2_600).step_by(7) {
        let opening = store.open_round(room, line);
        let stated: usize = text(&opening)
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a count");
        let left_out = match store.assemble(&Budget::with_room("m", room)) {
            Ok((_, manifest)) => store.turns.len() - manifest.carried.len(),
            Err(_) => store.turns.len(),
        };
        assert_eq!(stated, left_out, "room {room}");
    }
}

/// A turn committed while its calls ran is mirrored as incomplete.
#[tokio::test]
async fn an_incomplete_turn_is_mirrored_as_incomplete() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(interpreter, Window::DEFAULT);
    history.begin_round(Message::user("go"));
    history.commit(
        vec![call("a", "1"), Message::tool_result("a", "unknown")],
        true,
    );
    history.commit(vec![Message::assistant("done")], false);
    assert_eq!(fake.expect("turn").await["incomplete"], true);
    assert_eq!(fake.expect("turn").await["incomplete"], false);
}

/// The manifest is enough, with the store, to rebuild what was sent: the
/// carried turns in order, then the prompt.
#[tokio::test]
async fn a_manifest_rebuilds_the_call_it_describes() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(
        interpreter,
        Window {
            first: 1,
            recent: 1,
        },
    );
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&observed);
    history.on_manifest(move |manifest| seen.lock().expect("seen").push(manifest.clone()));
    for round in 1..=3 {
        history.begin_round(Message::user(format!("r{round}")));
        let (sent, manifest) = history
            .assemble(&Budget::with_room("m", ROOMY))
            .expect("room");
        assert_eq!(sent.last(), Some(&Message::user(format!("r{round}"))));
        assert_eq!(history.reconstruct(&manifest), sent, "a round's first call");
        history.commit(
            vec![call("a", "1"), Message::tool_result("a", "one")],
            false,
        );
        let (sent, manifest) = history
            .assemble(&Budget::with_room("m", ROOMY))
            .expect("room");
        assert_eq!(sent.last(), Some(&Message::tool_result("a", "one")));
        assert_eq!(history.reconstruct(&manifest), sent, "a later call");
        history.commit(vec![Message::assistant("done")], false);
        history.finish_round();
        for _ in 0..2 {
            fake.expect("turn").await;
        }
    }
    let calls: Vec<u64> = observed
        .lock()
        .expect("seen")
        .iter()
        .map(|m| m.call)
        .collect();
    assert_eq!(
        calls,
        [0, 1, 2, 3, 4, 5],
        "each call observed once, in order"
    );
}

// ---------------------------------------------------------------------------- memory

/// The measurement gate for keeping the whole conversation: what the host's
/// store holds and what crosses to the interpreter, over a long session of
/// full-size results, each measured on its own. The interpreter's side is
/// `a_long_sessions_mirror_is_measured_against_the_ceiling`.
#[tokio::test]
async fn the_host_record_and_the_transport_of_a_long_session_are_measured() {
    const TURNS: usize = 1_000;
    const RESULT: usize = 16 << 10;
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(interpreter, Window::DEFAULT);
    let result = "r".repeat(RESULT);
    let (mut transport, mut longest) = (0usize, 0usize);
    for turn in 0..TURNS {
        if turn % 10 == 0 {
            history.begin_round(Message::user(format!("round {turn}")));
        }
        history.commit(
            vec![
                call("a", "print(x)"),
                Message::tool_result("a", result.as_str()),
            ],
            false,
        );
        // Re-serialized as it was written: compact, with the same keys.
        let line = fake.expect("turn").await.to_string().len() + 1;
        transport += line;
        longest = longest.max(line);
    }
    let payload = TURNS * RESULT;
    let record: usize = (0..TURNS)
        .map(|id| {
            let store = history.store();
            serde_json::to_vec(&store.turns[id].messages)
                .expect("serializes")
                .len()
        })
        .sum();
    eprintln!(
        "{TURNS} turns of {RESULT} bytes: payload {payload}, host record {record}, transport \
         {transport}, longest line {longest}"
    );
    assert!(record < payload * 12 / 10, "host record {record}");
    assert!(transport < payload * 12 / 10, "transport {transport}");
    assert!(longest < RESULT + 1024, "one line is one turn: {longest}");
}
