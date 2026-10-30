use rig::completion::Message;
use rig::completion::message::{
    AssistantContent, AssistantMessage, CallId as RigCallId, Reasoning, ToolName,
    ToolResultContent, UserContent,
};
use serde_json::json;

use super::{
    Adjacent, History, Manifest, Prompt, Refused, Role, Store, TooLarge, Why, Window, notice,
    split_turns, without_results,
};
use crate::agent::budget::{self, Budget, MIN_REPLY, Scale, candidate};
use crate::agent::tool;
use crate::harness::event::{CallId, WindowSource};
use crate::python::host::ContextChange;
use crate::python::testing::{Fake, round_trip};

/// Room for anything a test here commits.
const ROOMY: u64 = 1_000_000;

/// The tool's name, as rig types one.
fn name() -> ToolName {
    ToolName::new(tool::NAME).expect("a name")
}

/// A reply of `parts`, hand-built: no origin, no stop.
fn reply(parts: Vec<AssistantContent>) -> Message {
    Message::Assistant(AssistantMessage::new(parts))
}

/// The result `text` of call `id`, as a message of its own.
fn tool_result(id: &str, text: &str) -> Message {
    Message::tool_result(RigCallId::from_wire(id), name(), text)
}

/// The result `text` of call `id`, as one part of a message.
fn result_part(id: &str, text: &str) -> UserContent {
    UserContent::tool_result(
        RigCallId::from_wire(id),
        name(),
        vec![ToolResultContent::text(text)],
    )
}

/// Reasoning `text`, signed as Anthropic signs its thinking: the signature
/// rides in the block's provider item.
fn signed(text: &str) -> AssistantContent {
    AssistantContent::Reasoning(Reasoning::new(text))
        .with_native(json!({"type": "thinking", "thinking": text, "signature": "sig"}))
}

fn call(id: &str, source: &str) -> Message {
    reply(vec![AssistantContent::tool_call(
        id,
        name(),
        json!({"source": source}),
    )])
}

