#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["jinja2>=3.1"]
# ///
"""Render an OutRig session to one self-contained HTML page.

    uv run --script scripts/render-session.py <session> [--session-root PATH] [--out PATH]

`<session>` is a session directory, or a session id -- or enough of one to name a single session,
as `outrig logs` takes it -- looked up under the session root, found the way `outrig` finds it:
`--session-root`, else `session-root` in the repo's `.agents/outrig/config.toml` or in the global
config, else `$XDG_DATA_HOME/outrig/sessions`. Reads `session.json`, `logs/events.jsonl` -- whose
schema is `doc/reference/events.md` -- and `logs/network.jsonl` when there is one.
`doc/usage/sessions.md` says what the page shows. It goes to `<session>/report.html` unless
`--out` says otherwise, readable by its owner only, since it holds what `events.jsonl` holds; its
path is printed.

Everything the page shows was written by a model, by code a model wrote, by a user, or by a
container -- source, tracebacks, captured output, message bodies, the host a connection claimed
-- so all of it is hostile-shaped text going into HTML. It reaches the page only through Jinja
with autoescaping enabled, which is not Jinja's default, and only into text and quoted
attributes: never through `|safe` or `Markup`, never into a URL or a `<style>`. Links are built
here, from integer ids alone. A Content-Security-Policy that allows no script is a second line,
not the first.

A record still being written, or one whose session died, can end in a partial line. A line that
does not parse is skipped and listed on the page rather than ending the render.

Exit status: 0 when the page was written, 1 when the session could not be read or the page could
not be written, 2 on a usage error.

Unlike the repo's CI scripts this is not standard-library only. It is run by a person, `uv`
provisions what the block above declares, and that block is kept short enough to read first.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
import sys
import tempfile
import tomllib
from collections import Counter
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from jinja2 import Environment, StrictUndefined

PREFIX = "org.outrig."
EVENTS = Path("logs") / "events.jsonl"
NETWORK = Path("logs") / "network.jsonl"
SESSION = Path("session.json")
REPORT = "report.html"
REPO_CONFIG = Path(".agents") / "outrig" / "config.toml"
# The token table's usage columns: each field as the record names it, and as the page does.
USAGE = (
    ("input_tokens", "input"),
    ("output_tokens", "output"),
    ("cached_input_tokens", "cache read"),
    ("cache_creation_input_tokens", "cache write"),
    ("reasoning_tokens", "reasoning"),
    ("total_tokens", "total"),
)
# The token table's columns.
TOKEN_COLUMNS = (
    ("round", False),
    ("outcome", False),
    ("calls", True),
    *((label, True) for _, label in USAGE),
    ("largest request", True),
    ("estimated", True),
    ("window", True),
)
MESSAGE_COLUMNS = tuple(
    (label, False) for label in ("id", "channel", "from", "to", "sent", "then", "body")
)
# The network table's columns, in the order `network_rows` gives its cells, and whether each is a
# number.
NETWORK_COLUMNS = (
    ("time", False),
    ("host", False),
    ("address", False),
    ("service", False),
    ("SNI", False),
    ("action", False),
    ("rule", False),
    ("bytes out / in", True),
    ("duration", True),
)


# ---------------------------------------------------------------------------
# Reading. Every field is read through one of these, so a missing or
# wrong-typed one renders as nothing rather than failing the page.


def text(value: object) -> str:
    """A field as text: a string as it is, nothing as nothing, anything else as JSON."""
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False)


# The largest number the record writes: its counts and ids are u64s.
U64 = 2**64 - 1


def num(value: object) -> int | float | None:
    """A number the record could have written: a u64 count, or a finite f64. JSON can say
    `1e309`, which is infinite, and integers too long to print, and neither is one of them."""
    if isinstance(value, bool) or not isinstance(value, int | float):
        return None
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    return value if -U64 <= value <= U64 else None


def ident(value: object) -> int | None:
    """An id that can name an anchor: an integer the record could have written, and nothing
    else."""
    n = num(value)
    return n if isinstance(n, int) else None


def decimal(value: object) -> int | None:
    """An envelope's `id`, a decimal string, as a number; or `None` for anything else, including
    digits too many for a u64 -- which `int` would refuse past 4,300 of them."""
    t = text(value)
    return int(t) if t.isdecimal() and len(t) <= len(str(U64)) else None


def seq(value: object) -> list:
    return value if isinstance(value, list) else []


def obj(value: object) -> dict:
    return value if isinstance(value, dict) else {}


def objs(value: object) -> list[dict]:
    return [obj(v) for v in seq(value)]


def key(value: object) -> int | str:
    """An id to correlate by: an integer as it is, anything else as its text."""
    n = ident(value)
    return n if n is not None else text(value)


def first_line(value: str, width: int = 80) -> str:
    line = value.strip().split("\n", 1)[0]
    more = len(line) > width or "\n" in value.strip()
    return line[:width] + (" ..." if more else "")


@dataclass
class Skipped:
    line: int
    why: str


def read_jsonl(path: Path) -> tuple[list[dict], list[Skipped]]:
    """Each whole line's object, and the lines that were not one.

    Decoded a line at a time, so a final line cut off mid-character costs that line alone.
    """
    records: list[dict] = []
    skipped: list[Skipped] = []
    with path.open("rb") as f:
        for number, raw in enumerate(f, 1):
            if not raw.strip():
                continue
            try:
                record = json.loads(raw.decode("utf-8"))
            except (ValueError, RecursionError) as e:
                why = (
                    "cut off: the record was being written when it was read, or when the session "
                    "ended"
                    if not raw.endswith(b"\n")
                    else f"not JSON ({e})"
                )
                skipped.append(Skipped(number, why))
                continue
            if isinstance(record, dict):
                records.append(record)
            else:
                skipped.append(Skipped(number, "not a JSON object"))
    return records, skipped


# ---------------------------------------------------------------------------
# What the template is given.


@dataclass
class Part:
    """A block of preformatted text, open or behind a `<details>`; or, with `href`, a link."""

    label: str
    body: str = ""
    open: bool = False
    href: str = ""


@dataclass
class Item:
    """One timeline row."""

    time: str
    kind: str
    summary: str
    href: str = ""
    anchor: str = ""
    parts: list[Part] = field(default_factory=list)
    subject: str = ""


@dataclass
class Block:
    """A round, from `round.started` to how it ended, or what happened outside one; and, closed,
    a row of the token table."""

    anchor: str
    title: str
    # The round's number as the record gives it; `None` for what happened outside a round.
    round: str | None = None
    outcome: str = ""
    items: list[Item] = field(default_factory=list)
    # Executions submitted in it, by source, not yet claimed by a turn's tool call.
    sources: list[tuple[str, Exec]] = field(default_factory=list)
    calls: int = 0
    reported: int = 0
    usage: dict[str, int | float] = field(default_factory=dict)
    peak: int | float | None = None
    estimate: int | float | None = None
    window: str = ""


@dataclass
class Exec:
    id: int | str
    anchor: str
    subject: str
    source: str = ""
    status: str = ""
    duration: str = ""
    output: str = ""
    dropped: int | float | None = None
    error: str = ""
    submitted: str = ""
    ended: str = ""
    background: list[Part] = field(default_factory=list)
    notes: list[Item] = field(default_factory=list)


@dataclass
class Msg:
    id: int | str
    anchor: str
    channel: str = ""
    sender: str = ""
    to: str = ""
    body: str | None = None
    sent: str = ""
    outcome: str = "not taken"
    outcome_at: str = ""


def anchor(prefix: str, value: object) -> str:
    n = ident(value)
    return "" if n is None else f"{prefix}-{n}"


def link(to: str) -> str:
    return f"#{to}" if to else ""


def clock(value: object) -> str:
    """`HH:MM:SS.mmm` of an RFC 3339 time, or the value as it is."""
    t = text(value)
    return t[11:23] if len(t) >= 23 and t[10] == "T" else t


def seconds(value: object) -> str:
    s = num(value)
    if s is None:
        return text(value)
    return f"{s:.2f} s" if s < 60 else f"{int(s // 60)}m {s % 60:.1f}s"


def tokens(value: object) -> str:
    n = num(value)
    return "" if n is None else f"{n:,}"


def usage_of(value: object) -> dict[str, int | float]:
    return {name: num(obj(value).get(name)) or 0 for name, _ in USAGE}


def usage_summary(value: object) -> str:
    usage = usage_of(value)
    return f"{usage['input_tokens']:,} in, {usage['output_tokens']:,} out"


def turn_list(chosen: object) -> str:
    return ", ".join(f"{text(c.get('turn'))} ({text(c.get('why'))})" for c in objs(chosen))


# ---------------------------------------------------------------------------
# One pass over the record, in file order -- which is the order things happened in, since
# every event is numbered and queued in one step.


class Builder:
    """The record, read in one pass. Each `on_<type>` handler is given the event's `data`; its
    type, time, and agent are on `self` while it is handled."""

    def __init__(self) -> None:
        self.blocks: list[Block] = []
        self.open: Block | None = None
        self.rounds: list[Block] = []
        self.attempts: Counter[str] = Counter()
        self.execs: dict[int | str, Exec] = {}
        self.messages: dict[int | str, Msg] = {}
        self.health: list[str] = []
        self.facts: dict[str, str] = {}
        self.instructions: list[Part] = []
        self.agents: set[str] = set()
        self.kind = ""
        self.time = ""
        self.subject = ""

    # -- blocks -------------------------------------------------------------

    def here(self) -> Block:
        """The open round, or the block of what happened outside one."""
        if self.open is not None:
            return self.open
        if not self.blocks or self.blocks[-1].round is not None:
            title = "Before the first round" if not self.blocks else "Outside a round"
            self.blocks.append(Block(f"block-{len(self.blocks)}", title))
        return self.blocks[-1]

    def begin(self, number: object) -> Block:
        """Open a round. Keyed by when it started, not by its number, which a round that
        commits no turn leaves to the next."""
        if self.open is not None:
            self.close("unfinished: the next round started before it ended")
        label = text(number) or "?"
        self.attempts[label] += 1
        attempt = self.attempts[label]
        title = f"Round {label}" + (f", attempt {attempt}" if attempt > 1 else "")
        self.open = Block(f"block-{len(self.blocks)}", title, label)
        self.blocks.append(self.open)
        return self.open

    def close(self, outcome: str, data: dict | None = None) -> None:
        block, self.open = self.open, None
        if block is None:
            return
        block.outcome = outcome
        data = data or {}
        calls = [usage_of(c.get("usage")) for c in objs(data.get("calls"))]
        block.reported = len(calls)
        # A round's own total when it carries one; otherwise its calls', summed here.
        if data.get("usage") is not None:
            block.usage = usage_of(data.get("usage"))
        else:
            block.usage = {name: sum(c[name] for c in calls) for name, _ in USAGE}
        block.peak = num(data.get("input_tokens_max"))
        if block.peak is None and calls:
            block.peak = max(c["input_tokens"] for c in calls)
        self.rounds.append(block)

    def add(self, summary: str, **kw: object) -> Item:
        item = Item(self.time, self.kind, summary, subject=self.subject, **kw)  # type: ignore[arg-type]
        self.here().items.append(item)
        return item

    # -- correlation --------------------------------------------------------

    def exec(self, value: object) -> Exec:
        k = key(value)
        if k not in self.execs:
            self.execs[k] = Exec(k, anchor("exec", value), self.subject or "(no subject)")
        return self.execs[k]

    def message(self, value: object, d: dict) -> Msg:
        """The message `value` names. Its ends are its sending's, or, with no sending recorded,
        those of the first event to name it."""
        k = key(value)
        if k not in self.messages:
            self.messages[k] = Msg(k, anchor("msg", value))
        m = self.messages[k]
        if m.body is None:
            m.channel, m.sender, m.to = (
                text(d.get("channel")),
                text(d.get("from")),
                text(d.get("to")),
            )
        return m

    def claim(self, source: str) -> Exec | None:
        """The first execution this round submitted with exactly `source`, not yet claimed by
        a turn's tool call. Matched by content, not position: the record does not say which
        call produced which execution, and a position would claim it did."""
        sources = self.here().sources
        for i, (submitted, x) in enumerate(sources):
            if submitted == source:
                del sources[i]
                return x
        return None

    def note(self, d: dict, summary: str, parts: list[Part] | None = None) -> None:
        """Something that happened to an execution: on its entry, and on the timeline."""
        x = self.exec(d.get("execid"))
        x.notes.append(Item(self.time, self.kind, summary, parts=parts or []))
        self.add(f"exec {x.id}: {summary}", href=link(x.anchor), parts=parts or [])

    # -- the record ---------------------------------------------------------

    def feed(self, record: dict) -> None:
        kind = text(record.get("type"))
        name = kind.removeprefix(PREFIX) if kind.startswith(PREFIX) else ""
        handler = getattr(self, "on_" + name.replace(".", "_"), None) if name else None
        self.kind = name if handler else kind or "(no type)"
        self.time = clock(record.get("time"))
        self.subject = text(record.get("subject"))
        if self.subject:
            self.agents.add(self.subject)
        data = obj(record.get("data"))
        if handler is None:
            self.add("", parts=[Part("data", text(data))])
        else:
            handler(data)

    def on_model_instructions(self, d: dict) -> None:
        tools = objs(d.get("tools"))
        self.facts["max tokens"] = text(d.get("max_tokens")) or "none sent"
        self.instructions = [Part("system prompt", text(d.get("preamble")))] + [
            Part(
                f"tool {text(t.get('name'))}",
                text(t.get("description")) + "\n\n" + json.dumps(t.get("parameters"), indent=2),
            )
            for t in tools
        ]
        self.add(f"{len(tools)} tool(s)", href="#instructions")

    def on_agent_started(self, d: dict) -> None:
        for name in ("model", "python", "container", "tool_call_max", "tool_result_max"):
            self.facts[name.replace("_", " ")] = text(d.get(name))
        self.add(f"{self.subject} on {text(d.get('model'))}")

    def on_agent_stopped(self, d: dict) -> None:
        self.add(self.subject)

    def on_session_state(self, d: dict) -> None:
        self.add(text(d.get("state")))

    def on_session_report(self, d: dict) -> None:
        closed, stopped = obj(d.get("closed_by")), obj(d.get("stopped"))
        self.facts["shutdown"] = text(d.get("verdict"))
        outcomes = [
            f"exec {text(e.get('execid'))}: {text(e.get('status'))}"
            for e in objs(d.get("executions"))
        ]
        lines = [
            f"closed by: {text(closed.get('by'))}",
            *([f"cause: {text(closed.get('cause'))}"] if "cause" in closed else []),
            f"stopped: {text(stopped.get('state'))}",
            *([f"why not: {text(stopped.get('reason'))}"] if "reason" in stopped else []),
            *(outcomes or ["no execution was running at the close"]),
        ]
        self.add(text(d.get("verdict")), parts=[Part("the shutdown report", "\n".join(lines))])

    def on_round_started(self, d: dict) -> None:
        self.begin(d.get("round"))
        self.add("")

    def within(self, number: object) -> Block:
        """The round an event that names round `number` belongs to: the open one, or, where the
        record lost a boundary, one opened for it and marked as such.

        A boundary is inferred only from a different number. The same number twice may be one
        round or two, since a round that commits no turn leaves its number to the next, and a
        round whose own number was never read says nothing either way."""
        label = text(number)
        if self.open is not None and label and self.open.round not in (label, "?"):
            self.close("unfinished: its end was not recorded")
        if self.open is None:
            self.begin(number).title += " (its start was not recorded)"
        return self.open

    def end_round(self, d: dict, outcome: str, parts: list[Part]) -> None:
        self.within(d.get("round"))
        summary = outcome
        if d.get("usage") is not None:
            summary += f" -- {usage_summary(d.get('usage'))}"
        self.add(summary, parts=parts)
        self.close(outcome, d)

    def on_model_round_completed(self, d: dict) -> None:
        stopped = text(d.get("stopped"))
        self.end_round(d, f"stopped by OutRig: {stopped}" if stopped else "completed", [])

    def on_model_round_failed(self, d: dict) -> None:
        self.end_round(d, "failed", [Part("error", text(d.get("error")), True)])

    def on_model_round_dropped(self, d: dict) -> None:
        self.end_round(d, "dropped by a Ctrl-C", [])

    def on_model_retry(self, d: dict) -> None:
        summary = (
            f"{text(d.get('model'))}: attempt {text(d.get('attempt'))} again after "
            f"{seconds(d.get('delay'))}"
        )
        self.add(summary, parts=[Part("error", text(d.get("error")))])

    def on_model_attempt(self, d: dict) -> None:
        summary = (
            f"request {text(d.get('attempt_id'))} of call {text(d.get('call_id'))}: "
            f"{text(d.get('model'))} ({text(d.get('identifier'))}), max tokens "
            f"{text(d.get('max_tokens')) or 'none sent'}"
        )
        if d.get("usage") is None:
            summary += " -- no usage reported"
        else:
            summary += f" -- {usage_summary(d.get('usage'))}"
        parts = [Part("error", text(d.get("error")))] if d.get("error") is not None else []
        self.add(summary, parts=parts)

    def on_model_usage_replaced(self, d: dict) -> None:
        self.add(
            f"request {text(d.get('attempt_id'))}'s usage, reported late: "
            f"{usage_summary(d.get('usage'))}"
        )

    def on_model_usage_refused(self, d: dict) -> None:
        self.add(
            f"a late usage for request {text(d.get('attempt_id'))} refused: "
            f"{text(d.get('reason'))}"
        )

    def on_model_failover(self, d: dict) -> None:
        summary = f"{text(d.get('from'))} -> {text(d.get('to'))}"
        self.add(summary, parts=[Part("error", text(d.get("error")))])

    def on_model_call(self, d: dict) -> None:
        block = self.within(d.get("round"))
        block.calls += 1
        budget = obj(d.get("budget"))
        estimate = num(d.get("estimate"))
        if estimate is not None and (block.estimate is None or estimate > block.estimate):
            block.estimate = estimate
        window = tokens(budget.get("window"))
        block.window = window + (" (assumed)" if budget.get("window_assumed") is True else "")
        summary = (
            f"call {text(d.get('call'))} to {text(budget.get('model'))}: "
            f"about {tokens(estimate)} tokens, window {block.window}, "
            f"{tokens(budget.get('reserve'))} reserved for the reply"
        )
        manifest = [
            f"carried: {turn_list(d.get('carried')) or 'nothing'}",
            f"evicted: {turn_list(d.get('evicted')) or 'nothing'}",
            *(
                [f"withheld so the roles alternate: {turn_list(w)}"]
                if (w := d.get("withheld"))
                else []
            ),
            f"overhead: {tokens(budget.get('overhead'))} tokens",
            f"role alternation: {text(budget.get('role_alternation'))}",
        ] + [
            f"left out of turn {text(part.get('turn'))}: part {text(part.get('part'))} of message "
            f"{text(part.get('message'))}, which this model's provider cannot take"
            for part in objs(d.get("left_out"))
        ] + [
            f"{text(a.get('role'))} follows itself at "
            + (f"turn {text(a['turn'])}" if a.get("turn") is not None else "the opening")
            for a in objs(d.get("adjacent"))
        ]
        parts = [Part("what it carried", "\n".join(manifest))]
        # Also the first thing the round's first turn holds, so folded away here; this is the
        # one place to read it when the round commits no turn.
        if d.get("opening") is not None:
            for opening in turn_parts([d.get("opening")], lambda _: None):
                opening.open, opening.label = False, "the round's opening"
                parts.append(opening)
        self.add(summary, parts=parts)

    def on_turn_committed(self, d: dict) -> None:
        summary = f"turn {text(d.get('turn'))} of round {text(d.get('round'))}"
        if d.get("incomplete") is True:
            summary += ", ended while its calls ran"
        parts = turn_parts(d.get("messages"), self.claim)
        self.add(summary, anchor=anchor("turn", d.get("turn")), parts=parts)

    def on_exec_submitted(self, d: dict) -> None:
        x = self.exec(d.get("execid"))
        x.source = text(d.get("source"))
        x.submitted = self.here().title
        self.here().sources.append((x.source, x))
        self.add(f"exec {x.id}: {first_line(x.source)}", href=link(x.anchor))

    def on_exec_refused(self, d: dict) -> None:
        x = self.exec(d.get("execid"))
        x.status = "refused"
        x.submitted = self.here().title
        if d.get("holder") is None:
            x.error = (
                f"Not run: the session was closed to new work ({text(d.get('reason'))}). "
                "Its source is in the turn that submitted it."
            )
            self.add(f"exec {x.id}: the session was closed to new work", href=link(x.anchor))
            return
        holder = self.exec(d.get("holder"))
        x.error = (
            f"Not run: exec {holder.id} held the interpreter. "
            "Its source is in the turn that submitted it."
        )
        self.add(f"exec {x.id}: exec {holder.id} held the interpreter", href=link(x.anchor))

    def on_exec_completed(self, d: dict) -> None:
        x = self.exec(d.get("execid"))
        x.status = text(d.get("status"))
        x.duration = seconds(d.get("duration"))
        x.output = text(d.get("output"))
        x.dropped = num(d.get("dropped"))
        x.error = text(d.get("error"))
        x.ended = self.here().title
        for b in objs(d.get("background")):
            earlier = self.exec(b.get("id"))
            label = f"output exec {earlier.id} wrote after it ended"
            if num(b.get("dropped")):
                label += f" ({tokens(b.get('dropped'))} bytes past the bound not recorded)"
            x.background.append(Part(label, text(b.get("output")), True))
            summary = f"wrote more output, recorded with exec {x.id}"
            earlier.notes.append(Item(self.time, "background", summary, href=link(x.anchor)))
        summary = f"exec {x.id}: {x.status} in {x.duration}"
        if x.submitted and x.submitted != x.ended:
            summary += f", submitted in {x.submitted}"
        elif not x.submitted:
            summary += ", with no submission recorded"
        self.add(summary, href=link(x.anchor))

    def on_memory_exhausted(self, d: dict) -> None:
        self.note(d, "raised MemoryError at the memory ceiling")

    def on_exec_cancel_sent(self, d: dict) -> None:
        self.note(d, "cancel sent")

    def on_exec_interrupt_sent(self, d: dict) -> None:
        why = "it kept a CPU busy" if d.get("runaway") is True else "a Ctrl-C"
        self.note(d, f"interrupt sent, for {why}")

    def on_exec_probe_failed(self, d: dict) -> None:
        self.note(d, f"liveness check failed: {text(d.get('verdict'))}")

    def on_exec_abandoned(self, d: dict) -> None:
        self.note(d, f"OutRig stopped waiting for it ({text(d.get('why'))})")

    def on_inventory_observed(self, d: dict) -> None:
        names = [f"{text(n.get('name'))}: {text(n.get('type'))}" for n in objs(d.get("names"))]
        if num(d.get("more")):
            names.append(f"... and {tokens(d.get('more'))} more")
        summary = f"its namespace held {tokens(d.get('total'))} name(s)"
        self.note(d, summary, [Part("names", "\n".join(names))])

    def on_tool_result_truncated(self, d: dict) -> None:
        self.note(
            d,
            f"result cut from {tokens(d.get('size'))} bytes to {tokens(d.get('kept'))} "
            f"(limit {tokens(d.get('max'))})",
        )

    def on_context_promoted(self, d: dict) -> None:
        self.add("turns " + ", ".join(text(t) for t in seq(d.get("turns"))))

    on_context_demoted = on_context_promoted

    def on_output_unattributed(self, d: dict) -> None:
        # A run of them is one row: a child process outside any execution can write thousands.
        items = self.here().items
        if items and items[-1].kind == self.kind:
            row = items[-1]
            row.parts.append(Part("text", text(d.get("text")), True))
            row.summary = f"{len(row.parts)} lines no execution can be billed for"
            return
        self.add(
            "a line no execution can be billed for", parts=[Part("text", text(d.get("text")), True)]
        )

    def on_interpreter_diagnostic(self, d: dict) -> None:
        self.add("", parts=[Part("text", text(d.get("text")), True)])

    def on_interpreter_exited(self, d: dict) -> None:
        # One the session's shutdown stopped is expected; any other is the interpreter dying.
        if d.get("expected") is not True:
            self.health.append(
                "The interpreter exited while the session ran; the timeline has its cause."
            )
        self.add("", parts=[Part("cause", text(d.get("cause")), True)])

    def on_message_sent(self, d: dict) -> None:
        m = self.message(d.get("message"), d)
        m.body = text(d.get("body"))
        m.sent = f"{self.time}, {self.here().title}"
        summary = f"message {m.id}: {m.sender} -> {m.to} on {m.channel}: {first_line(m.body)}"
        self.add(summary, href=link(m.anchor))

    def settle(self, d: dict, outcome: str) -> None:
        m = self.message(d.get("message"), d)
        m.outcome = outcome
        m.outcome_at = f"{self.time}, {self.here().title}"
        self.add(f"message {m.id}: {outcome}", href=link(m.anchor))

    def on_message_received(self, d: dict) -> None:
        self.settle(d, f"taken by {text(d.get('to'))}")

    def on_message_refused(self, d: dict) -> None:
        self.settle(d, f"refused: {text(d.get('reason'))}")

    # -- the end ------------------------------------------------------------

    def finish(self, records: list[dict]) -> None:
        """Close what the record left open, and say what about it as a whole is wrong."""
        if self.open is not None:
            self.close("unfinished: the record ends inside it")
        for item in (i for b in self.blocks for i in b.items if i.kind == "output.unattributed"):
            item.parts = [Part("text", "\n".join(p.body for p in item.parts), True)]
        if not records:
            self.health.append("The event log is empty: the agent never started.")
            return
        ids = [decimal(r.get("id")) for r in records]
        if None in ids:
            self.health.append("Some records' ids are not numbers, so gaps cannot be checked.")
        elif gaps := id_gaps(ids):
            shown = ", ".join(str(a) if a == b else f"{a}-{b}" for a, b in gaps[:10])
            self.health.append(
                f"{sum(b - a + 1 for a, b in gaps)} id(s) missing "
                f"({shown}{', ...' if len(gaps) > 10 else ''}): those events were published and "
                "never written -- the log fell behind or could not write them, and the shutdown "
                "report counted them."
            )
        types = [text(r.get("type")) for r in records]
        last = obj(records[-1].get("data"))
        if types[-1] != PREFIX + "session.state" or last.get("state") != "reported":
            self.health.append(
                "The record does not end with the shutdown report: the session is still "
                "running, or it ended without being shut down."
            )
        if (started := types.count(PREFIX + "agent.started")) > 1:
            self.health.append(f"The record holds {started} agent starts: logs were joined.")
        if len(sources := {text(r.get("source")) for r in records}) > 1:
            self.health.append(f"The records name {len(sources)} sessions as their source.")
        if versions := {text(r.get("specversion")) for r in records} - {"1.0"}:
            self.health.append(f"Records have specversion {', '.join(sorted(versions))}.")
        self.facts["recorded as"] = ", ".join(sorted(sources))


def id_gaps(ids: list[int]) -> list[tuple[int, int]]:
    """The runs of ids from 1 up that `ids` does not hold, first and last of each."""
    gaps, expected = [], 1
    for n in sorted(set(ids)):
        if n > expected:
            gaps.append((expected, n - 1))
        expected = max(expected, n + 1)
    return gaps


def turn_parts(messages: object, claim) -> list[Part]:
    """A turn's messages, in rig's JSON: tagged by `role`; user items tagged by `type`;
    assistant items untagged, so told apart by their fields."""
    parts: list[Part] = []
    for message in objs(messages):
        role = text(message.get("role"))
        content = message.get("content")
        if role == "system" or not isinstance(content, list):
            parts.append(Part(role or "message", text(content), True))
            continue
        for item in content:
            item = obj(item)
            if role == "user":
                parts.append(user_part(item))
            elif role == "assistant":
                parts.append(assistant_part(item, claim))
            else:
                parts.append(Part(role or "message", text(item)))
    return parts


def user_part(item: dict) -> Part:
    kind = text(item.get("type"))
    if kind == "text":
        return Part("sent to the model", text(item.get("text")), True)
    if kind == "toolresult":
        body = "\n".join(
            text(obj(c).get("text"))
            if obj(c).get("type") == "text"
            else f"[{text(obj(c).get('type'))}]"
            for c in seq(item.get("content"))
        )
        return Part("the tool result the model read", body)
    return Part(kind or "user content", text(item))


def assistant_part(item: dict, claim) -> Part:
    if "function" in item:
        function = obj(item.get("function"))
        arguments = function.get("arguments")
        if isinstance(arguments, str):
            with contextlib.suppress(ValueError, RecursionError):
                arguments = json.loads(arguments)
        source = obj(arguments).get("source")
        if isinstance(source, str):
            x = claim(source)
            if x is not None:
                return Part(f"submitted as exec {x.id}", href=link(x.anchor))
            name = text(function.get("name"))
            return Part(f"{name}, with no submission of this source recorded", source, True)
        return Part(f"call to {text(function.get('name'))}", text(arguments), True)
    if isinstance(item.get("text"), str):
        return Part("the model", item["text"], True)
    if isinstance(item.get("content"), list):
        body = []
        for block in objs(item["content"]):
            kind, inner = text(block.get("type")), block.get("content")
            if kind == "text":
                body.append(text(obj(inner).get("text")))
            elif kind == "summary":
                body.append(text(inner))
            else:
                body.append(f"[{kind} reasoning, not readable]")
        return Part("reasoning", "\n\n".join(body))
    return Part("assistant content", text(item))


def network_rows(records: list[dict]) -> list[list[str]]:
    """Each connection's cells, in `NETWORK_COLUMNS` order."""
    rows = []
    for r in records:
        ts = num(r.get("ts"))
        try:
            when = datetime.fromtimestamp(ts, UTC).isoformat(timespec="milliseconds") if ts else ""
        except (OverflowError, OSError, ValueError):
            when = text(r.get("ts"))
        host = text(r.get("outrig.host"))
        if host and r.get("outrig.host_source") is not None:
            host += f" ({text(r.get('outrig.host_source'))})"
        rows.append(
            [
                clock(when),
                host,
                f"{text(r.get('id.resp_h'))}:{text(r.get('id.resp_p'))}",
                f"{text(r.get('proto'))}/{text(r.get('service'))}",
                text(r.get("server_name")),
                text(r.get("outrig.action")),
                text(r.get("outrig.rule")),
                f"{tokens(r.get('orig_bytes'))} / {tokens(r.get('resp_bytes'))}",
                seconds(r.get("duration")),
            ]
        )
    return rows


