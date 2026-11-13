# A plugin is on when it applies

## Context

Under #933 a plugin starts only when the config names it, `[plugins.github]`. The user's picture
is that GitHub should just work: in a workspace whose `origin` is a GitHub repository, on a host
that holds a token, the agent could be given the plugin with nothing written.

This was left out of #933 on purpose, as a later addition. A plugin acts on the host with the
user's authority, and one that turns itself on grants that authority in a session where the user
asked for nothing.

## Shape

- **What "applies" means** is the plugin's to say, cheaply and without running the library:
  the workspace is a Git repository with a GitHub remote, and a token can be found.
- **Who allows it.** Off unless the operator says plugins may turn themselves on, in the global
  config only, since a repository's config is content the agent can write.
- **What the user sees.** A session says at start which plugins turned themselves on and why,
  and an event records it, so the grant is never silent.
- **Opting out** of one plugin, by name, where the operator allowed the rest.

## Acceptance

- With the operator's setting on, a session in a GitHub repository gives the agent the `github`
  plugin with no `[plugins.github]` line, and says so at start.
- With the setting off, the default, a plugin starts only when named.
- A repository's config cannot turn the setting on.
