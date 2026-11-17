# Path-based discard leaves a relative registration dangling

Flagged in review of #910; pre-existing and separate from root containment.

`SessionStore::remove_by_path` canonicalizes the session directory, deletes it,
then compares that canonical path with `self.root.join(target)` while sweeping
registrations. A supported relative link such as `<root>/<id> -> ../explicit`
is compared lexically, so its `..` prevents a match and the registration stays
behind. Current creation writes absolute links, limiting exposure to older or
manually written relative registrations.

Resolve and remember matching registration targets before deleting the directory,
then unlink those entries only after successful removal. Test relative targets
and roots, a symlink argument to discard, preservation of unrelated registrations,
and leaving all registrations intact when removal is refused or fails.