# ---------------------------------------------------------------------------
# The page.


def build(directory: Path) -> dict:
    records, skipped = read_jsonl(directory / EVENTS)
    builder = Builder()
    for record in records:
        builder.feed(record)
    builder.finish(records)

    session: dict = {}
    try:
        read = json.loads((directory / SESSION).read_text(encoding="utf-8"))
        if not isinstance(read, dict):
            raise ValueError("it is not a JSON object")
        session = read
    except FileNotFoundError:
        builder.health.append("There is no session.json.")
    # A `RecursionError` is how Python 3.12 and 3.13 refuse nesting too deep to parse.
    except (OSError, ValueError, RecursionError) as e:
        builder.health.append(f"session.json could not be read: {e}")
    container = text(session.get("container_name"))
    if container and builder.facts.get("container") and container != builder.facts["container"]:
        builder.health.append(
            f"session.json names the container {container}, and the agent ran in "
            f"{builder.facts['container']}."
        )

    network, network_skipped = None, []
    if (directory / NETWORK).is_file():
        records_, network_skipped = read_jsonl(directory / NETWORK)
        network = network_rows(records_)
    facts = {
        "session": text(session.get("id")),
        "started": text(session.get("started_at")),
        "ended": text(session.get("ended_at")),
        "exit code": text(session.get("exit_code")),
        "image": text(session.get("image_config_name")),
        "image tag": text(session.get("image_tag")),
        "working dir": text(session.get("working_dir")),
    } | builder.facts

    rounds = builder.rounds
    session_row = Block(
        "",
        "session",
        calls=sum(r.calls for r in rounds),
        reported=sum(r.reported for r in rounds),
        usage={name: sum(r.usage[name] for r in rounds) for name, _ in USAGE},
        peak=max((r.peak for r in rounds if r.peak is not None), default=None),
        estimate=max((r.estimate for r in rounds if r.estimate is not None), default=None),
    )
    agents: dict[str, list[Exec]] = {}
    for x in builder.execs.values():
        agents.setdefault(x.subject, []).append(x)
    return {
        "title": text(session.get("id")) or directory.name,
        "facts": {k: v for k, v in facts.items() if v},
        "instructions": builder.instructions,
        "health": builder.health,
        "skipped": skipped,
        "rounds": rounds,
        "session_row": session_row,
        "blocks": builder.blocks,
        "agents": agents,
        "messages": list(builder.messages.values()),
        "network": network,
        "network_skipped": network_skipped,
        "events": len(records),
        # An event's agent is said on its row only when there is more than one to tell apart.
        "many_agents": len(builder.agents) > 1,
    }


