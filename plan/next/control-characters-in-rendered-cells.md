# A control character in an image-config name breaks the `ls` table's shape

## Symptom

An `image-name` block's key is only a label -- it is held to no grammar, so it can be any
UTF-8 the user wrote, control characters included. `[images."line\nbreak"]` is valid TOML,
decodes to a label holding a line feed, and is stored in `session.json` unchanged. `outrig ls`
writes that label into the IMAGE cell verbatim, so one session renders as two lines and its
EXIT lands on the second:

```
ID                    STARTED              DURATION   IMAGE       EXIT
20260501T134412-3f2a  2023-11-14 22:13:20  25756h59m  line
break  0
```

Observed on `7890d2a` by setting `image_config_name` to `"line\nbreak"` and rendering the
table. A tab, a carriage return, or an ANSI escape introducer does its own version of this:
the cell's written width stops matching the width the column was measured for, and in the
escape's case the label reaches the terminal as a command.

## Scope

Pre-existing and unchanged by #371. That fix moved the measuring and the padding onto one
unit -- terminal display columns -- which lines up every cell whose text occupies the columns
it measures. A control character is the case where no width is the right one, because the
character is not a glyph at all: `unicode-width` reports `None` for a control `char` and
`str::width` counts it as 1, while a terminal acts on it. So this is a rendering question
(what may be written into a cell) rather than a measuring one, which is why it was recorded
rather than folded into that change.

## Suggested fix

Escape control characters in the *displayed* label before both the width measurement and the
padding, leaving the stored name as it is -- the record is history and the config key is the
user's. `char::escape_debug` is the ready-made spelling and renders the LF above as `line\nbreak`
in one cell of the width it measures. The same treatment belongs on any other cell built from
user-authored text; the ID, STARTED and DURATION cells are outrig's own and cannot hold one.

Worth checking the neighbors in the same pass: `outrig logs` pads a filename column
(`cli/logs.rs`), and those names come from the filesystem.

## See also

- #371 -- the width-measurement half of the same table, fixed.