/// The text of `message`'s first part, whatever its role.
fn text(message: &Message) -> String {
    match message {
        Message::User { content } => match content.first() {
            Some(UserContent::Text(text)) => text.text.clone(),
            other => panic!("not text: {other:?}"),
        },
        Message::Assistant(reply) => match reply.content.first() {
            Some(AssistantContent::Text(text)) => text.text.clone(),
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

/// The ids of `turns`, in their order.
fn ids(turns: &[(usize, Why)]) -> Vec<usize> {
    turns.iter().map(|&(id, _)| id).collect()
}

/// The ids a call answering the store's latest turn carries in `room`, or
/// the estimate of that turn when it does not fit.
fn carried(store: &Store, room: u64) -> Result<Vec<usize>, u64> {
    let prompt = Prompt::Turn(store.turns.len() - 1);
    let selection = store.select(store.start, prompt, room)?;
    Ok(ids(&selection.carried))
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

/// Commit a round to `store`: it opens on `opening`, makes each of `calls` in
/// a turn of its own, answered, and closes on `reply` when there is one.
/// Without one it is cut short, ending on its last result.
fn round_of_calls(store: &mut Store, opening: &str, calls: &[&str], reply: Option<&str>) {
    store.begin_round(Message::user(opening));
    for &id in calls {
        store.commit(vec![call(id, id), tool_result(id, "ok")]);
    }
    if let Some(reply) = reply {
        store.commit(vec![Message::assistant(reply)]);
    }
}

/// The side each of `messages` is on.
fn roles(messages: &[Message]) -> Vec<Role> {
    messages.iter().filter_map(Role::of).collect()
}

/// Whether `messages` alternate between the user's side and the model's.
fn alternates(messages: &[Message]) -> bool {
    roles(messages).windows(2).all(|pair| pair[0] != pair[1])
}

/// The count an opening line states, when the line opens on the count.
fn stated(opening: &Message) -> usize {
    text(opening)
        .split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a count")
}

/// How many turns the first call of the round `store` has open leaves out,
/// held to `budget`: all of them, when the opening alone does not fit.
fn first_call_leaves_out(store: &mut Store, budget: &Budget) -> usize {
    match store.assemble(budget, None) {
        Ok((_, manifest)) => store.turns.len() - manifest.carried.len(),
        Err(_) => store.turns.len(),
    }
}

#[test]
fn a_turn_begins_at_each_reply() {
    let round = vec![
        call("a", "1"),
        tool_result("a", "one"),
        call("b", "2"),
        tool_result("b", "two"),
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
        reply(vec![
            AssistantContent::text("Reading it."),
            AssistantContent::tool_call("a", name(), json!({"source": "1 + 1"})),
            AssistantContent::tool_call("b", name(), json!({"code": "x"})),
        ]),
        Message::User {
            content: vec![result_part("b", "refused"), result_part("a", "2\n")],
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
        .assemble(&Budget::with_room("m", ROOMY), CallId::new(1))
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

/// What does not fit goes in the order the maintainer settled: the window from
/// its oldest, the first rounds' turns with the recent ones', then promotions,
/// then this round's earlier turns, oldest first, and last the turns that open
/// the first round and this one. The latest turn never goes; when it alone
/// does not fit, nothing is sent.
#[test]
fn the_budget_evicts_the_window_then_promotions_then_the_round_then_the_openings() {
    let store = five_rounds();
    let everything = vec![0, 1, 2, 6, 7, 8, 9, 10];
    assert_eq!(carried(&store, 80), Ok(everything.clone()));
    let cases: [(u64, &[usize]); 7] = [
        (70, &[1]),
        (60, &[1, 6]),
        (50, &[1, 6, 7]),
        (40, &[1, 6, 7, 2]),
        (30, &[1, 6, 7, 2, 9]),
        (20, &[1, 6, 7, 2, 9, 8]),
        (10, &[1, 6, 7, 2, 9, 8, 0]),
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

/// Commit `turns` turns, each a call and its result priced 10, to the round
/// `store` has open, and assemble a call after each, held to `room`.
fn a_long_round(store: &mut Store, turns: usize, room: u64) -> Vec<(Vec<Message>, Manifest)> {
    calls_priced(store, &vec![10; turns], &Budget::with_room("m", room))
}

/// Commit a turn priced at each of `prices`, each a call and its result, to
/// the round `store` has open, and assemble a call after each, held to
/// `budget`.
fn calls_priced(
    store: &mut Store,
    prices: &[u64],
    budget: &Budget,
) -> Vec<(Vec<Message>, Manifest)> {
    let turns = prices
        .iter()
        .map(|&tokens| (vec![call("a", "1"), tool_result("a", "one")], tokens));
    calls_of(store, turns, budget)
}

/// Commit each of `turns`, priced at its tokens, to the round `store` has
/// open, and assemble a call after each, held to `budget`.
fn calls_of(
    store: &mut Store,
    turns: impl IntoIterator<Item = (Vec<Message>, u64)>,
    budget: &Budget,
) -> Vec<(Vec<Message>, Manifest)> {
    turns
        .into_iter()
        .map(|(messages, tokens)| {
            let (id, _) = store.commit(messages);
            store.turns[id].tokens = tokens;
            store.assemble(budget, None).expect("the latest fits")
        })
        .collect()
}

/// Once a round outgrows its room, each call is sent the turns the call
/// before it was and then only newer ones -- which is all a provider's prompt
/// cache can match -- until that no longer fits, and the view is cut to its
/// prompt and half the room again. Choosing afresh on every call changed the
/// request's front each time: each new turn evicted the round's oldest, and a
/// small turn came back whenever a few tokens came free (#666).
#[test]
fn a_full_view_is_cut_in_steps_and_each_call_extends_the_one_before() {
    let room = 100;
    let mut store = Store::default();
    store.begin_round(Message::user("round 1"));
    let prices: Vec<u64> = [12, 3, 7, 5, 9].into_iter().cycle().take(60).collect();
    let calls = calls_priced(&mut store, &prices, &Budget::with_room("m", room));

    let mut cuts = Vec::new();
    for (n, pair) in calls.windows(2).enumerate() {
        let ((_, before), (_, after)) = (&pair[0], &pair[1]);
        let (before, after_ids) = (ids(&before.carried), ids(&after.carried));
        if let Some(mark) = after.cut {
            assert_eq!(mark, room / 2 + prices[n + 1]);
            assert!(after.estimate <= mark, "call {}: {after:?}", n + 1);
            cuts.push(n + 1);
            continue;
        }
        let newest = before.last().expect("the latest");
        assert!(
            after_ids.starts_with(&before)
                && after_ids[before.len()..].iter().all(|id| id > newest),
            "call {}: {before:?} then {after_ids:?}",
            n + 1
        );
    }

    // Each cut leaves the view in half the room beside its prompt, and the
    // next comes once newer turns have taken it past the whole: at least the
    // other half, less that prompt.
    let first = *cuts.first().expect("the round outgrew its room");
    assert!(cuts.len() >= 3, "{cuts:?}");
    let since: u64 = prices[first + 1..].iter().sum();
    let largest = prices.iter().max().expect("prices");
    let most = 1 + since / (room / 2 - largest);
    assert!(cuts.len() as u64 <= most, "{cuts:?}, at most {most}");
    let overflowing = calls.iter().filter(|(_, m)| !m.evicted.is_empty()).count();
    assert!(cuts.len() * 4 < overflowing, "{cuts:?} of {overflowing}");
}

/// A call is chosen afresh when the agent changes what is chosen, and when a
/// round begins, since its cache is most often cold by then. Nothing else
/// brings back a turn a cut left out, however much room comes free; promoting
/// a turn that is already promoted changes nothing.
#[test]
fn a_promotion_or_a_new_round_chooses_the_view_afresh() {
    let mut store = five_rounds();
    let budget = Budget::with_room("m", 70);
    // The 80 chosen do not fit, so the view is cut to the latest turn and 35
    // besides: the openings and the round's newest.
    let (_, manifest) = store.assemble(&budget, None).expect("the latest fits");
    assert_eq!(
        (manifest.cut, ids(&manifest.carried)),
        (Some(45), vec![0, 8, 9, 10])
    );

    let (id, _) = store.commit(vec![Message::assistant("r5t4")]);
    store.turns[id].tokens = 10;
    let roomy = Budget::with_room("m", ROOMY);
    for budget in [&budget, &roomy] {
        let (_, manifest) = store.assemble(budget, None).expect("room for the cut");
        assert_eq!(
            (manifest.cut, ids(&manifest.carried)),
            (None, vec![0, 8, 9, 10, 11])
        );
        assert_eq!(
            manifest.evicted,
            [
                (1, Why::First),
                (2, Why::Promoted),
                (6, Why::Recent),
                (7, Why::Recent)
            ]
        );
    }
    promote(&mut store, &[2]);
    let (_, manifest) = store.assemble(&budget, None).expect("room for the cut");
    assert_eq!(
        (manifest.cut, ids(&manifest.carried)),
        (None, vec![0, 8, 9, 10, 11])
    );

    store.change(ContextChange::Demote(vec![2]));
    let (_, manifest) = store.assemble(&budget, None).expect("the latest fits");
    assert_eq!(
        (manifest.cut, ids(&manifest.carried)),
        (Some(45), vec![0, 8, 10, 11])
    );

    store.begin_round(Message::user("round 6"));
    let (_, manifest) = store.assemble(&roomy, None).expect("room for everything");
    assert_eq!(
        (manifest.cut, ids(&manifest.carried)),
        (None, vec![0, 1, 8, 9, 10, 11])
    );
}

/// A round longer than the room keeps the turn it opens on -- where the agent
/// read what the user asked -- on every call, and with it a prompt of the
/// user's, without which a provider's chat template can refuse the request.
/// The rest of the round goes oldest first, at each cut.
#[test]
fn a_first_round_longer_than_the_room_keeps_its_opening_turn_on_every_call() {
    let mut store = Store::default();
    store.begin_round(Message::user("round 1"));
    let calls = a_long_round(&mut store, 12, 50);
    for (n, (sent, manifest)) in calls.iter().enumerate() {
        assert_eq!(manifest.carried[0].0, 0, "call {n}: {manifest:?}");
        assert_eq!(text(&sent[0]), "round 1", "call {n}");
    }
    // Cut at turns 5, 8 and 11, each to the opening turn, the latest, and
    // the one before it.
    let (_, last) = calls.last().expect("twelve calls");
    assert_eq!(
        last.carried,
        [(0, Why::Round), (10, Why::Round), (11, Why::Latest)]
    );
}

/// So does a later round, beside the first round's opening turn.
#[test]
fn a_later_round_longer_than_the_room_keeps_its_opening_turn_on_every_call() {
    let window = Window {
        first: 1,
        recent: 1,
    };
    let mut store = priced(rounds_of(window, 2, 2), 10);
    store.begin_round(Message::user("round 3"));
    let calls = a_long_round(&mut store, 12, 50);
    for (n, (_, manifest)) in calls.iter().enumerate() {
        let ids = ids(&manifest.carried);
        assert!(ids.contains(&0) && ids.contains(&4), "call {n}: {ids:?}");
    }
    let (_, last) = calls.last().expect("twelve calls");
    assert_eq!(ids(&last.carried), [0, 4, 14, 15]);
}

/// A round after a first round longer than the room keeps the turns that
/// open the first rounds, then the window from its newest: all of round 2,
/// and round 1's end, where its work was done, rather than its beginning.
#[test]
fn a_round_after_a_long_first_round_is_sent_its_opening_and_its_end() {
    let mut store = Store::default();
    round_of_calls(&mut store, "round 1", &["a"; 9], Some("fixed"));
    round_of_calls(&mut store, "round 2", &["b", "c"], Some("committed"));
    let mut store = priced(store, 10);
    store.begin_round(Message::user("round 3"));
    let opening = budget::tokens([&Message::user("round 3")]);
    let selection = store
        .select(
            store.start,
            Prompt::Opening { tokens: opening },
            opening + 70,
        )
        .expect("the opening fits");
    assert_eq!(ids(&selection.carried), [0, 7, 8, 9, 10, 11, 12]);
    assert_eq!(
        selection.evicted,
        (1..=6).map(|id| (id, Why::First)).collect::<Vec<_>>()
    );
}

/// A turn that is both promoted and in the window is carried once, as a
/// promotion, which the budget keeps longer.
#[test]
fn a_promoted_turn_in_the_window_is_carried_once_as_promoted() {
    let mut store = five_rounds();
    promote(&mut store, &[7, 7]);
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", 80), None)
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
    // In 60, two turns go: the window's oldest, the first round's then the
    // recent one's -- not turn 7, which is promoted.
    let selection = store
        .select(store.start, Prompt::Turn(10), 60)
        .expect("the latest fits");
    assert_eq!(selection.evicted, [(1, Why::First), (6, Why::Recent)]);
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
        window_source: WindowSource::Assumed,
        reserve: 250,
        overhead: 300,
        scale: Scale::RULE,
        max_tokens: None,
        provider: "p".into(),
        wire: crate::agent::budget::Wire::OpenAi,
        alternation: crate::config::RoleAlternation::Relaxed,
    };
    let err = store.assemble(&budget, None).expect_err("450 of room");
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

    // A window the provider reported says so, and is as much the row's to
    // correct; one the row set is not the row's to correct.
    for (source, said, remedy) in [
        (
            WindowSource::Provider,
            "(reported by small's provider)",
            true,
        ),
        (WindowSource::Configured, "token context window,", false),
    ] {
        let text = store
            .assemble(
                &Budget {
                    window_source: source,
                    ..budget.clone()
                },
                None,
            )
            .expect_err("450 of room")
            .to_string();
        assert!(text.contains(said), "{source:?}: {said:?} in {text}");
        assert_eq!(
            text.contains("Set [models.small].context-window"),
            remedy,
            "{source:?}: {text}"
        );
    }

    // Alone, it fits exactly, and everything else goes.
    store.turns[10].tokens = 450;
    let (sent, manifest) = store.assemble(&budget, None).expect("fits alone");
    assert_eq!(manifest.carried, [(10, Why::Latest)]);
    assert_eq!(manifest.estimate, 450 + 300);
    assert_eq!(sent, store.turns[10].messages, "the latest turn alone");
}

/// A candidate whose provider counts more tokens than the rule estimates is
/// held to less of the same store, in the same order, and the call's manifest
/// records the scale it was held to beside the estimate, which stays the rule's.
#[test]
fn a_scaled_allowance_carries_less_of_the_same_store() {
    let mut store = five_rounds();
    let rule = Budget::with_room("m", 80);
    let scaled = Budget {
        scale: Scale::observed(5, 4),
        ..rule.clone()
    };

    let (_, at_rule) = store.assemble(&rule, None).expect("room for all 80");
    assert_eq!(at_rule.evicted, []);
    assert_eq!(at_rule.estimate, 80);

    // 80 of the provider's tokens are 64 of the rule's, which the 80 chosen
    // do not fit, so the view is cut to the latest turn and half of that
    // besides: the openings and the round's newest.
    let (_, at_scale) = store.assemble(&scaled, None).expect("room for some");
    assert_eq!(at_scale.cut, Some(42));
    assert_eq!(ids(&at_scale.carried), [0, 8, 9, 10]);
    assert_eq!(at_scale.estimate, 40, "the estimate is the rule's count");

    let event = |manifest: &Manifest| manifest.event(CallId::new(1)).budget.estimate_scale;
    assert!((event(&at_rule) - 1.0).abs() < 1e-9);
    assert!((event(&at_scale) - 1.25).abs() < 1e-9);
}

/// A turn too large for the room says why the room is less than the window
/// leaves, when the provider's count has scaled it down.
#[test]
fn a_turn_too_large_for_the_scaled_room_says_so() {
    let mut store = five_rounds();
    store.turns[10].tokens = 500;
    let rule = Budget {
        window: 1_000,
        reserve: 250,
        overhead: 300,
        ..Budget::with_room("small", 0)
    };
    let scaled = Budget {
        scale: Scale::observed(5, 4),
        ..rule.clone()
    };
    let err = store
        .assemble(&scaled, None)
        .expect_err("300 of room, and 450 at the rule");
    assert_eq!((err.turn, err.estimate), (Some(10), 500), "{err}");
    let text = err.to_string();
    for needle in [
        "room for about 300",
        "250 reserved for the reply",
        "counted down 1.25x",
        "small's provider has reported requests larger than OutRig estimated them",
    ] {
        assert!(text.contains(needle), "{needle:?} in {text}");
    }
    let at_rule = TooLarge {
        budget: rule,
        ..err
    };
    assert!(!at_rule.to_string().contains("counted down"), "{at_rule}");
}

/// A prompt the rule admits is carried however the provider's count scales the
/// request: the reply gives up the room, where held to the scale with the
/// reserve as it was the call could carry nothing. So is a prompt the scale
/// leaves no room for beside the least reply, with the scale cut back.
#[test]
fn a_scale_that_would_refuse_the_prompt_lowers_the_reply_instead() {
    let rule =
        Budget::new(&candidate("m", Some(200_000)), Some(64_000), 100_000).expect("36000 of room");
    let scaled = rule.scaled(Scale::observed(3, 2));
    assert_eq!(scaled.room(), 0);

    let mut store = priced(five_rounds(), 100);
    let (_, manifest) = store
        .assemble(&scaled, None)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(manifest.carried, [(10, Why::Latest)]);
    let held = &manifest.budget;
    assert!(
        MIN_REPLY <= held.reserve && held.reserve < rule.reserve,
        "{held:?}"
    );
    assert_eq!(held.max_tokens, Some(held.reserve));
    assert_eq!(held.scale, scaled.scale);

    // A prompt that leaves the window less than the least reply beside it.
    store.turns[10].tokens = 35_000;
    let (_, manifest) = store
        .assemble(&scaled, None)
        .unwrap_or_else(|err| panic!("{err}"));
    let held = &manifest.budget;
    assert!(held.reserve >= MIN_REPLY, "{held:?}");
    assert!(
        Scale::RULE < held.scale && held.scale < scaled.scale,
        "{held:?}"
    );

    // Beyond what the rule admits is refused all the same.
    store.turns[10].tokens = 40_000;
    store
        .assemble(&scaled, None)
        .expect_err("36000 at the rule");
}

/// A round's opening is a prompt like any other, and the count it states of the
/// turns its first call leaves out is that call's, with the reply lowered.
#[test]
fn an_opening_the_scale_would_refuse_is_carried_and_counted_exactly() {
    let rule =
        Budget::new(&candidate("m", Some(200_000)), Some(64_000), 100_000).expect("36000 of room");
    let scaled = rule.scaled(Scale::observed(3, 2));

    let mut store = priced(five_rounds(), 100);
    // An announcement of about 4,000 tokens, which the rule's room holds.
    let line = |n: usize| format!("{n} {}", "y".repeat(12_000));
    let opening = store.open_round(&scaled, line);
    assert_eq!(stated(&opening), first_call_leaves_out(&mut store, &scaled));
    let (_, manifest) = store
        .assemble(&scaled, None)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(manifest.opening, Some(opening));
    assert!(manifest.budget.reserve < rule.reserve);
}

/// Each candidate has its own allowance, and the same conversation assembled
/// for a smaller one leaves more out -- in the same order -- and names the
/// candidate it was held to. Its cut is what the next call is sent, whatever
/// candidate it goes to, while that fits.
#[test]
fn a_smaller_candidates_allowance_evicts_more_of_the_same_store() {
    // Priced so each allowance clears the least room a session starts with.
    let mut store = priced(five_rounds(), 1_000);
    let large = Budget::new(&candidate("large", Some(2_000 + 8_000)), Some(1_000), 1_000)
        .expect("8000 of room");
    let small = Budget::new(&candidate("small", Some(2_000 + 6_000)), Some(1_000), 1_000)
        .expect("6000 of room");
    let (_, manifest) = store.assemble(&large, None).expect("room for all");
    assert_eq!(
        (manifest.budget.model.as_str(), manifest.evicted.len()),
        ("large", 0)
    );
    // Cut to the latest and 3000 besides.
    let (_, manifest) = store.assemble(&small, None).expect("room for some");
    assert_eq!(manifest.budget.model, "small");
    assert_eq!(manifest.cut, Some(4_000));
    assert_eq!(
        manifest.evicted,
        [
            (1, Why::First),
            (2, Why::Promoted),
            (6, Why::Recent),
            (7, Why::Recent)
        ]
    );
    assert_eq!(manifest.call, 1, "calls are counted across candidates");
    let (_, after) = store.assemble(&large, None).expect("room for the cut");
    assert_eq!(
        (after.cut, ids(&after.carried)),
        (None, ids(&manifest.carried)),
        "the larger window is sent the cut, not everything again"
    );

    // A latest turn that one candidate's window holds and the other's does not.
    store.turns[10].tokens = 7_000;
    assert!(store.assemble(&large, None).is_ok());
    let err: TooLarge = store.assemble(&small, None).expect_err("7000 of 6000");
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
    store.commit(vec![call("a", "1"), tool_result("a", "one")]);
    store.commit(vec![call("b", "2"), tool_result("b", "two")]);
    // Round 3 opens, and its first call carries round 1 and 2 -- and the
    // opening after a result.
    store.begin_round(Message::user("round 3"));
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY), None)
        .expect("room");
    assert_eq!(
        manifest.adjacent,
        [Adjacent {
            turn: None,
            role: Role::User
        }]
    );
    assert!(manifest.withheld.is_empty(), "recorded, not withheld");
    // Round 2's second turn, promoted into a view without its first, follows
    // round 1's closing text with a tool call.
    store.window = Window {
        first: 1,
        recent: 0,
    };
    promote(&mut store, &[2]);
    let (_, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY), None)
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
    // Room for the opening and two turns, not three, so the first call is
    // cut to the opening and half that, which holds one.
    let opening = crate::agent::budget::tokens([&Message::user("1")]);
    assert_eq!(omitted(&history, opening + 2 * each + each / 2), 2);
    assert_eq!(omitted(&history, opening), 3);
}

/// The count an opening states is what its first call leaves out, however the
/// line's length moves what fits. Here the line grows with the count, and one
/// large turn sits among small ones, so a longer line can leave out the large
/// one and fit the small: costed at its own size, an opening would state a
/// count its call does not keep. So it is under a scale, which moves what fits
/// as well.
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
    for scale in [Scale::RULE, Scale::observed(3, 2)] {
        for room in (0..4_000).step_by(7) {
            let budget = Budget {
                scale,
                ..Budget::with_room("m", room)
            };
            let opening = store.open_round(&budget, line);
            assert_eq!(
                stated(&opening),
                first_call_leaves_out(&mut store, &budget),
                "room {room} at {scale:?}"
            );
        }
    }
}

/// A turn committed while its calls ran is mirrored as incomplete.
#[tokio::test]
async fn an_incomplete_turn_is_mirrored_as_incomplete() {
    let (interpreter, mut fake) = Fake::connected().await;
    let history = History::new(interpreter, Window::DEFAULT);
    history.begin_round(Message::user("go"));
    history.commit(vec![call("a", "1"), tool_result("a", "unknown")], true);
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
            .assemble(&Budget::with_room("m", ROOMY), CallId::new(1))
            .expect("room");
        assert_eq!(sent.last(), Some(&Message::user(format!("r{round}"))));
        assert_eq!(history.reconstruct(&manifest), sent, "a round's first call");
        history.commit(vec![call("a", "1"), tool_result("a", "one")], false);
        let (sent, manifest) = history
            .assemble(&Budget::with_room("m", ROOMY), CallId::new(1))
            .expect("room");
        assert_eq!(sent.last(), Some(&tool_result("a", "one")));
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
            vec![call("a", "print(x)"), tool_result("a", result.as_str())],
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

/// A reply that reasons, signed as Anthropic signs its thinking, then makes
/// call `id`, which its reasoning names too.
fn reasoned_call(id: &str) -> Message {
    reply(vec![
        signed(id),
        AssistantContent::tool_call(id, name(), json!({"source": id})),
    ])
}

/// A reply of `reasoning` alone.
fn thinking_alone(reasoning: AssistantContent) -> Message {
    reply(vec![reasoning])
}

/// The calls whose reasoning `sent` carries, in order.
fn reasoned(sent: &[Message]) -> Vec<String> {
    sent.iter()
        .filter_map(|message| match message {
            Message::Assistant(reply) => Some(reply.content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match part {
            AssistantContent::Reasoning(reasoning) => Some(reasoning.text.clone()),
            _ => None,
        })
        .collect()
}

/// Commit `turns` turns, each a reasoned call `c<n>` and its result priced
/// 10, to the round `store` has open, and assemble a call after each, held
/// to `budget`.
fn reasoned_round(
    store: &mut Store,
    turns: usize,
    budget: &Budget,
) -> Vec<(Vec<Message>, Manifest)> {
    let first = store.turns.len();
    let turns = (first..first + turns).map(|n| {
        let call = format!("c{n}");
        (vec![reasoned_call(&call), tool_result(&call, "ok")], 10)
    });
    calls_of(store, turns, budget)
}

/// To Anthropic, a call leaves out the thinking a change to what came before
/// it made stale, which the API would refuse as bound to another
/// conversation: from the first turn a cut changed what came before, on. The
/// thinking before that, and of every turn produced since, still goes, and
/// between cuts each request begins with the one before it. A turn once stale
/// stays stale.
#[test]
fn a_cut_leaves_out_the_thinking_it_made_stale_and_no_more() {
    use super::LeftOut;
    use crate::agent::budget::Wire;

    let anthropic = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", 100)
    };
    let mut store = Store::default();
    store.begin_round(Message::user("round 1"));
    let calls = reasoned_round(&mut store, 30, &anthropic);

    for (n, (sent, manifest)) in calls.iter().enumerate() {
        assert_eq!(&store.reconstruct(manifest), sent, "call {n}");
    }
    for (n, pair) in calls.windows(2).enumerate() {
        let ((before, _), (after, manifest)) = (&pair[0], &pair[1]);
        if manifest.cut.is_none() {
            assert!(after.starts_with(before), "call {}", n + 1);
        }
    }
    // Cut at calls 10, 15, 20 and 25, each to the opening turn and the
    // newest four. At the first, the opening turn's thinking stays, and the
    // rest it keeps goes.
    let (sent, manifest) = &calls[10];
    assert_eq!(ids(&manifest.carried), [0, 6, 7, 8, 9, 10]);
    assert_eq!(reasoned(sent), ["c0"]);
    assert_eq!(
        manifest.left_out,
        (6..=10)
            .map(|turn| LeftOut {
                turn,
                message: 0,
                part: 0
            })
            .collect::<Vec<_>>()
    );
    // What the cut's own call produced is sent with its thinking.
    assert_eq!(reasoned(&calls[11].0), ["c0", "c11"]);
    // The last call, after the cut at 25: the opening turn and every turn
    // produced since keep their thinking; the four the cut kept do not.
    let (sent, manifest) = calls.last().expect("thirty calls");
    assert_eq!(
        ids(&manifest.carried),
        [0, 21, 22, 23, 24, 25, 26, 27, 28, 29]
    );
    assert_eq!(reasoned(sent), ["c0", "c26", "c27", "c28", "c29"]);
}

/// An OpenAI-compatible provider's reasoning carries no binding to the
/// conversation, so a call to one is sent all of it, cut or not.
#[test]
fn a_cut_leaves_every_turns_thinking_to_an_openai_compatible_call() {
    let mut store = Store::default();
    store.begin_round(Message::user("round 1"));
    let calls = reasoned_round(&mut store, 30, &Budget::with_room("m", 100));
    let (sent, manifest) = calls.last().expect("thirty calls");
    assert!(manifest.left_out.is_empty(), "{manifest:?}");
    assert_eq!(
        reasoned(sent),
        ids(&manifest.carried)
            .iter()
            .map(|id| format!("c{id}"))
            .collect::<Vec<_>>()
    );
}

/// A round whose window moves past a middle round changes what came before
/// every turn after the gap, so their thinking goes; the first rounds', before
/// it, stays.
#[test]
fn a_window_that_moves_on_leaves_out_the_thinking_after_the_gap() {
    use crate::agent::budget::Wire;

    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    let anthropic = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", ROOMY)
    };
    let mut sent = Vec::new();
    for round in 1..=4 {
        store.begin_round(Message::user(format!("round {round}")));
        let call = format!("c{round}");
        store.commit(vec![reasoned_call(&call), tool_result(&call, "ok")]);
        sent.push(store.assemble(&anthropic, None).expect("room").0);
        store.commit(vec![Message::assistant(format!("done {round}"))]);
    }
    // Round 3's window is rounds 1 and 2, as round 2's was round 1: it only
    // grew. Round 4's is rounds 1 and 3, so round 3's turns follow round 1's
    // where round 2's were.
    assert_eq!(reasoned(&sent[2]), ["c1", "c2", "c3"]);
    assert_eq!(reasoned(&sent[3]), ["c1"]);
}

/// A turn a call to Anthropic sends nothing of -- another provider's thinking
/// alone -- changes nothing before what follows it, so promoting one leaves
/// the thinking after it be, on that call and every later one.
#[test]
fn a_promotion_sent_as_nothing_leaves_the_thinking_after_it() {
    use rig::completion::message::Reasoning;

    use crate::agent::budget::Wire;

    // Round 2 closes on an OpenAI-compatible model's thinking alone.
    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    round_of_calls(&mut store, "round 1", &[], Some("done"));
    round_of_calls(&mut store, "round 2", &["a"], None);
    store.commit(vec![thinking_alone(AssistantContent::Reasoning(
        Reasoning::new("mulling"),
    ))]);
    round_of_calls(&mut store, "round 3", &[], Some("done"));
    store.begin_round(Message::user("round 4"));
    let anthropic = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", ROOMY)
    };
    store.assemble(&anthropic, None).expect("room");

    store.commit(vec![reasoned_call("c4"), tool_result("c4", "ok")]);
    promote(&mut store, &[2]);
    let (sent, manifest) = store.assemble(&anthropic, None).expect("room");
    assert_eq!(ids(&manifest.carried), [0, 2, 3, 4]);
    assert_eq!(reasoned(&sent), ["c4"]);
    let (sent, _) = reasoned_round(&mut store, 1, &anthropic).remove(0);
    assert_eq!(reasoned(&sent), ["c4", "c5"]);
}

/// A call is sent only the reasoning its provider can take back. To
/// Anthropic, an unsigned block -- another provider's -- is left out, and with
/// it a reply of nothing else; Anthropic's own signed block goes as it came.
/// The manifest names each part left out, so the record alone rebuilds the
/// call, and the turns keep everything.
#[test]
fn a_call_is_sent_only_the_reasoning_its_provider_takes() {
    use super::LeftOut;
    use crate::agent::budget::Wire;

    let unsigned = AssistantContent::Reasoning(Reasoning::new("thinking it over"));
    let signed = signed("weighing it");
    let mut store = Store::default();
    store.begin_round(Message::user("go"));
    store.commit(vec![
        reply(vec![
            unsigned.clone(),
            AssistantContent::tool_call("call_1", name(), json!({"source": "1"})),
        ]),
        tool_result("call_1", "1"),
    ]);
    store.commit(vec![reply(vec![unsigned.clone()])]);
    store.commit(vec![reply(vec![signed.clone()])]);
    let whole: Vec<Message> = store
        .turns
        .iter()
        .flat_map(|turn| turn.messages.clone())
        .collect();

    for (wire, left_out) in [
        (
            Wire::Anthropic,
            vec![
                LeftOut {
                    turn: 0,
                    message: 1,
                    part: 0,
                },
                LeftOut {
                    turn: 1,
                    message: 0,
                    part: 0,
                },
            ],
        ),
        (Wire::OpenAi, Vec::new()),
    ] {
        let budget = Budget {
            wire,
            ..Budget::with_room("m", ROOMY)
        };
        let (sent, manifest) = store.assemble(&budget, None).expect("room for everything");
        assert_eq!(manifest.left_out, left_out, "{wire:?}");
        let unsigned_sent = sent.iter().any(|message| {
            matches!(message, Message::Assistant(reply) if reply.content.contains(&unsigned))
        });
        assert_eq!(unsigned_sent, wire == Wire::OpenAi, "{wire:?}: {sent:#?}");
        assert_eq!(
            store.reconstruct(&manifest),
            sent,
            "{wire:?}: the record rebuilds the call"
        );
    }
    let kept: Vec<Message> = store
        .turns
        .iter()
        .flat_map(|turn| turn.messages.clone())
        .collect();
    assert_eq!(kept, whole, "the turns keep what each model wrote");
}

/// Four rounds, the window the first and the latest: round 1 (turn 0) closes
/// on text, round 2 (turns 1 and 2) is cut short after its second result, and
/// round 3 (turns 3 to 5) and round 4 (turn 6) close on text.
fn one_round_cut_short() -> Store {
    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    round_of_calls(&mut store, "round 1", &[], Some("done"));
    round_of_calls(&mut store, "round 2", &["a", "b"], None);
    round_of_calls(&mut store, "round 3", &["c", "d"], Some("fine"));
    round_of_calls(&mut store, "round 4", &[], Some("ok"));
    store
}

/// For a provider that requires the user's and the model's turns to
/// alternate, the view withholds what would put one role after itself rather
/// than recording it. A round cut short goes, turn by turn, once the next
/// round opens after its results; a turn promoted without the rest of its
/// round goes when it would follow the model's own reply; a whole promoted
/// round alternates on its own and stays. The manifest names each turn
/// withheld, and the record still rebuilds the call.
#[test]
fn a_strict_view_withholds_what_would_put_one_role_after_itself() {
    let mut store = one_round_cut_short();
    store.begin_round(Message::user("round 5"));
    let strict = Budget::with_room("m", ROOMY).strict();

    // Rounds 1 and 4 alternate with the opening on their own.
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.carried, [(0, Why::First), (6, Why::Recent)]);
    assert!(manifest.withheld.is_empty(), "{manifest:?}");
    assert!(manifest.adjacent.is_empty(), "{manifest:?}");
    let alone = [
        Role::User,
        Role::Assistant,
        Role::User,
        Role::Assistant,
        Role::User,
    ];
    assert_eq!(roles(&sent), alone);

    // Round 2's second turn alone opens on a call right after round 1's reply.
    promote(&mut store, &[2]);
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.withheld, [(2, Why::Promoted)]);
    assert_eq!(manifest.carried, [(0, Why::First), (6, Why::Recent)]);
    assert!(manifest.adjacent.is_empty(), "{manifest:?}");
    assert_eq!(roles(&sent), alone);
    assert_eq!(store.reconstruct(&manifest), sent);

    // Both of round 2's turns: the first opens on the user's side and the
    // second follows its results, but round 4 then opens after a result the
    // model never answered, so the earlier side yields -- both turns.
    promote(&mut store, &[1]);
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.withheld, [(1, Why::Promoted), (2, Why::Promoted)]);
    assert_eq!(roles(&sent), alone);

    // A whole promoted round alternates on its own, and stays.
    store.change(ContextChange::Demote(vec![1, 2]));
    promote(&mut store, &[3, 4, 5]);
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert!(manifest.withheld.is_empty(), "{manifest:?}");
    assert_eq!(ids(&manifest.carried), [0, 3, 4, 5, 6]);
    assert!(alternates(&sent), "{:?}", roles(&sent));
    assert_eq!(store.reconstruct(&manifest), sent);

    // The window can hold the round cut short too: round 3 then opens after
    // round 2's last result, and round 2 goes turn by turn.
    store.change(ContextChange::Demote(vec![3, 4, 5]));
    store.window = Window {
        first: 1,
        recent: 3,
    };
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.withheld, [(1, Why::Recent), (2, Why::Recent)]);
    assert!(manifest.adjacent.is_empty(), "{manifest:?}");
    assert!(alternates(&sent), "{:?}", roles(&sent));
    assert_eq!(store.reconstruct(&manifest), sent);

    // Relaxed, the same view goes whole, and the pair is recorded instead.
    let (sent, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY), None)
        .expect("room");
    assert!(manifest.withheld.is_empty());
    assert_eq!(
        manifest.adjacent,
        [Adjacent {
            turn: Some(3),
            role: Role::User
        }]
    );
    assert!(!alternates(&sent));
}