def render(page: dict) -> str:
    # Autoescaping is off unless asked for. Every value below is hostile-shaped text.
    env = Environment(
        autoescape=True, undefined=StrictUndefined, trim_blocks=True, lstrip_blocks=True
    )
    env.filters["tokens"] = tokens
    env.globals.update(
        USAGE=USAGE,
        TOKEN_COLUMNS=TOKEN_COLUMNS,
        MESSAGE_COLUMNS=MESSAGE_COLUMNS,
        NETWORK_COLUMNS=NETWORK_COLUMNS,
    )
    return env.from_string(TEMPLATE).render(**page)


def write_private(path: Path, page: str) -> None:
    """Write `page` to `path`, owner-only, whatever was there: a fresh file replaces it, so an
    existing file's wider mode does not survive and a symlink there is replaced, not followed."""
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", errors="replace") as f:
            f.write(page)
        os.replace(tmp, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp)
        raise


def fail(message: str) -> int:
    print(f"render-session: {message}", file=sys.stderr)
    return 1


def session_root() -> Path:
    """Where sessions live, decided as `outrig` decides it: the `session-root` key of the repo
    config -- the nearest `.agents/outrig/config.toml` at or above the working directory -- else
    of the global config, else the platform's data directory. A config file that is there but
    cannot be read, or whose `session-root` is not a string, raises `ValueError` naming it."""
    cwd = Path.cwd()
    repo = next(filter(Path.is_file, (d / REPO_CONFIG for d in (cwd, *cwd.parents))), None)
    xdg = os.environ.get("XDG_CONFIG_HOME")
    user = (Path(xdg) / "outrig" if xdg else Path.home() / ".outrig") / "config.toml"
    for config in filter(Path.exists, [repo, user] if repo else [user]):
        try:
            root = tomllib.loads(config.read_text(encoding="utf-8")).get("session-root")
        except (OSError, ValueError) as e:
            raise ValueError(f"reading {config}: {e}") from e
        if isinstance(root, str):
            return Path(root)
        if root is not None:
            raise ValueError(f"reading {config}: session-root is not a string")
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "outrig" / "sessions"
    if sys.platform == "win32":
        base = Path(os.environ.get("APPDATA") or Path.home() / "AppData" / "Roaming")
        return base / "outrig" / "data" / "sessions"
    data = os.environ.get("XDG_DATA_HOME")
    return (Path(data) if data else Path.home() / ".local" / "share") / "outrig" / "sessions"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument(
        "session",
        help="a session directory, or a session id -- or enough of one to name a single "
        "session -- looked up under the session root",
    )
    parser.add_argument(
        "--session-root",
        type=Path,
        help="where sessions live (default: as outrig decides it, from the session-root key of "
        "the repo or global config, else <XDG_DATA_HOME>/outrig/sessions)",
    )
    parser.add_argument(
        "--out", type=Path, help=f"where to write the page (default: <session>/{REPORT})"
    )
    args = parser.parse_args(argv)

    session: str = args.session
    directory = Path(session)
    if not directory.is_dir():
        if directory.name == EVENTS.name and directory.is_file():
            return fail(
                f"pass the session directory, {directory.parent.parent}, not the log itself"
            )
        if any(sep in session for sep in (os.sep, os.altsep) if sep):
            return fail(f"{session} is not a directory")
        try:
            root = args.session_root or session_root()
        except ValueError as e:
            return fail(str(e))
        if not root.is_dir():
            return fail(f"no session root at {root}")
        directory = root / session
        if not directory.is_dir():
            names = sorted(p.name for p in root.iterdir() if p.is_dir() and session in p.name)
            if not names:
                return fail(f'no session matching "{session}" under {root}')
            if len(names) > 1:
                listed = "".join(f"\n  {name}" for name in names)
                return fail(f'ambiguous session "{session}"; candidates:{listed}')
            directory = root / names[0]
    if not (directory / EVENTS).is_file():
        if (directory / EVENTS.name).is_file():
            return fail(
                f"{directory} looks like a logs directory; pass the session directory above it"
            )
        return fail(
            f"no {EVENTS} in {directory}. Only `outrig run-new` records one, and only when its "
            'config has [events] mode = "record"'
        )
    out: Path = args.out or directory / REPORT
    inputs = [directory / EVENTS, directory / NETWORK, directory / SESSION]
    if any(out.resolve() == p.resolve() for p in inputs):
        return fail(f"refusing to write the page over {out}, which it reads")
    if out.is_dir():
        return fail(f"{out} is a directory")

    try:
        html = render(build(directory))
    except OSError as e:
        return fail(f"reading {directory}: {e}")
    try:
        write_private(out, html)
    except OSError as e:
        return fail(f"writing {out}: {e}; choose another place with --out")
    print(out)
    return 0


