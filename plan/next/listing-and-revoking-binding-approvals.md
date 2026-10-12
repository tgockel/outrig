# Approved bindings can be listed and revoked only by hand

## Context

`0003-20` remembers each approval of a repository's binding as a file under
`$XDG_STATE_HOME/outrig/approved-bindings/`, named by a digest of the declaration and the
repository's root and holding both. An operator who wants to see what they have approved, or to
withdraw an approval -- a repository changed hands, a binding turned out to grant more than they
thought -- reads and deletes those files by hand. Nothing says which file is which without
opening it.

## Shape

- `outrig bindings approvals` lists each approval: the repository, the binding's name, the file
  that declared it, and when it was approved.
- `outrig bindings revoke <repo> [<name>]` removes the approvals for a repository, or for one
  binding in it; the next session asks again.
- The store gains no index: each file already holds what was approved, so the listing reads them.

## Acceptance

- After approving two bindings in two repositories, the listing shows four rows with their
  repositories and names, and revoking one makes only that binding ask again.
