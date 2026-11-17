# The primary's external-channel counts come from the interpreter, a holder's from the host

`0003-51` counts an external channel's waiting items two ways. A runner-driven holder's
announcer reads the host's channel table (`Channels::counts_of`, merged into
`children::Inner::counts`); the primary's reads the interpreter's `pending` answer, where each
endpoint's `_counted` leaves a `silent` channel out. The two agree while every push the
interpreter queues is answered, and would drift if `_channel_msg` queued an item and its answer
were lost -- the host table would not count it, the interpreter would.

The host table is already the authority for the close report and the consumed positions
(`0003-53` makes it the registry). Have the primary's announcer merge `counts_of(PRIMARY)` as a
child's does, and `_pending` count only a kernel's own channels; the `silent` rule is then the
host's alone. Found by `0003-51`'s `/simplify` review; deferred as a rework of how
`Agent::build` gives the primary its announcer.
