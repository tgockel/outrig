# The session renderer lets a rejected call claim a sibling's execution

## Symptom

`scripts/render-session.py` links each `submit_python` call to the execution that ran its source
by matching the source text (`claim(source)` in the turn renderer), and does not ask whether the
call was rejected. A call whose source is `print(42)` with an unknown extra field is refused
before anything is submitted; a valid sibling with the same source runs. The renderer hands the
sibling's execution link to the rejected call and shows the call that ran as having no
submission. Reachable before the rig 0.45 upgrade as after it; found by its review.

## Correction

Pair each call with its tool result first, and let only a call whose result is not a rejection
consume an execution match; a rejected call is shown with its result and no link. Fixture: two
calls with one source, the first rejected, in `scripts/render-session-test.py`.
