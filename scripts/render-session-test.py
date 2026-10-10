#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Tests for `render-session.py`, through the command a person runs.

    uv run --script scripts/render-session-test.py

Each case writes a made-up session directory, runs `uv run --script scripts/render-session.py`
over it, and reads the page back. Going through `uv` rather than importing the script is the
point: the PEP 723 block, the dependency it provisions, and the Jinja environment as the script
configures it are what the escaping assertions are about.

The page is the one output of the event log that a browser executes. So escaping is asserted per
field -- submitted source, a traceback, captured output, and a message body each reach the page by
a different path -- and then swept: every field of every event type carries a payload that must
not survive as markup, and every field the page shows must show up escaped, so a field silently
dropped from the page cannot pass by being absent. The sweep's fixture is checked against the
event names the catalog in `crates/outrig/src/harness/event.rs` defines and
`doc/reference/events.md` documents, so an event added to either fails here until the renderer
has been looked at.

Standard library only. `uv` is the one thing it needs, and without it the run fails rather than
skipping.
"""

from __future__ import annotations

import functools
import json
import os
import re
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
RENDERER = SCRIPTS / "render-session.py"
CATALOG_RS = SCRIPTS.parent / "crates" / "outrig" / "src" / "harness" / "event.rs"
EVENTS_MD = SCRIPTS.parent / "doc" / "reference" / "events.md"
UV = os.environ.get("UV") or shutil.which("uv")
EVENTS = Path("logs") / "events.jsonl"
NETWORK = Path("logs") / "network.jsonl"
REPORT = "report.html"


@functools.cache
def uv_keeps() -> dict[str, str]:
    """Where `uv` keeps its cache and its Pythons, so that a run given a home of its own does not
    look for them there and provision both again."""

    def ask(*words: str) -> str:
        done = subprocess.run([UV, *words], capture_output=True, text=True, check=True)
        return done.stdout.strip()

    return {"UV_CACHE_DIR": ask("cache", "dir"), "UV_PYTHON_INSTALL_DIR": ask("python", "dir")}


def payload(marker: str) -> str:
    return f"\"><script>{marker}</script><img src=x onerror=1>'"


def escaped(marker: str) -> str:
    """How `payload(marker)` reads once escaped -- and so how it reads on a page that shows it."""
    return f"&lt;script&gt;{marker}&lt;/script&gt;"


def envelope(n: int, kind: str, data: dict, subject: str | None = "agent/primary") -> dict:
    record = {
        "specversion": "1.0",
        "id": str(n),
        "source": "/outrig/session/20261001T120000-abcd",
        "type": f"org.outrig.{kind}",
        "subject": subject,
        "time": f"2026-10-01T12:{n // 60:02d}:{n % 60:02d}.000Z",
        "datacontenttype": "application/json",
        "data": data,
    }
    if subject is None:
        del record["subject"]
    return record


def ordinary(
    source: str = "print('hi')",
    output: str = "hi\n",
    error: str | None = None,
    body: str = "hello",
) -> list[tuple[str, dict]]:
    """A whole session: one round in which the model submits code that reads a message."""
    call = {"input_tokens": 100, "output_tokens": 10, "total_tokens": 110}
    return [
        (
            "agent.started",
            {
                "model": "sonnet",
                "python": "3.13.15",
                "container": "outrig-x",
                "tool_call_max": 50,
                "tool_result_max": 262144,
            },
        ),
        (
            "model.instructions",
            {
                "model": "sonnet",
                "preamble": "You act by writing Python.",
                "tools": [],
                "max_tokens": 4096,
            },
        ),
        (
            "message.sent",
            {"message": 2, "channel": "user", "from": "user", "to": "agent/primary", "body": body},
        ),
        ("round.started", {"round": 1}),
        ("model.call", model_call(0, 1)),
        ("exec.submitted", {"execid": 4, "source": source}),
        (
            "message.received",
            {"message": 2, "channel": "user", "from": "user", "to": "agent/primary"},
        ),
        (
            "exec.completed",
            {
                "execid": 4,
                "status": "error" if error else "ok",
                "duration": 0.25,
                "output": output,
                "dropped": 0,
                "error": error,
                "background": [],
            },
        ),
        (
            "turn.committed",
            {
                "turn": 0,
                "round": 1,
                "incomplete": False,
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "[outrig] 1 message"}]},
                    {
                        "role": "assistant",
                        "id": None,
                        "content": [
                            {"text": "Running it."},
                            {
                                "id": "toolu_1",
                                "call_id": None,
                                "function": {
                                    "name": "submit_python",
                                    "arguments": {"source": source},
                                },
                            },
                        ],
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "toolresult",
                                "id": "toolu_1",
                                "content": [{"type": "text", "text": "the result"}],
                            }
                        ],
                    },
                ],
            },
        ),
        ("model.call", model_call(1, 1)),
        (
            "turn.committed",
            {
                "turn": 1,
                "round": 1,
                "incomplete": False,
                "messages": [{"role": "assistant", "id": None, "content": [{"text": "Done."}]}],
            },
        ),
        (
            "model.round.completed",
            {
                "round": 1,
                "stopped": None,
                "usage": call,
                "calls": [{"index": 0, "usage": call}],
                "input_tokens_max": 100,
            },
        ),
        ("agent.stopped", {}),
        (
            "session.report",
            {
                "closed_by": {"by": "owner"},
                "stopped": {"state": "proven"},
                "executions": [],
                "verdict": "clean",
            },
        ),
        ("session.state", {"state": "reported"}),
    ]


def model_call(call: int, round_: int, estimate: int = 1700, **over: object) -> dict:
    return {
        "call": call,
        "round": round_,
        "budget": {
            "model": "sonnet",
            "window": 128000,
            "window_assumed": True,
            "reserve": 4096,
            "overhead": 1662,
            "max_tokens": 4096,
            "role_alternation": "relaxed",
        },
        "estimate": estimate,
        "carried": [],
        "evicted": [],
        "withheld": [],
        "opening": None,
        "adjacent": [],
        "left_out": [],
    } | over


def connection(**over: object) -> dict:
    """A `network.jsonl` record, with every field `network.rs` writes."""
    return {
        "ts": 1790000000.5,
        "uid": "C1",
        "id.orig_h": "10.0.0.2",
        "id.orig_p": 40000,
        "id.resp_h": "93.184.215.14",
        "id.resp_p": 443,
        "proto": "tcp",
        "service": "ssl",
        "duration": 0.1,
        "orig_bytes": 1,
        "resp_bytes": 2,
        "conn_state": "SF",
        "local_orig": True,
        "local_resp": False,
        "missed_bytes": 0,
        "server_name": "example.com",
        "outrig.session_id": "20261001T120000-abcd",
        "outrig.container": "outrig-x",
        "outrig.host": "example.com",
        "outrig.host_source": "asserted",
        "outrig.action": "allow",
        "outrig.rule": "default",
    } | over


def every_event() -> list[tuple[str, dict, str | None]]:
    """Every event type, each field holding something of the type the schema gives it."""
    usage = {
        "input_tokens": 1,
        "output_tokens": 1,
        "total_tokens": 2,
        "cached_input_tokens": 0,
        "cache_creation_input_tokens": 0,
        "reasoning_tokens": 0,
    }
    calls = [{"index": 0, "model": "m", "call_id": 1, "attempt_id": 2, "usage": usage}]
    a = "agent/primary"
    message = {"role": "user", "content": [{"type": "text", "text": "opening"}]}
    return [
        ("session.state", {"state": "starting"}, None),
        (
            "agent.started",
            {
                "model": "m",
                "python": "3.13",
                "container": "c",
                "tool_call_max": 1,
                "tool_result_max": 1,
            },
            a,
        ),
        (
            "model.instructions",
            {
                "model": "m",
                "preamble": "p",
                "max_tokens": 1,
                "tools": [
                    {
                        "name": "submit_python",
                        "description": "d",
                        "parameters": {"type": "object", "description": "schema text"},
                    }
                ],
            },
            a,
        ),
        (
            "message.sent",
            {"message": 1, "channel": "user", "from": "user", "to": a, "body": "b"},
            a,
        ),
        ("round.started", {"round": 1}, a),
        (
            "model.call",
            model_call(
                0,
                1,
                carried=[{"turn": 0, "why": "latest"}],
                evicted=[{"turn": 1, "why": "recent"}],
                opening=message,
                adjacent=[{"turn": None, "role": "user"}],
            ),
            a,
        ),
        ("exec.submitted", {"execid": 2, "source": "s"}, a),
        ("exec.refused", {"execid": 3, "holder": 2, "reason": "held"}, a),
        ("exec.refused", {"execid": 5, "holder": None, "reason": "closed"}, a),
        ("message.received", {"message": 1, "channel": "user", "from": "user", "to": a}, a),
        (
            "message.sent",
            {"message": 4, "channel": "user", "from": a, "to": "user", "body": "b"},
            a,
        ),
        (
            "message.refused",
            {"message": 4, "channel": "user", "from": a, "to": "user", "reason": "r"},
            a,
        ),
        ("exec.cancel.sent", {"execid": 2}, a),
        ("exec.probe.failed", {"execid": 2, "verdict": "blocked"}, a),
        ("exec.interrupt.sent", {"execid": 2, "runaway": True}, a),
        (
            "inventory.observed",
            {"execid": 2, "names": [{"name": "n", "type": "t"}], "total": 1, "more": 0},
            a,
        ),
        ("exec.abandoned", {"execid": 2, "why": "user"}, a),
        (
            "exec.completed",
            {
                "execid": 2,
                "status": "error",
                "duration": 1.0,
                "output": "o",
                "dropped": 1,
                "error": "e",
                "background": [{"id": 2, "output": "o", "dropped": 1}],
            },
            a,
        ),
        ("memory.exhausted", {"execid": 2}, a),
        ("tool.result.truncated", {"execid": 2, "size": 2, "max": 1, "kept": 1}, a),
        ("context.promoted", {"turns": [0]}, a),
        ("context.demoted", {"turns": [0]}, a),
        ("output.unattributed", {"text": "u"}, None),
        ("output.unattributed", {"text": "u"}, None),
        ("interpreter.diagnostic", {"text": "d"}, None),
        (
            "turn.committed",
            {
                "turn": 0,
                "round": 1,
                "incomplete": True,
                "messages": [
                    message,
                    {
                        "role": "assistant",
                        "id": "i",
                        "content": [
                            {"text": "t"},
                            {
                                "id": "c",
                                "call_id": None,
                                "function": {"name": "submit_python", "arguments": {"source": "s"}},
                            },
                            {
                                "id": "c2",
                                "call_id": None,
                                "function": {
                                    "name": "submit_python",
                                    "arguments": {"source": "refused"},
                                },
                            },
                            {
                                "id": "c3",
                                "call_id": None,
                                "function": {"name": "other", "arguments": {"x": "a"}},
                            },
                            {
                                "id": "r",
                                "content": [
                                    {"type": "text", "content": {"text": "t"}},
                                    {"type": "summary", "content": "s"},
                                ],
                            },
                        ],
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "toolresult",
                                "id": "c",
                                "content": [{"type": "text", "text": "t"}],
                            }
                        ],
                    },
                    {"role": "system", "content": "s"},
                ],
            },
            a,
        ),
        (
            "model.attempt",
            {
                "call_id": 1,
                "attempt_id": 1,
                "model": "m",
                "identifier": "i",
                "max_tokens": 1,
                "error": "e",
                "usage": None,
            },
            a,
        ),
        (
            "model.retry",
            {
                "model": "m",
                "attempt": 1,
                "delay": 1.0,
                "error": "e",
                "call_id": 1,
                "attempt_id": 1,
            },
            a,
        ),
        (
            "model.failover",
            {"from": "m", "to": "n", "error": "e", "call_id": 1, "attempt_id": None},
            a,
        ),
        (
            "model.attempt",
            {
                "call_id": 1,
                "attempt_id": 2,
                "model": "n",
                "identifier": "i",
                "max_tokens": None,
                "error": None,
                "usage": usage,
            },
            a,
        ),
        (
            "model.round.completed",
            {
                "round": 1,
                "stopped": "cap",
                "usage": usage,
                "calls": calls,
                "attempts": [1, 2],
                "input_tokens_max": 1,
            },
            a,
        ),
        ("model.usage.replaced", {"call_id": 1, "attempt_id": 1, "usage": usage}, a),
        (
            "model.usage.refused",
            {"call_id": None, "attempt_id": 9, "usage": usage, "reason": "unknown"},
            a,
        ),
        ("round.started", {"round": 2}, a),
        (
            "model.round.failed",
            {"round": 2, "error": "e", "calls": calls, "usage": None, "attempts": [3]},
            a,
        ),
        ("round.started", {"round": 2}, a),
        (
            "model.round.dropped",
            {"round": 2, "calls": calls, "usage": usage, "attempts": [4]},
            a,
        ),
        ("session.state", {"state": "closing"}, None),
        ("interpreter.exited", {"cause": "c", "expected": False}, None),
        ("agent.stopped", {}, a),
        (
            "session.report",
            {
                "closed_by": {"by": "interpreter_exited", "cause": "c"},
                "stopped": {"state": "not_proven", "reason": "r"},
                "executions": [{"execid": 2, "status": "unknown"}],
                "verdict": "not_proven_stopped",
            },
            None,
        ),
        ("session.state", {"state": "reported"}, None),
    ]


# The tags rig's JSON tells a turn's items apart by. The string sweep leaves them as they are, so
# each item is read the way it would be; the sweep of every field plants them too.
TAGS = r"(turn\.committed\.data\.messages\.\d+|model\.call\.data\.opening)(\..+)?\.(role|type)"

# Where a field is not on the page, and why. Everything else the sweep plants must be found.
NOT_SHOWN = [
    # A refusal names the execution that held the interpreter; its reason says only that one did.
    r"exec\.refused\.data\.reason",
    # The model that answered each call is named by the attempt that answered it.
    r"model\.round\.(completed|failed|dropped)\.data\.calls\.\d+\.model",
    # A message's channel and ends are shown from its sending, which a taking or a refusal repeats.
    r"message\.(received|refused)\.data\.(channel|from|to)",
    # The model is shown from agent.started, which names the same one.
    r"model\.instructions\.data\.model",
    # The provider's ids for a message, a tool call, reasoning, and the call a result answers.
    r"turn\.committed\.data\.messages\.\d+(\.content\.\d+)?\.id",
    # A connection's own id, the container end, its Zeek state, and the session and container it
    # belongs to, which the page already names.
    r"network\.(uid|id\.orig_h|conn_state|outrig\.session_id|outrig\.container)",
]


class Planter:
    """Replaces each string -- or, with `every_leaf`, each value that is not a list or an object
    -- with a payload, and keeps which field each payload's marker was planted in."""

    def __init__(self, every_leaf: bool) -> None:
        self.every_leaf = every_leaf
        self.where: dict[str, str] = {}

    def __call__(self, value: object, path: str) -> object:
        if isinstance(value, dict):
            return {k: self(v, f"{path}.{k}") for k, v in value.items()}
        if isinstance(value, list):
            return [self(v, f"{path}.{i}") for i, v in enumerate(value)]
        if self.every_leaf or (isinstance(value, str) and not re.fullmatch(TAGS, path)):
            marker = f"F{len(self.where)}"
            self.where[marker] = path
            return payload(marker)
        return value

    def events(self) -> list:
        return [
            (kind, self(data, f"{kind}.data"), self(subject, f"{kind}.subject"))
            for kind, data, subject in every_event()
        ]


