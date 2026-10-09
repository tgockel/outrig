# A binding process counts neither its connections nor its threads

## Context

`0003-16`'s binding program starts a connection, and a serving thread for it, for every
connection id a kernel presents, and keeps both until that connection closes. The interpreter
opens one connection per kernel and binding, and a new one each time the old one was closed by a
refused frame. Nothing on the host side bounds how many a kernel may open: a kernel that sends
frames for a fresh id each time -- by hand, since the shim never does -- accumulates a thread and
a connection per id in the binding process, which runs on the host outside the container's
cgroup and the interpreter's memory ceiling.

`0003-17`'s pool bounds what the *interpreter* opens, at four per kernel and binding. That is a
bound the container side applies to itself, which is not the kind the binding can rely on.

## Shape

- The binding refuses to start a connection past a per-agent count, with a close notice naming
  the limit, and the limit is read from the owner at start rather than compiled in.
- Closed ids are forgotten once the owner says the kernel is gone (`0003-26`'s release), so a
  long session with many children does not keep a set of every id it ever saw.

## Acceptance

- Frames for ids past the limit, from one agent, are answered with a close notice and start no
  thread; the binding's thread count stays at the limit plus its own.
- A kernel's ordinary use -- the pool's four, reopened after refusals -- stays under the limit.