/// A strict view's cut keeps the turns it withholds so the roles alternate,
/// so the calls after it withhold them again rather than call them evicted.
#[test]
fn a_cut_keeps_what_a_strict_view_withholds_withheld() {
    // Round 2's second turn (2) alone opens on a call right after round 1's
    // reply. Round 4's reply (6) is too large for half the room.
    let mut store = one_round_cut_short();
    promote(&mut store, &[2]);
    for (id, tokens) in [(0, 10), (2, 10), (6, 60)] {
        store.turns[id].tokens = tokens;
    }
    store.begin_round(Message::user("round 5"));
    let strict = Budget::with_room("m", 90).strict();
    let calls = calls_priced(&mut store, &[1; 13], &strict);

    let cut = calls
        .iter()
        .position(|(_, manifest)| manifest.cut.is_some())
        .expect("the round outgrew its room");
    for (_, manifest) in &calls[cut..] {
        assert_eq!(manifest.withheld, [(2, Why::Promoted)], "{manifest:?}");
        assert_eq!(manifest.evicted, [(6, Why::Recent)], "{manifest:?}");
    }
    assert!(
        calls
            .last()
            .is_some_and(|(_, manifest)| manifest.cut.is_none())
    );
}

/// A turn a strict view withholds so the roles alternate changes nothing a
/// call is sent, so the thinking after it is not stale. Once a call is sent
/// it, the thinking after it is.
#[test]
fn a_withheld_turn_makes_the_thinking_after_it_stale_only_once_it_is_sent() {
    use crate::agent::budget::Wire;

    let mut store = one_round_cut_short();
    store.begin_round(Message::user("round 5"));
    let anthropic = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", ROOMY)
    };
    let strict = anthropic.clone().strict();
    store.assemble(&strict, None).expect("room");

    // Round 5's first turn promotes round 2's second, which alone opens on a
    // call right after round 1's reply, and is withheld.
    store.commit(vec![reasoned_call("c7"), tool_result("c7", "ok")]);
    promote(&mut store, &[2]);
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.withheld, [(2, Why::Promoted)]);
    assert_eq!(reasoned(&sent), ["c7"]);

    // A model that takes the roles as they come is sent it, before round 4's
    // turn and round 5's.
    let (sent, manifest) = store.assemble(&anthropic, None).expect("room");
    assert_eq!(ids(&manifest.carried), [0, 2, 6, 7]);
    assert!(reasoned(&sent).is_empty(), "{sent:#?}");
}

