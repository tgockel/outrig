# A dropped handle's notice races the child's reply in a test

`agent::instance_tests::a_dropped_instance_lives_until_it_is_released` fails about one run in
eight, on `version/0.3.x` as it stood at `1aec3b7` as well as on `0003-46`'s branch: "its handle
was collected open" finds no `agent.instance.handle.collected`.

The handle of `d.fizz('x')` is collected at once, and its `__del__` schedules
`_Instance.dropped` on the constructing kernel's loop
(`crates/outrig/src/python/outrig/_agents.py`, `Handle.__del__`). That runs only when the
primary's execution next yields, at `await until(...)`, after it has opened the flag the child
waits for. The child, on its own thread, may answer the request first; `dropped` then finds the
request ended and says nothing, as it should for a handle collected once its request is over.

The behavior is right; the test's order is not. Have the child wait for a second flag the parent
opens after a step that yields, such as `await asyncio.sleep(0)`, so `dropped` has run before
the child can answer.
