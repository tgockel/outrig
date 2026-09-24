# A reused passwd name can climb out of `/home`

## Problem

`Container::bootstrap_user` reuses whatever name the image's `/etc/passwd` has at the host uid
(`userdb::lookup_id`), and derives the home path from it with `userdb::home_dir` --
`format!("/home/{name}")`. Nothing checks that the name is a single path component. `lookup_id`
takes the first field verbatim, so it can hold `/` or be `..`. `sanitize_name`, which guards the
host-derived names, keeps `.` too, so `..` survives it as well (though no real host account is
named that).

Since #173, `namespace::make_home` refuses a home whose final component is a file, a FIFO, or a
symlink, by opening it `O_DIRECTORY | O_NOFOLLOW` and `fchown`ing the descriptor. `..` is not a
symlink, so it gets through:

- a name of `..` gives `/home/..`, which opens as `/`, and bootstrap `chown`s the container's
  root directory to the session uid;
- `../etc` does the same to `/etc`;
- `a/b` creates a root-owned `/home/a` on the way to `/home/a/b`.

Each of these then goes out as `HOME` on every exec. The image is inside the trust boundary
(`doc/concepts/mcp-trust-model.md`), and the ids written are the session's own, so this is the
same severity class as #173: an image-staged shape that bootstrap should refuse and doesn't.

## Sketch

Validate the name where it becomes a path. The simplest approach is `home_dir`, or a check just
before `create_home` in `bootstrap_user`: the name must be non-empty, not `.` or `..`, and free
of `/` (and NUL, which `cstring` already rejects). What to do with a name that fails needs
deciding:

- **Fail the bootstrap** and name the entry. This is simplest and matches how #173 treats a bad
  home.
- **Skip the entry and append a fresh one** at the same uid. Resolution by uid still finds the
  image's entry first, so `id` keeps printing the bad name, but `HOME` would be sane.

`sanitize_name` should drop `.`-only results to the fallback either way, so the two paths share
one rule.

## See also

- #173 and `crates/outrig/src/container/namespace.rs` (`make_home`): the final-component check
  this complements.
