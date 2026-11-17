# A work child's attempt-limit test races the session's end

`agent::children_tests::the_attempt_limit_rejects_a_work_childs_third_wrong_answer` failed once
in three full runs of `cargo test -p outrig --lib` on `0003-51`'s branch, with `index out of
bounds: the len is 1 but the index is 1` at `children_tests.rs:557`, and passed in isolation and
in the other runs. It reads the child's second model call, `child[1]`, which carries its third
completion's result; under load the session can end -- `tree.end()` right after the primary's
round -- before the child's runner has made that call, so the mock never recorded it.

Nothing in `0003-51` reaches a work child that holds no channel, and the failure does not
reproduce alone. The test, not the behavior, is what to fix: wait for the child's
`model.round.completed` (or `agent.request.settled`) before ending the session, as
`Tree::seen` waits for an event, rather than assuming the primary's round outlasts it.
