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

`--host` is exercised the same way: the script is started serving, the page is fetched over
HTTP, and Ctrl-C stops it, so what is asserted is what a browser would be handed.

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

import contextlib
import functools
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from collections.abc import Iterator
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
RENDERER = SCRIPTS / "render-session.py"
CATALOG_RS = SCRIPTS.parent / "crates" / "outrig" / "src" / "harness" / "event.rs"
EVENTS_MD = SCRIPTS.parent / "doc" / "reference" / "events.md"
UV = os.environ.get("UV") or shutil.which("uv")
EVENTS = Path("logs") / "events.jsonl"
NETWORK = Path("logs") / "network.jsonl"
REPORT = "report.html"


def free_port() -> int:
    """A port nothing was listening on a moment ago. Nothing holds it between here and the bind
    under test, so this is for the cases where the number itself is what is asserted."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def has_ipv6_loopback() -> bool:
    """Whether `::1` can be bound here: a container or a runner can be built without it."""
    if not socket.has_ipv6:
        return False
    try:
        with socket.socket(socket.AF_INET6) as s:
            s.bind(("::1", 0))
    except OSError:
        return False
    return True


def fetch(url: str, method: str = "GET") -> tuple[int, dict[str, str], str]:
    """A request to the served page, as a browser makes it: status, headers, and body."""
    request = urllib.request.Request(url, method=method)  # noqa: S310 - http, built by the test
    try:
        with urllib.request.urlopen(request, timeout=30) as answer:  # noqa: S310
            return answer.status, dict(answer.headers), answer.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode()


def refused(url: str, within: float = 10.0) -> bool:
    """Whether nothing answers at `url` within `within` seconds. Polled rather than asked once:
    the signal that stops the server reaches it a moment after the process that sent it
    returns."""
    deadline = time.monotonic() + within
    while True:
        try:
            fetch(url)
        except urllib.error.URLError:
            return True
        if time.monotonic() > deadline:
            return False
        time.sleep(0.1)


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
                "hosted": [],
                "bindings": None,
                "verdict": "clean",
            },
        ),
        ("session.state", {"state": "reported"}),
    ]


def token_rows(html: str) -> list[list[str]]:
    """The token table's numeric cells, a row per round and then the session's."""
    tokens = html[html.index('id="tokens"') : html.index('id="timeline"')]
    return [
        re.findall(r'<td class="n">([^<]*)</td>', row)
        for row in re.findall(r"<tr>(.*?)</tr>", tokens, re.DOTALL)[1:]
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
    c = "agent/c1"
    message = {"role": "user", "content": [{"type": "text", "text": "opening"}]}
    return [
        ("session.state", {"state": "starting"}, None),
        (
            "binding.refused",
            {"binding": "b", "declared_in": "/r/config.toml", "reason": "r"},
            None,
        ),
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
        ("model.text", {"text": "t"}, a),
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
                "descendants": [5],
                "total": usage,
            },
            a,
        ),
        ("model.round.usage", {"round": 1, "attempts": [6], "usage": usage}, a),
        (
            "agent.spawned",
            {
                "name": "n",
                "parent": a,
                "kind": "request",
                "model": "m",
                "depth": 2,
                "inputs": [{"name": "xs", "type": "list[int]", "preview": "[1]"}],
                "channels": [{"name": "fizz", "request": "str", "reply": "Thing"}],
            },
            c,
        ),
        (
            "agent.started",
            {
                "model": "m",
                "python": "3.13",
                "container": "c",
                "tool_call_max": 1,
                "tool_result_max": 1,
            },
            c,
        ),
        (
            "model.instructions",
            {
                "model": "m",
                "preamble": "p",
                "max_tokens": 1,
                "tools": [{"name": "submit_python", "description": "d", "parameters": {}}],
            },
            c,
        ),
        (
            "agent.request.sent",
            {"request": 7, "channel": "fizz", "sender": a, "body": '"b"', "attempts_max": 3},
            c,
        ),
        ("agent.request.received", {"request": 7, "round": 1}, c),
        (
            "agent.request.replied",
            {"request": 7, "attempt": 1, "value": '{"n": "five"}'},
            c,
        ),
        (
            "agent.request.invalid",
            {"request": 7, "attempt": 1, "problems": ["$.n: expected an integer"]},
            c,
        ),
        ("agent.request.failed", {"request": 7, "message": "why"}, c),
        (
            "agent.request.settled",
            {"request": 7, "outcome": "failed", "attempts": 1, "rounds": [1]},
            c,
        ),
        ("agent.request.refused", {"request": 7, "ended": "failed"}, c),
        (
            "agent.request.sent",
            {"request": 8, "channel": None, "sender": a, "body": '{"xs": [1]}', "attempts_max": 3},
            c,
        ),
        ("agent.request.progress", {"request": 8, "body": "p"}, c),
        ("agent.request.cancelled", {"request": 8, "received": True}, c),
        ("agent.released", {"released": ["c1"], "ended": ["c1", "c2"], "requests": [8]}, a),
        ("agent.wedged", {}, "agent/c2"),
        ("model.usage.replaced", {"call_id": 1, "attempt_id": 1, "usage": usage}, a),
        (
            "model.usage.refused",
            {"call_id": None, "attempt_id": 9, "usage": usage, "reason": "unknown"},
            a,
        ),
        ("round.started", {"round": 2}, a),
        (
            "model.round.failed",
            {
                "round": 2,
                "error": "e",
                "calls": calls,
                "usage": None,
                "attempts": [3],
                "descendants": [],
                "total": None,
            },
            a,
        ),
        ("round.started", {"round": 2}, a),
        (
            "model.round.dropped",
            {
                "round": 2,
                "calls": calls,
                "usage": usage,
                "attempts": [4],
                "descendants": [],
                "total": usage,
            },
            a,
        ),
        (
            "hosted.received",
            {
                "request": 1,
                "agent": "primary",
                "binding": "repo",
                "execid": 2,
                "operation": "callattr",
                "member": "push",
                "target": "git.remote.Remote",
                "arguments": ["'main'"],
                "keywords": {"force": "False"},
                "parent": None,
                "by": "binding",
            },
            a,
        ),
        ("hosted.dispatched", {"request": 1, "agent": "primary", "binding": "repo"}, a),
        ("hosted.callback", {"parent": 1, "agent": "primary", "binding": "repo"}, a),
        (
            "hosted.ended",
            {
                "request": 1,
                "agent": "primary",
                "binding": "repo",
                "outcome": "raised",
                "reason": None,
                "detail": None,
                "exception": "git.exc.GitCommandError",
                "traceback": "Traceback (most recent call last): ...",
                "interrupted": False,
                "duration": 0.5,
            },
            a,
        ),
        ("binding.exited", {"bindings": ["repo"], "cause": "it was killed by SIGKILL"}, None),
        ("session.state", {"state": "closing"}, None),
        ("interpreter.exited", {"cause": "c", "expected": False}, None),
        ("agent.stopped", {}, a),
        (
            "session.report",
            {
                "closed_by": {"by": "interpreter_exited", "cause": "c"},
                "stopped": {"state": "not_proven", "reason": "r"},
                "executions": [{"execid": 2, "status": "unknown"}],
                "hosted": [
                    {
                        "request": 1,
                        "agent": "primary",
                        "binding": "repo",
                        "outcome": "unknown",
                        "reason": "killed",
                    }
                ],
                "bindings": {
                    "names": ["repo"],
                    "group_empty": True,
                    "killed": False,
                    "survivors": [],
                    "exited": "it was killed by SIGKILL",
                },
                "requests": [
                    {"request": 9, "child": "c1", "channel": "fizz", "outcome": "closed"},
                    {"request": 10, "child": "c1", "channel": None, "outcome": "terminated"},
                ],
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
        # What a `serving` run printed, filled in as it ends, and what it printed before its URL.
        self.served = subprocess.CompletedProcess[str]([], 0, "", "")
        self.printed_first: list[str] = []

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

    def environment(self, env: dict[str, str | None] | None = None) -> dict[str, str]:
        """The environment a run gets: this one, laid over with `env`, a None in it taking a
        variable away."""
        environment = {k: v for k, v in os.environ.items() if k != "VIRTUAL_ENV"}
        for name, value in (env or {}).items():
            if value is None:
                environment.pop(name, None)
            else:
                environment[name] = value
        return environment

    def run_renderer(
        self,
        *args: str | Path,
        env: dict[str, str | None] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run the renderer as a person does. `env` is laid over the environment, a None in it
        taking a variable away; `cwd` is where it runs, the repo being where a test runs."""
        return subprocess.run(
            [UV, "run", "--quiet", "--script", RENDERER, *args],
            capture_output=True,
            text=True,
            env=self.environment(env),
            cwd=cwd,
            timeout=300,
        )

    @contextlib.contextmanager
    def serving(self, *args: str | Path) -> Iterator[str]:
        """The renderer serving `self.dir`, as `--host` does. Yields the URL it printed, leaving
        in `self.printed_first` what it printed before that, and on the way out stops it with the
        Ctrl-C a terminal sends: SIGINT to the whole group, so that `uv` and the script it runs
        each get the one a person would send. The finished run is left in `self.served`."""
        process = subprocess.Popen(  # noqa: S603 - the renderer, through uv, as a person runs it
            [UV, "run", "--quiet", "--script", RENDERER, self.dir, *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.environment(),
            start_new_session=True,
        )
        try:
            url = ""
            assert process.stdout is not None
            for line in process.stdout:
                url = line.strip()
                if url.startswith("http://"):
                    break
                self.printed_first.append(url)
            else:
                self.fail(f"it stopped without a URL, having printed {self.printed_first}")
            yield url
        finally:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGINT)
            out, err = process.communicate(timeout=120)
            self.served = subprocess.CompletedProcess(process.args, process.returncode, out, err)
        self.assertNotIn("Traceback", self.served.stderr)
        # 0 when the script answered the signal, which is what it does; a status of
        # killed-by-SIGINT when `uv`, which gets the same signal, ends on it rather than on its
        # child's exit. Either way the serving stopped, which the case after this one asserts.
        self.assertIn(self.served.returncode, (0, -signal.SIGINT), self.served.stderr)

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

    def test_a_childs_start_and_instructions_are_its_own(self) -> None:
        started = {"model": "haiku", "python": "3.13.15", "container": "outrig-x"}
        instructions = {"model": "haiku", "preamble": "CHILD", "tools": [], "max_tokens": 99}
        events, c = ordinary(), "agent/c1"
        child = [("agent.started", started, c), ("model.instructions", instructions, c)]
        html = self.page(events[:2] + child + events[2:])
        self.assertIn("<th>model</th><td>sonnet</td>", html)
        self.assertIn("<th>max tokens</th><td>4096</td>", html)
        self.assertIn('<h4 id="instructions-1">agent/c1, max tokens 99</h4>', html)
        self.assertIn('<a href="#instructions-1">0 tool(s)</a>', html)
        self.assertIn("CHILD", html)
        self.assertNotIn("logs were joined", html)
        self.assertIn("2 starts of one agent: logs were joined", self.page(events[:1] + events))

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
        rows = [row[:2] for row in token_rows(html)[:2]]
        # Each round its own call, and round 2's tokens on round 2; round 1's, whose end was
        # lost, are unknown rather than zero.
        self.assertEqual(rows, [["1 (0 reported)", ""], ["1", "70"]])

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
        # calls, input, output, cache read, cache write, reasoning, total; then the largest
        # request reported, the largest OutRig estimated, and the window.
        self.assertEqual(
            token_rows(self.page(events)),
            [
                ["2", "1,300", "10", "0", "0", "0", "1,310", "1,000", "1,700", "128,000 (assumed)"],
                # Failed, so summed over the one call it reported.
                ["1", "2,000", "5", "0", "0", "0", "2,005", "2,000", "2,500", "128,000 (assumed)"],
                # The session: totals add up, and the largest request is the largest of any.
                ["3", "3,300", "15", "0", "0", "0", "3,315", "2,000", "2,500", ""],
            ],
        )

    def test_usage_no_call_reported_is_unknown_not_zero(self) -> None:
        def unreported(index: int) -> dict:
            return {"index": index, "call_id": index + 1, "attempt_id": index + 1, "usage": None}

        events = ordinary()[:-4] + [
            (
                "model.round.failed",
                {
                    "round": 1,
                    "error": "e",
                    "calls": [unreported(0), unreported(1)],
                    "usage": None,
                    "input_tokens_max": None,
                    "attempts": [1, 2],
                },
            ),
        ] + ordinary()[-3:]
        self.assertEqual(
            token_rows(self.page(events)),
            [
                ["2 (0 reported)", "", "", "", "", "", "", "", "1,700", "128,000 (assumed)"],
                ["2 (0 reported)", "", "", "", "", "", "", "", "1,700", ""],
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

    # -- serving it instead of writing it ----------------------------------------------------

    def test_host_serves_the_page_and_writes_nothing(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertTrue(url.startswith("http://127.0.0.1:"), url)
            status, headers, body = fetch(url)
            self.assertEqual(status, 200)
            self.assertEqual(headers["Content-Type"], "text/html; charset=utf-8")
            self.assertIn("Done.", body)
        self.assertFalse((self.dir / REPORT).exists(), "serving left a page behind")

    def test_a_served_page_escapes_what_a_written_one_does(self) -> None:
        self.write(ordinary(source=payload("SERVED")))
        with self.serving("--host") as url:
            body = fetch(url)[2]
        self.assert_escaped(body, "SERVED")

    def test_a_served_page_carries_the_policy_as_a_header_too(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            headers, body = fetch(url)[1:]
        meta = re.search(r'http-equiv="Content-Security-Policy"\s+content="([^"]*)"', body)
        self.assertIsNotNone(meta)
        self.assertEqual(headers["Content-Security-Policy"], meta.group(1))
        self.assertEqual(headers["X-Content-Type-Options"], "nosniff")
        self.assertEqual(headers["Cache-Control"], "no-store")

    def test_a_reload_shows_what_the_record_has_grown(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertNotIn("LATER", fetch(url)[2])
            with (self.dir / EVENTS).open("a") as log:
                log.write(json.dumps(envelope(99, "interpreter.diagnostic", {"text": "LATER"})))
                log.write("\n")
            self.assertIn("LATER", fetch(url)[2])

    def test_nothing_but_the_page_is_served(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertEqual(fetch(url + "logs/events.jsonl")[0], 404)
            self.assertEqual(fetch(url + "../../etc/passwd")[0], 404)
            self.assertEqual(fetch(url, "HEAD")[0], 200)
            self.assertEqual(fetch(url, "HEAD")[2], "")

    def test_a_record_that_goes_away_mid_serve_does_not_end_the_serving(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertEqual(fetch(url)[0], 200)
            (self.dir / EVENTS).unlink()
            self.assertEqual(fetch(url)[0], 500)
            self.write(ordinary())
            self.assertEqual(fetch(url)[0], 200)

    def test_a_given_port_is_the_one_bound(self) -> None:
        self.write(ordinary())
        port = free_port()
        with self.serving("--host", f"127.0.0.1:{port}") as url:
            self.assertEqual(url, f"http://127.0.0.1:{port}/")
            self.assertEqual(fetch(url)[0], 200)

    def test_a_port_alone_is_the_loopback(self) -> None:
        self.write(ordinary())
        port = free_port()
        with self.serving("--host", f":{port}") as url:
            self.assertEqual(url, f"http://127.0.0.1:{port}/")
            self.assertEqual(fetch(url)[0], 200)

    def test_every_interface_is_bound_on_request_and_said_to_be_wide(self) -> None:
        self.write(ordinary())
        with self.serving("--host", "0.0.0.0:0") as url:
            port = url.rsplit(":", 1)[1].rstrip("/")
            self.assertEqual(url, f"http://0.0.0.0:{port}/")
            self.assertEqual(fetch(f"http://127.0.0.1:{port}/")[0], 200)
        self.assertIn("reachable from other machines", self.served.stderr)

    def test_the_loopback_is_not_said_to_be_wide(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertEqual(fetch(url)[0], 200)
        self.assertNotIn("reachable from other machines", self.served.stderr)

    def test_an_ipv6_address_is_bound_and_its_url_is_bracketed(self) -> None:
        if not has_ipv6_loopback():
            self.skipTest("no IPv6 loopback here")
        self.write(ordinary())
        port = free_port()
        with self.serving("--host", f"[::1]:{port}") as url:
            self.assertEqual(url, f"http://[::1]:{port}/")
            self.assertEqual(fetch(url)[0], 200)

    def test_ctrl_c_stops_the_serving(self) -> None:
        self.write(ordinary())
        with self.serving("--host") as url:
            self.assertEqual(fetch(url)[0], 200)
        self.assertTrue(refused(url), f"something still answers at {url}")

    def test_out_beside_host_writes_the_page_and_serves_it(self) -> None:
        self.write(ordinary())
        out = self.dir.parent / "elsewhere.html"
        with self.serving("--host", "--out", out) as url:
            self.assertEqual(self.printed_first, [str(out)])
            self.assertIn("Done.", out.read_text())
            self.assertIn("Done.", fetch(url)[2])

    def test_a_port_already_taken_is_reported(self) -> None:
        self.write(ordinary())
        with socket.socket() as held:
            held.bind(("127.0.0.1", 0))
            held.listen()
            port = held.getsockname()[1]
            result = self.run_renderer(self.dir, "--host", f"127.0.0.1:{port}")
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"binding 127.0.0.1:{port}", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_an_address_this_machine_does_not_have_is_reported(self) -> None:
        self.write(ordinary())
        # 192.0.2.0/24 is the documentation range: no machine routes it, so no machine has it.
        result = self.run_renderer(self.dir, "--host", "192.0.2.1:0")
        self.assertEqual(result.returncode, 1)
        self.assertIn("binding 192.0.2.1:0", result.stderr)

    def test_an_address_that_is_not_one_is_a_usage_error(self) -> None:
        self.write(ordinary())
        for given in (":twenty", "127.0.0.1:99999", "[::1", "[not-v6]:80"):
            with self.subTest(given=given):
                result = self.run_renderer(self.dir, "--host", given)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("--host", result.stderr)

    def test_the_session_after_host_is_not_taken_for_an_address(self) -> None:
        self.write(ordinary())
        result = self.run_renderer("--host", self.dir)
        self.assertEqual(result.returncode, 2)
        self.assertIn("is a path, not an address", result.stderr)
        self.assertIn("the session goes before it", result.stderr)

    def test_a_session_that_did_not_record_is_not_served(self) -> None:
        result = self.run_renderer(self.dir, "--host")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Only `outrig run-new` records one", result.stderr)

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
