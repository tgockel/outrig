# The maintainer's `ReviewResult`, copied unchanged from
# `plan/phase/0003-python/typed-agents.md` ("The maintainer's result type"): what
# `outrig.schema` has to take as written. Its example completion is `review_fixture.json`.

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Annotated, Literal, Optional

import outrig
from outrig.schema import alias, doc

Status = Literal["approve", "comment", "reject-changes"]
Severity = Literal["blocker", "high", "medium", "low"]
DuplicateStatus = Literal["duplicate", "not-found", "unknown"]


@dataclass(frozen=True, kw_only=True)
class Verdict:
    status: Annotated[Status, doc(
        "Overall outcome. approve: the change can merge as is. comment: "
        "remarks the author should read, none of which block merging. "
        "reject-changes: the change should not merge in its current form.")]
    message: Annotated[str, doc(
        "Brief rationale based only on actionable PR findings.")]


@dataclass(frozen=True, kw_only=True)
class Note:
    location: Annotated[str, doc(
        "`path/to/file.ext:start-end`. The location or range of locations "
        "this note refers to.")]
    severity: Annotated[Severity, doc(
        "blocker: likely data loss/corruption, critical security issue, "
        "widespread outage, or a change that fundamentally cannot work. "
        "high: serious correctness, compatibility, concurrency, or "
        "reliability failure in a normal supported scenario. "
        "medium: real defect with narrower conditions or limited blast "
        "radius. "
        "low: minor but concrete defect worth fixing; not a stylistic "
        "preference.")]
    message: Annotated[str, doc(
        "Self-contained defect explanation and correction. For "
        "pre-existing issues, explicitly state that base behavior is "
        "unchanged.")]
    duplicate_status: Annotated[DuplicateStatus, alias("duplicate-status"), doc(
        "Whether this is already tracked elsewhere. duplicate: an existing "
        "report matches it. not-found: you searched and found none. "
        "unknown: you did not or could not search.")]
    duplicate_reference: Annotated[Optional[str], alias("duplicate-reference"), doc(
        "URL or stable identifier, or null.")]


@dataclass(frozen=True, kw_only=True)
class Question:
    location: Annotated[Optional[str], doc(
        "`path/to/file.ext:start-end` or None for a question not tied to "
        "a specific source location.")]
    message: Annotated[str, doc(
        "The unresolved question and why its answer matters.")]


@dataclass(frozen=True, kw_only=True)
class ReviewResult:
    verdict: Annotated[Verdict, doc("The overall review decision.")]
    notes: Annotated[dict[str, Note], doc(
        "Problems introduced or exposed by this change. Keys in the map "
        "are problem-forward kebab-case IDs. Titles and identifiers must "
        "describe the defect, not an imagined solution. Prefer "
        "failed-probe-overwrites-working-credential and avoid "
        "keep-old-credential-after-failed-probe.")]
    pre_existing_issues: Annotated[dict[str, Note], alias("pre-existing-issues"), doc(
        "Problems noticed in surrounding code that this change did not "
        "introduce. Same map structure as notes: keys are problem-forward "
        "kebab-case IDs that name the defect, not a fix.")]
    questions: Annotated[dict[str, Question], doc(
        "Questions for the author, keyed by a problem-forward kebab-case "
        "id. Question IDs are independent of notes and need not match "
        "any of them.")] = field(default_factory=dict)
