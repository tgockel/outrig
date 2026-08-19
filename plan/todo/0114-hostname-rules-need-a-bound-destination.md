# 0114 -- A hostname rule grants only when the destination is bound to it

## Context

`[network] mode = "filter"` is the enforcement half of the interceptor arc: 0059 gave outrig
audited egress, 0060 made host:port policy enforceable, and `SECURITY.md` names failure to
enforce a host:port allow/deny policy as in scope. The policy evaluator does not hold that
property today.

`CompiledNetworkEntry::matches` (`crates/outrig/src/network.rs:121-140`) permits a hostname
pattern when **either** side matches:

```rust
NetworkHostPattern::HostGlob(pattern) => {
    glob_matches(pattern, &dst.ip().to_string())
        || sniff
            .host
            .as_deref()
            .is_some_and(|host| glob_matches(pattern, &host.to_ascii_lowercase()))
}
```

The right-hand disjunct is client-supplied. `sniff` is filled by `sniff_client_bytes`
(`network.rs:1103`) from the container's own first bytes -- a TLS `ClientHello` SNI or an HTTP
`Host:` header. Nothing resolves the accepted hostname and proves the destination belongs to it,
and `handle_tcp` then dials the unchanged `SO_ORIGINAL_DST`.

So with `mode = "filter"`, `default = "deny"`, `allow = ["allowed.example:443"]`, a container
connects to `203.0.113.66:443`, sends SNI `allowed.example`, and is allowed through to
`203.0.113.66`. A cleartext forged `Host: allowed.example` does the same. The audit called
`decide` directly with that destination and that sniff and got `Allow` where it expected `Deny`
-- deterministically, with no race.

**This is not `plan/next/network-interceptor-mitm.md`.** That entry is opt-in TLS termination so
policy can see URLs and bodies. This task needs no TLS interception at all: it is the much
smaller claim that a name the client asserts cannot, by itself, authorize an address outrig never
resolved to that name.

## The evidence outrig has is not yet trustworthy

The obvious fix -- "consult the DNS cache instead of the sniff" -- does not work against the
cache as it exists. Four separate problems, each of which has to be named in the design or the
fix is theater:

1. **The cache is session-global.** `type DnsCache = Arc<Mutex<BTreeMap<IpAddr, String>>>`
   (`network.rs:55`), constructed once per `NetworkInterceptor` (`network.rs:263`) and cloned
   into every attachment (`network.rs:314,320`). One container resolving an allowed name grants
   *every other attached container* authority over that IP. Bindings must be scoped to the
   attachment that earned them.
2. **`IpAddr -> String` cannot hold two names.** `cache_dns_response` (`network.rs:1227`) does
   `cache.insert(ip, host)` per answer IP, so the later lookup overwrites the earlier. Shared
   hosting -- two allowed names behind one address, or an allowed and a denied name -- is
   unrepresentable. An acceptance criterion about shared IPs cannot pass against this shape.
3. **Answers are attributed to the query, not to the record.** `cache_dns_response` associates
   the queried host with every A/AAAA address in the answer section, and `dns_answer_ips`
   (`network.rs:1247`) walks the answer RRs without checking owner names or following the CNAME
   chain. A response can therefore bind addresses to a name that no record in it actually owns.
4. **"The interceptor answered it" is not yet evidence.** `forward_dns` (`network.rs:844`) sends
   the query and returns the first datagram that arrives: the source address is discarded
   (`Ok(Ok((n, _)))`), and nothing checks the transaction ID, the echoed question, the QR bit,
   the RCODE, or truncation. Any host that can land a UDP packet on that ephemeral port before
   the real resolver can write the binding.

## Goal

A hostname rule grants only against a destination this attachment has independent, validated
reason to believe is that host. Client-supplied `Host`/SNI may narrow a decision or trigger a
deny; it may never originate an allow, and it may never be mistaken for resolved identity.

## Deliverables

- **Trusted identity and client assertion stop sharing a slot.** `Sniff.host` is currently
  filled from the ClientHello/`Host:` header *and* backfilled from the cache when the sniff is
  empty (`network.rs:644,657`), so by the time `matches` runs the two are indistinguishable.
  Carry them as separate fields on separate data paths, so recombining them is a type error
  rather than a review catch.
- **`matches` splits granting evidence from refining evidence.** The `HostGlob` allow path
  consults resolved identity only. The deny path may keep using the client assertion -- a forged
  name that lands a client in a deny rule costs the client only its own connection.
- **Bindings become per-attachment, multi-name, and TTL-bound.** Scope the map to the
  attachment; key it so one address can carry several names; carry each answer's TTL and honor
  it; cap the map. This replaces `DnsCache` rather than adding a field to it.
- **DNS answers are validated before they bind anything.** Match the response's source address to
  the resolver the query went to, check the transaction ID and the echoed question, require
  `QR=1` and a success RCODE, handle truncation, and attribute addresses by RR owner through the
  CNAME chain instead of to the original query name.
- **The wildcard and direct-IP contract is stated.** `HostGlob("*")` currently matches the
  textual destination IP, so `*:22` and bare-IP rules work today by accident of that path.
  Requiring resolved identity for every host glob changes both. Decide and document; see fork 3.
