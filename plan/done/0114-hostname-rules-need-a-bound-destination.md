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

**Live interceptor** -- against a real attachment, gated with the other e2e work (0129):

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
is exercised by 0129.

## See also

- `crates/outrig/src/network.rs` -- `matches` (121), `decide` (95), `DnsCache` (55),
  `cached_host` (1223), `cache_dns_response` (1227), `dns_answer_ips` (1247), `forward_dns`
  (844), `dns_loop` (805), `handle_tcp` (624), `SNIFF_TIMEOUT` (50).
- `plan/next/network-interceptor-mitm.md` -- the adjacent entry this task is *not*.
- `plan/done/0060-network-interceptor-enforcement.md` -- where host:port enforcement was built.

## Decisions

- **Fork 1 -- an unbound hostname rule does not match.** A hostname `allow` entry with no
  resolved binding falls through: evaluation continues with the remaining entries and then
  `default`. Denying outright was rejected because it turns an allow entry into a deny and so
  inverts what `default = "allow"` means; resolving rule hostnames in the interceptor was
  rejected because it puts a host-side lookup on the policy path and lets a hostile authority
  choose which addresses an allow entry covers. Under `default = "deny"` the practical effect
  is that name resolution has to go through the interceptor, which it does by construction --
  `/etc/resolv.conf` is rewritten and nft redirects UDP/53.

- **Fork 2 -- late identity is re-evaluated, not documented away.** `bridge` re-runs `decide`
  on the client's first bytes whenever the sniff window expired without an assertion, and tears
  the connection down with a deny record before any of those bytes reach upstream. Rejecting
  the `default = "allow"` plus hostname-deny configuration shape at validation time was the
  alternative; it was rejected because that shape is legal today and useful, and because the
  window is closed properly rather than declared unreachable. The cost is that the un-sniffed
  path uses a manual split bridge instead of `copy_bidirectional`.

- **Fork 3 -- address-shaped globs keep the destination-address path, and *only* that path.**
  A glob with no ASCII letter in it (`*`, `10.0.*`) still matches the destination address's
  text; a glob with a letter needs resolved identity. An address glob is deliberately not
  matched against resolved names either: `10.0.attacker.example` is a name anyone can register,
  and letting it satisfy `allow = ["10.0.*"]` would grant an arbitrary destination. The base
  code did match the cached hostname there, so this narrows an existing rule rather than
  preserving it. `allow = ["*"]` and `deny = ["*:22"]` are documented examples
  and both keep working. The distinction is drawn by `parse_network_host_pattern`, which owns
  the pattern grammar: it now returns `NetworkHostPattern::AddressGlob` alongside `HostGlob`,
  so the interceptor matches on a variant rather than re-scanning the glob for letters. Keeping
  the classification next to the parser means a future change to the glob charset cannot
  silently change what counts as address-shaped.

- **Fork 4 -- stated as a breaking change** in `doc/reference/config.md`, since an existing
  allowlist is narrower under the new rule.

- **Trusted identity and client assertion are separate types.** `Sniff` is gone. `ResolvedNames`
  carries what the attachment's DNS listener validated; `ClientAssertion` carries what the
  client claimed. `CompiledNetworkEntry::matches` takes `claimed: Option<&str>`, and `decide`
  passes `asserted.asserted_host()` when walking the deny list and `None` when walking the
  allow list. That puts the whole property on one readable line in `decide` rather than behind
  a discriminant an entry could misread. `Ip` and `Cidr` allow entries lost their
  asserted-address disjunct for the same reason the hostname one went.

- **The service label moved off `ClientAssertion`.** It is derived from which parser matched,
  and the ssh case is settled by the *server's* banner -- so a field on the untrusted-input
  type would have had a standing exemption from being untrusted input. `ConnOutcome` and
  `AuditEvent` carry it instead, leaving `ClientAssertion` to hold only what the client said.

