# Interceptor detach: restore resolv.conf

`NetworkInterceptor::detach` (0078) tears down one container's sockets and nft table but
leaves `/etc/resolv.conf` pointing at the now-closed loopback DNS listener, so a container
that keeps running after detach has silently dead DNS. Today that state is unreachable:
every planned caller (session teardown, sidecar stop in 0079+) detaches immediately before
stopping the container, and v0 has no `/sidecar stop` surface. The invariant lives only in
`detach`'s doc comment.

If detach-while-running ever becomes a real surface (e.g. a `/sidecar stop` command, or
re-attach after transient detach), make attach/detach a true inverse pair: snapshot the
original `resolv.conf` in the `Attachment` during `attach` and restore it in
`teardown_attachment`, tolerating failure when the container is already dead.
