# DNS carried over TCP binds nothing

The nft table redirects `udp dport 53` to the interceptor's DNS listener and every TCP
connection to its TCP listener (`crates/outrig/src/network.rs`, `nft_rules`). A container that
resolves over TCP/53 -- a stub resolver falling back after a truncated answer, or one
configured for `use-vc` -- therefore gets answers the interceptor forwards as opaque bytes and
never parses, so no name binding is created for them.

Since 0114 that matters: a hostname `allow` entry grants only against a bound address, so a
container whose resolution went over TCP is not covered by its own allowlist.

The shape is mostly benign today. Under `default = "deny"` the TCP/53 connection to the
resolver is itself denied unless the policy allows it, so the fallback fails and the stub
retries over UDP. Under `default = "allow"` there is no allowlist to fall short of. The gap is
real for `default = "deny"` policies that allow the resolver's address explicitly.

Closing it means parsing DNS-over-TCP in `handle_tcp`: destination port 53, two-byte length
prefix, then the same `dns_response_matches_query` / `dns_bindings` path the UDP listener uses.
The awkward part is that the TCP path bridges bytes rather than owning a request/response
exchange, so it would need to tee both directions through a framing parser instead of
`copy_bidirectional`.

DNS over HTTPS is a separate problem and stays out of reach until the MITM work in
`network-interceptor-mitm.md`.