class RenderSession(unittest.TestCase):
    def setUp(self) -> None:
        self.assertIsNotNone(UV, "uv is not on PATH; these tests run the renderer through it")
        self.dir = Path(self.enterContext(tempfile.TemporaryDirectory())) / "session"
        (self.dir / "logs").mkdir(parents=True)

    def write(
        self,
        events: list,
        network: list[dict] | None = None,
        session: object = None,
        tail: bytes = b"",
        where: Path | None = None,
    ) -> None:
        where = where or self.dir
        lines = []
        for n, event in enumerate(events, 1):
            kind, data, *subject = event
            lines.append(json.dumps(envelope(n, kind, data, *subject)) + "\n")
        (where / EVENTS).write_bytes("".join(lines).encode() + tail)
        if network is not None:
            text = "".join(json.dumps(r) + "\n" for r in network)
            (where / NETWORK).write_text(text)
        if session is None:
            session = {
                "id": "20261001T120000-f00d",
                "started_at": "2026-10-01T12:00:00Z",
                "container_name": "outrig-x",
                "image_tag": "alpine",
                "working_dir": "/repo",
                "session_dir": str(where),
            }
        if session is not False:
            body = session if isinstance(session, str) else json.dumps(session)
            (where / "session.json").write_text(body)

    def record(self, where: Path) -> Path:
        """A session that recorded, at `where` rather than at `self.dir`."""
        (where / "logs").mkdir(parents=True)
        self.write(ordinary(), where=where)
        return where

    def elsewhere(self) -> tuple[Path, dict[str, str | None]]:
        """A home directory of this test's own, and the environment that puts the renderer in it,
        so that nothing of the developer's is read; `uv` keeps its own directories."""
        home = Path(self.enterContext(tempfile.TemporaryDirectory()))
        env: dict[str, str | None] = {
            "HOME": str(home),
            "XDG_DATA_HOME": str(home / "data"),
            "XDG_CONFIG_HOME": str(home / "config"),
            **uv_keeps(),
        }
        return home, env

    def run_renderer(
        self,
        *args: str | Path,
        env: dict[str, str | None] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run the renderer as a person does. `env` is laid over the environment, a None in it
        taking a variable away; `cwd` is where it runs, the repo being where a test runs."""
        environment = {k: v for k, v in os.environ.items() if k != "VIRTUAL_ENV"}
        for name, value in (env or {}).items():
            if value is None:
                environment.pop(name, None)
            else:
                environment[name] = value
        return subprocess.run(
            [UV, "run", "--quiet", "--script", RENDERER, *args],
            capture_output=True,
            text=True,
            env=environment,
            cwd=cwd,
            timeout=300,
        )

    def page(self, events: list | None = None, **kw: object) -> str:
        """Render `events`, which must succeed, and return the page."""
        if events is not None:
            self.write(events, **kw)
        result = self.run_renderer(self.dir)
        self.assertEqual(result.returncode, 0, result.stderr)
        out = self.dir / REPORT
        self.assertEqual(result.stdout.strip(), str(out))
        return out.read_text()

    def assert_no_markup(self, html: str) -> None:
        """The page has no script and no image of its own, so any is one the record planted."""
        for tag in ("<script", "<img"):
            self.assertNotIn(tag, html.lower())

    def assert_escaped(self, html: str, marker: str) -> None:
        self.assert_no_markup(html)
        self.assertIn(escaped(marker), html, f"{marker} is not on the page at all")

    # -- hostile input, field by field: each reaches the page by its own path ----------------

    def test_submitted_source_is_escaped(self) -> None:
        self.assert_escaped(self.page(ordinary(source=payload("SOURCE"))), "SOURCE")

    def test_a_traceback_is_escaped(self) -> None:
        self.assert_escaped(self.page(ordinary(error=payload("TRACEBACK"))), "TRACEBACK")

    def test_captured_output_is_escaped(self) -> None:
        self.assert_escaped(self.page(ordinary(output=payload("OUTPUT"))), "OUTPUT")

    def test_a_message_body_is_escaped(self) -> None:
        self.assert_escaped(self.page(ordinary(body=payload("BODY"))), "BODY")

    # -- and then every field ----------------------------------------------------------------

    def test_every_string_field_is_escaped_and_shown(self) -> None:
        plant = Planter(every_leaf=False)
        html = self.page(plant.events(), network=[plant(connection(), "network")])
        self.assert_no_markup(html)
        missing = [
            f"{marker} ({path})"
            for marker, path in plant.where.items()
            if escaped(marker) not in html
            and not any(re.fullmatch(rule, path) for rule in NOT_SHOWN)
        ]
        self.maxDiff = None
        self.assertEqual(missing, [], "fields planted but not on the page")

    def test_a_payload_in_any_field_of_any_type_is_escaped(self) -> None:
        plant = Planter(every_leaf=True)
        self.assert_no_markup(self.page(plant.events(), network=[plant(connection(), "network")]))

    def test_the_sweep_covers_every_event_there_is(self) -> None:
        swept = {kind for kind, _, _ in every_event()}
        written = set(re.findall(r'Payload::\w+[^=\n]*=> "([a-z.]+)"', CATALOG_RS.read_text()))
        documented = {
            name
            for line in EVENTS_MD.read_text().splitlines()
            if (bullet := re.match(r"- ((?:`[a-z.]+`(?:, )?)+) --", line))
            for name in re.findall(r"`([a-z.]+)`", bullet.group(1))
        }
        self.assertEqual(written, swept, "the catalog has events the sweep does not plant")
        self.assertEqual(documented, swept, "events.md documents events the sweep does not plant")

    def test_the_page_allows_no_script(self) -> None:
        csp = r'<meta http-equiv="Content-Security-Policy"\s+content="default-src \'none\';'
        self.assertRegex(self.page(ordinary()), csp)

    # -- what the record is missing ----------------------------------------------------------

    def test_a_session_without_a_network_log_renders_without_it(self) -> None:
        html = self.page(ordinary())
        self.assertNotIn('id="network"', html)

    def test_a_network_log_is_shown(self) -> None:
        html = self.page(ordinary(), network=[connection(**{"id.resp_p": 80})])
        self.assertIn('id="network"', html)
        self.assertIn("example.com (asserted)", html)
        self.assertIn("93.184.215.14:80", html)

    def test_a_cut_off_final_line_is_skipped_and_said(self) -> None:
        events = ordinary()
        html = self.page(events, tail=b'{"specversion": "1.0", "id": "99", "da')
        self.assertIn(f"events.jsonl line {len(events) + 1} skipped: cut off", html)
        self.assertIn("Done.", html)

    def test_a_final_line_cut_inside_a_character_is_skipped(self) -> None:
        cut = '{"data": {"body": "café'.encode()[:-1]
        html = self.page(ordinary(), tail=cut)
        self.assertIn(f"events.jsonl line {len(ordinary()) + 1} skipped: cut off", html)

    def test_an_empty_log_renders(self) -> None:
        self.assertIn("the agent never started", self.page([]))

    def test_an_unreadable_session_record_renders(self) -> None:
        html = self.page(ordinary(), session="{not json")
        self.assertIn("session.json could not be read", html)

    def test_no_session_record_renders(self) -> None:
        self.assertIn("There is no session.json.", self.page(ordinary(), session=False))

    def test_a_call_names_what_its_provider_was_not_sent(self) -> None:
        left_out = {"left_out": [{"turn": 0, "message": 1, "part": 0}]}
        events = [
            (kind, data | left_out) if kind == "model.call" else (kind, data)
            for kind, data in ordinary()
        ]
        self.assertIn("left out of turn 0: part 0 of message 1", self.page(events))

    def test_a_call_names_what_it_withheld_so_the_roles_alternate(self) -> None:
        withheld = {"withheld": [{"turn": 3, "why": "recent"}, {"turn": 5, "why": "promoted"}]}
        events = [
            (kind, data | withheld) if kind == "model.call" else (kind, data)
            for kind, data in ordinary()
        ]
        self.assertIn("withheld so the roles alternate: 3 (recent), 5 (promoted)", self.page(events))
        self.assertNotIn("withheld so the roles alternate", self.page(ordinary()))
        self.assertIn("role alternation: relaxed", self.page(ordinary()))

    def test_an_unknown_type_is_shown_as_it_is(self) -> None:
        html = self.page(ordinary() + [("something.new", {"what": "it says"})])
        self.assertIn("org.outrig.something.new", html)
        self.assertIn("it says", html)

    def test_a_record_that_stops_short_says_so(self) -> None:
        html = self.page(ordinary()[:5])
        self.assertIn("Round 1 -- unfinished: the record ends inside it", html)
        self.assertIn("does not end with the shutdown report", html)

    def test_only_an_exit_the_shutdown_did_not_ask_for_is_a_health_note(self) -> None:
        def closed(expected: bool) -> list[tuple[str, dict]]:
            events = ordinary()
            at = next(i for i, (kind, _) in enumerate(events) if kind == "agent.stopped")
            exited = ("interpreter.exited", {"cause": "end of output", "expected": expected})
            return events[:at] + [("session.state", {"state": "closing"}), exited] + events[at:]

        note = "The interpreter exited while the session ran"
        self.assertNotIn(note, self.page(closed(True)))
        self.assertIn(note, self.page(closed(False)))

    def test_missing_ids_are_reported(self) -> None:
        self.write(ordinary())
        log = self.dir / EVENTS
        lines = log.read_text().splitlines(keepends=True)
        log.write_text("".join(lines[:3] + lines[5:]))
        html = self.page()
        self.assertIn("2 id(s) missing (4-5)", html)

    # -- what a damaged or crafted record does not get to do ----------------------------------

    def test_a_call_with_no_recorded_submission_is_not_said_not_to_have_run(self) -> None:
        html = self.page([e for e in ordinary() if e[0] != "exec.submitted"])
        self.assertIn("submit_python, with no submission of this source recorded", html)
        self.assertNotIn("not run", html)

    def test_a_round_whose_start_was_lost_keeps_its_calls(self) -> None:
        html = self.page([e for e in ordinary() if e[0] != "round.started"])
        self.assertIn("Round 1 (its start was not recorded) -- completed", html)
        tokens = html[html.index('id="tokens"') : html.index('id="timeline"')]
        cells = re.findall(r'<td class="n">([^<]*)</td>', tokens)
        # Its two calls, one of which reported usage, and what they were held to.
        self.assertEqual([cells[0], *cells[8:10]], ["2 (1 reported)", "1,700", "128,000 (assumed)"])

    def test_lost_executions_are_not_said_never_to_have_been_submitted(self) -> None:
        events = [e for e in ordinary() if not e[0].startswith("exec.")]
        html = self.page(events)
        self.assertIn("No executions were recorded.", html)
        self.assertNotIn("Nothing was submitted", html)

    def test_a_lost_boundary_between_rounds_splits_their_calls(self) -> None:
        start, rest = ordinary()[:5], ordinary()[5:]
        # Round 1's end and round 2's start are lost; round 2's call and its end survive.
        usage = {"input_tokens": 70, "output_tokens": 1, "total_tokens": 71}
        events = [
            *start,
            ("model.call", model_call(1, 2, estimate=2500)),
            ("model.round.completed", {"round": 2, "usage": usage, "calls": [{"usage": usage}]}),
            rest[-1],
        ]
        html = self.page(events)
        self.assertIn("Round 1 -- unfinished: its end was not recorded", html)
        self.assertIn("Round 2 (its start was not recorded) -- completed", html)
        tokens = html[html.index('id="tokens"') : html.index('id="timeline"')]
        rows = [
            re.findall(r'<td class="n">([^<]*)</td>', row)[:2]
            for row in re.findall(r"<tr>(.*?)</tr>", tokens, re.DOTALL)[1:3]
        ]
        # Each round its own call, and round 2's tokens on round 2.
        self.assertEqual(rows, [["1 (0 reported)", "0"], ["1", "70"]])

    def test_a_record_without_gaps_is_not_said_to_have_lost_nothing(self) -> None:
        html = self.page(ordinary())
        self.assertNotIn("Nothing missing", html)
        self.assertIn("unless it came\nafter the last one the log wrote, which leaves none", html)

    def test_numbers_the_record_could_not_have_written_are_shown_as_they_are(self) -> None:
        self.write(ordinary())
        log = self.dir / EVENTS
        log.write_text(
            log.read_text()
            .replace('"duration": 0.25', '"duration": 1e309')
            .replace('"output_tokens": 10', '"output_tokens": NaN')
            .replace('"input_tokens": 100', f'"input_tokens": {10**30}')
        )
        self.assertIn("exec 4: ok in Infinity", self.page())

    def test_an_id_too_long_to_be_a_number_is_reported(self) -> None:
        self.write(ordinary())
        log = self.dir / EVENTS
        log.write_text(log.read_text().replace('"id": "1",', f'"id": "{"1" * 4301}",', 1))
        self.assertIn("ids are not numbers", self.page())

    def test_a_session_record_too_deeply_nested_to_read_is_reported(self) -> None:
        # Python 3.12 and 3.13 refuse this nesting; 3.14 parses it as a list.
        html = self.page(ordinary(), session="[" * 10000 + "]" * 10000)
        self.assertIn("session.json could not be read", html)

    # -- the timeline ------------------------------------------------------------------------

    def test_a_round_number_used_twice_is_two_rounds(self) -> None:
        events = (
            ordinary()[:3]
            + [
                ("round.started", {"round": 1}),
                ("model.call", model_call(0, 1)),
                ("model.round.failed", {"round": 1, "error": "HTTP 500", "calls": []}),
            ]
            + ordinary()[3:]
        )
        html = self.page(events)
        self.assertIn("Round 1 -- failed", html)
        self.assertIn("Round 1, attempt 2 -- completed", html)

    def test_a_round_links_its_executions_and_messages_by_id(self) -> None:
        html = self.page(ordinary())
        round_1 = html[html.index("Round 1 -- completed") : html.index('id="executions"')]
        self.assertIn('<a href="#exec-4">exec 4: ok', round_1)
        self.assertIn('<a href="#msg-2">message 2: taken by agent/primary', round_1)
        self.assertIn('submitted as exec 4 -- <a href="#exec-4">', round_1)
        self.assertIn('id="exec-4"', html)
        self.assertIn('id="msg-2"', html)

    def test_a_late_result_says_where_it_was_submitted(self) -> None:
        events = ordinary()
        completed = next(e for e in events if e[0] == "exec.completed")
        events.remove(completed)
        events[-1:-1] = [
            ("round.started", {"round": 2}),
            completed,
            ("model.round.completed", {"round": 2, "usage": {}, "calls": []}),
        ]
        html = self.page(events)
        self.assertIn("exec 4: ok in 0.25 s, submitted in Round 1", html)
        self.assertIn("submitted in Round 1, ended in Round 2", html)

    def test_tokens_keep_round_totals_apart_from_the_largest_request(self) -> None:
        def call(index: int, tokens_in: int) -> dict:
            return {
                "index": index,
                "usage": {
                    "input_tokens": tokens_in,
                    "output_tokens": 5,
                    "total_tokens": tokens_in + 5,
                },
            }

        # Up to the round's end, which is replaced, and the session's own ending, kept.
        events = ordinary()[:-4] + [
            (
                "model.round.completed",
                {
                    "round": 1,
                    "stopped": None,
                    "usage": {"input_tokens": 1300, "output_tokens": 10, "total_tokens": 1310},
                    "calls": [call(0, 300), call(1, 1000)],
                    "input_tokens_max": 1000,
                },
            ),
            ("round.started", {"round": 2}),
            ("model.call", model_call(2, 2, estimate=2500)),
            ("model.round.failed", {"round": 2, "error": "e", "calls": [call(0, 2000)]}),
        ] + ordinary()[-3:]
        html = self.page(events)
        tokens = html[html.index('id="tokens"') : html.index('id="timeline"')]
        rows = [
            re.findall(r'<td class="n">([^<]*)</td>', row)
            for row in re.findall(r"<tr>(.*?)</tr>", tokens, re.DOTALL)[1:]
        ]
        # calls, input, output, cache read, cache write, reasoning, total; then the largest
        # request reported, the largest OutRig estimated, and the window.
        self.assertEqual(
            rows,
            [
                ["2", "1,300", "10", "0", "0", "0", "1,310", "1,000", "1,700", "128,000 (assumed)"],
                # Failed, so summed over the one call it reported.
                ["1", "2,000", "5", "0", "0", "0", "2,005", "2,000", "2,500", "128,000 (assumed)"],
                # The session: totals add up, and the largest request is the largest of any.
                ["3", "3,300", "15", "0", "0", "0", "3,315", "2,000", "2,500", ""],
            ],
        )

    # -- the page file -----------------------------------------------------------------------

    def test_the_page_is_its_owners_alone(self) -> None:
        self.page(ordinary())
        mode = stat.S_IMODE((self.dir / REPORT).stat().st_mode)
        self.assertEqual(mode, 0o600)

    def test_a_wider_page_already_there_is_replaced(self) -> None:
        out = self.dir / REPORT
        out.write_text("old")
        out.chmod(0o644)
        self.page(ordinary())
        self.assertEqual(stat.S_IMODE(out.stat().st_mode), 0o600)
        self.assertNotEqual(out.read_text(), "old")

    def test_out_names_the_page(self) -> None:
        self.write(ordinary())
        out = self.dir.parent / "elsewhere.html"
        result = self.run_renderer(self.dir, "--out", out)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(out))
        self.assertIn("Done.", out.read_text())

    def test_the_page_is_never_written_over_what_it_reads(self) -> None:
        self.write(ordinary())
        log = self.dir / EVENTS
        before = log.read_bytes()
        result = self.run_renderer(self.dir, "--out", log)
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing to write the page over", result.stderr)
        self.assertEqual(log.read_bytes(), before)

    # -- naming a session by its id, as the outrig subcommands take one ----------------------

    def rendered(self, result: subprocess.CompletedProcess[str], session: Path) -> None:
        """`result` rendered `session`, and wrote the page into it."""
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(session / REPORT))
        self.assertIn("Done.", (session / REPORT).read_text())

    def test_an_id_is_looked_up_under_the_default_session_root(self) -> None:
        home, env = self.elsewhere()
        session = self.record(home / "data" / "outrig" / "sessions" / "20261006T192731-cc23")
        self.rendered(self.run_renderer("20261006T192731-cc23", env=env, cwd=home), session)

    def test_part_of_an_id_is_enough_when_it_names_one_session(self) -> None:
        home, env = self.elsewhere()
        root = home / "data" / "outrig" / "sessions"
        session = self.record(root / "20261006T192731-cc23")
        self.record(root / "20261006T201500-0b8e")
        self.rendered(self.run_renderer("cc23", env=env, cwd=home), session)

    def test_part_of_an_id_that_names_two_sessions_lists_them(self) -> None:
        home, env = self.elsewhere()
        root = home / "data" / "outrig" / "sessions"
        self.record(root / "20261006T192731-cc23")
        self.record(root / "20261006T201500-0b8e")
        result = self.run_renderer("20261006", env=env, cwd=home)
        self.assertEqual(result.returncode, 1)
        self.assertIn('ambiguous session "20261006"; candidates:', result.stderr)
        self.assertIn("20261006T192731-cc23", result.stderr)
        self.assertIn("20261006T201500-0b8e", result.stderr)

    def test_an_id_no_session_has_is_not_blamed_on_the_config(self) -> None:
        home, env = self.elsewhere()
        root = home / "data" / "outrig" / "sessions"
        self.record(root / "20261006T192731-cc23")
        result = self.run_renderer("ffff", env=env, cwd=home)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f'no session matching "ffff" under {root}', result.stderr)
        self.assertNotIn("[events]", result.stderr)

    def test_no_session_root_yet_says_so(self) -> None:
        home, env = self.elsewhere()
        result = self.run_renderer("cc23", env=env, cwd=home)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"no session root at {home / 'data' / 'outrig' / 'sessions'}", result.stderr)

    def test_a_session_kept_elsewhere_is_found_through_its_link(self) -> None:
        home, env = self.elsewhere()
        kept = self.record(home / "debug-run")
        root = home / "data" / "outrig" / "sessions"
        root.mkdir(parents=True)
        (root / "20261006T192731-cc23").symlink_to(kept)
        self.rendered(self.run_renderer("cc23", env=env, cwd=home), root / "20261006T192731-cc23")
        self.assertTrue((kept / REPORT).is_file())

    def test_the_session_root_flag_wins(self) -> None:
        home, env = self.elsewhere()
        config = home / "config" / "outrig" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text('session-root = "/nowhere"\n')
        session = self.record(home / "kept" / "20261006T192731-cc23")
        result = self.run_renderer("cc23", "--session-root", home / "kept", env=env, cwd=home)
        self.rendered(result, session)

    def test_the_global_config_names_the_session_root(self) -> None:
        home, env = self.elsewhere()
        session = self.record(home / "kept" / "20261006T192731-cc23")
        config = home / "config" / "outrig" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text(f'session-root = "{home / "kept"}"\n')
        self.rendered(self.run_renderer("cc23", env=env, cwd=home), session)

    def test_without_xdg_the_global_config_is_under_home(self) -> None:
        home, env = self.elsewhere()
        env["XDG_CONFIG_HOME"] = None
        session = self.record(home / "kept" / "20261006T192731-cc23")
        (home / ".outrig").mkdir()
        (home / ".outrig" / "config.toml").write_text(f'session-root = "{home / "kept"}"\n')
        self.rendered(self.run_renderer("cc23", env=env, cwd=home), session)

    def test_the_repo_config_found_above_the_working_directory_wins(self) -> None:
        home, env = self.elsewhere()
        session = self.record(home / "kept" / "20261006T192731-cc23")
        config = home / "config" / "outrig" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text('session-root = "/nowhere"\n')
        repo = home / "repo"
        (repo / ".agents" / "outrig").mkdir(parents=True)
        (repo / ".agents" / "outrig" / "config.toml").write_text(
            f'session-root = "{home / "kept"}"\n'
        )
        (repo / "deep" / "er").mkdir(parents=True)
        self.rendered(self.run_renderer("cc23", env=env, cwd=repo / "deep" / "er"), session)

    def test_a_config_that_does_not_parse_is_named(self) -> None:
        home, env = self.elsewhere()
        config = home / "config" / "outrig" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text("session-root =\n")
        result = self.run_renderer("cc23", env=env, cwd=home)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"reading {config}", result.stderr)

    def test_a_directory_given_by_path_needs_no_lookup(self) -> None:
        home, env = self.elsewhere()
        config = home / "config" / "outrig" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text("session-root =\n")
        self.write(ordinary())
        self.rendered(self.run_renderer(self.dir, env=env, cwd=home), self.dir)

    # -- being pointed at the wrong thing ----------------------------------------------------

    def test_a_session_that_did_not_record_says_how_to(self) -> None:
        result = self.run_renderer(self.dir)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Only `outrig run-new` records one", result.stderr)
        self.assertIn('[events] mode = "record"', result.stderr)

    def test_a_path_that_is_not_a_directory_is_not_blamed_on_the_config(self) -> None:
        result = self.run_renderer(self.dir / "nope")
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"{self.dir / 'nope'} is not a directory", result.stderr)
        self.assertNotIn("[events]", result.stderr)

    def test_the_logs_directory_points_at_its_parent(self) -> None:
        self.write(ordinary())
        result = self.run_renderer(self.dir / "logs")
        self.assertEqual(result.returncode, 1)
        self.assertIn("pass the session directory above it", result.stderr)

    def test_the_log_itself_points_at_its_session(self) -> None:
        self.write(ordinary())
        result = self.run_renderer(self.dir / EVENTS)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"pass the session directory, {self.dir}, not the log itself", result.stderr)


if __name__ == "__main__":
    unittest.main()
