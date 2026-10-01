# A bounded preview of user input in the announcement

## Shipped

Input reaches the agent as data. A message on the user channel is announced to the model by
channel name and count, in the round's opening line or on the next tool result, and the agent's
code reads it with `await runtime.channels["user"].receive()`, which returns a `Delivery` and
removes the message from the queue (`messages.md`, "The user channel"). The body enters the
model's context when the agent observes it, and not before. So every request, short or long,
costs one receive-and-observe call before the model has read it. The maintainer chose one way to
get messages, accepting that it is slower out of the box by that call.

## Alternative

The announcement carries an optional, bounded preview of the message: the first part of the body,
attributed to its sender, with the body's length and a mark that the preview is a prefix. The
preview consumes nothing -- the message stays queued and `receive()` returns it whole -- and it
never implies that the preview is the message, since a truncated instruction read as complete is
the failure to avoid. It applies to primary-user input only. Generalizing it to every channel
waits for evidence, because a channel between agents can carry data that should not enter the
parent's context by default.

## Evaluation

Instruction-to-acknowledgement latency and duplicate work, against the receive-and-observe
baseline: the time from a user's line to the agent acting on it, and how often an agent repeats
work because it misread or did not read the whole message. Successful queue insertion measures
nothing. The maintainer called the preview a good feature to test.

## When

A 0.3.1 candidate if the measurement favors it. It changes the text of the announcement and no
API, so nothing in 0.3.0 waits on it.