TEMPLATE = """\
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OutRig session {{ title }}</title>
<style>
:root { color-scheme: light dark;
        --muted: #6b7280; --rule: #d1d5db; --bad: #b91c1c; --code: #f3f4f6; }
@media (prefers-color-scheme: dark) {
  :root { --muted: #9ca3af; --rule: #374151; --bad: #f87171; --code: #1f2937; }
}
body { font: 15px/1.45 system-ui, sans-serif; max-width: 72rem; margin: 2rem auto;
       padding: 0 1rem; }
h1 { font-size: 1.4rem; }
h2 { font-size: 1.15rem; margin-top: 2.5rem; border-bottom: 1px solid var(--rule); }
h3 { font-size: 1rem; margin: 1.5rem 0 .5rem; }
table { border-collapse: collapse; margin: .5rem 0; }
th, td { padding: .2rem .6rem; text-align: left; vertical-align: top; }
tr + tr td, thead + tbody td { border-top: 1px solid var(--rule); }
.n { text-align: right; font-variant-numeric: tabular-nums; }
pre { background: var(--code); padding: .5rem .7rem; margin: .25rem 0 .5rem; font-size: 13px;
      white-space: pre-wrap; overflow-wrap: anywhere; }
details { margin: .2rem 0; }
summary { cursor: pointer; color: var(--muted); }
.time, .kind, .label { color: var(--muted); font-size: 13px; }
.time { font-variant-numeric: tabular-nums; }
.kind { font-family: ui-monospace, monospace; }
.bad { color: var(--bad); }
.row { margin: .35rem 0; }
.row > .parts { margin-left: 2rem; }
.exec { border-left: 3px solid var(--rule); padding-left: .8rem; margin: 1rem 0; }
nav a { margin-right: 1rem; }
</style>
</head>
<body>
{% macro part(p) %}
{% if p.href %}
<div class="label">{{ p.label }} -- <a href="{{ p.href }}">see it</a></div>
{% elif p.open %}
<div class="label">{{ p.label }}</div><pre>{{ p.body }}</pre>
{% else %}
<details><summary>{{ p.label }}</summary><pre>{{ p.body }}</pre></details>
{% endif %}
{% endmacro %}
{% macro row(i) %}
<div class="row"{% if i.anchor %} id="{{ i.anchor }}"{% endif %}>
<span class="time">{{ i.time }}</span>
{% if many_agents and i.subject %}<span class="label">{{ i.subject }}</span>{% endif %}
<span class="kind">{{ i.kind }}</span>
{% if i.href %}<a href="{{ i.href }}">{{ i.summary }}</a>{% else %}{{ i.summary }}{% endif %}
{% if i.parts %}<div class="parts">{% for p in i.parts %}{{ part(p) }}{% endfor %}</div>{% endif %}
</div>
{% endmacro %}
{% macro head(columns) %}
<thead><tr>{% for label, numeric in columns %}<th{% if numeric %} class="n"{% endif %}>
{{- label }}</th>{% endfor %}</tr></thead>
{% endmacro %}
{% macro usage(r) %}
<td class="n">{{ r.calls }}
{%- if r.reported != r.calls %} ({{ r.reported }} reported){% endif %}</td>
{% for name, _ in USAGE %}<td class="n">{{ r.usage[name]|tokens }}</td>
{% endfor %}
<td class="n">{{ r.peak|tokens }}</td><td class="n">{{ r.estimate|tokens }}</td>
<td class="n">{{ r.window }}</td>
{% endmacro %}
<h1>OutRig session {{ title }}</h1>
<nav>
<a href="#health">Record</a><a href="#tokens">Tokens</a><a href="#timeline">Timeline</a>
<a href="#executions">Executions</a><a href="#messages">Messages</a>
{% if network is not none %}<a href="#network">Network</a>{% endif %}
</nav>
<table>
{% for name, value in facts.items() %}<tr><th>{{ name }}</th><td>{{ value }}</td></tr>
{% endfor %}
</table>
{% if instructions %}
<h3 id="instructions">What every model call was sent besides the conversation</h3>
{% for p in instructions %}{{ part(p) }}{% endfor %}
{% endif %}

<h2 id="health">The record</h2>
<p>{{ events }} event(s) read.</p>
{% if health or skipped or network_skipped %}
<ul>
{% for note in health %}<li>{{ note }}</li>
{% endfor %}
{% for s in skipped %}<li class="bad">events.jsonl line {{ s.line }} skipped: {{ s.why }}</li>
{% endfor %}
{% for s in network_skipped %}
<li class="bad">network.jsonl line {{ s.line }} skipped: {{ s.why }}</li>
{% endfor %}
</ul>
{% else %}
<p>No gaps in its ids, no skipped lines, and the session shut down.</p>
{% endif %}
<p>That is what this page can check. Every event the session published has an id, so one the log
did not get -- because it fell behind, or could not write it -- shows as a gap, unless it came
after the last one the log wrote, which leaves none. <code>outrig run-new</code> counts every such
loss in a warning as the session ends.</p>

<h2 id="tokens">Tokens</h2>
<p>What each round used, as its provider reported it. A round's total adds up every call it made,
so it says what the round cost, not how much context any one request carried. That is the
<em>largest request</em> column: the most input any single call reported, beside OutRig's own
estimate of its largest request and the context window that request was held to. Providers differ
on whether reported input counts cached input: Anthropic's does not, OpenAI's does.</p>
<table>
{{ head(TOKEN_COLUMNS) }}
<tbody>
{% for r in rounds %}
<tr><td><a href="#{{ r.anchor }}">{{ r.title }}</a></td><td>{{ r.outcome }}</td>
{{ usage(r) }}</tr>
{% endfor %}
<tr><th>{{ session_row.title }}</th><td></td>
{{ usage(session_row) }}</tr>
</tbody>
</table>

<h2 id="timeline">Timeline</h2>
{% for b in blocks %}
<h3 id="{{ b.anchor }}">{{ b.title }}{% if b.outcome %} -- {{ b.outcome }}{% endif %}</h3>
{% for i in b.items %}{{ row(i) }}{% endfor %}
{% endfor %}

<h2 id="executions">Executions</h2>
{% for subject, execs in agents.items() %}
<h3>{{ subject }}</h3>
{% for x in execs %}
<div class="exec"{% if x.anchor %} id="{{ x.anchor }}"{% endif %}>
<div><strong>exec {{ x.id }}</strong>
<span class="{{ 'bad' if x.status not in ('ok', '') else '' }}">
{{- x.status or "no outcome recorded" }}</span>
{% if x.duration %}in {{ x.duration }} {% endif %}
<span class="label">
{%- if x.submitted %}submitted in {{ x.submitted }}{% endif %}
{%- if x.ended and x.ended != x.submitted %}, ended in {{ x.ended }}{% endif %}</span></div>
{% if x.source %}<pre>{{ x.source }}</pre>{% endif %}
{% if x.output %}<div class="label">output</div><pre>{{ x.output }}</pre>{% endif %}
{% if x.dropped %}
<div class="label">
{{- x.dropped|tokens }} more bytes of output past the bound were not recorded</div>
{% endif %}
{% if x.error %}<div class="label bad">error</div><pre>{{ x.error }}</pre>{% endif %}
{% for p in x.background %}{{ part(p) }}{% endfor %}
{% for i in x.notes %}{{ row(i) }}{% endfor %}
</div>
{% endfor %}
{% else %}
<p>No executions were recorded.</p>
{% endfor %}

<h2 id="messages">Messages</h2>
{% if messages %}
<table>
{{ head(MESSAGE_COLUMNS) }}
<tbody>
{% for m in messages %}
<tr{% if m.anchor %} id="{{ m.anchor }}"{% endif %}>
<td>{{ m.id }}</td><td>{{ m.channel }}</td><td>{{ m.sender }}</td><td>{{ m.to }}</td>
<td class="time">{{ "not recorded" if m.body is none else m.sent }}</td>
<td>{{ m.outcome }}
{%- if m.outcome_at %} <span class="time">{{ m.outcome_at }}</span>{% endif %}</td>
<td>{% if m.body is none %}<span class="label">its sending was not recorded</span>
{%- else %}<pre>{{ m.body }}</pre>{% endif %}</td></tr>
{% endfor %}
</tbody>
</table>
{% else %}
<p>No messages.</p>
{% endif %}

{% if network is not none %}
<h2 id="network">Network</h2>
<table>
{{ head(NETWORK_COLUMNS) }}
<tbody>
{% for row in network %}
<tr>{% for cell in row %}<td{% if NETWORK_COLUMNS[loop.index0][1] %} class="n"{% endif %}>
{{- cell }}</td>{% endfor %}</tr>
{% endfor %}
</tbody>
</table>
{% endif %}
</body>
</html>
"""

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