/// Where the roles must alternate, a reply of thinking alone that goes stale
/// leaves its turn with neither role, and the results before it answered by
/// nothing, so the view withholds them -- which changes what comes before
/// what follows too. The call is sent what alternates.
#[test]
fn a_strict_view_withholds_the_results_stale_thinking_answered() {
    use crate::agent::budget::Wire;

    // Rounds 1 and 2 close on text, and round 3 on thinking alone after its
    // one call.
    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    round_of_calls(&mut store, "round 1", &[], Some("done"));
    round_of_calls(&mut store, "round 2", &[], Some("done"));
    store.begin_round(Message::user("round 3"));
    store.commit(vec![reasoned_call("c2"), tool_result("c2", "ok")]);
    store.commit(vec![thinking_alone(signed("r3"))]);
    store.begin_round(Message::user("round 4"));
    let strict = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", ROOMY)
    }
    .strict();
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(ids(&manifest.carried), [0, 2, 3]);
    assert_eq!(reasoned(&sent), ["c2", "r3"]);

    // Round 2, promoted, comes before round 3, whose thinking goes stale.
    promote(&mut store, &[1]);
    let (sent, manifest) = store.assemble(&strict, None).expect("room");
    assert_eq!(manifest.withheld, [(2, Why::Recent)]);
    assert!(reasoned(&sent).is_empty(), "{sent:#?}");
    assert!(alternates(&sent), "{:?}", roles(&sent));
    assert_eq!(store.reconstruct(&manifest), sent);
}