- **`NameBindings` replaces `DnsCache`.** Keyed address -> name -> expiry, so one address holds
  several names; TTLs are clamped to `[30s, 1h]` (a TTL-0 answer must still serve the
  connection that prompted it, and a generous TTL must not pin a name for the session); the map
  is capped at 4096 addresses and evicts the soonest-expiring entry. It is constructed in
  `attach`, and `NetworkInterceptor` no longer has a field for it at all -- the absence of a
  session-global map is the compile-time half of the per-attachment guarantee.

- **A decoded name has to be a name that could have been written.** A wire label is a counted
  byte string and may legally contain a `.`, so `read_dns_name` validated nothing while
  producing the one string able to grant a hostname rule. The wire labels
  `["allowed.evil", "attacker", "example"]` -- a name genuinely delegated to whoever runs
  `attacker.example` -- decoded to `allowed.evil.attacker.example` and satisfied
  `allow = ["allowed.*"]`, binding any address that authority chose, with no client assertion
  anywhere in it. Labels are now rejected unless they are 1-63 bytes of letters, digits, `-`,
  or `_`; an unrepresentable name parses to nothing and therefore binds nothing. The length
  half matters independently: a length byte with reserved high bits can claim more than 63.

  Framing and meaning are decided separately, by `skip_dns_name` and `read_dns_name`
  respectively. Walking the answer section with the strict decoder made one record the policy
  cannot name end the walk, so a resolver that put such a record first would hide the address
  record after it and turn a hostname allow into a denial. A structurally malformed packet
  still stops the walk; a merely unnameable owner only makes its own record ineligible.

- **Addresses bind under the queried name only.** The CNAME chain decides *which* answer
  records belong to the query; the name recorded is always the queried one. Binding chain
  members would let an authority for `attacker.example` answer
  `attacker.example CNAME allowed.example` plus `allowed.example A <its own address>` and mint
  a binding for a name it does not own.

- **Truncated and error responses are forwarded but bind nothing.** Retrying over TCP was out of
  scope; the container's stub resolver retries on its own, and a `TC=1` answer authorizing
  nothing is the safe reading. The same holds for a query the interceptor cannot parse at all:
  it is forwarded and its reply passed straight back, binding nothing. The receive loop has to
  accept that reply explicitly, because a loop whose acceptance test can never pass would hold
  the attachment's only DNS listener for the full timeout per resolver and stall every query
  behind it.

- **Bytes are counted as they move, not when a copy returns.** `try_join!` cancels the sibling
  direction when one fails -- which the late-deny path does deliberately -- and a cancelled
  `tokio::io::copy` takes its running total with it. A connection denied on a late name had
  already delivered the server's greeting to the container, and the record claimed zero
  response bytes, which also mislabeled `conn_state` as `S0`. `write_counting` credits each
  write as it lands rather than each completed `write_all`, so a write that delivers a prefix
  and then fails or is cancelled does not drop up to a chunk from the record.

- **Audit records gained `outrig.host_source`.** `outrig.host` keeps its meaning (the asserted
  name when there is one, else a resolved name) so existing tooling and e2e assertions still
  work; the new field says whether that name was evidence or a claim.

- **The DNS-validation acceptance criteria are met at the unit tier, not the live tier.** The
  task listed "a DNS response from an unexpected source or with a mismatched transaction ID
  binds nothing" under the live interceptor. Driving that through a live attachment would need
  control of the host's resolver path, which the e2e harness does not have.
  `forward_dns_ignores_datagrams_that_do_not_answer_the_query` gets the same evidence with real
  UDP sockets: a fake resolver, a second socket spoofing a response to the interceptor's
  ephemeral port, and a stale transaction id, all discarded in favor of the genuine answer.

- **The resolved set is read after the sniff, not before.** The window is up to
  `SNIFF_TIMEOUT` long, and a lookup the container completes inside it is evidence the
  connection is entitled to have weighed. Reading first could only lose a deny -- a missing
  binding fails a hostname allow closed -- but the base code did refresh after the read, and
  dropping that was an unforced narrowing.

- **One out-of-scope fix rode along.** `check_glob_syntax` in `config/validate.rs` tripped
  `clippy::collapsible_match` on current stable, which made the task's own
  `clippy -D warnings` gate unpassable. It is a one-line rewrite to `ok_or(..)?`.
