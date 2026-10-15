# An object's own code a hosted call runs beyond boxing ignores interrupts

## Context

On the primary, `_on_sigint` declines inside a hosted call's own work, short of the frames in
`_INTERRUPTIBLE_CODE`: RPyC's request handlers, beneath which a callback's code runs, and the
boxing of an argument or a callback's result, `_box` and RPyC's `get_id_pack` (`0003-23`). An
exception from those leaves the connection as it was. Elsewhere a call still runs an object's own
code beneath a frame the rule declines. A hook there that never returns ignores every interrupt,
and the call never reaches its next check. Decoding a host exception, which runs the module's
code, the class's `__new__`, a property's setter and an `__init_subclass__`, was the first, and
`0003-23` took it, in `vinegar.load` and `_get_exception_class`; boxing the exception a callback
raised, which reads its public properties, in `vinegar.dump`, was the second. These remain:

- **The host reading an attribute of a callable the container passed.** RPyC's `_check_attr`
  calls `hasattr(obj, "exposed_" + name)`, which runs the object's `__getattr__`. `_access_attr`
  then reads the attribute, which can be a property. Both run beneath a request handler, and its
  exception is the answer.
- **`HANDLE_INSPECT`.** `get_methods` reads the class's attributes.

## Options

- Add `_access_attr`, `_check_attr` and `get_methods` to `_INTERRUPTIBLE_CODE`. The reason for
  each is above.

## Evaluation

One test per path, as `an_interrupt_raises_in_an_objects_own_code_as_a_call_boxes_it` does for
boxing: a hook that never returns, an interrupt, and the connection serving the next call.