/// The turn a call answers is never withheld. When the budget leaves out the
/// turns of its round before it, so that it opens on a call right after an
/// earlier round's closing reply, what was kept after the nearest results
/// yields instead, and the call still alternates. When nothing kept ends on
/// the user's side, the pair stands and is recorded, as a relaxed provider's
/// would be; and a closing reply that would open the view on the model's side
/// is withheld, so the latest turn can be sent alone. Those two are the shapes
/// a strict provider still refuses, and only a larger window mends them.
#[test]
fn a_strict_view_never_withholds_the_turn_a_call_answers() {
    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    // Round 1: a call and its result (turn 0), then a closing reply (turn 1).
    // Round 2: a closing reply alone (turn 2). Round 3: its first turn (turn
    // 3) is too large for the room, and its second (turn 4) is the latest.
    round_of_calls(&mut store, "round 1", &["a"], Some("done"));
    round_of_calls(&mut store, "round 2", &[], Some("ok"));
    round_of_calls(&mut store, "round 3", &["b", "c"], None);
    let mut store = priced(store, 10);
    store.turns[3].tokens = 1_000;
    let strict = Budget::with_room("m", 100).strict();

    // The latest would follow round 2's reply: back to round 1's results.
    let (sent, manifest) = store.assemble(&strict, None).expect("the latest fits");
    assert_eq!(manifest.evicted, [(3, Why::Round)]);
    assert_eq!(manifest.withheld, [(1, Why::First), (2, Why::Recent)]);
    assert_eq!(manifest.carried, [(0, Why::First), (4, Why::Latest)]);
    assert!(manifest.adjacent.is_empty(), "{manifest:?}");
    assert_eq!(
        roles(&sent),
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
    assert_eq!(store.reconstruct(&manifest), sent);

    // Round 1's results out as well: its closing reply would open the view
    // on the model's side and is withheld, round 2 alone ends on a reply, and
    // the pair stands, recorded.
    store.turns[0].tokens = 1_000;
    let (sent, manifest) = store.assemble(&strict, None).expect("the latest fits");
    assert_eq!(manifest.evicted, [(0, Why::First), (3, Why::Round)]);
    assert_eq!(manifest.withheld, [(1, Why::First)]);
    assert_eq!(manifest.carried, [(2, Why::Recent), (4, Why::Latest)]);
    assert_eq!(
        manifest.adjacent,
        [Adjacent {
            turn: Some(4),
            role: Role::Assistant
        }]
    );
    assert_eq!(
        roles(&sent),
        [Role::User, Role::Assistant, Role::Assistant, Role::User]
    );

    // Round 2 out too: the latest is sent alone.
    store.turns[2].tokens = 1_000;
    let (sent, manifest) = store.assemble(&strict, None).expect("the latest fits");
    assert_eq!(manifest.withheld, [(1, Why::First)]);
    assert_eq!(manifest.carried, [(4, Why::Latest)]);
    assert!(manifest.adjacent.is_empty(), "{manifest:?}");
    assert_eq!(roles(&sent), [Role::Assistant, Role::User]);
}

/// A closing reply the provider cannot take -- reasoning no signature vouches
/// for, to Anthropic -- leaves its round ending on results, as a call to that
/// provider is sent it. The seam is judged on what the wire carries, so the
/// turn before yields; to a provider that takes the reply, nothing does.
#[test]
fn an_emptied_reply_leaves_its_round_ending_on_results() {
    use rig::completion::message::Reasoning;

    use super::LeftOut;
    use crate::agent::budget::Wire;

    let mut store = Store {
        window: Window {
            first: 1,
            recent: 1,
        },
        ..Store::default()
    };
    round_of_calls(&mut store, "round 1", &[], Some("done"));
    round_of_calls(&mut store, "round 2", &["a"], None);
    store.commit(vec![thinking_alone(AssistantContent::Reasoning(
        Reasoning::new("thinking"),
    ))]);
    store.begin_round(Message::user("round 3"));

    let anthropic = Budget {
        wire: Wire::Anthropic,
        ..Budget::with_room("m", ROOMY).strict()
    };
    let (sent, manifest) = store.assemble(&anthropic, None).expect("room");
    assert_eq!(manifest.withheld, [(1, Why::Recent)]);
    assert_eq!(manifest.carried, [(0, Why::First), (2, Why::Recent)]);
    assert_eq!(
        manifest.left_out,
        [LeftOut {
            turn: 2,
            message: 0,
            part: 0
        }]
    );
    assert_eq!(roles(&sent), [Role::User, Role::Assistant, Role::User]);
    assert_eq!(store.reconstruct(&manifest), sent);

    let (sent, manifest) = store
        .assemble(&Budget::with_room("m", ROOMY).strict(), None)
        .expect("room");
    assert!(manifest.withheld.is_empty(), "{manifest:?}");
    assert!(manifest.left_out.is_empty());
    assert!(alternates(&sent), "{:?}", roles(&sent));
}

/// The count an opening states is what its first call leaves out, the turns
/// withheld so the roles alternate included: a round cut short is counted
/// whenever it would have fit.
#[test]
fn the_opening_counts_what_a_strict_first_call_withholds() {
    let mut store = rounds_of(
        Window {
            first: 1,
            recent: 2,
        },
        3,
        1,
    );
    // A fourth round, cut short, which every opening after it would follow.
    round_of_calls(&mut store, "round 4", &["a"], None);
    for (turn, tokens) in store.turns.iter_mut().zip([2_000, 100, 100, 100]) {
        turn.tokens = tokens;
    }
    let line = |n: usize| format!("{n} {}", "y".repeat(30 * n));
    for room in (0..2_600).step_by(7) {
        let budget = Budget::with_room("m", room).strict();
        let opening = store.open_round(&budget, line);
        assert_eq!(
            stated(&opening),
            first_call_leaves_out(&mut store, &budget),
            "room {room}"
        );
    }
    // With room for everything, the count is round 2, outside the window,
    // and round 4's turn, withheld.
    let budget = Budget::with_room("m", ROOMY).strict();
    let opening = store.open_round(&budget, line);
    let (sent, manifest) = store.assemble(&budget, None).expect("room");
    assert_eq!(stated(&opening), 2);
    assert_eq!(manifest.withheld, [(3, Why::Recent)]);
    assert!(alternates(&sent), "{:?}", roles(&sent));
}

/// Each tool result in `messages`: the call it answers, and its text.
fn results(messages: &[Message]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => Some(content.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|part| match part {
            UserContent::ToolResult(result) => match result.content.first() {
                Some(ToolResultContent::Text(text)) => {
                    Some((result.call.to_string(), text.text.clone()))
                }
                other => panic!("not text: {other:?}"),
            },
            _ => None,
        })
        .collect()
}

/// A call a provider refused for what it carried is sent again with a notice
/// in place of each result of the turn it answers, and only that turn's: the
/// call beside each stays, and the store keeps the results whole. That view
/// leaves nothing out of the next; once the store withholds the turn, every
/// later call carries the notice, in this round and the next. The record
/// rebuilds each call.
#[test]
fn a_refused_turns_results_are_sent_as_a_notice_and_kept_whole() {
    let mut store = Store::default();
    round_of_calls(&mut store, "go", &["a", "b"], None);
    let budget = Budget::with_room("m", ROOMY);
    let refused = Refused {
        turn: 1,
        finish_reason: "content_filter".to_string(),
    };
    let ok = |id: &str| (id.to_string(), "ok".to_string());
    let noted = |id: &str| (id.to_string(), notice(1, "content_filter"));

    let (sent, manifest) = store.assemble(&budget, Some(&refused)).expect("room");
    assert_eq!(results(&sent), [ok("a"), noted("b")]);
    assert_eq!(manifest.refused, [(1, notice(1, "content_filter"))]);
    assert_eq!(store.reconstruct(&manifest), sent);
    assert_eq!(results(&store.turns[1].messages), [ok("b")], "kept whole");
    assert!(
        notice(1, "content_filter").contains("runtime.history.turns[1]")
            && notice(1, "content_filter").contains("finish reason content_filter"),
        "{}",
        notice(1, "content_filter")
    );

    let (sent, manifest) = store.assemble(&budget, None).expect("room");
    assert_eq!(results(&sent), [ok("a"), ok("b")], "nothing stays withheld");
    assert!(manifest.refused.is_empty());

    store.withhold(&refused);
    store.commit(vec![call("c", "c"), tool_result("c", "ok")]);
    let (sent, manifest) = store.assemble(&budget, None).expect("room");
    assert_eq!(results(&sent), [ok("a"), noted("b"), ok("c")]);
    assert_eq!(manifest.refused, [(1, notice(1, "content_filter"))]);
    assert_eq!(store.reconstruct(&manifest), sent);

    store.commit(vec![Message::assistant("done")]);
    round_of_calls(&mut store, "again", &["d"], None);
    let (sent, manifest) = store.assemble(&budget, None).expect("room");
    assert_eq!(manifest.carried[1], (1, Why::First));
    assert_eq!(results(&sent), [ok("a"), noted("b"), ok("c"), ok("d")]);
    assert_eq!(store.reconstruct(&manifest), sent);
}

/// A notice can be longer than the results it stands in for, so a turn sent
/// with one is counted as it is sent, in choosing what fits as well as in the
/// estimate: a call that held fifty short results with room to spare holds
/// their notices only by leaving out what it carried before, or not at all.
#[test]
fn a_refused_turn_is_counted_with_its_notices() {
    let mut store = Store::default();
    round_of_calls(&mut store, "go", &["a"], None);
    let ids: Vec<String> = (0..50).map(|n| format!("c{n}")).collect();
    let calls = ids
        .iter()
        .map(|id| AssistantContent::tool_call(id, name(), json!({"source": "1"})))
        .collect();
    let results = ids.iter().map(|id| result_part(id, "")).collect();
    store.commit(vec![reply(calls), Message::User { content: results }]);
    let refused = Refused {
        turn: 1,
        finish_reason: "content_filter".to_string(),
    };
    let (first, latest) = (store.turns[0].tokens, store.turns[1].tokens);
    let noted = budget::tokens(&without_results(
        store.turns[1].messages.clone(),
        &notice(1, "content_filter"),
    ));
    assert!(noted > first + latest, "{noted} against {first} + {latest}");

    let both = Budget::with_room("m", first + latest);
    let (_, manifest) = store.assemble(&both, None).expect("room for both");
    assert_eq!(manifest.carried, [(0, Why::Round), (1, Why::Latest)]);
    let err = store
        .assemble(&both, Some(&refused))
        .expect_err("no room for the notices");
    assert_eq!((err.turn, err.estimate), (Some(1), noted));

    let alone = Budget::with_room("m", noted + first - 1);
    let (sent, manifest) = store.assemble(&alone, Some(&refused)).expect("room");
    assert_eq!(manifest.carried, [(1, Why::Latest)]);
    assert_eq!(manifest.evicted, [(0, Why::Round)]);
    assert_eq!(manifest.estimate, noted);
    assert_eq!(
        budget::tokens(&sent),
        noted,
        "what was sent is what was counted"
    );

    // As the store withholds it, every later call counts it so too.
    store.withhold(&refused);
    let (_, manifest) = store.assemble(&alone, None).expect("room");
    assert_eq!(manifest.evicted, [(0, Why::Round)]);
    assert_eq!(manifest.estimate, noted);
}
