# The payload's threads get musl's 128 KiB stack

## Problem

The payload's `bin/python3.13` has a `PT_GNU_STACK` header whose memory size is 0. musl reads that
field to decide a new thread's default stack size, so every thread the binary starts gets 128 KiB.
CPython's recursion limits assume far more than that. Deep but legal recursion off the main
thread, such as `json.loads` of a nested document or `repr` of a nested list, overflows the stack
and the process dies with `SIGSEGV` instead of raising `RecursionError`. Measured on the payload:
1 MiB still crashes, and 2 MiB does not.

`0003-02` fixed this for the interpreter only. `interpreter.py` calls
`threading.stack_size(8 << 20)` at boot, so its own threads and the ones the agent starts through
`threading` get the main thread's 8 MiB. Two cases are still exposed:

- **Every other process started from the payload.** `subprocess.run([sys.executable, "x.py"])` in a
  session starts a fresh interpreter, which does not run `interpreter.py`. A thread in it that
  recurses deeply still crashes. The design review measured this: the same script exits 139.
- **Threads not started through `threading`.** A C extension that spawns its own threads would be
  affected, but the payload cannot load third-party extensions, so this is theoretical today.

## Sketch

Set the `PT_GNU_STACK` memory size to 8 MiB in the unpacked binary, next to where `payload.rs`
unpacks the tree. `container/enter/elf.rs` already parses the program headers. That fixes every
process and thread the payload starts, and the `threading.stack_size` call can then go.

The cost is that the file on disk is no longer byte-for-byte what `build.rs` verified. Two ways to
handle that:

- **Patch at unpack time**, after the digest check. Record the patch where the pin is recorded, so
  the check stays meaningful: the file is "the verified archive plus this 8-byte edit".
- **Patch the archive at build time** and embed the patched archive. `build.rs` then verifies,
  patches, and embeds. The unpacked tree matches the embedded bytes exactly.

Either way, the patch must be re-checked when the pin moves: a new release could fix the header,
and then the patch would change nothing.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- "A thread's stack, found in the port".
- `plan/done/phase/0003-python/tasks/0003-02-one-interpreter-many-agent-kernels.md` -- the
  in-process fix and its measurements.