- **Docs.** `doc/concepts/workspace.md` and `doc/reference/config.md` (a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/reference/config.md` -- edit the target) must say what a
  hostname rule means now. `SECURITY.md` gains the property in words, since it is the file that
  claims the boundary.

## Acceptance

Three tiers, because the audited property spans three layers and a test at the wrong one proves
nothing.

**Pure policy** -- `decide` called directly, no I/O:

- The audit's repro: with `allow = ["allowed.example:443"]` and `default = "deny"`,
  `decide(203.0.113.66:443, ...)` with a client-asserted host of `allowed.example` and no
  resolved identity is `Deny`.
- A destination with validated resolved identity for `allowed.example` is `Allow`.
- Two names on one address: the allowed one grants, the denied one denies, and neither answer
  depends on which lookup happened last.
- A binding past its TTL does not grant. A binding earned by attachment A does not grant for
  attachment B.
- Forged `Host` and forged SNI each get their own case, since they arrive by different parsers.

**DNS validation** -- one case per rule in the deliverables, because a rule with no test is a
rule that will be dropped during implementation:

- A response from an address other than the resolver the query went to binds nothing.
- A mismatched transaction ID binds nothing.
- A response whose echoed question differs from the query binds nothing.
- `QR=0`, a non-success RCODE, and the truncation bit each bind nothing.
- An answer section carrying records for an owner name unrelated to the query binds only what
  the chain actually reaches.
- A valid CNAME chain binds the addresses at its end to the queried name; a broken or looping
  chain binds nothing.
- Per-record TTLs are extracted distinctly -- two records with different TTLs expire at different
  times, which a single per-response TTL would get wrong.

**Sniff timing** -- driven with `tokio::time` paused, against `handle_tcp`'s timed read:

- A `ClientHello` that arrives after `SNIFF_TIMEOUT` (`network.rs:50`) behaves as fork 2 decides.
  This cannot be tested at policy level: lateness exists only in that read, and a policy-level
  test with a late-arriving string is testing nothing.
- The same for a late cleartext `Host:` header. It reaches the same window through a different
  parser, and testing only the TLS side leaves half the bypass unproven.

**Live interceptor** -- against a real attachment, gated with the other e2e work (0128):

- A container that resolves `allowed.example` through the interceptor's own DNS and then connects
  is allowed. This is the test that fails if the fix is "delete the sniff disjunct" and stops.
- A container that connects by IP and forges SNI is denied, and the audit record says why.
- A DNS response from an unexpected source or with a mismatched transaction ID binds nothing.

`crates/outrig/public-api.txt` regenerated if `NetworkSpec` or `NetworkEntry` move.

## Design forks

1. **What a hostname rule does with no binding -- Open, and it is the whole ergonomic cost.**
   Denying is the safe reading and breaks any container that resolves DNS by some path outrig did
   not answer (a baked `/etc/hosts`, a hardcoded address, a resolver the nft rules missed).
   Falling through to `default` is friendlier and, under `default = "allow"`, gives the bypass
   back. A third option is to resolve the name in the interceptor on first use and treat the
   answer as the binding, which costs a lookup per rule per address rather than per connection.
   Whichever is chosen, it must be stated in `doc/`, because it decides what an existing
   allowlist permits.

2. **The late-identity window -- Open, but documentation alone does not close it.** Deciding once
   at `network.rs:662` is what lets a client connect by IP, wait out the sniff window, and only
   then send a name a deny rule covers. Two acceptable answers: re-evaluate when identity arrives
   late and terminate the connection if it now denies, or reject the configuration shape --
   `default = "allow"` combined with hostname deny rules -- at validation time. The earlier
   version of this task allowed "document that this is not a containment boundary" as a third
   answer. It is not acceptable for a release that claims host:port enforcement in `SECURITY.md`;
   a documented hole in the enforcement path is still a hole.

3. **Wildcards and bare IPs -- Open.** Requiring resolved identity for every `HostGlob` makes
   `*:22` stop matching a direct-IP connection, which is a real behavior change for anyone using
   a broad allow. Keeping the destination-IP-text path for patterns that contain no alphabetic
   character preserves it, at the cost of a second matching rule to explain. Test direct-IP
   behavior both before and after an intercepted lookup for the same address, since those are
   different states under any answer.

4. **Whether tightening this is a breaking change -- Recommended: yes, say so.** It narrows what
   an existing allowlist permits. That is precisely the argument for doing it before 0.2.0 rather
   than after: the compatibility cost is paid once, pre-freeze.

## Dependencies

None. Independent of every other queued task, which is why it leads. Its live-interceptor tier
is exercised by 0128.

## See also

- `crates/outrig/src/network.rs` -- `matches` (121), `decide` (95), `DnsCache` (55),
  `cached_host` (1223), `cache_dns_response` (1227), `dns_answer_ips` (1247), `forward_dns`
  (844), `dns_loop` (805), `handle_tcp` (624), `SNIFF_TIMEOUT` (50).
- `plan/next/network-interceptor-mitm.md` -- the adjacent entry this task is *not*.
- `plan/done/0060-network-interceptor-enforcement.md` -- where host:port enforcement was built.
