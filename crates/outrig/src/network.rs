//! Per-session network audit and filtering plumbing.
//!
//! Interception is intentionally opt-in. One interceptor per session owns the
//! compiled policy and the audit sink; each session container is covered by a
//! per-container attachment. When a container is attached, OutRig installs a
//! small nftables nat table in that container's network namespace and keeps
//! host-side listener sockets in that namespace. The accepted sockets carry
//! the original destination metadata; upstream connections are opened from
//! the host namespace, so OutRig's own traffic is not routed back through the
//! interceptor.
//!
//! Policy evaluation keeps two kinds of evidence apart. The names an
//! attachment's own DNS listener validated for an address (`ResolvedNames`)
//! are the only thing that may *grant* a hostname rule; the name a client
//! writes into a `Host:` header or a TLS `ClientHello` (`ClientAssertion`)
//! may deny, and may never authorize a destination OutRig never resolved to
//! that name.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File as StdFile;
use std::future::Future;
use std::io::{self, Write as _};

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use rand::Rng;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{
    NetworkAction, NetworkEntry, NetworkHostPattern, NetworkPolicy, parse_network_host_pattern,
};
use crate::container::Container;
use crate::error::{
    IoPathExt, NetworkAttachFailure, NetworkTeardownCause, NetworkTeardownFailure, OutrigError,
    Result,
};
use crate::nsfork;
use crate::process::{self, Cmd, Transcript};
use crate::supervise::{Reissue, detach_cleanup_chain};

const NETWORK_LOG: &str = "network.jsonl";

/// Resolver the interceptor requires inside every attached container: DNS to
/// the loopback listener, `ndots:0` so bare names resolve without
/// search-domain expansion. Installed by `podman exec` on running containers
/// ([`install_resolv_conf`]) and baked in via `podman create --dns` for
/// entrypoint-stdio containers, which cannot be exec'd before start.
pub(crate) const INTERCEPT_DNS_NAMESERVER: &str = "127.0.0.1";
pub(crate) const INTERCEPT_DNS_OPTION: &str = "ndots:0";
const SO_ORIGINAL_DST: libc::c_int = 80;
const SNIFF_TIMEOUT: Duration = Duration::from_millis(750);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
/// How much of a client's opening bytes is examined for an asserted name. A
/// `ClientHello` or a request head is far smaller; this is the ceiling.
const SNIFF_BUFFER: usize = 16 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Leading byte [`read_resolv_conf`] answers with when the container has a
/// resolver file; anything else means it has none.
const RESOLV_PRESENT: u8 = b'1';
/// The probe's verdict for a resolver that is a symbolic link to something
/// that does not exist. `[ -e ]` alone reports that as absent, which is the
/// one shape whose restore would destroy the original: installing follows the
/// link and creates its target, while `rm -f` removes the link.
const RESOLV_DANGLING: u8 = b'L';
/// The largest resolver a restore can promise to put back.
///
/// The bytes travel as one `execve` argument, and Linux caps a single argument
/// at `MAX_ARG_STRLEN`, 128 KiB. Arming an undo whose process could never
/// start -- and finding that out only after the resolver had been replaced --
/// is the failure this bound exists to make impossible. A resolver file
/// anywhere near it is not a resolver file.
const MAX_RESOLV_SNAPSHOT: usize = 64 * 1024;
/// How many audit records may be queued ahead of the writer. Bounded so a
/// stalled log applies backpressure to the connections producing records
/// instead of growing, and so a session cannot be made to hold an unbounded
/// number of them by opening connections faster than the disk accepts them.
const AUDIT_QUEUE: usize = 1024;

/// Bounds on how long one validated answer keeps authorizing an address. The
/// floor keeps a TTL-0 answer usable by the connection that prompted it; the
/// ceiling stops a generous TTL from pinning a name for the whole session.
const MIN_BINDING_TTL: Duration = Duration::from_secs(30);
const MAX_BINDING_TTL: Duration = Duration::from_secs(60 * 60);
/// Cap on the distinct addresses one attachment holds bindings for, so a
/// container that resolves in a loop cannot grow the map without bound.
const MAX_BOUND_ADDRESSES: usize = 4096;

const DNS_TYPE_A: u16 = 1;
const DNS_TYPE_CNAME: u16 = 5;
const DNS_TYPE_AAAA: u16 = 28;
/// Longest CNAME chain followed inside one answer section.
const MAX_CNAME_DEPTH: usize = 8;
/// Longest single label a name may carry, per RFC 1035.
const MAX_DNS_LABEL: usize = 63;
/// Chunk size for the bridge's two copy directions.
const COPY_BUFFER: usize = 8 * 1024;

#[derive(Debug, Clone)]
struct PolicyDecision {
    action: NetworkAction,
    rule: String,
}

impl PolicyDecision {
    #[cfg(test)]
    fn allow_default() -> Self {
        Self {
            action: NetworkAction::Allow,
            rule: "default".to_string(),
        }
    }
}

/// The names this attachment's own DNS listener validated for one destination
/// address. Resolved identity is the only evidence that may grant a hostname
/// rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResolvedNames(Vec<String>);

impl ResolvedNames {
    fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }
}

impl<S: Into<String>> FromIterator<S> for ResolvedNames {
    fn from_iter<I: IntoIterator<Item = S>>(names: I) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }
}

/// What a client said about where it wants to go, read out of its own first
/// bytes. A name a client supplies is a request, not evidence: it may refine a
/// decision toward deny -- which costs that client only its own connection --
/// and it may never originate an allow. Names are held lowercased, the form
/// policy globs are compiled in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ClientAssertion {
    http_host: Option<String>,
    sni: Option<String>,
}

impl ClientAssertion {
    fn http(host: String) -> Self {
        Self {
            http_host: Some(host.to_ascii_lowercase()),
            sni: None,
        }
    }

    fn tls(sni: String) -> Self {
        Self {
            http_host: None,
            sni: Some(sni.to_ascii_lowercase()),
        }
    }

    /// The name the client claimed, whichever parser found it.
    fn asserted_host(&self) -> Option<&str> {
        self.http_host.as_deref().or(self.sni.as_deref())
    }

    fn is_empty(&self) -> bool {
        self.asserted_host().is_none()
    }

    /// The Zeek-style service label these bytes identify the connection as.
    fn service(&self) -> &'static str {
        match (&self.http_host, &self.sni) {
            (Some(_), _) => "http",
            (None, Some(_)) => "ssl",
            (None, None) => "-",
        }
    }
}

#[derive(Debug)]
struct CompiledNetworkPolicy {
    default: NetworkAction,
    allow: Vec<CompiledNetworkEntry>,
    deny: Vec<CompiledNetworkEntry>,
}

#[derive(Debug)]
struct CompiledNetworkEntry {
    pattern: NetworkHostPattern,
    port: Option<u16>,
}

impl CompiledNetworkPolicy {
    fn new(policy: NetworkPolicy) -> Result<Self> {
        policy.validate(false).map_err(OutrigError::Configuration)?;
        Ok(Self {
            default: policy.default,
            allow: compile_network_entries(policy.allow)?,
            deny: compile_network_entries(policy.deny)?,
        })
    }

    /// Deny entries are matched against the destination, the names this
    /// attachment resolved for it, *and* the name the client claimed; allow
    /// entries never see the claim. That asymmetry is the whole property: a
    /// client-supplied name may cost a client its own connection, and may
    /// never buy it one.
    fn decide(
        &self,
        dst: SocketAddr,
        resolved: &ResolvedNames,
        asserted: &ClientAssertion,
    ) -> PolicyDecision {
        let dst_text = dst.ip().to_string();
        for (idx, entry) in self.deny.iter().enumerate() {
            if entry.matches(dst, &dst_text, resolved, asserted.asserted_host()) {
                return PolicyDecision {
                    action: NetworkAction::Deny,
                    rule: format!("deny[{idx}]"),
                };
            }
        }
        for (idx, entry) in self.allow.iter().enumerate() {
            if entry.matches(dst, &dst_text, resolved, None) {
                return PolicyDecision {
                    action: NetworkAction::Allow,
                    rule: format!("allow[{idx}]"),
                };
            }
        }
        PolicyDecision {
            action: self.default,
            rule: "default".to_string(),
        }
    }
}

impl CompiledNetworkEntry {
    /// `claimed` is the name the client asserted, or `None` where a claim is
    /// not admissible evidence.
    fn matches(
        &self,
        dst: SocketAddr,
        dst_text: &str,
        resolved: &ResolvedNames,
        claimed: Option<&str>,
    ) -> bool {
        if self.port.is_some_and(|port| port != dst.port()) {
            return false;
        }
        let claimed_ip = claimed.and_then(|host| host.parse::<IpAddr>().ok());
        match &self.pattern {
            NetworkHostPattern::Ip(ip) => *ip == dst.ip() || claimed_ip == Some(*ip),
            NetworkHostPattern::Cidr { base, prefix } => {
                ip_in_cidr(dst.ip(), *base, *prefix)
                    || claimed_ip.is_some_and(|ip| ip_in_cidr(ip, *base, *prefix))
            }
            // Deliberately not matched against `resolved`: a glob with no
            // letter in it describes addresses, and a hostname that happens to
            // match one (`10.0.attacker.example` against `10.0.*`) is a name
            // an attacker can register, not an address it controls.
            NetworkHostPattern::AddressGlob(pattern) => {
                glob_matches(pattern, dst_text)
                    || claimed.is_some_and(|host| glob_matches(pattern, host))
            }
            NetworkHostPattern::HostGlob(pattern) => {
                resolved.iter().any(|name| glob_matches(pattern, name))
                    || claimed.is_some_and(|host| glob_matches(pattern, host))
            }
        }
    }
}

fn compile_network_entries(entries: Vec<NetworkEntry>) -> Result<Vec<CompiledNetworkEntry>> {
    entries
        .into_iter()
        .map(|entry| {
            Ok(CompiledNetworkEntry {
                pattern: parse_network_host_pattern(&entry.host)
                    .map_err(OutrigError::Configuration)?,
                port: entry.port,
            })
        })
        .collect()
}

fn ip_in_cidr(ip: IpAddr, base: IpAddr, prefix: u8) -> bool {
    match (ip, base) {
        (IpAddr::V4(ip), IpAddr::V4(base)) => {
            let ip = u32::from(ip);
            let base = u32::from(base);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (ip & mask) == (base & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(base)) => {
            let ip = u128::from(ip);
            let base = u128::from(base);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            (ip & mask) == (base & mask)
        }
        _ => false,
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let mut rest = value;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            first = false;
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(stripped) = rest.strip_prefix(part) else {
                return false;
            };
            rest = stripped;
        } else {
            let Some(idx) = rest.find(part) else {
                return false;
            };
            rest = &rest[idx + part.len()..];
        }
        first = false;
    }
    pattern.ends_with('*') || rest.is_empty()
}

/// The names one attachment's DNS listener validated for each destination
/// address, each held until the answering record's TTL runs out. Scoped to the
/// attachment: a lookup one container made never authorizes another's traffic.
#[derive(Debug, Default)]
struct NameBindings {
    by_ip: BTreeMap<IpAddr, BTreeMap<String, Instant>>,
}

type Bindings = Arc<Mutex<NameBindings>>;

impl NameBindings {
    /// Records that `ip` answered for `name`. One address can hold several
    /// names, so an allowed and a denied name behind one address of shared
    /// hosting both survive, whichever was looked up last.
    fn bind(&mut self, ip: IpAddr, name: &str, ttl: Duration, now: Instant) {
        let expires = now + ttl.clamp(MIN_BINDING_TTL, MAX_BINDING_TTL);
        if !self.by_ip.contains_key(&ip) && self.by_ip.len() >= MAX_BOUND_ADDRESSES {
            self.make_room(now);
        }
        let names = self.by_ip.entry(ip).or_default();
        // Probe before inserting: a container re-resolving a name it already
        // holds is the common case, and `entry` would allocate the key for it.
        match names.get_mut(name) {
            Some(slot) => *slot = (*slot).max(expires),
            None => {
                names.insert(name.to_ascii_lowercase(), expires);
            }
        }
    }

    fn names(&self, ip: IpAddr, now: Instant) -> ResolvedNames {
        self.by_ip
            .get(&ip)
            .into_iter()
            .flatten()
            .filter(|(_, expires)| **expires > now)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    fn purge_expired(&mut self, now: Instant) {
        self.by_ip.retain(|_, names| {
            names.retain(|_, expires| *expires > now);
            !names.is_empty()
        });
    }

    /// Purges what has expired, and if that was not enough, drops the address
    /// whose last name expires soonest.
    fn make_room(&mut self, now: Instant) {
        self.purge_expired(now);
        if self.by_ip.len() < MAX_BOUND_ADDRESSES {
            return;
        }
        let victim = self
            .by_ip
            .iter()
            .min_by_key(|(_, names)| names.values().max().copied())
            .map(|(ip, _)| *ip);
        if let Some(victim) = victim {
            self.by_ip.remove(&victim);
        }
    }
}

fn resolved_names(bindings: &Bindings, ip: IpAddr) -> ResolvedNames {
    bindings
        .lock()
        .map(|bindings| bindings.names(ip, Instant::now()))
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct NetworkInterceptor {
    cancel: CancellationToken,
    policy: Arc<CompiledNetworkPolicy>,
    audit: AuditSink,
    /// Kept rather than a table name, because the name is per attach: see
    /// [`nft_table_name`].
    session_id: String,
    /// Handed out once per attach, so an attachment's audit losses are filed
    /// under something a later attachment of the same name cannot be.
    generation: u64,
    attachments: BTreeMap<String, Attachment>,
}

/// Per-container interception state: the container's loops, the undo log for
/// everything [`NetworkInterceptor::attach`] changed about it, and the
/// transcript that undo is logged to. Sockets live inside the container's
/// namespaces, so every attachment owns its own listeners and loops.
#[derive(Debug)]
struct Attachment {
    cancel: CancellationToken,
    /// The accept and DNS loops. A `JoinSet` rather than handles because
    /// dropping one aborts what it holds: an interceptor that is dropped
    /// rather than shut down leaves nothing behind still moving bytes.
    tasks: JoinSet<()>,
    rollback: Rollback,
    transcript: Option<Transcript>,
    /// This container's handle on the session's audit sink, kept so teardown
    /// can collect the records its connections could not write.
    audit: AuditSink,
    /// Answers once no connection task is still running.
    ///
    /// The accept loop normally drains its own connections before it returns,
    /// so joining it is enough -- but a loop that outlasts the grace is
    /// *aborted*, and that drops the set it kept them in, which requests their
    /// abort without awaiting it. Every connection holds a clone of the sender
    /// this receives from and never sends on it, so this comes back when the
    /// last one is gone, whichever way it went.
    idle: mpsc::Receiver<()>,
}

impl NetworkInterceptor {
    pub async fn start(container: &Container, log_dir: &Path, session_id: &str) -> Result<Self> {
        Self::start_with_policy(container, log_dir, session_id, NetworkPolicy::allow_all()).await
    }

    pub async fn start_with_policy(
        container: &Container,
        log_dir: &Path,
        session_id: &str,
        policy: NetworkPolicy,
    ) -> Result<Self> {
        let mut interceptor = Self::new(log_dir, session_id, policy).await?;
        interceptor.attach(container).await?;
        Ok(interceptor)
    }

    /// Session-level construction: validates host tooling, compiles the
    /// policy, and opens the audit sink. Containers are covered only once
    /// [`attach`](Self::attach)ed.
    pub async fn new(log_dir: &Path, session_id: &str, policy: NetworkPolicy) -> Result<Self> {
        require_tool("nft")?;
        require_tool("nsenter")?;
        let policy = Arc::new(CompiledNetworkPolicy::new(policy)?);

        tokio::fs::create_dir_all(log_dir)
            .await
            .path_ctx("create directory", log_dir)?;
        let audit = AuditSink::open(log_dir.join(NETWORK_LOG), session_id.to_string()).await?;

        Ok(Self {
            cancel: CancellationToken::new(),
            policy,
            audit,
            session_id: session_id.to_string(),
            generation: 0,
            attachments: BTreeMap::new(),
        })
    }

    /// Attaches `container`: binds listener sockets inside its user/net
    /// namespaces, points its resolver at the DNS listener, applies the nft
    /// redirect table in its netns (the session-derived table name cannot
    /// collide across containers because each netns has its own table
    /// namespace), and spawns its accept loops. Works mid-session; audit
    /// records from this container are stamped with its name.
    ///
    /// The name bindings its DNS listener earns are created here and shared
    /// with nothing else, so one container resolving an allowed name grants no
    /// authority over that address to any other attachment.
    ///
    /// Also works on a container in the created+initialized state
    /// (`Container::create_initialized`), whose entrypoint has not yet
    /// executed -- `podman init` materializes the PID and namespaces this
    /// needs. Attaching before `podman start` is what closes the
    /// entrypoint-stdio race: policy is live in the netns before the
    /// entrypoint's first packet.
    pub async fn attach(&mut self, container: &Container) -> Result<()> {
        let name = container.name();
        if self.attachments.contains_key(name) {
            return Err(OutrigError::Configuration(format!(
                "container {name:?} is already attached to the network interceptor"
            )));
        }

        let pid = container.pid().await?;
        let sockets = bind_interceptor_sockets(pid)?;
        let tcp_port = sockets.tcp.local_addr()?.port();
        let dns_port = sockets.dns.local_addr()?.port();

        let target = Target::for_attach(name, pid, &self.session_id, container.dns_preconfigured());
        let transcript = container.transcript();
        let run = |cmd: Cmd| run_step(cmd, transcript.clone());

        // The undo log is this frame's, so a caller that drops this future
        // while a command is in flight drops it too, and its destructor puts
        // the container back without needing a runtime to do it.
        // Before the first mutation, so a namespace that cannot be identified
        // is a refusal to start rather than an undo that cannot be trusted.
        let mut rollback = Rollback::new(pid)?;
        if let Err(e) = install_interception(&run, &mut rollback, &target, tcp_port, dns_port).await
        {
            let residue = rollback.undo_now(&run).await;
            if residue.is_empty() {
                // Undone completely: the container is as this found it, so the
                // failure that stopped the attach is the whole story and the
                // call can simply be tried again.
                return Err(e);
            }
            // Otherwise the caller is owed more than the cause. Whatever could
            // not be undone is still armed, so `rollback` dropping on the way
            // out of this frame hands it to `supervise`.
            return Err(OutrigError::NetworkAttachNotUndone(Box::new(
                NetworkAttachFailure {
                    container: name.to_string(),
                    source: Box::new(e),
                    residue,
                },
            )));
        }

        // One generation per attach, never reused. Audit losses are filed
        // under it rather than under the container's name, which a later
        // attachment can have again -- so a failure arriving after this
        // attachment's drain gave up cannot be read as the next one's.
        self.generation += 1;
        let generation = self.generation;
        let bindings: Bindings = Arc::new(Mutex::new(NameBindings::default()));
        let cancel = self.cancel.child_token();
        let mut tasks = JoinSet::new();
        // Cloned into every connection and never sent on, so the receiver
        // answers exactly when the last one has gone. One is enough: nothing
        // is ever queued on it.
        let (live, idle) = mpsc::channel(1);
        tasks.spawn(tcp_accept_loop(
            sockets.tcp,
            self.audit.for_attachment(name, generation),
            bindings.clone(),
            self.policy.clone(),
            cancel.clone(),
            live,
        ));
        tasks.spawn(dns_loop(
            sockets.dns,
            host_resolvers(),
            bindings,
            cancel.clone(),
        ));

        self.attachments.insert(
            name.to_string(),
            Attachment {
                idle,
                cancel,
                tasks,
                rollback,
                transcript,
                audit: self.audit.for_attachment(name, generation),
            },
        );
        Ok(())
    }

    /// Detaches one container, undoing exactly what [`attach`](Self::attach)
    /// did to it: its connections are ended, its `/etc/resolv.conf` is put
    /// back to the bytes attach found there, and its nft table is deleted.
    /// Other attachments are undisturbed.
    ///
    /// Takes the container name rather than a [`Container`] so an
    /// already-dead container can still be detached; that case is a success,
    /// since a container that has exited took both the namespace holding the
    /// table and the resolver worth restoring with it.
    ///
    /// Every obligation is attempted, and the ones that failed are reported
    /// together as [`OutrigError::NetworkTeardown`].
    pub async fn detach(&mut self, container: &str) -> Result<()> {
        let attachment = self.attachments.remove(container).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container {container:?} is not attached to the network interceptor"
            ))
        })?;
        teardown_result(teardown_attachment(container, attachment).await)
    }

    /// Detaches every attachment, reporting what none of them could
    /// discharge. One container's failure does not skip another's teardown.
    pub async fn shutdown(mut self) -> Result<()> {
        self.cancel.cancel();
        // Attachments live in disjoint namespaces, so their grace periods,
        // resolver restores and nft deletes can overlap.
        let failures =
            futures_util::future::join_all(std::mem::take(&mut self.attachments).into_iter().map(
                |(name, attachment)| async move { teardown_attachment(&name, attachment).await },
            ))
            .await;
        let mut causes: Vec<NetworkTeardownCause> = failures.into_iter().flatten().collect();
        // Quiesced before the sweep, not after. An attachment whose drain
        // timed out left the writer still holding its records, and a failure
        // landing after the sweep would go into a map whose only collector had
        // already run. Dropping the last sender ends the writer once it has
        // worked through what it has; joining it is what makes "everything it
        // was ever going to record is recorded" true.
        if let Some(writer) = self.audit.close().await {
            causes.push(NetworkTeardownCause {
                container: String::new(),
                source: Box::new(writer),
            });
        }
        // The sweep. Any attachment whose drain timed out left its losses
        // behind rather than reporting an account it knew was short; this is
        // the last thing that outlives them, so it collects whatever the
        // writer ended up recording.
        causes.extend(self.audit.take_every_unwritten().into_iter().map(|source| {
            NetworkTeardownCause {
                container: String::new(),
                source: Box::new(source),
            }
        }));
        teardown_result(causes)
    }
}

impl Drop for NetworkInterceptor {
    fn drop(&mut self) {
        // The attachments go with `self`: each one's `JoinSet` aborts its
        // tasks and each one's `Rollback` detaches its undo commands, which
        // is everything a destructor can do about either.
        self.cancel.cancel();
    }
}

/// Ends one attachment and undoes everything [`NetworkInterceptor::attach`]
/// did to its container, returning one rendering per obligation it could not
/// discharge.
async fn teardown_attachment(name: &str, attachment: Attachment) -> Vec<NetworkTeardownCause> {
    let Attachment {
        cancel,
        mut idle,
        mut tasks,
        mut rollback,
        transcript,
        audit,
    } = attachment;
    cancel.cancel();
    let mut failures = stop_tasks(&mut tasks).await;
    // The accept loop drains its own connections before it returns, so joining
    // it is normally the whole story -- but a loop that outlasted the grace
    // was *aborted*, and that drops the set holding them, which asks for their
    // abort without waiting for it. Waiting here is what makes "nothing this
    // attachment started is still running" true on that path too, and it has
    // to happen before the audit drain below, or a connection could still
    // queue a record behind the marker that drain sends.
    if tokio::time::timeout(SHUTDOWN_GRACE, idle.recv())
        .await
        .is_err()
    {
        // Its own variant, not the one `stop_tasks` uses: that says the loops
        // had to be aborted, this says a connection outlived the abort, and
        // the second is the one worth acting on.
        failures.push(OutrigError::NetworkConnectionsUnfinished {
            grace: SHUTDOWN_GRACE,
        });
    }
    // After the join, and then drained. Joining makes "every record this
    // attachment owed is queued" true; the drain makes it "written". Without
    // the second half a `detach` could return while the writer still had the
    // last record in hand, which is the claim this whole path exists to
    // support. A record promised and not written is a teardown that did not
    // complete, not a log line.
    // A drain that gave up leaves the writer still holding records this
    // attachment queued, so what it reports now is not the whole account. The
    // generation stays registered in that case: a failure arriving afterwards
    // lands in a slot `shutdown` sweeps rather than one nobody will ever read.
    let drained = audit.drain().await;
    if let Some(e) = drained.as_ref().err() {
        failures.push(OutrigError::Configuration(e.to_string()));
    }
    if drained.is_ok() {
        failures.extend(audit.take_unwritten());
    }
    let run = |cmd: Cmd| run_step(cmd, transcript.clone());
    failures.extend(rollback.undo_now(&run).await);
    failures
        .into_iter()
        .map(|source| NetworkTeardownCause {
            container: name.to_string(),
            source: Box::new(source),
        })
        .collect()
}

/// Ends `tasks`, which have already been cancelled: each gets
/// [`SHUTDOWN_GRACE`] to stop on its own, and whatever is left is aborted and
/// then joined. The join is the point -- a handle that is merely dropped
/// after a timeout *detaches* its task, leaving it running past the detach
/// that was supposed to have ended it.
///
/// This covers these tasks and not what they spawned. The accept loop keeps
/// its connections in a set of its own and drains it before returning, so on
/// every path but this one they are joined with it -- but aborting the loop
/// drops that set, which requests their abort and does not wait. The caller
/// waits them out separately; see `Attachment::idle`.
async fn stop_tasks(tasks: &mut JoinSet<()>) -> Vec<OutrigError> {
    let mut failures = Vec::new();
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while let Some(joined) = tasks.join_next().await {
            // Nothing has been aborted yet, so the only way a join fails
            // inside the grace is a task that panicked.
            if let Err(source) = joined {
                failures.push(OutrigError::NetworkTaskPanicked { source });
            }
        }
    })
    .await;
    if drained.is_err() {
        failures.push(OutrigError::NetworkTasksAborted {
            grace: SHUTDOWN_GRACE,
        });
        tasks.shutdown().await;
    }
    failures
}

/// One error carrying every obligation a teardown could not discharge, or
/// `Ok(())` when it discharged them all.
fn teardown_result(causes: Vec<NetworkTeardownCause>) -> Result<()> {
    if causes.is_empty() {
        Ok(())
    } else {
        Err(OutrigError::NetworkTeardown(Box::new(
            NetworkTeardownFailure { causes },
        )))
    }
}

#[derive(Debug)]
struct InterceptorSockets {
    tcp: TcpListener,
    dns: UdpSocket,
}

/// Everything the commands that install and remove interception are keyed to.
#[derive(Debug, Clone)]
struct Target {
    name: String,
    pid: u32,
    table: String,
    /// What the installed resolver is stamped with. Separate from `table` on
    /// purpose: see [`resolv_marker`].
    marker: String,
    /// A container whose resolver was baked in by `podman create --dns` has
    /// no resolv.conf to snapshot and none to put back.
    dns_preconfigured: bool,
}

impl Target {
    /// Everything one attach's commands are keyed to.
    ///
    /// The table name and the resolver marker are drawn independently, and
    /// that is the point rather than an accident: the resolver is written
    /// before the table exists, so whatever is in it is readable inside the
    /// container first. A marker that *was* the table name would tell an actor
    /// in that namespace which table to create ahead of outrig, making its
    /// `create table` fail and its rollback delete a table it never made.
    fn for_attach(name: &str, pid: u32, session_id: &str, dns_preconfigured: bool) -> Self {
        Self {
            name: name.to_string(),
            pid,
            table: nft_table_name(session_id),
            marker: attach_nonce(),
            dns_preconfigured,
        }
    }
}

/// The undo log for one container's interception.
///
/// Every mutation is armed here *before* it is made, so no instant exists at
/// which the container is changed with nothing responsible for changing it
/// back. The ordinary paths discharge it awaited and report what failed
/// ([`Self::undo_now`]); a destructor cannot await, so `Drop` hands the same
/// commands to [`crate::supervise`], which runs them as detached processes
/// that survive this runtime being torn down.
#[derive(Debug)]
struct Rollback {
    /// The container's init pid: how the commands get to the namespace, and
    /// on its own no evidence that the namespace is still the right one.
    pid: u32,
    /// Which namespace instance they were armed against, as its nsfs device
    /// and inode. Compared before any undo is issued, so a pid that has been
    /// handed to another container does not carry this attach's undos into it.
    netns: (u64, u64),
    /// Undo commands in the order their mutations were made, discharged in
    /// reverse.
    undo: Vec<Cmd>,
    /// Mutations this made and then could neither undo nor keep owed: they are
    /// reported with whatever the caller is told, and nothing reissues them.
    ///
    /// The one shape that lands here is a command that is safe to run *now*
    /// and not later. Keeping it armed would mean issuing it at some distant
    /// teardown, against whatever answers to its target by then; dropping it
    /// silently would leave the caller thinking the machine is as it was. So
    /// it is run once, here, and what it could not do is said out loud.
    left_behind: Vec<OutrigError>,
}

impl Rollback {
    /// Records which namespace these undos are for, before anything is armed.
    ///
    /// Fails when it cannot be identified, which is the fail-closed direction:
    /// an undo that cannot tell its namespace from a stranger's is one that
    /// must not run, and this is asked before the first mutation, so a refusal
    /// here means there is nothing to undo yet.
    fn new(pid: u32) -> Result<Self> {
        Ok(Self {
            netns: match namespace_id(pid) {
                NamespaceAnswer::Is(netns) => netns,
                NamespaceAnswer::Gone | NamespaceAnswer::Unknown => {
                    return Err(OutrigError::Configuration(format!(
                        "the network namespace of the container at pid {pid} could not \
                         be identified, so nothing could tell it later from another \
                         container's"
                    )));
                }
            },
            pid,
            undo: Vec::new(),
            left_behind: Vec::new(),
        })
    }

    /// The inode of the namespace these undos were armed against, for an undo
    /// that carries the check into the namespace with it.
    fn netns_inode(&self) -> u64 {
        self.netns.1
    }

    /// Whether the namespace the undos name is still the one they were armed
    /// against.
    ///
    /// The pid is how `nsenter` gets there and pids come round again, so
    /// "something is at that pid" is not the question -- "is it still the same
    /// namespace" is. A container that exited took its namespace with it, and
    /// with it the nft table and any point in restoring its resolv.conf, so
    /// there is nothing left to undo and nothing to report.
    fn same_namespace(&self) -> NamespaceAnswer {
        match namespace_id(self.pid) {
            NamespaceAnswer::Is(now) if now == self.netns => NamespaceAnswer::Is(now),
            // A different namespace is as good as gone for these undos: what
            // they were armed against is not there any more.
            NamespaceAnswer::Is(_) | NamespaceAnswer::Gone => NamespaceAnswer::Gone,
            NamespaceAnswer::Unknown => NamespaceAnswer::Unknown,
        }
    }

    /// Take responsibility for `undo`, before the mutation it reverses runs.
    /// Every undo here is idempotent, so arming one that turns out never to
    /// have been needed costs nothing -- while arming afterwards would lose
    /// whichever mutation a cancellation landed in the middle of.
    ///
    /// Returns where it was armed, for [`narrow`](Self::narrow).
    fn arm(&mut self, undo: Cmd) -> usize {
        self.undo.push(undo);
        self.undo.len() - 1
    }

    /// Take an armed undo back, so nothing issues it later.
    fn disarm(&mut self, armed: usize) -> Option<Cmd> {
        (armed < self.undo.len()).then(|| self.undo.remove(armed))
    }

    /// Record a mutation this could not undo and will not try again.
    fn leave_behind(&mut self, why: OutrigError) {
        self.left_behind.push(why);
    }

    /// Replace an armed undo with one that names its target more exactly.
    ///
    /// Some identities only exist once the mutation has been made: the kernel
    /// assigns an nft table its handle when it creates it, and until then the
    /// name is all there is to arm with. Narrowing afterwards keeps the
    /// invariant -- at no instant is the mutation unowned -- while ending up
    /// with the stronger undo. Only ever narrower: a replacement must be
    /// unable to reach anything the command it replaces could not.
    fn narrow(&mut self, armed: usize, undo: Cmd) {
        if let Some(slot) = self.undo.get_mut(armed) {
            *slot = undo;
        }
    }

    /// Discharges every armed undo, most recent first, returning one
    /// rendering per command that failed. A command stays armed until it has
    /// returned, so a cancellation mid-command leaves it to `Drop` rather
    /// than dropping it on the floor.
    async fn undo_now<F, Fut>(&mut self, run: &F) -> Vec<OutrigError>
    where
        F: Fn(Cmd) -> Fut,
        Fut: Future<Output = Result<Vec<u8>>>,
    {
        let mut failures = std::mem::take(&mut self.left_behind);
        match self.same_namespace() {
            // It took the table, the resolver and anything else these would
            // have put back with it.
            NamespaceAnswer::Gone => {
                self.undo.clear();
                return failures;
            }
            // Owed, and unrunnable: entering a namespace this cannot identify
            // is the thing the guard exists to prevent, and clearing the list
            // on an answer that was never given would discharge obligations
            // against a namespace that is still there.
            NamespaceAnswer::Unknown => {
                failures.push(OutrigError::Configuration(format!(
                    "whether the network namespace of the container at pid {} is \
                     still the one these undos were armed against could not be \
                     determined, so none of them were run",
                    self.pid
                )));
                return failures;
            }
            NamespaceAnswer::Is(_) => {}
        }
        // Walked back to front by index, and an entry leaves the list only
        // once its command has *returned* successfully. Everything not yet
        // discharged therefore stays in `self.undo` across every await,
        // including the one in flight, so a caller cancelled here drops a
        // future that owns nothing and the destructor still holds every
        // obligation.
        //
        // Removing first and putting the failures back at the end -- which
        // this used to do -- got both halves wrong: a cancellation mid-command
        // lost the command, since it existed only in the dropped frame, and
        // the ones it kept came back newest-first for `Drop` to reverse a
        // second time, undoing the resolver before the redirect that pointed
        // at it.
        let mut idx = self.undo.len();
        while idx > 0 {
            idx -= 1;
            match run(self.undo[idx].clone()).await {
                Ok(_) => {
                    self.undo.remove(idx);
                }
                Err(e) => failures.push(e),
            }
        }
        failures
    }

    #[cfg(test)]
    fn armed(&self) -> Vec<String> {
        self.undo.iter().map(Cmd::render).collect()
    }
}

impl Drop for Rollback {
    fn drop(&mut self) {
        // Only a namespace that is provably gone lets the destructor drop
        // these; an answer it could not get leaves them to be reissued, which
        // is what `detach_cleanup_chain` is for.
        if matches!(self.same_namespace(), NamespaceAnswer::Gone) {
            return;
        }
        // One obligation, not one apiece. Handing the reaper each command
        // separately hands it no ordering -- every call spawns its own child
        // and returns -- and the resolver must not go back while the redirect
        // aimed at it is still there, or the container resolves through a rule
        // pointing at a listener that is gone.
        //
        // Issued once: these select namespaces by pid, and the kernel hands
        // pids out again. A retry landing after the container exited would
        // enter whatever holds that pid now -- rewriting some other
        // container's resolver, or dropping its nft table.
        let ordered: Vec<Cmd> = self.undo.drain(..).rev().collect();
        detach_cleanup_chain(ordered, Reissue::Once);
    }
}

/// Which network namespace `pid` is in, as the nsfs device and inode behind
/// `/proc/<pid>/ns/net`. `None` when there is no such namespace to name.
///
/// The namespace rather than the pid entry: a zombie init keeps the latter
/// after the container is gone, and the namespace is what `nsenter` enters and
/// what the container's `/etc` lives beside. The identity rather than mere
/// existence: pids come round again, and "a namespace is there" says nothing
/// about whose.
///
/// # What this does not establish
///
/// nsfs inode numbers are drawn from a pool that a destroyed namespace returns
/// to, and a handle is only unique within the namespace that issued it. So
/// three recycled values -- the pid, the inode behind it, and a table handle
/// in the new namespace matching this one -- would together satisfy every
/// check here. Each is independently unlikely and the undos are issued
/// promptly and never retried (`Reissue::Once`), but "unlikely" is the claim,
/// not "impossible".
///
/// The stronger identity is an open descriptor on the namespace, which cannot
/// be recycled while it is held. It is not used because these undos have to
/// survive the thing that armed them: they are issued from a destructor with
/// no runtime, through commands `supervise` spawns and may re-spawn, and an
/// inherited descriptor does not survive an `exec` that closes it -- carrying
/// one would mean outrig's own launcher in place of `nsenter` on the one path
/// that must work when everything else is being torn down.
fn namespace_id(pid: u32) -> NamespaceAnswer {
    use std::os::unix::fs::MetadataExt;
    classify_namespace(
        std::fs::metadata(format!("/proc/{pid}/ns/net")).map(|ns| (ns.dev(), ns.ino())),
        // Only consulted for the one error that is ambiguous, and asked at
        // that point rather than up front.
        || !Path::new(&format!("/proc/{pid}")).exists(),
    )
}

/// What a probe's answer means for the undos armed against that namespace.
///
/// Only "there is no such namespace" ends an obligation. Every other error
/// says nothing about whether the namespace is there -- the kernel can fail
/// this for its own reasons, allocating the nsfs dentry for one -- and reading
/// them all as "gone", which is what mapping the error away did, throws away
/// every undo still owed to a namespace that is very much alive.
fn classify_namespace(
    probed: io::Result<(u64, u64)>,
    task_gone: impl FnOnce() -> bool,
) -> NamespaceAnswer {
    match probed {
        Ok(id) => NamespaceAnswer::Is(id),
        // The ordinary answer for a container that has exited: its `nsproxy`
        // is cleared, so the link is not there.
        Err(e) if e.kind() == io::ErrorKind::NotFound => NamespaceAnswer::Gone,
        // The kernel's other way of saying the task went: the lookup behind
        // this link fails with `ESRCH` when it loses the race with an exit.
        // Rust has no `ErrorKind` for that one, so it is matched raw.
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => NamespaceAnswer::Gone,
        // Ambiguous on its own: permission is refused both for a task that is
        // not ours to look at and for one that went while being looked at. The
        // pid's own directory tells them apart, and is only asked here.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && task_gone() => {
            NamespaceAnswer::Gone
        }
        Err(_) => NamespaceAnswer::Unknown,
    }
}

/// What asking which namespace a pid is in can come back with.
enum NamespaceAnswer {
    /// It is this one.
    Is((u64, u64)),
    /// There is no such namespace. Whatever was owed to it went with it.
    Gone,
    /// The question could not be answered. Not the same as `Gone`, and the
    /// difference is what stops a transient failure discharging obligations
    /// that are still owed.
    Unknown,
}

/// How the real interceptor runs one interception command: exit status
/// checked -- a teardown that quietly failed is the thing this reports --
/// output teed to the container's transcript, stdout handed back.
async fn run_step(cmd: Cmd, transcript: Option<Transcript>) -> Result<Vec<u8>> {
    process::run_capture_logged(cmd, "network", transcript.as_ref())
        .await
        .map(|output| output.stdout)
}

/// The container-mutating half of [`NetworkInterceptor::attach`]: snapshot the
/// resolver and point it at the DNS listener, then install the redirect
/// table. Split from the socket and task plumbing, and parameterized over how
/// a command runs, so the ordering and the rollback are exercisable without a
/// container.
///
/// All-or-nothing rests on `rollback` belonging to the caller: a failure
/// returns with the undos armed, and a caller who drops this future
/// mid-command drops it holding them.
async fn install_interception<F, Fut>(
    run: &F,
    rollback: &mut Rollback,
    target: &Target,
    tcp_port: u16,
    dns_port: u16,
) -> Result<()>
where
    F: Fn(Cmd) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    // A dns-preconfigured container had the loopback resolver baked in via
    // `podman create --dns` (`podman exec` cannot reach it before start), so
    // there is nothing here to change and nothing to change back.
    if !target.dns_preconfigured {
        let snapshot =
            checked_resolv_snapshot(&target.name, run(read_resolv_conf(target.pid)).await?)?;
        rollback.arm(restore_resolv_conf(target.pid, &target.marker, snapshot));
        run(install_resolv_conf(target.pid, &target.marker)).await?;
    }

    let mut rules = tempfile::NamedTempFile::new()?;
    rules.write_all(nft_rules(&target.table, tcp_port, dns_port).as_bytes())?;
    rules.as_file_mut().sync_all()?;
    // Safe to arm before the apply because the name belongs to this attach
    // alone -- see `nft_table_name`. `create table` is belt to that brace: it
    // fails rather than merging if anything by this name somehow exists, where
    // a plain `table` block would merge into it, take the chain count from one
    // to two and exit zero (measured against nft in podman 4.9.3), leaving the
    // undo to delete whatever it merged into along with its own rules.
    //
    // What that reasoning missed for a while is that `create table` earns it
    // only in the flat form `nft_rules` now emits: given a nested block, nft
    // 1.0.9 creates the table, drops the block, and exits zero. See
    // `nft_rules`.
    //
    // By name and not yet by handle, because there is no handle until the
    // kernel has made the table. A cancellation landing in the apply therefore
    // undoes by the name -- which is correct there: the name is unobservable
    // until this transaction commits, so nothing else can be holding it.
    let armed = rollback.arm(delete_nft_table(target.pid, &target.table));
    // `--echo --handle` makes the commit report the handle it assigned, in the
    // same transaction that created the table. (nft 0.9.0 and later; outrig
    // already requires `create table`, which is no older.)
    let echo = run(nsenter_nft(target.pid)
        .args(["--echo", "--handle", "-f"])
        .arg(rules.path()))
    .await?;
    // The table exists from here on, and so does its name in a ruleset anyone
    // in the namespace can list. Narrowing the undo to the handle is what
    // stops a table deleted and recreated under that name from being deleted
    // by this attach's teardown.
    //
    // A handle that cannot be read fails the attach rather than leaving the
    // name-based undo in place: an nft too old for `--echo`, output that is
    // not UTF-8, a format that moved. Removal by name alone is the thing this
    // exists to stop, and carrying on would mean promising a teardown that
    // could reach a table this attach did not create.
    // Ownership is taken only from the transaction that created the table:
    // the echo is that transaction's own word, and a handle read from it can
    // be nothing but this table's. A later look cannot make that claim -- a
    // ruleset can move between the commit and it, and what came back would
    // then be a replacement's handle, adopted as though it were ours and
    // deleted at teardown.
    let Some(handle) = table_handle_in(&echo, &target.table) else {
        // No identity, so no removal -- not later, and not here either.
        //
        // The delete armed before the apply can only name the table, and a
        // name is not this attach's to act on once the transaction has
        // committed and made it visible. Running it "immediately" only makes
        // the window small: the delete is still an await, so a cancellation
        // inside it strands the table with nothing owning it, and an actor
        // that replaced the table first has that replacement deleted instead.
        // Both were reachable, and neither is worth the table this would
        // otherwise clear up.
        //
        // So it comes off the rollback unrun, and what was made is reported as
        // left behind. There is no await between here and the error, which is
        // what makes the outcome the same whether the caller waits for it or
        // walks away.
        rollback.disarm(armed);
        rollback.leave_behind(OutrigError::Configuration(format!(
            "the redirect table {} is still in the container: nft would not say \
             what handle it gave the table it had just created, and removing it \
             by name could reach a table of that name that this attach did not \
             create",
            target.table
        )));
        return Err(OutrigError::Configuration(format!(
            "nft reported no handle for the redirect table {} it created, so \
             nothing can tell that table from one of the same name that \
             replaced it; the attach is undone rather than kept on that footing",
            target.table
        )));
    };
    rollback.narrow(
        armed,
        delete_nft_table_by_handle(target.pid, rollback.netns_inode(), handle),
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct AuditSink {
    /// Records queued for the session's writer. Bounded, so a sink that
    /// cannot keep up applies backpressure to the connections producing
    /// records rather than growing without limit.
    /// `None` once [`close`](Self::close) has ended the writer.
    records: Option<mpsc::Sender<AuditJob>>,
    session_id: String,
    container: String,
    /// The writer's handle, held only by the session-level sink so `shutdown`
    /// can end it and wait. `None` on every per-attachment clone.
    writer: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Which attachment this handle belongs to. Failures are filed under it
    /// rather than under the container's name, which a later attachment can
    /// have again.
    generation: u64,
    /// Records the writer has been handed and has not answered for, per
    /// attachment.
    ///
    /// The writer is the only thing that knows what is in its queue, and
    /// stopping it by abort takes that with it -- so the same fact is kept
    /// where a caller can still read it. Whatever is left here when the writer
    /// stops is exactly what it accepted and never accounted for, by owner and
    /// by count.
    pending: AuditPending,
    /// What the writer could not write, per attachment: the first failure and
    /// how many followed it.
    ///
    /// A connection's task returning is what `detach` treats as proof its
    /// record landed, and that holds only once the writer has been drained.
    /// Bounded on purpose -- one entry per container rather than per record --
    /// because a container that can open connections can make writing fail as
    /// often as it likes, and an outage such as `ENOSPC` would otherwise grow
    /// host memory, and the teardown error, for as long as it lasted.
    unwritten: AuditLosses,
}

/// Which attachment a record belongs to: the generation `attach` handed out,
/// and the container's name. The generation is what makes it an identity --
/// a name comes round again, an attachment does not.
type AuditOwner = (u64, String);

/// Per attachment: how many of its records were lost, and the failure worth
/// telling someone about.
type AuditLosses = Arc<Mutex<BTreeMap<AuditOwner, AuditLoss>>>;

/// Per attachment: how many of its records the writer has taken and not yet
/// answered for. Zero entries are removed rather than kept.
type AuditPending = Arc<Mutex<BTreeMap<AuditOwner, u64>>>;

/// One attachment's audit losses: how many records, the failure that broke the
/// first append, and -- if the log may hold a partial record -- what stopped
/// the writer proving otherwise.
#[derive(Debug)]
struct AuditLoss {
    records: u64,
    source: OutrigError,
    integrity: Option<OutrigError>,
}

/// What the audit writer is asked to do.
enum AuditJob {
    /// One record, already encoded, with the container it belongs to and a
    /// channel the writer answers on once it has dealt with it. The producer
    /// waits on that, so a connection's task returning still means its record
    /// is on disk -- the queue moves the bytes out of reach of the producer's
    /// cancellation without moving the guarantee.
    Record {
        who: AuditOwner,
        line: Vec<u8>,
        done: tokio::sync::oneshot::Sender<()>,
    },
    /// Answer once everything queued ahead of this has been written. This is
    /// what lets teardown say "every record this attachment owed is on disk"
    /// rather than assuming it because the connections returned.
    Drained(tokio::sync::oneshot::Sender<()>),
}

impl AuditSink {
    /// Opens the session-level sink. Its container field is empty; each
    /// attachment writes through a [`for_container`](Self::for_container)
    /// handle so records carry that container's name.
    async fn open(path: PathBuf, session_id: String) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .path_ctx("create directory", parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .path_ctx("open", &path)?;
        // Claimed exclusively, because the writer's rollback truncates this
        // file back to where a failed record began -- and `ftruncate` acts on
        // the inode, not on one descriptor's appends. A second writer's record
        // sitting past that mark would be destroyed, and its owner would
        // report a clean teardown having lost it. The lock is advisory and
        // held by the open file description, so it also refuses a second
        // interceptor in another process, and the kernel drops it when the
        // file closes with the writer.
        //
        // `flock` on a `tokio::fs::File` is a raw-fd call and does not block:
        // the non-blocking form is the point, since the answer wanted here is
        // "someone already has this" rather than a wait.
        //
        // Only for a regular file, which is the only thing that truncation
        // means anything for. A caller who points this at a device or a pipe
        // has no rollback to protect and no exclusivity to lose -- and the
        // tests that need every write to fail point it at `/dev/full`, whose
        // one inode every one of them would otherwise queue behind.
        //
        // The exemption is for a file positively known not to be regular. A
        // `stat` that *failed* says nothing, and taking the exemption on it
        // would pair a truncating writer with no lock, which is the one
        // combination this exists to prevent.
        if claims_exclusively(file.metadata().await).path_ctx("stat", &path)? {
            // The typed `fcntl::Flock` that replaces this takes *ownership*
            // of the file and unlocks on drop, and the writer owns a
            // `tokio::fs::File` it goes on appending to -- so the lock has to
            // outlive the call that takes it, which is what the raw form does.
            #[allow(deprecated)]
            nix::fcntl::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                nix::fcntl::FlockArg::LockExclusiveNonblock,
            )
            .map_err(|e| {
                OutrigError::Configuration(format!(
                    "the network audit log {} is already owned by another \
                     interceptor ({e}); one writer owns it, because the rollback \
                     that keeps it a sequence of whole records truncates the file",
                    path.display()
                ))
            })?;
        }

        let unwritten: AuditLosses = Arc::new(Mutex::new(BTreeMap::new()));
        let pending: AuditPending = Arc::new(Mutex::new(BTreeMap::new()));
        let (records, queue) = mpsc::channel(AUDIT_QUEUE);
        // The writer owns the file and is the only thing that touches it, so
        // no caller's cancellation can land inside a record. It ends when the
        // last sink handle drops.
        let writer = tokio::spawn(audit_writer(
            file,
            queue,
            unwritten.clone(),
            pending.clone(),
        ));

        Ok(Self {
            writer: Arc::new(Mutex::new(Some(writer))),
            records: Some(records),
            session_id,
            container: String::new(),
            generation: 0,
            pending,
            unwritten,
        })
    }

    /// Waits until every record queued so far has been written.
    ///
    /// Teardown calls this after joining an attachment's connections, which is
    /// what makes "queued so far" mean "everything this attachment owed".
    /// Bounded: a writer that cannot drain is reported rather than waited on
    /// forever, since the caller is a `detach` that has to return.
    async fn drain(&self) -> Result<()> {
        // One deadline over both halves. Getting the marker *into* the queue
        // is itself a wait -- the queue is bounded, and a stalled writer with
        // every slot full never accepts it -- so timing only the answer would
        // leave `detach` blocked here forever and never reach the resolver and
        // nft undos behind it.
        let Some(records) = self.records.as_ref() else {
            return Ok(());
        };
        let queued = tokio::time::timeout(SHUTDOWN_GRACE, async {
            let (done, wait) = tokio::sync::oneshot::channel();
            if records.send(AuditJob::Drained(done)).await.is_err() {
                // The writer is gone; nothing is still queued behind it.
                return Ok(());
            }
            wait.await.map_err(|_| ())
        })
        .await;
        match queued {
            Ok(Ok(())) => Ok(()),
            _ => Err(OutrigError::Configuration(format!(
                "the network audit log did not finish writing within {SHUTDOWN_GRACE:?}"
            ))),
        }
    }

    /// A handle writing to the same file whose records are stamped with
    /// `container`.
    /// A handle for one attachment, keyed by a generation nothing else can
    /// reuse.
    ///
    /// Keying failures by container name alone misattributes them across
    /// generations: a drain that timed out leaves the writer still holding a
    /// record, and a failure arriving after that would land in the slot a
    /// *new* attachment of the same name is reading. The name is still stamped
    /// on the records themselves; this is only about whose loss it is.
    fn for_attachment(&self, container: &str, generation: u64) -> Self {
        Self {
            container: container.to_string(),
            generation,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn for_container(&self, container: &str) -> Self {
        self.for_attachment(container, 0)
    }

    /// End the writer and wait for it to finish, so nothing can record a loss
    /// after the sweep that follows.
    ///
    /// Returns what went wrong if it could not be waited out; the sweep runs
    /// either way, since a writer that will not stop is a reason to report
    /// rather than a reason to skip collecting what it already recorded.
    async fn close(&mut self) -> Option<OutrigError> {
        let writer = self.writer.lock().ok().and_then(|mut w| w.take())?;
        // Every other handle is gone by now: `shutdown` has taken the
        // attachments, and this is the session's own. Taking the sender ends
        // the writer's loop once its queue is empty; a record offered
        // afterwards has nowhere to go and is reported rather than lost
        // quietly.
        self.records.take();
        // Held by reference, then aborted and joined -- not moved into the
        // timeout. Dropping a timed-out `JoinHandle` *detaches* the task, and
        // a detached writer goes on appending and recording losses after the
        // sweep that was supposed to be the last word on both. This is the
        // same mistake `terminate` exists to avoid, one level down.
        let mut writer = writer;
        let stopped = match tokio::time::timeout(SHUTDOWN_GRACE, &mut writer).await {
            Ok(Ok(())) => return None,
            // It ended on its own, badly. Whatever it was holding is as lost
            // as if it had been stopped, so the same accounting runs.
            Ok(Err(joined)) => format!("the network audit writer ended abnormally: {joined}"),
            Err(_) => {
                writer.abort();
                let _ = writer.await;
                format!(
                    "the network audit writer did not finish within {SHUTDOWN_GRACE:?} \
                     and was stopped"
                )
            }
        };
        // Aborting ends the task, not the syscall. `tokio::fs` runs its writes
        // on a blocking pool, and one already submitted completes whatever
        // happens to the future awaiting it -- so bytes may still land, and the
        // rollback that would have undone a partial write will not run because
        // the task that does it is gone. The log is therefore of unknown
        // integrity, and that is recorded where the sweep will find it rather
        // than only described in the error returned here.
        //
        // Per owner and by count, from what the writer was actually holding.
        // One synthetic entry under this sink's own name -- which this used to
        // record -- named nobody, claimed one record however many were lost,
        // and left every real owner unreported.
        self.convert_pending(&stopped);
        Some(OutrigError::Configuration(format!(
            "{stopped}; what it still held is unaccounted for"
        )))
    }

    /// File everything the writer accepted and never answered for as lost, by
    /// the attachment that queued it.
    ///
    /// Called only once the writer is joined, so nothing can still be moving
    /// records out of `pending` while this reads it.
    fn convert_pending(&self, stopped: &str) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        for (who, records) in std::mem::take(&mut *pending) {
            Self::remember_stopped(&self.unwritten, &who, records, stopped);
        }
    }

    /// Every loss still on the books, whichever attachment incurred it.
    ///
    /// `shutdown`'s sweep. A detach whose drain timed out left its generation
    /// registered rather than taking an account it knew was incomplete, so
    /// whatever the writer reported afterwards is collected here -- once, at
    /// the end, by the only caller that outlives every attachment.
    fn take_every_unwritten(&self) -> Vec<OutrigError> {
        let Ok(mut unwritten) = self.unwritten.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *unwritten)
            .into_iter()
            .map(
                |((_, container), loss)| OutrigError::NetworkAuditUnwritten {
                    container,
                    integrity: loss.integrity.map(Box::new),
                    records: loss.records,
                    source: Box::new(loss.source),
                },
            )
            .collect()
    }

    /// Takes the failures recorded for `container`. Called once its
    /// connections have been joined, so everything they were going to write
    /// has been attempted by then.
    fn take_unwritten(&self) -> Vec<OutrigError> {
        let Ok(mut unwritten) = self.unwritten.lock() else {
            return Vec::new();
        };
        unwritten
            .remove(&(self.generation, self.container.clone()))
            .map(|loss| OutrigError::NetworkAuditUnwritten {
                container: self.container.clone(),
                integrity: loss.integrity.map(Box::new),
                records: loss.records,
                source: Box::new(loss.source),
            })
            .into_iter()
            .collect()
    }

    /// Queues one record for the writer.
    ///
    /// The bytes never leave this task, so nothing a caller does can cut a
    /// record in half: what gets cancelled here is the *queueing*, which
    /// leaves a record absent rather than a line truncated. Absent is already
    /// reported -- a connection cancelled that late is one teardown aborted,
    /// and says so.
    async fn write(&self, record: &AuditRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record)
            .map_err(|e| OutrigError::Configuration(format!("encoding network audit: {e}")))?;
        line.push(b'\n');
        let (done, written) = tokio::sync::oneshot::channel();
        let records = self.records.as_ref().ok_or_else(|| {
            OutrigError::Configuration("the network audit writer has stopped".to_string())
        })?;
        let who = (self.generation, self.container.clone());
        // Capacity first, then the count, then the send -- and no await
        // between the last two, which is what makes the count mean something.
        //
        // The queue is bounded, so offering a record can wait, and a producer
        // cancelled in that wait used to leave its owner counted for a record
        // the writer never saw: over-reported if the writer was later stopped,
        // and silently dropped if it closed cleanly, which is a connection
        // record missing from the log with nothing saying so. `reserve` is
        // cancellation-safe -- tokio guarantees nothing was sent if it is
        // dropped -- and the permit it hands back sends synchronously and
        // infallibly, so the record is counted only once nothing can stop it
        // being queued.
        let Ok(permit) = records.reserve().await else {
            return Err(OutrigError::Configuration(
                "the network audit writer has stopped".to_string(),
            ));
        };
        Self::enter_pending(&self.pending, &who);
        permit.send(AuditJob::Record {
            who: who.clone(),
            line,
            done,
        });
        // Deliberately *not* released on this error: the writer took the
        // record and then died holding it, so it is one of the records
        // `close` reports, under the owner that is still counted here.
        written.await.map_err(|_| {
            OutrigError::Configuration("the network audit writer has stopped".to_string())
        })
    }

    fn enter_pending(pending: &Mutex<BTreeMap<AuditOwner, u64>>, who: &AuditOwner) {
        if let Ok(mut pending) = pending.lock() {
            *pending.entry(who.clone()).or_insert(0) += 1;
        }
    }

    /// Releases one of `who`'s counted records. The writer is the only caller:
    /// a record is counted when nothing can stop it reaching the queue, so
    /// from there on the writer is the only thing that can account for it.
    fn leave_pending(pending: &Mutex<BTreeMap<AuditOwner, u64>>, who: &AuditOwner) {
        if let Ok(mut pending) = pending.lock()
            && let std::collections::btree_map::Entry::Occupied(mut held) =
                pending.entry(who.clone())
        {
            *held.get_mut() -= 1;
            if *held.get() == 0 {
                held.remove();
            }
        }
    }

    /// Records why the log cannot be trusted for `container`, without
    /// counting a record as lost on its own: a write that failed has already
    /// counted itself through [`remember_unwritten`](Self::remember_unwritten),
    /// and adding to the count here would report the same record twice.
    fn remember_integrity(
        unwritten: &Mutex<BTreeMap<AuditOwner, AuditLoss>>,
        who: &AuditOwner,
        why: &str,
    ) {
        if let Ok(mut unwritten) = unwritten.lock() {
            let slot = unwritten.entry(who.clone()).or_insert_with(|| AuditLoss {
                records: 1,
                source: std::io::Error::other(why.to_string()).into(),
                integrity: None,
            });
            // Alongside the write failure, not instead of it: one says what
            // broke the append, the other what stopped it being undone, and a
            // reader needs both to know the file's state.
            slot.integrity = Some(std::io::Error::other(why.to_string()).into());
        }
    }

    /// Records `records` losses for `who`, with the file's integrity in doubt.
    ///
    /// The writer stopped holding them, so which one was mid-write is not
    /// knowable from here -- what is knowable is that the file may hold a
    /// partial line, and that is a fact about the file rather than about one
    /// record, so every owner that lost records is told it.
    fn remember_stopped(
        unwritten: &Mutex<BTreeMap<AuditOwner, AuditLoss>>,
        who: &AuditOwner,
        records: u64,
        stopped: &str,
    ) {
        if let Ok(mut unwritten) = unwritten.lock() {
            let slot = unwritten.entry(who.clone()).or_insert_with(|| AuditLoss {
                records: 0,
                source: std::io::Error::other(format!(
                    "{stopped}, so these records were never written"
                ))
                .into(),
                integrity: None,
            });
            slot.records = slot.records.saturating_add(records);
            slot.integrity = Some(
                std::io::Error::other(format!(
                    "{stopped} with a write in flight, so the log may hold a partial \
                     record that nothing rolled back"
                ))
                .into(),
            );
        }
    }

    fn remember_unwritten(
        unwritten: &Mutex<BTreeMap<AuditOwner, AuditLoss>>,
        who: &AuditOwner,
        error: std::io::Error,
    ) {
        if let Ok(mut unwritten) = unwritten.lock() {
            unwritten
                .entry(who.clone())
                .and_modify(|loss| loss.records = loss.records.saturating_add(1))
                .or_insert_with(|| AuditLoss {
                    records: 1,
                    source: error.into(),
                    integrity: None,
                });
        }
    }
}

/// Whether this file is one the writer has to claim exclusively.
///
/// Only a regular file: truncation is what the claim protects, and truncation
/// means nothing for a device or a pipe -- the tests that need every write to
/// fail point at `/dev/full`, whose single inode they would otherwise queue
/// on. A `stat` that *failed* is neither answer: taking the exemption on it
/// would pair a truncating writer with no lock, which is the one combination
/// the lock exists to prevent, so it is an error rather than a default.
fn claims_exclusively(opened: io::Result<std::fs::Metadata>) -> io::Result<bool> {
    opened.map(|f| f.is_file())
}

/// The one thing that writes `network.jsonl`.
///
/// Owning the file in a single task is what makes a record atomic: no caller's
/// cancellation reaches the bytes, and there is exactly one writer, so no
/// interleaving either. It runs until the last sink handle drops.
///
/// "Written" here means handed to the filesystem and visible to anything that
/// reads the file. It is not `fsync`ed: the acknowledgement a producer waits
/// for says its record is in the file, not that it would survive the host
/// losing power. Durability would cost a sync per connection on a log that
/// gets one record per connection, and nothing here promises it.
async fn audit_writer(
    mut file: tokio::fs::File,
    mut queue: mpsc::Receiver<AuditJob>,
    unwritten: AuditLosses,
    pending: AuditPending,
) {
    // Set when the file may hold a partial record: see the rollback below.
    // Everything after that point is refused rather than appended -- but still
    // received, answered and counted, because a caller waiting for its record
    // is owed an answer and a record refused is still a record lost.
    let mut poisoned: Option<String> = None;
    while let Some(job) = queue.recv().await {
        let AuditJob::Record { who, line, done } = job else {
            // A drain marker: everything queued ahead of it is written by the
            // time this is reached, so answering is the whole job. A receiver
            // that has given up is not an error.
            if let AuditJob::Drained(done) = job {
                let _ = done.send(());
            }
            continue;
        };

        if let Some(why) = &poisoned {
            AuditSink::remember_unwritten(&unwritten, &who, std::io::Error::other(why.clone()));
            AuditSink::leave_pending(&pending, &who);
            let _ = done.send(());
            continue;
        }

        // Where the file ended before this record. `write_all` is a retry
        // loop, not an atomic commit: a filesystem can take a prefix and then
        // fail with `ENOSPC`, and the prefix would make every later record
        // unparseable. Truncating back to here is what keeps the file a
        // sequence of whole records even when a write fails partway.
        let before = match file.metadata().await {
            Ok(meta) => Some(meta.len()),
            Err(_) => None,
        };
        let written = async {
            file.write_all(&line).await?;
            file.flush().await
        }
        .await;

        if let Err(e) = written {
            // Rolling back is what keeps the file a sequence of whole records.
            // When it cannot be done -- the length before the append was never
            // read, or the truncation itself failed -- a prefix may be sitting
            // there, and appending the next record to it would produce a line
            // nothing can parse. There is no recovering from that by writing
            // more, so the writer stops: every later record is reported
            // unwritten rather than added to a file already broken.
            // Why recovery could not be proved, kept rather than collapsed to
            // a boolean: if no further record ever arrives, this is the only
            // thing that will tell a reader the file may be corrupt and what
            // stopped it being repaired.
            let recovery = match before {
                None => Some("the file's length before the record was never read".to_string()),
                Some(before) => match file.set_len(before).await {
                    Err(e) => Some(format!("truncating back to {before} failed: {e}")),
                    Ok(()) => match file.flush().await {
                        Err(e) => Some(format!("flushing the truncation failed: {e}")),
                        Ok(()) => None,
                    },
                },
            };
            tracing::warn!(target: "outrig::network", "network audit write failed: {e}");
            AuditSink::remember_unwritten(&unwritten, &who, e);
            if let Some(why) = recovery {
                tracing::error!(
                    target: "outrig::network",
                    "the network audit log may hold a partial record and cannot be \
                     recovered ({why}); no further records will be written to it"
                );
                let integrity = format!(
                    "the network audit log may hold a partial record that could not be \
                     rolled back ({why}), so nothing further may be appended to it"
                );
                // Replaces the write error this record already recorded
                // rather than counting a second loss: the same record, and
                // this is the more useful thing to be told about it. Recorded
                // now so a teardown that follows immediately still learns the
                // file is suspect, even if nothing else is ever written.
                AuditSink::remember_integrity(&unwritten, &who, &integrity);
                poisoned = Some(integrity);
            }
        }
        // Answered whatever the outcome: the producer is waiting to learn the
        // record has been dealt with, and a failure it can read back from
        // `take_unwritten` is dealt with. Released for the same reason -- this
        // record has been accounted for one way or the other, so it is no
        // longer one the writer is holding.
        AuditSink::leave_pending(&pending, &who);
        let _ = done.send(());
    }
}

#[derive(Debug, Serialize)]
struct AuditRecord {
    ts: f64,
    uid: String,
    #[serde(rename = "id.orig_h")]
    id_orig_h: String,
    #[serde(rename = "id.orig_p")]
    id_orig_p: u16,
    #[serde(rename = "id.resp_h")]
    id_resp_h: String,
    #[serde(rename = "id.resp_p")]
    id_resp_p: u16,
    proto: &'static str,
    service: &'static str,
    duration: f64,
    orig_bytes: u64,
    resp_bytes: u64,
    conn_state: &'static str,
    local_orig: bool,
    local_resp: bool,
    missed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_name: Option<String>,
    #[serde(rename = "outrig.session_id")]
    outrig_session_id: String,
    #[serde(rename = "outrig.container")]
    outrig_container: String,
    #[serde(rename = "outrig.host", skip_serializing_if = "String::is_empty")]
    outrig_host: String,
    /// Where `outrig.host` came from: `resolved` for a name this attachment's
    /// DNS listener validated for the address, `asserted` for one the client
    /// claimed. Only a resolved name can have granted an allow.
    #[serde(rename = "outrig.host_source", skip_serializing_if = "Option::is_none")]
    outrig_host_source: Option<&'static str>,
    #[serde(rename = "outrig.action")]
    outrig_action: &'static str,
    #[serde(rename = "outrig.rule")]
    outrig_rule: String,
}

impl AuditRecord {
    fn new(session_id: &str, container: &str, event: AuditEvent) -> Self {
        let (host, host_source) = match (event.assertion.asserted_host(), event.resolved.first()) {
            (Some(host), _) => (host.to_string(), Some("asserted")),
            (None, Some(host)) => (host.to_string(), Some("resolved")),
            (None, None) => (String::new(), None),
        };
        Self {
            ts: zeek_timestamp(event.opened),
            uid: zeek_uid(),
            id_orig_h: event.orig.ip().to_string(),
            id_orig_p: event.orig.port(),
            id_resp_h: event.dst.ip().to_string(),
            id_resp_p: event.dst.port(),
            proto: "tcp",
            service: event.service,
            duration: event.duration.as_secs_f64(),
            orig_bytes: event.bytes_tx,
            resp_bytes: event.bytes_rx,
            conn_state: if event.bytes_rx == 0 { "S0" } else { "SF" },
            local_orig: true,
            local_resp: false,
            missed_bytes: 0,
            server_name: event.assertion.sni,
            outrig_session_id: session_id.to_string(),
            outrig_container: container.to_string(),
            outrig_host: host,
            outrig_host_source: host_source,
            outrig_action: event.decision.action.as_str(),
            outrig_rule: event.decision.rule,
        }
    }
}

#[derive(Debug, Clone)]
struct AuditEvent {
    opened: SystemTime,
    duration: Duration,
    orig: SocketAddr,
    dst: SocketAddr,
    resolved: ResolvedNames,
    assertion: ClientAssertion,
    /// Zeek-style service label. Usually derived from the client's own bytes,
    /// but the ssh case is settled by the server's banner, which is why it
    /// does not live on [`ClientAssertion`].
    service: &'static str,
    bytes_tx: u64,
    bytes_rx: u64,
    decision: PolicyDecision,
}

fn zeek_timestamp(ts: SystemTime) -> f64 {
    ts.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

fn zeek_uid() -> String {
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    format!(
        "C{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        buf[0],
        buf[1],
        buf[2],
        buf[3],
        buf[4],
        buf[5],
        buf[6],
        buf[7],
        buf[8],
        buf[9],
        buf[10],
        buf[11],
        buf[12],
        buf[13],
        buf[14],
        buf[15]
    )
}

/// Accepts redirected connections and owns the ones it accepted. Holding them
/// in a `JoinSet` rather than detaching them is what lets a detach end them:
/// this task is joined, and it does not return until every connection it
/// started has.
async fn tcp_accept_loop(
    listener: TcpListener,
    audit: AuditSink,
    bindings: Bindings,
    policy: Arc<CompiledNetworkPolicy>,
    cancel: CancellationToken,
    live: mpsc::Sender<()>,
) {
    // A child of the attachment's token. Connections are still cancelled when
    // the attachment is, but the accept-failure path below can end them
    // without reaching the DNS listener, which is a separate socket in a
    // separate task and is still working.
    let conn_cancel = cancel.child_token();
    let mut conns = JoinSet::new();
    accept_into(
        &listener,
        &audit,
        &bindings,
        &policy,
        &cancel,
        &conn_cancel,
        &mut conns,
        &live,
    )
    .await;
    // Every connection watches `conn_cancel`, which by here has been cancelled
    // either way -- directly on the accept-failure path, and through its
    // parent on the detach path -- so their records are all written before
    // this task, and therefore the detach joining it, returns.
    conn_cancel.cancel();
    while conns.join_next().await.is_some() {}
}

/// The accepting half, with its carrier passed in.
///
/// Split out for one reason: the claim that finished connections are taken
/// back out of the set as they complete is only observable in the set itself,
/// and a test cannot see one the loop owns privately. Removing the
/// `reap_finished` call below otherwise fails nothing -- a finished task is
/// not an alive task, so no task count notices the handles piling up.
#[allow(clippy::too_many_arguments)]
async fn accept_into(
    listener: &TcpListener,
    audit: &AuditSink,
    bindings: &Bindings,
    policy: &Arc<CompiledNetworkPolicy>,
    cancel: &CancellationToken,
    conn_cancel: &CancellationToken,
    conns: &mut JoinSet<()>,
    live: &mpsc::Sender<()>,
) {
    loop {
        reap_finished(conns);
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => match original_dst(&stream) {
                        Ok(dst) => {
                            // The connection carries a clone of `live` for
                            // as long as it runs, so teardown can wait every
                            // one of them out even on the path where this
                            // loop is aborted and its carrier dropped.
                            let held = live.clone();
                            let (audit, bindings) = (audit.clone(), bindings.clone());
                            let (policy, cancel) = (policy.clone(), conn_cancel.clone());
                            conns.spawn(async move {
                                handle_tcp(
                                    stream,
                                    peer,
                                    dst,
                                    audit,
                                    bindings,
                                    policy,
                                    cancel,
                                )
                                .await;
                                drop(held);
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "outrig::network",
                                "SO_ORIGINAL_DST failed: {e}"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(target: "outrig::network", "tcp accept failed: {e}");
                        // The listener is gone, so this attachment is over.
                        // Cancelling ends the connections it already has the
                        // same cooperative way a detach would -- each still
                        // writes its audit record -- rather than parking this
                        // task on connections nothing is coming to end. The
                        // child token, so a TCP listener failing does not take
                        // this attachment's DNS interception down with it.
                        conn_cancel.cancel();
                        break;
                    }
                }
            }
        }
    }
}

/// Takes the handles of connections that have already finished out of
/// `conns`. Without this a `JoinSet` only ever spawned into keeps one handle
/// per connection the attachment has ever served, which is exactly the state
/// a long-lived attachment must not accumulate.
fn reap_finished(conns: &mut JoinSet<()>) {
    while conns.try_join_next().is_some() {}
}

/// Whether the server on `port` is expected to write first. Sniffing such a
/// connection would stall it: there are no client bytes to read until the
/// server's greeting has been delivered. Only ssh is recognized today; its
/// banner is read from upstream instead, in [`proxy`].
fn server_speaks_first(port: u16) -> bool {
    port == 22
}

/// The client's first bytes and what they claim, or nothing at all when the
/// client sent nothing inside [`SNIFF_TIMEOUT`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Sniffed {
    assertion: ClientAssertion,
    initial: Vec<u8>,
}

/// Reads the client's opening bytes, giving up after [`SNIFF_TIMEOUT`] so a
/// silent client cannot stall its own connection indefinitely. A client that
/// waits out this window asserts nothing here; [`bridge`] re-checks whatever
/// it says later.
async fn sniff_client_stream<S: AsyncRead + Unpin>(client: &mut S) -> Sniffed {
    let mut buf = vec![0; SNIFF_BUFFER];
    let Ok(Ok(n)) = tokio::time::timeout(SNIFF_TIMEOUT, client.read(&mut buf)).await else {
        return Sniffed::default();
    };
    if n == 0 {
        return Sniffed::default();
    }
    // Right-sized rather than truncated: these bytes are held for the life of
    // the connection, and a truncated buffer keeps its whole capacity.
    Sniffed {
        assertion: sniff_client_bytes(&buf[..n]).unwrap_or_default(),
        initial: buf[..n].to_vec(),
    }
}

/// What the audit record for one connection is built from. [`proxy`] updates
/// it as bytes move and as a late client assertion changes the decision.
#[derive(Debug)]
struct ConnOutcome {
    assertion: ClientAssertion,
    service: &'static str,
    decision: PolicyDecision,
    bytes_tx: u64,
    bytes_rx: u64,
}

/// Bridges one accepted connection to `dst` and records it. `dst` is the
/// original destination the redirect displaced, read from the socket by the
/// accept loop.
async fn handle_tcp(
    mut client: TcpStream,
    orig: SocketAddr,
    dst: SocketAddr,
    audit: AuditSink,
    bindings: Bindings,
    policy: Arc<CompiledNetworkPolicy>,
    cancel: CancellationToken,
) {
    let opened = SystemTime::now();
    let started = Instant::now();

    // Seeded with what this destination earns on no evidence at all. A
    // connection cut before its client ever spoke is recorded as exactly
    // that: the decision its address alone earned, and no bytes -- which is
    // the truth about it, since nothing it might have claimed was ever read
    // and nothing was ever forwarded.
    let mut resolved = ResolvedNames::default();
    let mut outcome = ConnOutcome {
        decision: policy.decide(dst, &resolved, &ClientAssertion::default()),
        service: "-",
        assertion: ClientAssertion::default(),
        bytes_tx: 0,
        bytes_rx: 0,
    };

    // Cancellation cuts the connection wherever it is parked -- the sniff
    // read, the upstream connect, the copy -- and falls through to the audit
    // write below. That write is deliberately outside the cancelled region:
    // a connection detach cut still owes a record of what it moved.
    let _ = cancel
        .run_until_cancelled(async {
            let sniffed = if server_speaks_first(dst.port()) {
                Sniffed::default()
            } else {
                sniff_client_stream(&mut client).await
            };
            // Read after the sniff, not before: the window is up to
            // `SNIFF_TIMEOUT` long, and a lookup the container completes
            // inside it is evidence this connection is entitled to have
            // weighed.
            resolved = resolved_names(&bindings, dst.ip());

            outcome.decision = policy.decide(dst, &resolved, &sniffed.assertion);
            outcome.service = sniffed.assertion.service();
            outcome.assertion = sniffed.assertion;
            if outcome.decision.action == NetworkAction::Deny {
                let _ = client.shutdown().await;
            } else {
                proxy(
                    &mut client,
                    dst,
                    &sniffed.initial,
                    &resolved,
                    &policy,
                    &mut outcome,
                )
                .await;
            }
        })
        .await;

    write_audit(
        &audit,
        AuditEvent {
            opened,
            duration: started.elapsed(),
            orig,
            dst,
            resolved,
            assertion: outcome.assertion,
            service: outcome.service,
            bytes_tx: outcome.bytes_tx,
            bytes_rx: outcome.bytes_rx,
            decision: outcome.decision,
        },
    )
    .await;
}

/// Opens the upstream connection an allowed decision earned and bridges it.
async fn proxy(
    client: &mut TcpStream,
    dst: SocketAddr,
    initial: &[u8],
    resolved: &ResolvedNames,
    policy: &CompiledNetworkPolicy,
    outcome: &mut ConnOutcome,
) {
    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(e)) => {
            tracing::warn!(target: "outrig::network", "connect upstream {dst} failed: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!(target: "outrig::network", "connect upstream {dst} timed out: {e}");
            return;
        }
    };

    if server_speaks_first(dst.port()) {
        let mut buf = vec![0; 1024];
        if let Ok(Ok(n)) = tokio::time::timeout(SNIFF_TIMEOUT, upstream.read(&mut buf)).await
            && n > 0
        {
            if buf[..n].starts_with(b"SSH-") {
                outcome.service = "ssh";
            }
            outcome.bytes_rx += n as u64;
            if let Err(e) = client.write_all(&buf[..n]).await {
                tracing::debug!(target: "outrig::network", "ssh banner write failed: {e}");
                return;
            }
        }
    }

    // A client that asserted nothing inside the sniff window can still assert
    // a name afterwards. That name has to be able to deny before any of its
    // bytes reach upstream, or waiting out the window would be a bypass.
    let recheck = outcome.assertion.is_empty().then_some(LateRecheck {
        dst,
        resolved,
        policy,
    });
    bridge(client, &mut upstream, initial, recheck, outcome).await;
}

/// What a late client assertion is re-evaluated against.
#[derive(Debug, Clone, Copy)]
struct LateRecheck<'a> {
    dst: SocketAddr,
    resolved: &'a ResolvedNames,
    policy: &'a CompiledNetworkPolicy,
}

/// Copies both directions until either side closes. When `recheck` is set the
/// client's first bytes -- which arrived too late for the sniff window -- are
/// run back through the policy before any of them reach upstream, and the
/// connection is torn down if the name they carry now denies.
async fn bridge(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    initial: &[u8],
    recheck: Option<LateRecheck<'_>>,
    outcome: &mut ConnOutcome,
) {
    let (mut client_rx, mut client_tx) = client.split();
    let (mut upstream_rx, mut upstream_tx) = upstream.split();

    // Written through to the outcome as they happen rather than applied at
    // the end. Three things end this copy without returning: `try_join!`
    // cancelling the sibling direction when one fails (which the late-deny
    // path does on purpose), one direction erroring, and a detach dropping
    // this whole future -- and anything only applied on a clean return would
    // erase from the record every byte the container had already moved, and
    // the identity it turned out to be talking to.
    let ConnOutcome {
        assertion,
        service,
        decision,
        bytes_tx,
        bytes_rx,
    } = outcome;
    let bridged = {
        let downstream = async {
            copy_counting(&mut upstream_rx, &mut client_tx, bytes_rx).await?;
            let _ = client_tx.shutdown().await;
            io::Result::Ok(())
        };
        let upward = async {
            if !initial.is_empty() {
                write_counting(&mut upstream_tx, initial, bytes_tx).await?;
            }
            if let Some(recheck) = recheck {
                // Scoped so the sniff buffer is not held for the life of the
                // copy loop below.
                let mut buf = vec![0; SNIFF_BUFFER];
                let n = client_rx.read(&mut buf).await?;
                if n > 0 {
                    // `recheck` is only set when the sniff window closed with
                    // no assertion, so an empty one here re-derives the
                    // decision already taken and has nothing to record.
                    let claimed = sniff_client_bytes(&buf[..n]).unwrap_or_default();
                    if !claimed.is_empty() {
                        let verdict =
                            recheck
                                .policy
                                .decide(recheck.dst, recheck.resolved, &claimed);
                        let denied = verdict.action == NetworkAction::Deny;
                        *service = claimed.service();
                        *assertion = claimed;
                        *decision = verdict;
                        if denied {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "denied by network policy on late client identity",
                            ));
                        }
                    }
                    write_counting(&mut upstream_tx, &buf[..n], bytes_tx).await?;
                }
            }
            copy_counting(&mut client_rx, &mut upstream_tx, bytes_tx).await?;
            let _ = upstream_tx.shutdown().await;
            io::Result::Ok(())
        };
        tokio::try_join!(downstream, upward)
    };

    if let Err(e) = bridged {
        tracing::debug!(target: "outrig::network", "tcp bridge ended with error: {e}");
    }
}

/// `tokio::io::copy`, but reporting progress into `moved` as it goes rather
/// than only on a clean return.
async fn copy_counting<R, W>(reader: &mut R, writer: &mut W, moved: &mut u64) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        write_counting(writer, &buf[..n], moved).await?;
    }
}

/// `write_all`, crediting each write as it lands rather than only once the
/// whole slice is through. A write that delivers a prefix and then fails --
/// or is cancelled, as the late-deny path cancels its sibling direction --
/// still put those bytes across the boundary, so they belong in the record.
async fn write_counting<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    moved: &mut u64,
) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let wrote = writer.write(&bytes[written..]).await?;
        if wrote == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        written += wrote;
        *moved += wrote as u64;
    }
    Ok(())
}

async fn write_audit(audit: &AuditSink, event: AuditEvent) {
    let record = AuditRecord::new(&audit.session_id, &audit.container, event);
    if let Err(e) = audit.write(&record).await {
        // Failures of the write itself are kept by the writer, which is the
        // only thing that can see them; this is the queueing half, and a
        // queue that will not take a record means the writer has stopped.
        tracing::warn!(target: "outrig::network", "network audit record not queued: {e}");
    }
}

/// Answers the container's lookups from the host's resolvers, recording what
/// each name validly resolved to. `resolvers` is passed in rather than read
/// here so a caller -- and a test -- decides who this forwards to.
async fn dns_loop(
    socket: UdpSocket,
    resolvers: Vec<SocketAddr>,
    bindings: Bindings,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            received = socket.recv_from(&mut buf) => {
                let Ok((n, peer)) = received else {
                    break;
                };
                let raw = buf[..n].to_vec();
                let query = dns_query(&raw);
                tracing::debug!(
                    target: "outrig::network",
                    "dns query from {peer}: {:?}",
                    query.as_ref().map(|query| &query.question.name)
                );
                let socket_ref = &socket;
                // A forward waits out `DNS_TIMEOUT` per resolver, which
                // outlasts the grace a detach gives this task. Awaited bare,
                // a detach landing mid-query would abort this task and report
                // the abort -- a routine detach returning an error for
                // nothing having gone wrong.
                let Some(forwarded) = cancel
                    .run_until_cancelled(forward_dns(&raw, query.as_ref(), &resolvers))
                    .await
                else {
                    break;
                };
                match forwarded {
                    Ok(response) => {
                        if let Some(query) = &query
                            && dns_response_is_bindable(&response)
                        {
                            record_dns_bindings(&bindings, &query.question.name, &response);
                        }
                        tracing::debug!(
                            target: "outrig::network",
                            "dns response to {peer}: {} bytes",
                            response.len()
                        );
                        let _ = socket_ref.send_to(&response, peer).await;
                    }
                    Err(e) => {
                        tracing::debug!(target: "outrig::network", "dns forward failed: {e}");
                    }
                }
            }
        }
    }
}

fn record_dns_bindings(bindings: &Bindings, name: &str, response: &[u8]) {
    let bound = dns_bindings(name, response);
    if bound.is_empty() {
        return;
    }
    if let Ok(mut bindings) = bindings.lock() {
        let now = Instant::now();
        for (ip, ttl) in bound {
            bindings.bind(ip, name, ttl, now);
        }
    }
}

/// Forwards `raw` to each resolver in turn until one answers it. `query` is
/// the same packet already parsed, or `None` when it did not parse -- an
/// unparsable query is still forwarded, but nothing it comes back with is
/// accepted as an answer.
async fn forward_dns(
    raw: &[u8],
    query: Option<&DnsQuery>,
    resolvers: &[SocketAddr],
) -> io::Result<Vec<u8>> {
    let mut last_err = None;
    for resolver in resolvers {
        let bind_addr = if resolver.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        let socket = UdpSocket::bind(bind_addr).await?;
        if let Err(e) = socket.send_to(raw, resolver).await {
            last_err = Some(e);
            continue;
        }
        match recv_dns_answer(&socket, query, *resolver).await {
            Ok(response) => return Ok(response),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no DNS resolvers configured")))
}

/// Waits out the whole timeout for a datagram that actually answers `query`,
/// discarding everything else. Returning the first datagram to arrive would
/// let anything able to land a packet on this ephemeral port decide what a
/// name resolves to.
async fn recv_dns_answer(
    socket: &UdpSocket,
    query: Option<&DnsQuery>,
    resolver: SocketAddr,
) -> io::Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + DNS_TIMEOUT;
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, peer) = match tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            Ok(Ok(received)) => received,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(io::Error::new(io::ErrorKind::TimedOut, e)),
        };
        let answers = match query {
            Some(query) => query.answered_by(resolver, peer, &buf[..n]),
            // A query the interceptor could not parse is still forwarded, and
            // the resolver's reply goes straight back. There is no evidence to
            // protect -- nothing binds from it -- and holding the listener for
            // the whole timeout waiting for an answer that can never be
            // recognized would stall every later query behind it.
            None => peer == resolver,
        };
        if answers {
            return Ok(buf[..n].to_vec());
        }
        tracing::debug!(
            target: "outrig::network",
            "discarding dns datagram from {peer} that does not answer the query"
        );
    }
}

fn host_resolvers() -> Vec<SocketAddr> {
    let primary = read_resolvers("/etc/resolv.conf");
    let systemd_upstream = read_resolvers("/run/systemd/resolve/resolv.conf");
    select_host_resolvers(primary, systemd_upstream)
}

fn read_resolvers(path: &str) -> Vec<SocketAddr> {
    std::fs::read_to_string(path)
        .map(|text| parse_resolvers(&text))
        .unwrap_or_default()
}

fn parse_resolvers(text: &str) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some(raw) = line.strip_prefix("nameserver").map(str::trim) else {
            continue;
        };
        if let Ok(ip) = raw.parse::<IpAddr>() {
            out.push(SocketAddr::new(ip, 53));
        }
    }
    out
}

fn select_host_resolvers(
    primary: Vec<SocketAddr>,
    systemd_upstream: Vec<SocketAddr>,
) -> Vec<SocketAddr> {
    if !primary.is_empty() && primary.iter().all(|resolver| resolver.ip().is_loopback()) {
        let upstream: Vec<_> = systemd_upstream
            .into_iter()
            .filter(|resolver| !resolver.ip().is_loopback())
            .collect();
        if !upstream.is_empty() {
            return upstream;
        }
    }

    if primary.is_empty() {
        vec![SocketAddr::from(([1, 1, 1, 1], 53))]
    } else {
        primary
    }
}

/// Reads the container's current resolver, so the undo armed against the
/// install can put exactly that state back.
///
/// Answers with [`RESOLV_PRESENT`] followed by the file's bytes, or with `0`
/// and nothing else when the container has no resolver file at all. A bare
/// `cat` could report neither: it cannot tell an absent file from an empty
/// one, and it fails on the absent one, which would make a container that
/// never had a resolver impossible to attach to rather than a state to put
/// back.
fn read_resolv_conf(pid: u32) -> Cmd {
    nsenter_sh(pid).args([
        "if [ -L /etc/resolv.conf ] && [ ! -e /etc/resolv.conf ]; then printf L; \
             elif [ -e /etc/resolv.conf ]; then printf 1; cat /etc/resolv.conf; \
             else printf 0; fi",
    ])
}

/// The command that puts back whatever [`read_resolv_conf`] found: the file
/// with exactly its old bytes, or its absence.
fn restore_resolv_conf(pid: u32, marker: &str, snapshot: Vec<u8>) -> Cmd {
    match snapshot.split_first() {
        Some((&RESOLV_PRESENT, original)) => write_resolv_conf(pid, marker, original.to_vec()),
        _ => remove_resolv_conf(pid, marker),
    }
}

/// The inverse of an absent resolver.
///
/// Reachable only for a container that has no resolver file at all. Podman
/// bind-mounts one, and `rm` on a bind mount fails with `EBUSY` -- checked
/// against podman 4.9.3 -- so for a container it created the snapshot says
/// present and this is never the undo that gets armed. Where it is armed the
/// file is a real one and the removal works; where it somehow is not, the
/// failure is reported and the undo stays armed rather than being dropped.
fn remove_resolv_conf(pid: u32, marker: &str) -> Cmd {
    let installed = intercepted_resolv(marker);
    let bytes = installed.len().to_string();
    nsenter_sh(pid)
        .args([REMOVE_RESOLV_SCRIPT, "_", ""])
        .arg(installed)
        .arg(bytes)
}

/// Points the resolver at the DNS listener.
///
/// Unguarded, unlike the undos: this is the change, and what it is replacing
/// is whatever the snapshot just read.
fn install_resolv_conf(pid: u32, marker: &str) -> Cmd {
    nsenter_sh(pid)
        .args(["printf '%s' \"$1\" > /etc/resolv.conf", "_"])
        .arg(intercepted_resolv(marker))
}

/// Writes the bytes handed in as `$1` to the resolver file.
///
/// They ride in as an argument rather than interpolated into the script, so
/// nothing between the snapshot and the file interprets them -- no quoting, no
/// escape processing -- and `printf '%s'` adds no newline of its own. What an
/// argument cannot carry is a NUL, and it cannot carry more than
/// `MAX_ARG_STRLEN`; [`checked_resolv_snapshot`] refuses both before anything
/// is mutated, because a restore that cannot be spawned is not a restore.
///
/// A `const` so a test can run this exact script rather than a paraphrase: the
/// claim is that it reproduces arbitrary bytes, and a copy proves that about
/// the copy.
/// Acts only on a resolver carrying this attach's own marker.
///
/// Pure shell, deliberately. The obvious spelling is `printf ... | cmp -s -`,
/// and `cmp` is not something a container has to have: a missing one exits
/// 127, which `|| exit 0` reads as "not ours" -- so the undo would skip
/// silently and `detach` would report success over a resolver still pointing
/// at a stopped listener. `case` and `cat` are what the snapshot already
/// needs, so this adds no dependency of its own.
///
/// A read that *fails* is not a mismatch. Discarding `cat`'s status made an
/// unreadable resolver look like one that was never this attach's: the undo
/// exited zero, teardown struck it off, and `detach` reported success over a
/// container still pointing at a listener that has stopped. An absent file is
/// the one case that legitimately owes nothing, and it is checked separately
/// so it cannot be confused with a file that is there and could not be read.
///
/// The whole installed text, not just the marker it carries. Containment of
/// the marker answers "did this attach install this", but not "is this still
/// what it installed" -- a resolver manager that changed the nameservers and
/// left the comment alone would have had its work discarded. The marker is
/// still what makes the text unique to this attachment; comparing all of it is
/// what keeps a later legitimate change.
///
/// Both sides go through command substitution with a `.` appended inside it.
/// Command substitution strips trailing newlines, so without that a file that
/// gained or lost a terminal newline compared equal to one that had not --
/// byte-distinct state the undo would then have overwritten, which is the
/// opposite of the contract. The sentinel is the last thing in each
/// substitution, so nothing before it is stripped, and the comparison needs no
/// external tool.
///
/// The byte count is checked where `wc` exists, because a shell variable is
/// not a byte string: implementations differ on what command substitution does
/// with an embedded NUL, and one that drops them would let a resolver with a
/// NUL added after installation compare equal to one without. `wc -c` reads
/// the file rather than a variable, and `-eq` rather than `=` because some
/// `wc`s pad their output.
///
/// Guarded by `command -v`, a builtin, and deliberately so. A container need
/// not ship `wc`, and an unguarded call exits 127 -- which `|| exit 0` reads
/// as "not this attach's", retiring the undo while `detach` reports success
/// over a resolver still pointing at a stopped listener. That is the `cmp`
/// mistake again, and it is worse than the gap it closes: without `wc` the
/// text comparison still stands, and what is lost is only a NUL inserted after
/// installation on a shell that drops NULs in substitution.
const RESTORE_RESOLV_SCRIPT: &str = "if [ ! -e /etc/resolv.conf ]; then exit 0; fi; \
     current=$(cat /etc/resolv.conf && printf .) || exit 1; \
     [ \"$current\" = \"$(printf '%s.' \"$2\")\" ] || exit 0; \
     if command -v wc > /dev/null 2>&1; then \
     [ \"$(wc -c < /etc/resolv.conf)\" -eq \"$3\" ] || exit 0; fi; \
     printf '%s' \"$1\" > /etc/resolv.conf";

/// The inverse of an absent resolver, under the same marker.
const REMOVE_RESOLV_SCRIPT: &str = "if [ ! -e /etc/resolv.conf ]; then exit 0; fi; \
     current=$(cat /etc/resolv.conf && printf .) || exit 1; \
     [ \"$current\" = \"$(printf '%s.' \"$2\")\" ] || exit 0; \
     if command -v wc > /dev/null 2>&1; then \
     [ \"$(wc -c < /etc/resolv.conf)\" -eq \"$3\" ] || exit 0; fi; \
     rm -f /etc/resolv.conf";

/// What [`install_resolv_conf`] writes, and therefore what an undo expects to
/// find before it acts.
///
/// The trailing comment carries `marker`, which is [`attach_nonce`]'s and
/// *not* the table's name -- the two are drawn independently, and
/// `a_resolver_marker_never_names_the_table_it_was_attached_with` is there to
/// keep them that way. This file becomes readable inside the container the
/// moment it is written, which is before the nft table exists; a marker that
/// named the table would hand anyone in that namespace the one thing they
/// need to create that table first and make `create table` fail.
///
/// Per attach rather than per outrig, because the text is otherwise identical
/// for every attachment: a pid reused by *another* outrig container would
/// satisfy a shared sentinel and get the first container's resolver written
/// into it. Resolver files ignore `#` lines, so this costs the container
/// nothing.
fn intercepted_resolv(marker: &str) -> String {
    format!(
        "nameserver {INTERCEPT_DNS_NAMESERVER}\noptions {INTERCEPT_DNS_OPTION}\n{}\n",
        resolv_marker(marker)
    )
}

/// The line that says which attach installed a resolver.
///
/// Carries a nonce of its own, deliberately *not* the nft table's name. The
/// resolver is written before the table is created, so whatever is in it is
/// readable inside the container before the table exists -- and a name an
/// actor in that namespace can read is a name it can create first, making
/// outrig's `create table` fail and its rollback delete a table it never made.
/// Two independent nonces mean publishing one tells nobody anything about the
/// other. Resolver files ignore `#` lines, so this costs the container
/// nothing.
fn resolv_marker(marker: &str) -> String {
    format!("# {marker}")
}

/// Reads the probe's verdict and refuses every resolver state this could not
/// faithfully put back.
///
/// Refusing here is the whole point. Each of these describes a container whose
/// resolver an undo could not restore, and discovering that after the resolver
/// had already been replaced would leave exactly the state this task exists to
/// prevent: loopback DNS with nothing listening and no way back.
fn checked_resolv_snapshot(container: &str, snapshot: Vec<u8>) -> Result<Vec<u8>> {
    let refuse = |why: String| {
        Err(OutrigError::Configuration(format!(
            "container {container:?} cannot be network-intercepted: {why}"
        )))
    };
    match snapshot.split_first() {
        Some((&RESOLV_DANGLING, _)) => refuse(
            "/etc/resolv.conf is a symbolic link to a file that does not exist. \
             Installing would follow the link and create its target, and undoing \
             that would remove the link itself"
                .to_string(),
        ),
        Some((&RESOLV_PRESENT, original)) if original.contains(&0) => refuse(
            "/etc/resolv.conf contains a NUL byte, which cannot be carried in the \
             argument a restore would put it back with"
                .to_string(),
        ),
        Some((&RESOLV_PRESENT, original)) if original.len() > MAX_RESOLV_SNAPSHOT => {
            refuse(format!(
                "/etc/resolv.conf is {} bytes, past the {MAX_RESOLV_SNAPSHOT} a \
                 restore can carry in one argument",
                original.len()
            ))
        }
        Some((&RESOLV_PRESENT, _)) => Ok(snapshot),
        Some((_, _)) => Ok(snapshot),
        None => refuse("reading /etc/resolv.conf produced no verdict".to_string()),
    }
}

/// Puts `content` back, but only if the resolver still holds what this attach
/// installed.
///
/// The guard is what keeps a delayed undo honest. These commands name a
/// namespace by pid, and the kernel hands pids out again: an undo that runs
/// after its container exited -- because the command before it in the chain
/// was slow, or because a destructor fired late -- would otherwise write one
/// container's resolver into whatever holds that pid now. Checking the content
/// first also means an undo will not clobber a resolver that something else
/// legitimately changed after interception was installed.
fn write_resolv_conf(pid: u32, marker: &str, content: Vec<u8>) -> Cmd {
    let installed = intercepted_resolv(marker);
    let bytes = installed.len().to_string();
    nsenter_sh(pid)
        .args([RESTORE_RESOLV_SCRIPT, "_"])
        .arg(OsString::from_vec(content))
        .arg(installed)
        .arg(bytes)
}

/// A shell inside the container's user and mount namespaces, run as a process
/// this host owns.
///
/// Deliberately not `podman exec`. That starts the shell under conmon, so
/// killing the client -- which is all a dropped future can do -- leaves the
/// writer running: measured against podman 4.9.3, a `podman exec` whose client
/// was killed went on to complete its write two seconds later, and a rollback
/// racing that loses. `nsenter` execs the shell directly, so it *is* the
/// process this owns, and killing it kills the writer -- which is what carries
/// [`crate::process::Owned`]'s guarantee across the container boundary.
fn nsenter_sh(pid: u32) -> Cmd {
    Cmd::new("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .args(["-U", "-m", "--", "sh", "-c"])
}

fn require_tool(name: &str) -> Result<()> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(());
        }
    }
    Err(OutrigError::Configuration(format!(
        "network interception requires `{name}` on PATH"
    )))
}

fn bind_interceptor_sockets(pid: u32) -> Result<InterceptorSockets> {
    let user_ns_path = format!("/proc/{pid}/ns/user");
    let net_ns_path = format!("/proc/{pid}/ns/net");
    let user_ns = StdFile::open(&user_ns_path).path_ctx("open", &user_ns_path)?;
    let net_ns = StdFile::open(&net_ns_path).path_ctx("open", &net_ns_path)?;
    let (tcp_fd, dns_fd) = bind_interceptor_socket_fds(user_ns.as_raw_fd(), net_ns.as_raw_fd())?;
    let tcp = unsafe { std::net::TcpListener::from_raw_fd(tcp_fd) };
    let dns = unsafe { std::net::UdpSocket::from_raw_fd(dns_fd) };
    tcp.set_nonblocking(true)?;
    dns.set_nonblocking(true)?;

    Ok(InterceptorSockets {
        tcp: TcpListener::from_std(tcp)?,
        dns: UdpSocket::from_std(dns)?,
    })
}

fn bind_interceptor_socket_fds(user_ns: RawFd, net_ns: RawFd) -> io::Result<(RawFd, RawFd)> {
    let (_status, fds) =
        nsfork::fork_collect(|sock| child_bind_and_send_fds(sock, user_ns, net_ns))?;
    let mut fds = fds.into_iter();
    match (fds.next(), fds.next()) {
        (Some(tcp), Some(dns)) => Ok((tcp.into_raw_fd(), dns.into_raw_fd())),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network namespace helper did not return listener sockets",
        )),
    }
}

fn child_bind_and_send_fds(sock: RawFd, user_ns: RawFd, net_ns: RawFd) {
    if nsfork::setns_raw(user_ns, libc::CLONE_NEWUSER).is_err() {
        return;
    }
    unsafe {
        let _ = libc::setgid(0);
        let _ = libc::setuid(0);
    }
    if nsfork::setns_raw(net_ns, libc::CLONE_NEWNET).is_err() {
        return;
    }

    let tcp = match std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) {
        Ok(listener) => listener,
        Err(_) => return,
    };
    let dns = match std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 53))) {
        Ok(socket) => socket,
        Err(_) => return,
    };

    let _ = nsfork::send_status(
        sock,
        nsfork::Status::OK,
        &[tcp.as_raw_fd(), dns.as_raw_fd()],
    );
}

fn delete_nft_table(pid: u32, table: &str) -> Cmd {
    nsenter_nft(pid)
        .args(["delete", "table", "inet"])
        .arg(table)
}

/// Delete the table the kernel gave `handle` to, and nothing else.
///
/// One selector, because the handle is exact where it is used: nftables draws
/// table handles from a counter per network namespace and never hands one out
/// twice, so within a namespace a handle names the table that transaction
/// created or it names nothing. Measured against nft 1.0.9 -- create, delete
/// and create again gives 1, 2, 3, and a `flush ruleset` in between does not
/// send it back to 1 -- so a table deleted and recreated under this attach's
/// name does not answer to the handle its predecessor had.
///
/// "Within a namespace" is [`Rollback::same_namespace`]'s job, checked before
/// this is issued. Pairing this with a name check instead -- `list table inet
/// <name> ; delete table inet handle <h>`, which is what this used to do --
/// only looks like it binds the two: the list is a separate command whose
/// success says the name resolves *somewhere* in the namespace, not that it
/// resolves to the table being deleted. In a namespace that was not this
/// attach's, both could resolve to different tables and the delete would take
/// the wrong one.
fn delete_nft_table_by_handle(pid: u32, netns: u64, handle: u64) -> Cmd {
    // Checked from *inside*, against the namespace this process actually
    // landed in, because a check made before entering is a different question
    // from the one that matters. `nsenter -t <pid>` resolves the pid when it
    // runs, and a pid checked and then handed to another container between
    // the check and the exec would carry this undo into a namespace where the
    // handle names a stranger's table. A process cannot be moved between
    // namespaces by anyone else, so what `/proc/self/ns/net` reads back here
    // is what the `nft` that follows it will act in: the check and the action
    // are bound by being in the same process, which no ordering of the two
    // commands could achieve.
    //
    // The shell, `readlink` and `nft` are the *host's*: only the user and
    // network namespaces are entered, not the mount namespace, so this asks
    // nothing of the container's image.
    Cmd::new("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .args(["-U", "-n", "--", "sh", "-c"])
        .arg(NFT_DELETE_IN_NAMESPACE)
        .arg("_")
        .arg(format!("net:[{netns}]"))
        .arg(handle.to_string())
}

/// Delete the table at `$2`, but only while the namespace this landed in is
/// the one named by `$1`. Exits zero without acting when it is not: a
/// namespace that is not the one the undo was armed against owes it nothing,
/// and acting there is worse than not acting at all.
///
/// Run as `sh -c <script> _ <namespace> <handle>`, so `_` is `$0` and the two
/// arguments are `$1` and `$2`.
const NFT_DELETE_IN_NAMESPACE: &str = "[ \"$(readlink /proc/self/ns/net)\" = \"$1\" ] || exit 0; \
     exec nft delete table inet handle \"$2\"";

/// The handle nft reports for `table`, from either the echo of the transaction
/// that created it (`create table inet <name> # handle 3`) or a later listing
/// of it (`table inet <name> { # handle 3`).
///
/// Matched on the name as a whole token, so a table whose name merely starts
/// the same is not mistaken for this one.
fn table_handle_in(output: &[u8], table: &str) -> Option<u64> {
    let output = std::str::from_utf8(output).ok()?;
    output.lines().find_map(|line| {
        let (named, handle) = line.split_once("# handle ")?;
        let tokens: Vec<&str> = named.split_whitespace().collect();
        tokens
            .windows(3)
            .any(|w| w == ["table", "inet", table])
            .then(|| handle.split_whitespace().next()?.parse().ok())
            .flatten()
    })
}

fn nsenter_nft(pid: u32) -> Cmd {
    Cmd::new("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .args(["-U", "-n", "nft"])
}

/// A value for one attach that nothing else can have chosen.
fn attach_nonce() -> String {
    let mut nonce = [0u8; 8];
    rand::rng().fill_bytes(&mut nonce);
    format!("outrig_{:016x}", u64::from_ne_bytes(nonce))
}

/// The redirect table's name, unique to one attach.
///
/// The session-derived part keeps it recognizable -- a sweeper, an operator,
/// and the e2e tests all look for the `outrig_` prefix -- and the random tail
/// is what makes deleting it safe. A deterministic name is a name something
/// else can hold: a stale table left by a crashed run of the same session, an
/// operator's own, or one another actor creates in the same namespace between
/// a check and an apply. Each turns the undo into a command that destroys
/// state this attach never made, and no preflight check closes the last case,
/// because the window is exactly between the check and the create.
///
/// A name nothing else could have chosen removes the question rather than
/// narrowing it: a table by this name is one this attach created.
fn nft_table_name(session_id: &str) -> String {
    let suffix: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut nonce = [0u8; 8];
    rand::rng().fill_bytes(&mut nonce);
    let nonce = u64::from_ne_bytes(nonce);
    format!("outrig_{suffix}_{nonce:016x}")
}

/// The redirect script, as a sequence of top-level commands.
///
/// Deliberately **not** a `create table` with the chain nested inside it. nft
/// accepts that form, exits zero, and creates the table with the whole block
/// silently dropped -- measured against nft 1.0.9, which is what Ubuntu 24.04
/// ships and therefore what every GitHub runner has. The result was an empty
/// table and an interceptor that redirected nothing: no audit records in audit
/// mode, and every connection allowed in filter mode, including the ones a
/// `default = deny` policy exists to stop. `nft list table` showed the table
/// with no chain in it, and `--echo` reported only the table line.
///
/// The flat form installs the same ruleset and keeps the two properties
/// `create` is here for: it still fails rather than merging if a table of this
/// name already exists, and `nft -f` is one transaction either way, so a
/// failure anywhere in the script leaves nothing behind.
fn nft_rules(table: &str, tcp_port: u16, dns_port: u16) -> String {
    format!(
        "\
create table inet {table}
add chain inet {table} output {{ type nat hook output priority dstnat; policy accept; }}
add rule inet {table} output ip daddr 127.0.0.0/8 return
add rule inet {table} output ip6 daddr ::1 return
add rule inet {table} output meta l4proto tcp redirect to :{tcp_port}
add rule inet {table} output udp dport 53 redirect to :{dns_port}
"
    )
}

fn original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    match original_dst_v4(stream) {
        Ok(addr) => Ok(addr),
        Err(v4_err) => original_dst_v6(stream).map_err(|_| v4_err),
    }
}

fn original_dst_v4(stream: &TcpStream) -> io::Result<SocketAddr> {
    let fd = stream.as_raw_fd();
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);
    Ok(SocketAddr::new(IpAddr::V4(ip), port))
}

fn original_dst_v6(stream: &TcpStream) -> io::Result<SocketAddr> {
    let fd = stream.as_raw_fd();
    let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IPV6,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let ip = Ipv6Addr::from(addr.sin6_addr.s6_addr);
    let port = u16::from_be(addr.sin6_port);
    Ok(SocketAddr::new(IpAddr::V6(ip), port))
}

fn sniff_client_bytes(bytes: &[u8]) -> Option<ClientAssertion> {
    if let Some(host) = http_host(bytes) {
        return Some(ClientAssertion::http(host));
    }
    tls_sni(bytes).map(ClientAssertion::tls)
}

fn http_host(bytes: &[u8]) -> Option<String> {
    const METHODS: &[&[u8]] = &[
        b"GET ",
        b"POST ",
        b"PUT ",
        b"PATCH ",
        b"DELETE ",
        b"HEAD ",
        b"OPTIONS ",
        b"CONNECT ",
    ];
    if !METHODS.iter().any(|method| bytes.starts_with(method)) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("host") {
            return Some(strip_host_port(value.trim()).to_string());
        }
    }
    None
}

fn strip_host_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.trim_matches(|c| c == '[' || c == ']');
    }
    host.rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map(|(host, _)| host)
        .unwrap_or(host)
}

fn tls_sni(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 5 || bytes[0] != 22 {
        return None;
    }
    let record_len = u16::from_be_bytes([bytes[3], bytes[4]]) as usize;
    if bytes.len() < 5 + record_len || bytes.get(5).copied()? != 1 {
        return None;
    }
    let mut off = 9;
    checked_skip(bytes, &mut off, 2 + 32)?;
    let session_len = *bytes.get(off)? as usize;
    checked_skip(bytes, &mut off, 1 + session_len)?;
    let cipher_len = read_u16(bytes, &mut off)? as usize;
    checked_skip(bytes, &mut off, cipher_len)?;
    let compression_len = *bytes.get(off)? as usize;
    checked_skip(bytes, &mut off, 1 + compression_len)?;
    let extensions_len = read_u16(bytes, &mut off)? as usize;
    let extensions_end = off.checked_add(extensions_len)?;
    while off + 4 <= extensions_end && off + 4 <= bytes.len() {
        let ext_type = read_u16(bytes, &mut off)?;
        let ext_len = read_u16(bytes, &mut off)? as usize;
        let ext_end = off.checked_add(ext_len)?;
        if ext_end > bytes.len() {
            return None;
        }
        if ext_type == 0 {
            return parse_sni_extension(&bytes[off..ext_end]);
        }
        off = ext_end;
    }
    None
}

fn parse_sni_extension(bytes: &[u8]) -> Option<String> {
    let mut off = 0;
    let list_len = read_u16(bytes, &mut off)? as usize;
    let list_end = off.checked_add(list_len)?;
    while off + 3 <= list_end && off + 3 <= bytes.len() {
        let name_type = *bytes.get(off)?;
        off += 1;
        let name_len = read_u16(bytes, &mut off)? as usize;
        let name_end = off.checked_add(name_len)?;
        if name_end > bytes.len() {
            return None;
        }
        if name_type == 0 {
            return std::str::from_utf8(&bytes[off..name_end])
                .ok()
                .map(str::to_string);
        }
        off = name_end;
    }
    None
}

fn checked_skip(bytes: &[u8], off: &mut usize, len: usize) -> Option<()> {
    *off = off.checked_add(len)?;
    if *off <= bytes.len() { Some(()) } else { None }
}

fn read_u16(bytes: &[u8], off: &mut usize) -> Option<u16> {
    let value = u16::from_be_bytes([*bytes.get(*off)?, *bytes.get(*off + 1)?]);
    *off += 2;
    Some(value)
}

/// A query the interceptor forwarded, in the terms an answer has to match.
#[derive(Debug, PartialEq, Eq)]
struct DnsQuery {
    txid: u16,
    question: DnsQuestion,
}

/// The single question a packet carries. A packet with any other question
/// count is not something the interceptor will bind from.
#[derive(Debug, PartialEq, Eq)]
struct DnsQuestion {
    /// Lowercased.
    name: String,
    qtype: u16,
    qclass: u16,
    /// Offset just past the question, where the answer section begins.
    end: usize,
}

fn dns_query(packet: &[u8]) -> Option<DnsQuery> {
    Some(DnsQuery {
        txid: dns_txid(packet)?,
        question: dns_question(packet)?,
    })
}

impl DnsQuery {
    /// Whether `datagram` may be treated as the answer to this query.
    /// Everything checked here is evidence the interceptor holds itself: an
    /// off-path host that guesses the ephemeral port still has to come from
    /// the resolver's address and echo the transaction id and the question.
    fn answered_by(&self, resolver: SocketAddr, peer: SocketAddr, datagram: &[u8]) -> bool {
        if peer != resolver || dns_txid(datagram) != Some(self.txid) {
            return false;
        }
        // QR must be set: a query reflected back is not an answer.
        if dns_flags(datagram).is_none_or(|flags| flags & 0x8000 == 0) {
            return false;
        }
        dns_question(datagram).is_some_and(|echoed| {
            echoed.name == self.question.name
                && echoed.qtype == self.question.qtype
                && echoed.qclass == self.question.qclass
        })
    }
}

/// Whether a matched response is trustworthy enough to bind names from: no
/// truncation, and a success RCODE.
fn dns_response_is_bindable(datagram: &[u8]) -> bool {
    dns_flags(datagram).is_some_and(|flags| flags & 0x0200 == 0 && flags & 0x000f == 0)
}

fn dns_txid(packet: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*packet.first()?, *packet.get(1)?]))
}

fn dns_flags(packet: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*packet.get(2)?, *packet.get(3)?]))
}

fn dns_question(packet: &[u8]) -> Option<DnsQuestion> {
    if packet.len() < 12 || u16::from_be_bytes([packet[4], packet[5]]) != 1 {
        return None;
    }
    let (mut name, next) = read_dns_name(packet, 12, 0)?;
    name.make_ascii_lowercase();
    Some(DnsQuestion {
        name,
        qtype: u16::from_be_bytes([*packet.get(next)?, *packet.get(next + 1)?]),
        qclass: u16::from_be_bytes([*packet.get(next + 2)?, *packet.get(next + 3)?]),
        end: next.checked_add(4)?,
    })
}

/// One record from an answer section, reduced to what binding decisions need.
#[derive(Debug, PartialEq, Eq)]
enum DnsAnswer {
    Address {
        owner: String,
        ttl: Duration,
        ip: IpAddr,
    },
    Alias {
        owner: String,
        target: String,
    },
}

/// The addresses a response actually attributes to the queried name, each with
/// its own record's TTL.
///
/// Records are attributed by owner name through the CNAME chain, so an answer
/// carrying an address record for an unrelated owner binds nothing. The
/// addresses always bind under the *queried* name and never under a chain
/// member, so an authority for one name cannot mint a binding for another by
/// aliasing to it.
fn dns_bindings(question_name: &str, packet: &[u8]) -> Vec<(IpAddr, Duration)> {
    let answers = dns_answer_records(packet);
    let mut chain = vec![question_name.to_ascii_lowercase()];
    // Each pass adds one name; a target already in the chain adds nothing, so
    // a looping chain terminates having reached no address record.
    while chain.len() <= MAX_CNAME_DEPTH {
        let next = answers.iter().find_map(|answer| match answer {
            DnsAnswer::Alias { owner, target }
                if chain.contains(owner) && !chain.contains(target) =>
            {
                Some(target.clone())
            }
            _ => None,
        });
        let Some(next) = next else {
            break;
        };
        chain.push(next);
    }
    answers
        .iter()
        .filter_map(|answer| match answer {
            DnsAnswer::Address { owner, ttl, ip } if chain.contains(owner) => Some((*ip, *ttl)),
            _ => None,
        })
        .collect()
}

fn dns_answer_records(packet: &[u8]) -> Vec<DnsAnswer> {
    let Some(question) = dns_question(packet) else {
        return Vec::new();
    };
    let mut off = question.end;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut answers = Vec::new();
    for _ in 0..ancount {
        // Framing first, meaning second. A record whose owner the policy would
        // not let mean anything makes *that record* ineligible; it must not
        // make the rest of the section unreadable, or an answer could hide the
        // address record after it and turn a hostname allow into a denial.
        let Some(next) = skip_dns_name(packet, off) else {
            break;
        };
        let owner = read_dns_name(packet, off, 0).map(|(owner, _)| owner);
        off = next;
        if off + 10 > packet.len() {
            break;
        }
        let rr_type = u16::from_be_bytes([packet[off], packet[off + 1]]);
        let ttl = u32::from_be_bytes([
            packet[off + 4],
            packet[off + 5],
            packet[off + 6],
            packet[off + 7],
        ]);
        let rdlen = u16::from_be_bytes([packet[off + 8], packet[off + 9]]) as usize;
        off += 10;
        if off + rdlen > packet.len() {
            break;
        }
        if let Some(mut owner) = owner {
            owner.make_ascii_lowercase();
            let ttl = Duration::from_secs(ttl.into());
            match (rr_type, rdlen) {
                (DNS_TYPE_A, 4) => answers.push(DnsAnswer::Address {
                    owner,
                    ttl,
                    ip: IpAddr::V4(Ipv4Addr::new(
                        packet[off],
                        packet[off + 1],
                        packet[off + 2],
                        packet[off + 3],
                    )),
                }),
                (DNS_TYPE_AAAA, 16) => {
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&packet[off..off + 16]);
                    answers.push(DnsAnswer::Address {
                        owner,
                        ttl,
                        ip: IpAddr::V6(Ipv6Addr::from(octets)),
                    });
                }
                (DNS_TYPE_CNAME, _) => {
                    if let Some((mut target, _)) = read_dns_name(packet, off, 0) {
                        target.make_ascii_lowercase();
                        answers.push(DnsAnswer::Alias { owner, target });
                    }
                }
                _ => {}
            }
        }
        off += rdlen;
    }
    answers
}

/// Decodes a name to its presentation form, or `None` if any label makes that
/// form a lie. This is load-bearing rather than cosmetic: a decoded name is
/// the only evidence that can grant a hostname rule, and a wire label is a
/// counted byte string that may legally contain a `.`. Without this check,
/// the wire labels `["allowed.evil", "attacker", "example"]` -- a name
/// genuinely delegated to whoever runs `attacker.example` -- would decode to
/// `allowed.evil.attacker.example` and satisfy `allow = ["allowed.*"]`,
/// letting that authority bind any address under a glob it does not own.
fn read_dns_name(packet: &[u8], mut off: usize, depth: usize) -> Option<(String, usize)> {
    if depth > 8 {
        return None;
    }
    let mut name = String::new();
    let end;
    loop {
        let len = *packet.get(off)?;
        if len & 0b1100_0000 == 0b1100_0000 {
            let b2 = *packet.get(off + 1)?;
            let ptr = (((len & 0b0011_1111) as usize) << 8) | b2 as usize;
            let (suffix, _) = read_dns_name(packet, ptr, depth + 1)?;
            push_dns_label(&mut name, &suffix);
            end = off + 2;
            break;
        }
        off += 1;
        if len == 0 {
            end = off;
            break;
        }
        let next = off.checked_add(len as usize)?;
        if next > packet.len() {
            return None;
        }
        push_dns_label(&mut name, dns_label(&packet[off..next])?);
        off = next;
    }
    Some((name, end))
}

/// Walks a name for framing only, returning where the record's fixed header
/// begins. Bounds are checked; label contents are not, because deciding what a
/// name may *mean* is [`read_dns_name`]'s job and a packet that is merely
/// unnameable is still readable.
fn skip_dns_name(packet: &[u8], mut off: usize) -> Option<usize> {
    loop {
        let len = *packet.get(off)?;
        if len & 0b1100_0000 == 0b1100_0000 {
            packet.get(off + 1)?;
            return Some(off + 2);
        }
        off += 1;
        if len == 0 {
            return Some(off);
        }
        let next = off.checked_add(len as usize)?;
        if next > packet.len() {
            return None;
        }
        off = next;
    }
}

fn push_dns_label(name: &mut String, label: &str) {
    if !name.is_empty() {
        name.push('.');
    }
    name.push_str(label);
}

/// One wire label, if it can appear in a name the policy matches against.
/// Length is checked because a length byte with reserved high bits would
/// otherwise claim more than the 63 a label may hold; the charset is checked
/// because `.` forges a label boundary and anything outside letters, digits,
/// `-`, and `_` cannot occur in a name a rule could legitimately name.
fn dns_label(bytes: &[u8]) -> Option<&str> {
    if bytes.is_empty() || bytes.len() > MAX_DNS_LABEL {
        return None;
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
    {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;

    fn compiled(policy: NetworkPolicy) -> CompiledNetworkPolicy {
        CompiledNetworkPolicy::new(policy).expect("compile policy")
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("socket addr")
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("ip addr")
    }

    fn resolved(names: &[&str]) -> ResolvedNames {
        names.iter().copied().collect()
    }

    /// A decision with no evidence at all: no resolved name, no client claim.
    fn decide_bare(policy: &CompiledNetworkPolicy, dst: &str) -> PolicyDecision {
        policy.decide(
            addr(dst),
            &ResolvedNames::default(),
            &ClientAssertion::default(),
        )
    }

    /// A DNS query header's flags: RD set, nothing else.
    const DNS_QUERY_FLAGS: u16 = 0x0100;
    /// A successful response: QR, RD, RA, RCODE 0.
    const DNS_RESPONSE_FLAGS: u16 = 0x8180;

    enum Rr<'a> {
        A(&'a str, [u8; 4], u32),
        Aaaa(&'a str, [u8; 16], u32),
        Cname(&'a str, &'a str),
    }

    fn dns_name_bytes(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.').filter(|label| !label.is_empty()) {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    /// Minimal DNS wire encoder: one question plus whatever answer records the
    /// case needs, with no name compression.
    fn dns_packet(txid: u16, flags: u16, question: &str, answers: &[Rr]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&txid.to_be_bytes());
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&dns_name_bytes(question));
        out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        for answer in answers {
            let (owner, rr_type, ttl, rdata) = match answer {
                Rr::A(owner, octets, ttl) => (*owner, DNS_TYPE_A, *ttl, octets.to_vec()),
                Rr::Aaaa(owner, octets, ttl) => (*owner, DNS_TYPE_AAAA, *ttl, octets.to_vec()),
                Rr::Cname(owner, target) => (*owner, DNS_TYPE_CNAME, 60, dns_name_bytes(target)),
            };
            out.extend_from_slice(&dns_name_bytes(owner));
            out.extend_from_slice(&rr_type.to_be_bytes());
            out.extend_from_slice(&1u16.to_be_bytes());
            out.extend_from_slice(&ttl.to_be_bytes());
            out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            out.extend_from_slice(&rdata);
        }
        out
    }

    /// A TLS `ClientHello` carrying `sni`, enough of one for [`tls_sni`].
    fn tls_client_hello(sni: &str) -> Vec<u8> {
        let mut server_name = Vec::new();
        server_name.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes());
        server_name.push(0);
        server_name.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        server_name.extend_from_slice(sni.as_bytes());

        let mut extensions = Vec::new();
        extensions.extend_from_slice(&0u16.to_be_bytes());
        extensions.extend_from_slice(&(server_name.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&server_name);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x13, 0x01]);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let mut handshake = vec![1];
        handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);

        let mut record = vec![22, 0x03, 0x01];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    /// Encodes `labels` verbatim, so a test can build the wire form of a name
    /// whose labels hold bytes a presentation-form name could not.
    fn dns_labels_bytes(labels: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for label in labels {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    /// A response whose question and single A record both carry `labels`
    /// verbatim, so a test can present a name the encoder in [`dns_packet`]
    /// could not spell.
    fn dns_packet_with_raw_name(labels: &[&str], address: [u8; 4]) -> Vec<u8> {
        let name = dns_labels_bytes(labels);
        let mut out = Vec::new();
        out.extend_from_slice(&0x1234u16.to_be_bytes());
        out.extend_from_slice(&DNS_RESPONSE_FLAGS.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        out.extend_from_slice(&name);
        out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());

        out.extend_from_slice(&name);
        out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&60u32.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&address);
        out
    }

    /// Everything `dns_bindings` attributes to `name`, recorded as the DNS
    /// listener would record it.
    fn bind_all(name: &str, packet: &[u8], now: Instant) -> NameBindings {
        let mut bindings = NameBindings::default();
        for (address, ttl) in dns_bindings(name, packet) {
            bindings.bind(address, name, ttl, now);
        }
        bindings
    }

    /// The parsed form of a query for `name`, transaction id `0x1234`.
    fn asked(name: &str) -> DnsQuery {
        dns_query(&dns_packet(0x1234, DNS_QUERY_FLAGS, name, &[])).expect("query")
    }

    fn http_request(host: &str) -> Vec<u8> {
        format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
    }

    /// The resolver the fake container is holding before attach touches it.
    /// The apostrophe is load-bearing: it is the one byte that can end a
    /// single-quoted shell string, so a restore that mishandles it is a
    /// restore that does not put the file back.
    const ORIGINAL_RESOLV: &str = "nameserver 10.0.2.3\nsearch it's.test\n";

    /// The handle [`FakeRunner`] says nft assigned the table it created.
    const FAKE_HANDLE: u64 = 7;

    /// The `run` seam under test: records every command it is handed and does
    /// whatever the injected plan says that command should do. This is what
    /// lets the ordering, an nft apply that fails, and a cancellation landing
    /// mid-mutation all be exercised with no container, no root and no
    /// `podman` anywhere.
    #[derive(Default)]
    struct FakeRunner {
        ran: Mutex<Vec<String>>,
        /// Fragment of a rendered command that should fail instead of run.
        fail_on: Option<&'static str>,
        /// Fragment of a rendered command that should never complete, which
        /// is how a cancellation is aimed at one mutation in particular.
        hang_on: Option<&'static str>,
        /// What nft answers an apply with, in place of the well-formed echo.
        /// For the outputs an attach cannot read a handle out of.
        echo: Option<Vec<u8>>,
    }

    impl FakeRunner {
        async fn run(&self, cmd: Cmd) -> Result<Vec<u8>> {
            let rendered = cmd.render();
            self.ran
                .lock()
                .expect("fake runner log")
                .push(rendered.clone());
            if self.hang_on.is_some_and(|needle| rendered.contains(needle)) {
                std::future::pending::<()>().await;
            }
            if self.fail_on.is_some_and(|needle| rendered.contains(needle)) {
                return Err(OutrigError::Configuration(format!(
                    "injected failure: {rendered}"
                )));
            }
            // nft answers an `--echo --handle` apply with what it committed;
            // every other command here is a resolver read, whose answer is the
            // snapshot. Without this the tests would exercise only the
            // unnarrowed undo, which is the fallback rather than the rule.
            if rendered.contains("--echo --handle") {
                return Ok(self.echo.clone().unwrap_or_else(|| {
                    format!(
                        "create table inet {} # handle {FAKE_HANDLE}\n",
                        target().table
                    )
                    .into_bytes()
                }));
            }
            Ok(present_snapshot())
        }

        fn ran(&self) -> Vec<String> {
            self.ran.lock().expect("fake runner log").clone()
        }
    }

    /// What [`read_resolv_conf`] answers for a container holding
    /// [`ORIGINAL_RESOLV`].
    fn present_snapshot() -> Vec<u8> {
        let mut snapshot = vec![RESOLV_PRESENT];
        snapshot.extend_from_slice(ORIGINAL_RESOLV.as_bytes());
        snapshot
    }

    /// A target whose pid is this test process: `Rollback` asks whether the
    /// container is still alive before undoing anything, and this one is.
    fn target() -> Target {
        Target {
            name: "outrig-test".to_string(),
            pid: std::process::id(),
            table: "outrig_test".to_string(),
            marker: "outrig_marker".to_string(),
            dns_preconfigured: false,
        }
    }

    /// R5's mechanism, on the real command path. Every other teardown test
    /// injects failure at the runner, which means none of them would notice
    /// `run_step` losing its exit-status check -- and a teardown that quietly
    /// failed is the thing this reports.
    #[tokio::test]
    async fn a_non_zero_undo_reaches_the_caller() {
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        rollback.arm(Cmd::new("/bin/sh").arg("-c").arg("exit 3"));

        let run = |cmd: Cmd| run_step(cmd, None);
        let failures = rollback.undo_now(&run).await;

        assert_eq!(failures.len(), 1, "a non-zero undo is a failure");
        assert!(
            matches!(
                failures[0],
                OutrigError::Process {
                    exit_code: Some(3),
                    ..
                }
            ),
            "and it arrives typed, carrying the status: {:?}",
            failures[0]
        );
    }

    /// The obligations are independent, so one failing is no reason to skip
    /// the rest -- and what failed has to reach the caller naming its
    /// container, not a log line.
    #[tokio::test]
    async fn every_undo_runs_after_one_fails_and_the_failures_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reached = dir.path().join("reached");
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        // Armed first, so it is discharged last: reaching it at all is the
        // claim.
        rollback.arm(touch(&reached));
        rollback.arm(Cmd::new("/bin/sh").arg("-c").arg("exit 1"));

        let run = |cmd: Cmd| run_step(cmd, None);
        let failures = rollback.undo_now(&run).await;

        assert!(reached.exists(), "a failed undo must not cancel the rest");
        assert_eq!(failures.len(), 1);

        let err = teardown_result(
            failures
                .into_iter()
                .map(|source| NetworkTeardownCause {
                    container: "outrig-a".to_string(),
                    source: Box::new(source),
                })
                .collect(),
        )
        .expect_err("a failed undo has to reach the caller");
        assert!(
            err.to_string().contains("outrig-a"),
            "named by the container it belongs to: {err}"
        );
    }

    /// The restore script reproduces whatever that container happened to have.
    /// The bytes travel as `$1` so no quoting rule stands between them and the
    /// file, but the script still has to be right: `printf '%s'` and not
    /// `echo`, no newline of its own, and the redirect where it belongs.
    #[tokio::test]
    async fn the_restore_script_reproduces_arbitrary_bytes() {
        const NASTY: &[u8] =
            b"nameserver 10.0.0.1 # ' \"$(touch pwned)\" `id` \\ '' \n%s%d\noptions x";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        // The guard has to pass for the write to happen at all, so the file
        // starts out holding what an install would have put there.
        std::fs::write(&target, intercepted_resolv("outrig_test")).expect("install");

        // The production script, with only its redirect retargeted.
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));
        run_step(
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script)
                .arg("_")
                .arg(OsString::from_vec(NASTY.to_vec()))
                .arg(intercepted_resolv("outrig_test"))
                .arg(intercepted_resolv("outrig_test").len().to_string()),
            None,
        )
        .await
        .expect("the restore script must run");

        assert_eq!(std::fs::read(&target).expect("restored").as_slice(), NASTY);
        assert!(
            !dir.path().join("pwned").exists(),
            "the snapshot is data, and must never be evaluated"
        );
    }

    /// A command whose only effect is a file a test can wait for.
    fn touch(path: &Path) -> Cmd {
        Cmd::new("/bin/sh")
            .arg("-c")
            .arg(format!("touch {}", path.display()))
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} should have been created by a detached undo",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Attach mutates, and detach undoes, in opposite orders: the redirect
    /// table goes before the resolver that was pointed at it.
    #[tokio::test]
    async fn interception_is_undone_in_the_reverse_of_the_order_it_was_installed() {
        let fake = FakeRunner::default();
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");

        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");
        let failures = rollback.undo_now(&run).await;

        assert!(failures.is_empty(), "{failures:#?}");
        let ran = fake.ran();
        assert_eq!(ran.len(), 5, "{ran:#?}");
        assert!(ran[0].contains("cat /etc/resolv.conf"), "{ran:#?}");
        assert!(ran[1].contains("nameserver 127.0.0.1"), "{ran:#?}");
        assert!(ran[2].contains("nft --echo --handle -f"), "{ran:#?}");
        assert!(
            ran[3].contains("delete table inet handle") && ran[3].contains("nsenter"),
            "{ran:#?}"
        );
        assert!(
            ran[3].contains(&FAKE_HANDLE.to_string()),
            "the handle travels as an argument: {ran:#?}"
        );
        assert!(ran[4].contains("nameserver 10.0.2.3"), "{ran:#?}");
        assert!(rollback.armed().is_empty(), "{:#?}", rollback.armed());
    }

    /// One connection's worth of audit input, for the tests that care about
    /// whether the write landed rather than what it said.
    fn audit_event() -> AuditEvent {
        AuditEvent {
            opened: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            duration: Duration::from_millis(1),
            orig: addr("10.0.2.100:50123"),
            dst: addr("93.184.216.34:443"),
            resolved: ResolvedNames::default(),
            assertion: ClientAssertion::default(),
            service: "-",
            bytes_tx: 0,
            bytes_rx: 0,
            decision: PolicyDecision::allow_default(),
        }
    }

    /// A record the sink promised and could not write. Joining a connection's
    /// task is what `detach` treats as proof its record landed, so a write
    /// that failed has to be kept rather than logged -- otherwise `detach`
    /// returns `Ok(())` with the record simply missing.
    #[tokio::test]
    async fn an_audit_record_that_could_not_be_written_is_reported() {
        // `/dev/full` opens like any file and fails every write with ENOSPC,
        // which is the shape of the audit file's storage filling up after the
        // sink was opened.
        let sink = AuditSink::open(PathBuf::from("/dev/full"), "sid-1".to_string())
            .await
            .expect("open sink");
        let container = sink.for_container("outrig-a");

        write_audit(&container, audit_event()).await;

        let kept = container.take_unwritten();
        assert_eq!(kept.len(), 1, "the failed write must be kept: {kept:#?}");
        assert!(
            matches!(
                kept[0],
                OutrigError::NetworkAuditUnwritten { records: 1, .. }
            ),
            "{kept:#?}"
        );
        assert!(container.take_unwritten().is_empty(), "and taken only once");
        assert!(
            sink.take_unwritten().is_empty(),
            "and attributed to the container whose record it was"
        );
    }

    /// A record is whole or absent, never half. Teardown aborts a connection
    /// that outstays its grace, and an abort lands at an await point -- so a
    /// record written through async I/O can be cut between chunks, leaving a
    /// partial line that makes every record after it unparseable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_writer_does_not_leave_half_a_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);
        let sink = AuditSink::open(path.clone(), "sid-1".to_string())
            .await
            .expect("open sink");
        let container = sink.for_container("outrig-a");

        // A record far larger than any single write buffer, so a cancellable
        // write would have to be more than one chunk.
        let mut event = audit_event();
        event.resolved = (0..4096)
            .map(|n| format!("name{n}.example.test"))
            .collect::<Vec<_>>()
            .into_iter()
            .collect();

        let mut writing = JoinSet::new();
        writing.spawn(async move {
            write_audit(&container, event).await;
        });
        tokio::task::yield_now().await;
        writing.abort_all();
        while writing.join_next().await.is_some() {}

        // A second record, written normally. Without it the assertion below
        // could pass on an empty file, since the abort may land before the
        // write is ever reached; with it there is always a record that a torn
        // prefix ahead of it would make unparseable.
        write_audit(&sink.for_container("outrig-b"), audit_event()).await;

        let text = std::fs::read_to_string(&path).expect("read audit log");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(!lines.is_empty(), "the second record should be on disk");
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("every line in the audit log must be a whole record");
        }
    }

    /// A `stat` that failed is not an answer, and must not be read as one.
    /// Treating it like a device -- which is what "not a regular file" would
    /// mean -- spawns a writer that truncates with nothing claiming the file,
    /// which is the pairing the claim exists to prevent.
    #[test]
    fn a_file_this_cannot_identify_is_not_taken_for_a_device() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);
        std::fs::write(&path, b"").expect("create");

        assert!(
            claims_exclusively(std::fs::metadata(&path)).expect("a readable file"),
            "a regular file is claimed"
        );
        assert!(
            !claims_exclusively(std::fs::metadata("/dev/full")).expect("a readable device"),
            "and a device is not: there is no truncation to protect"
        );
        assert!(
            claims_exclusively(Err(io::Error::from(io::ErrorKind::PermissionDenied))).is_err(),
            "and a file this could not identify is neither"
        );
    }

    /// One writer owns the log, and a second is refused rather than allowed to
    /// destroy the first's records. The writer's rollback truncates the file
    /// back to where a failed record began, and `ftruncate` acts on the inode:
    /// a second writer's fully written, already-acknowledged record sitting
    /// past that mark goes with it, and its owner reports a clean teardown.
    #[tokio::test]
    async fn a_second_writer_for_one_log_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);

        let mut first = AuditSink::open(path.clone(), "sid-1".to_string())
            .await
            .expect("the first owns it");

        let refused = AuditSink::open(path.clone(), "sid-2".to_string())
            .await
            .expect_err("a second writer for the same file is refused");
        assert!(
            refused.to_string().contains("already owned"),
            "and says why: {refused}"
        );

        // Released with the writer, so the next session gets it. Waited for
        // rather than asserted at once: `tokio::fs::File` hands its close to
        // the blocking pool, so the descriptor -- and the lock the kernel ties
        // to it -- goes a moment after the task holding it has ended.
        first.close().await;
        drop(first);
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        loop {
            match AuditSink::open(path.clone(), "sid-3".to_string()).await {
                Ok(_) => break,
                Err(e) => assert!(
                    tokio::time::Instant::now() < deadline,
                    "the lock has to go with the writer that held it: {e}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A connection outliving the abort is reported as itself, not as the
    /// abort. The two say different things -- "the loops had to be stopped"
    /// and "a bridge is still running after that" -- and both windows can
    /// expire in one teardown, which put the same variant in the causes twice
    /// and left the worse of the two unreadable.
    #[tokio::test(start_paused = true)]
    async fn a_connection_that_outlives_the_abort_is_reported_as_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Held for the whole teardown, which is what a connection still
        // running looks like from the attachment's side.
        let (live, idle) = mpsc::channel(1);

        let causes = teardown_attachment(
            "outrig-test",
            Attachment {
                cancel: CancellationToken::new(),
                // Nothing to stop: what is under test is the wait *after*
                // that, so the abort window must not be the one that expires.
                tasks: JoinSet::new(),
                idle,
                rollback: Rollback::new(std::process::id()).expect("a namespace"),
                transcript: None,
                audit: audit_sink(dir.path()).await,
            },
        )
        .await;

        let reported: Vec<String> = causes.iter().map(|c| c.source.to_string()).collect();
        let unfinished = reported
            .iter()
            .find(|r| r.contains("still running"))
            .unwrap_or_else(|| {
                panic!("a connection outliving the abort has to be said: {reported:#?}")
            });
        // Read end to end, not by a fragment: a message assembled across
        // source lines carries the gap between them, and an assertion that
        // stops before the gap passes over it.
        assert!(
            unfinished.starts_with("connections were still running")
                && unfinished.ends_with("after this attachment's tasks were stopped"),
            "the whole message has to read as one: {unfinished:?}"
        );
        assert!(
            !unfinished.contains("  "),
            "and reach a reader without the source's indentation in it: {unfinished:?}"
        );
        assert!(
            !reported.iter().any(|r| r.contains("were aborted")),
            "and not as the abort, which did not happen here: {reported:#?}"
        );
        drop(live);
    }

    /// Teardown waits the connections out even when the accept loop had to be
    /// aborted. Aborting drops the set it kept them in, and dropping a
    /// `JoinSet` asks for an abort without waiting for it -- so a connection
    /// being polled elsewhere outlived the `detach` that had declared it over
    /// and could still queue a record behind the drain marker.
    #[tokio::test]
    async fn a_connection_outliving_an_aborted_accept_loop_is_still_waited_out() {
        // Stands in for the carrier the accept loop owns: the loop is the
        // thing that gets aborted, and what it held goes with it.
        let (live, mut idle) = mpsc::channel::<()>(1);
        let held = live.clone();
        drop(live);
        let connection = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
        });

        // The receiver answers when the last clone is gone, and not before.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), idle.recv())
                .await
                .is_err(),
            "a connection still running is not an idle attachment"
        );
        assert!(
            tokio::time::timeout(SHUTDOWN_GRACE, idle.recv())
                .await
                .is_ok_and(|last| last.is_none()),
            "and the wait ends when it finishes"
        );
        connection.await.expect("the connection ends");
    }

    /// A drain has to come back even when the writer is stuck and the queue
    /// behind it is full. Timing only the acknowledgement leaves `detach`
    /// parked on getting the marker *into* a queue with no free slot, so it
    /// never reports the timeout and never reaches the resolver and nft undos
    /// queued behind it.
    #[tokio::test]
    async fn a_drain_gives_up_on_a_stalled_writer_with_a_full_queue() {
        // A sink whose writer never runs: the receiver is held here and never
        // read, which is what a stalled writer looks like from this side with
        // none of the timing a real stall would need.
        let (records, _stalled) = mpsc::channel(AUDIT_QUEUE);
        let sink = AuditSink {
            writer: Arc::new(Mutex::new(None)),
            records: Some(records),
            session_id: "sid-1".to_string(),
            container: "outrig-a".to_string(),
            generation: 1,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            unwritten: Arc::new(Mutex::new(BTreeMap::new())),
        };
        for _ in 0..AUDIT_QUEUE {
            let (done, _) = tokio::sync::oneshot::channel();
            sink.records
                .as_ref()
                .expect("open")
                .send(AuditJob::Record {
                    who: (1, "outrig-a".to_string()),
                    line: b"{}\n".to_vec(),
                    done,
                })
                .await
                .expect("fill the queue");
        }
        assert_eq!(
            sink.records.as_ref().expect("open").capacity(),
            0,
            "the queue has to be full"
        );

        // Bounded, so an implementation that only times the acknowledgement
        // fails here as an assertion rather than hanging the suite.
        let drained = tokio::time::timeout(SHUTDOWN_GRACE * 3, sink.drain())
            .await
            .expect("the drain has to give up on its own deadline");

        assert!(drained.is_err(), "a stalled writer must not drain cleanly");
    }

    /// What broke the append and what stopped it being undone are different
    /// facts, and a reader needs both: the first says which record was lost,
    /// the second says whether the file can still be trusted.
    #[tokio::test]
    async fn an_audit_loss_carries_both_its_cause_and_its_integrity() {
        let sink = AuditSink::open(PathBuf::from("/dev/full"), "sid-1".to_string())
            .await
            .expect("open sink");
        let container = sink.for_attachment("outrig-a", 1);

        write_audit(&container, audit_event()).await;

        let kept = container.take_unwritten();
        assert_eq!(kept.len(), 1, "{kept:#?}");
        let OutrigError::NetworkAuditUnwritten {
            source, integrity, ..
        } = &kept[0]
        else {
            panic!("expected an audit loss, got {kept:#?}");
        };
        assert!(
            integrity.is_some(),
            "a rollback that could not be proved has to be reported: {kept:#?}"
        );
        assert!(
            source.to_string().contains("space") || source.to_string().contains("full"),
            "and not in place of what broke the append: {source}"
        );
    }

    /// Nothing may record a loss after the sweep that reports them. An
    /// attachment whose drain timed out leaves the writer still holding
    /// records, so the writer has to be ended and waited for first.
    #[tokio::test]
    async fn closing_the_sink_lets_nothing_record_after_the_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);
        let mut sink = AuditSink::open(path.clone(), "sid-1".to_string())
            .await
            .expect("open sink");
        let attached = sink.for_attachment("outrig-a", 1);

        write_audit(&attached, audit_event()).await;
        // Mirrors `shutdown`, which takes the attachments before it closes:
        // the writer ends when the last handle is gone, and this is it.
        drop(attached);

        let closed = sink.close().await;
        assert!(closed.is_none(), "the writer has to finish: {closed:?}");

        // Finished means its queue is on disk, so there is nothing left that
        // could record a loss behind the sweep that follows.
        let text = std::fs::read_to_string(&path).expect("read audit log");
        assert_eq!(
            text.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "closing waits the writer out rather than abandoning what it holds"
        );
        // And closed means closed: a record offered afterwards is refused
        // rather than quietly dropped.
        assert!(
            sink.write(&AuditRecord::new("sid-1", "outrig-a", audit_event()))
                .await
                .is_err(),
            "a closed sink must refuse"
        );
        // A record the writer answered for is not one it was holding. Were it
        // left counted, a clean close would report a loss that never happened
        // -- and a refused record must not be counted either, since nothing
        // ever took it.
        assert!(
            sink.take_every_unwritten().is_empty(),
            "a writer that finished its queue lost nothing"
        );
    }

    /// A producer cancelled while waiting for room in the queue is charged
    /// for nothing. The queue is bounded, so offering a record can wait, and
    /// counting before that wait charged an owner for a record the writer
    /// never saw: reported as lost if the writer was later stopped, and
    /// dropped without a word if it closed cleanly -- a connection record
    /// missing from the log with nothing to say so.
    #[tokio::test(start_paused = true)]
    async fn a_record_cancelled_before_it_is_queued_is_charged_to_nobody() {
        let (records, _queue) = mpsc::channel(AUDIT_QUEUE);
        let pending: AuditPending = Arc::new(Mutex::new(BTreeMap::new()));
        let mut sink = AuditSink {
            writer: Arc::new(Mutex::new(Some(tokio::spawn(std::future::pending())))),
            records: Some(records),
            session_id: "sid-1".to_string(),
            container: String::new(),
            generation: 0,
            pending: pending.clone(),
            unwritten: Arc::new(Mutex::new(BTreeMap::new())),
        };

        // Filled past the sink, so nothing here is counted and the next
        // producer has to wait for room that is never going to come.
        for _ in 0..AUDIT_QUEUE {
            let (done, _) = tokio::sync::oneshot::channel();
            sink.records
                .as_ref()
                .expect("open")
                .send(AuditJob::Record {
                    who: (9, "outrig-filler".to_string()),
                    line: b"{}\n".to_vec(),
                    done,
                })
                .await
                .expect("fill the queue");
        }
        assert!(pending.lock().expect("pending").is_empty());

        let handle = sink.for_attachment("outrig-a", 1);
        let blocked = tokio::spawn(async move { write_audit(&handle, audit_event()).await });
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            pending.lock().expect("pending").is_empty(),
            "a record still waiting for room is not one the writer is holding"
        );

        blocked.abort();
        let _ = blocked.await;

        assert!(
            pending.lock().expect("pending").is_empty(),
            "and a producer that was cancelled there leaves nothing behind"
        );
        // Which is what the accounting is for: stopping the writer now
        // reports the records it actually took, and not this one.
        assert!(sink.close().await.is_some(), "the writer will not stop");
        assert!(
            sink.take_every_unwritten().is_empty(),
            "a record that never reached the queue is not the writer's loss"
        );
    }

    /// A writer that will not stop is aborted, and aborting ends the task
    /// rather than the syscall it submitted: bytes may still land and the
    /// rollback that would have undone them is gone with the task. What it was
    /// holding is lost with it, and who lost what is knowable -- every record
    /// it took was queued by an attachment that is still counted.
    #[tokio::test(start_paused = true)]
    async fn a_writer_that_had_to_be_aborted_reports_what_it_was_holding() {
        // A writer that never takes anything off its queue, which is what a
        // wedged one looks like from this side with none of the timing.
        let (records, _queue) = mpsc::channel(AUDIT_QUEUE);
        let pending: AuditPending = Arc::new(Mutex::new(BTreeMap::new()));
        let mut sink = AuditSink {
            writer: Arc::new(Mutex::new(Some(tokio::spawn(std::future::pending())))),
            records: Some(records),
            session_id: "sid-1".to_string(),
            container: String::new(),
            generation: 0,
            pending: pending.clone(),
            unwritten: Arc::new(Mutex::new(BTreeMap::new())),
        };

        // Two attachments, three records: each producer queues one and then
        // waits to be told it landed, which is where a record the writer is
        // holding leaves its producer.
        for (container, generation) in [("outrig-a", 1), ("outrig-a", 1), ("outrig-b", 2)] {
            let handle = sink.for_attachment(container, generation);
            tokio::spawn(async move { write_audit(&handle, audit_event()).await });
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            pending.lock().expect("pending").len(),
            2,
            "both attachments have to have queued before the close"
        );

        let closed = sink.close().await;
        assert!(
            closed
                .expect("a writer that will not stop is reported")
                .to_string()
                .contains("unaccounted"),
            "and says what it left behind"
        );

        let swept = sink.take_every_unwritten();
        let counted: Vec<(String, u64, bool)> = swept
            .iter()
            .map(|loss| {
                let OutrigError::NetworkAuditUnwritten {
                    container,
                    records,
                    integrity,
                    ..
                } = loss
                else {
                    panic!("expected an audit loss, got {loss:#?}");
                };
                (container.clone(), *records, integrity.is_some())
            })
            .collect();
        assert_eq!(
            counted,
            vec![
                ("outrig-a".to_string(), 2, true),
                ("outrig-b".to_string(), 1, true),
            ],
            "each attachment is told how many of *its* records were lost, and \
             that the file may be short a rollback: {swept:#?}"
        );
    }

    /// A writer stopped while holding nothing lost nothing. Reporting a
    /// record anyway -- under a name no attachment has, which is what a
    /// synthetic entry amounts to -- is an invented loss, and one a reader
    /// cannot act on. The record written first is the other half of it: one
    /// the writer answered for is not one it was still holding.
    #[tokio::test(start_paused = true)]
    async fn a_writer_stopped_holding_nothing_invents_no_loss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sink = AuditSink::open(dir.path().join(NETWORK_LOG), "sid-1".to_string())
            .await
            .expect("open sink");
        let attached = sink.for_attachment("outrig-a", 1);
        write_audit(&attached, audit_event()).await;
        drop(attached);

        // A writer that will not stop, in place of the one that just finished
        // its queue: `close` takes the abort path with nothing outstanding.
        *sink.writer.lock().expect("writer slot") =
            Some(tokio::spawn(std::future::pending::<()>()));

        let closed = sink.close().await;
        assert!(
            closed.is_some(),
            "a writer that would not stop is still worth reporting"
        );
        assert!(
            sink.take_every_unwritten().is_empty(),
            "but nothing was outstanding, so nothing is claimed lost"
        );
    }

    /// A drain that gave up leaves its losses on the books rather than taking
    /// an account it knows is short. Something has to collect them afterwards,
    /// or the concrete loss is never reported at all.
    #[tokio::test]
    async fn losses_left_by_a_timed_out_drain_are_swept_at_shutdown() {
        let sink = AuditSink::open(PathBuf::from("/dev/full"), "sid-1".to_string())
            .await
            .expect("open sink");
        write_audit(&sink.for_attachment("outrig-a", 1), audit_event()).await;
        write_audit(&sink.for_attachment("outrig-b", 2), audit_event()).await;

        // Neither attachment collected its own -- which is what a timed-out
        // drain leaves behind.
        let swept = sink.take_every_unwritten();
        assert_eq!(swept.len(), 2, "{swept:#?}");
        assert!(
            sink.take_every_unwritten().is_empty(),
            "and the sweep takes them only once"
        );
    }

    /// One attachment's audit loss is not another's, even when they share a
    /// container name. A drain that gave up leaves the writer still holding a
    /// record; the failure that follows must not land in the slot the *next*
    /// attachment of that name is reading.
    #[tokio::test]
    async fn an_audit_loss_belongs_to_the_attachment_that_incurred_it() {
        let sink = AuditSink::open(PathBuf::from("/dev/full"), "sid-1".to_string())
            .await
            .expect("open sink");

        // The same container name, attached twice.
        let first = sink.for_attachment("outrig-a", 1);
        let second = sink.for_attachment("outrig-a", 2);

        write_audit(&first, audit_event()).await;

        assert_eq!(
            second.take_unwritten().len(),
            0,
            "a later attachment must not inherit the earlier one's loss"
        );
        assert_eq!(
            first.take_unwritten().len(),
            1,
            "and the attachment that incurred it still gets told"
        );
    }

    /// Teardown drains the writer rather than assuming a joined connection
    /// means its record reached the disk, and reports rather than waiting
    /// forever when it cannot.
    #[tokio::test]
    async fn a_drain_answers_only_once_what_was_queued_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);
        let sink = audit_sink(dir.path()).await;

        write_audit(&sink.for_container("outrig-a"), audit_event()).await;
        sink.drain().await.expect("the writer drains");

        let text = std::fs::read_to_string(&path).expect("read audit log");
        assert_eq!(
            text.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "everything queued before the drain is on disk when it answers"
        );
    }

    /// A sink that keeps failing is a container's to drive: it can open
    /// connections as fast as it likes, and every one of them writes. What is
    /// retained has to be bounded by the number of containers, not by the
    /// number of records lost, or an outage grows host memory for as long as
    /// it lasts and hands teardown an error as long as the outage.
    #[tokio::test]
    async fn a_sink_that_keeps_failing_retains_a_bounded_amount() {
        let sink = AuditSink::open(PathBuf::from("/dev/full"), "sid-1".to_string())
            .await
            .expect("open sink");
        let container = sink.for_container("outrig-a");

        for _ in 0..10_000 {
            write_audit(&container, audit_event()).await;
        }

        assert_eq!(
            sink.unwritten.lock().expect("unwritten").len(),
            1,
            "one entry per container, whatever the record count"
        );
        let kept = container.take_unwritten();
        assert_eq!(kept.len(), 1, "{kept:#?}");
        assert!(
            matches!(
                kept[0],
                OutrigError::NetworkAuditUnwritten {
                    records: 10_000,
                    ..
                }
            ),
            "and the count says how much was lost: {kept:#?}"
        );
        assert_eq!(
            std::fs::metadata("/dev/full").map(|m| m.len()).unwrap_or(0),
            0,
            "a failed write leaves nothing behind to corrupt the next record"
        );
    }

    /// The resolver is written before the table exists, so its marker is
    /// readable inside the container first. If the marker *were* the table's
    /// name, that would hand an actor in the namespace the one thing it needs
    /// to create the table ahead of outrig -- making the `create` fail and the
    /// rollback delete a table this attach never made.
    #[test]
    fn a_resolver_marker_never_names_the_table_it_was_attached_with() {
        for _ in 0..64 {
            let target = Target::for_attach("outrig-a", 1, "sid-1", false);
            assert_ne!(
                target.marker, target.table,
                "publishing the marker must tell nobody the table's name"
            );
            assert!(
                !target.marker.contains(&target.table) && !target.table.contains(&target.marker),
                "nor any part of it: {target:?}"
            );
        }
    }

    /// A writer that will not stop is stopped. Moving the handle into a
    /// timeout and letting it drop *detaches* the task -- which would go on
    /// appending and recording losses after the sweep meant to be the last
    /// word on both, which is the defect this whole task started from, one
    /// level down.
    #[tokio::test]
    async fn closing_a_stuck_writer_ends_it_rather_than_detaching_it() {
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ended.clone();
        let stuck = tokio::spawn(async move {
            struct Ends(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Ends {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _ends = Ends(flag);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let (records, _queue) = mpsc::channel(1);
        let mut sink = AuditSink {
            writer: Arc::new(Mutex::new(Some(stuck))),
            records: Some(records),
            session_id: "sid-1".to_string(),
            container: String::new(),
            generation: 0,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            unwritten: Arc::new(Mutex::new(BTreeMap::new())),
        };

        let closed = sink.close().await;

        assert!(closed.is_some(), "a writer that would not stop is reported");
        assert!(
            ended.load(std::sync::atomic::Ordering::SeqCst),
            "and is actually gone, not merely unwatched"
        );
    }

    /// The undo deletes a table by name, so the name has to be one nothing
    /// else could be holding. A deterministic one can be held by a stale table
    /// from a crashed run of the same session, by an operator's own, or by
    /// another actor creating it between a check and an apply -- and that last
    /// window is why this is an identity rather than a preflight check.
    #[test]
    fn every_attach_gets_a_table_name_of_its_own() {
        let names: std::collections::BTreeSet<String> =
            (0..256).map(|_| nft_table_name("sid-1")).collect();
        assert_eq!(
            names.len(),
            256,
            "a repeated name is a name something else can hold"
        );
        for name in &names {
            assert!(
                name.starts_with("outrig_sid_1_"),
                "still recognizable as outrig's: {name}"
            );
        }
        assert_ne!(
            nft_table_name("sid-1"),
            nft_table_name("sid-1"),
            "two attaches in one session must not share a table"
        );
    }

    /// Cancelled with an undo in flight. The command has to still be armed,
    /// or the destructor has nothing to reissue and the container keeps
    /// whatever that undo was going to take back.
    #[tokio::test]
    async fn a_cancelled_undo_is_still_armed_for_the_destructor() {
        let fake = FakeRunner {
            hang_on: Some("delete table"),
            ..FakeRunner::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");
        assert_eq!(rollback.armed().len(), 2);

        {
            let mut undoing = Box::pin(rollback.undo_now(&run));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        _ = &mut undoing => panic!("the delete is held"),
                        () = tokio::time::sleep(Duration::from_millis(5)) => {}
                    }
                    if fake.ran().iter().any(|cmd| cmd.contains("delete table")) {
                        break;
                    }
                }
            })
            .await
            .expect("the undo never started");
        }

        assert_eq!(
            rollback.armed().len(),
            2,
            "a cancelled undo gives up nothing the rollback was holding: {:#?}",
            rollback.armed()
        );
    }

    /// What survives a partial undo survives in arming order, because `Drop`
    /// reverses it. Retained newest-first it would be reversed a second time
    /// and put the resolver back before removing the redirect aimed at it.
    #[tokio::test]
    async fn a_partly_discharged_rollback_keeps_its_arming_order() {
        let installing = FakeRunner::default();
        let run = |cmd: Cmd| installing.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");

        let undoing = FakeRunner {
            fail_on: Some("nsenter"),
            ..FakeRunner::default()
        };
        let undo = |cmd: Cmd| undoing.run(cmd);
        let failures = rollback.undo_now(&undo).await;
        assert_eq!(failures.len(), 2, "{failures:#?}");

        let armed = rollback.armed();
        assert_eq!(armed.len(), 2, "{armed:#?}");
        assert!(
            armed[0].contains("nameserver 10.0.2.3"),
            "the resolver restore was armed first and must stay first: {armed:#?}"
        );
        assert!(
            armed[1].contains("delete table"),
            "and the nft delete, armed second, must stay second: {armed:#?}"
        );
    }

    /// The undo that fails is not discharged. It stays armed so the destructor
    /// reissues it, which is the difference between reporting a failure and
    /// forgetting an obligation.
    #[tokio::test]
    async fn an_undo_that_fails_stays_armed_for_the_destructor() {
        let installing = FakeRunner::default();
        let run = |cmd: Cmd| installing.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");

        let undoing = FakeRunner {
            fail_on: Some("nameserver 10.0.2.3"),
            ..FakeRunner::default()
        };
        let undo = |cmd: Cmd| undoing.run(cmd);
        let residue = rollback.undo_now(&undo).await;

        assert_eq!(residue.len(), 1, "{residue:#?}");
        assert_eq!(
            rollback.armed().len(),
            1,
            "the failed restore must still be armed: {:#?}",
            rollback.armed()
        );
    }

    /// An apply that fails and an inverse that fails with it. "The attach
    /// failed" and "the container is not back the way it was" are different
    /// things to be told, and only the second means it cannot be retried, so
    /// the caller is owed both.
    #[tokio::test]
    async fn an_attach_whose_rollback_also_fails_reports_both() {
        let fake = FakeRunner {
            fail_on: Some("--echo --handle"),
            ..FakeRunner::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");

        let cause = install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect_err("the apply was injected to fail");

        let failing_undo = FakeRunner {
            fail_on: Some("nameserver 10.0.2.3"),
            ..FakeRunner::default()
        };
        let undo = |cmd: Cmd| failing_undo.run(cmd);
        let residue = rollback.undo_now(&undo).await;
        assert!(!residue.is_empty(), "the restore was injected to fail");

        let reported = OutrigError::NetworkAttachNotUndone(Box::new(NetworkAttachFailure {
            container: "outrig-test".to_string(),
            source: Box::new(cause),
            residue,
        }));
        let text = reported.to_string();
        assert!(text.contains("outrig-test"), "{text}");
        assert!(text.contains("could not be fully undone"), "{text}");
        assert!(
            text.contains("nameserver 10.0.2.3"),
            "the residue is named, not just counted: {text}"
        );
    }

    /// A resolver that is a link to nothing. `[ -e ]` calls it absent, but
    /// installing would follow the link and create its target, and the undo
    /// for an absent resolver is `rm -f` -- which would take the link and
    /// leave the file. Refused before anything is written.
    #[tokio::test]
    async fn a_dangling_resolver_link_refuses_the_attach() {
        let err = checked_resolv_snapshot("outrig-test", vec![RESOLV_DANGLING])
            .expect_err("a dangling link cannot be put back");
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }

    /// The bytes ride back as one `execve` argument, which is binary-safe
    /// except for the two things an argument cannot be. Both are refused
    /// before the resolver is touched, because the alternative is finding out
    /// afterwards -- with loopback DNS installed and no way to undo it.
    #[tokio::test]
    async fn a_resolver_no_argument_could_carry_refuses_the_attach() {
        let mut with_nul = vec![RESOLV_PRESENT];
        with_nul.extend_from_slice(b"nameserver 10.0.0.1\0\n");
        let err = checked_resolv_snapshot("outrig-test", with_nul.clone())
            .expect_err("a NUL cannot be carried in an argument");
        assert!(err.to_string().contains("NUL"), "{err}");

        let mut oversized = vec![RESOLV_PRESENT];
        oversized.resize(MAX_RESOLV_SNAPSHOT + 2, b'x');
        let err = checked_resolv_snapshot("outrig-test", oversized)
            .expect_err("an oversized resolver cannot be carried in an argument");
        assert!(err.to_string().contains("past the"), "{err}");

        // The refusal is not theoretical: the command such a snapshot would
        // build cannot be spawned at all, which is why the check has to come
        // before the mutation rather than after it.
        let doomed = write_resolv_conf(std::process::id(), "outrig_test", with_nul[1..].to_vec());
        assert!(
            doomed.to_tokio_command().spawn().is_err(),
            "a NUL in argv is refused by the kernel interface itself"
        );
    }

    /// The undo only fires if the resolver still holds what this attach put
    /// there. These commands name a namespace by pid and the kernel reuses
    /// pids, so an undo delayed past its container's exit -- by a slow command
    /// ahead of it in the chain, or a destructor firing late -- would
    /// otherwise write one container's resolver into whatever holds that pid
    /// now. The real script is run here, not a paraphrase of it.
    #[tokio::test]
    async fn a_restore_leaves_a_resolver_it_did_not_install_alone() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\nsearch example.test\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));
        let restore = |content: &[u8]| {
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script.clone())
                .arg("_")
                .arg(OsString::from_vec(content.to_vec()))
                .arg(intercepted_resolv("outrig_test"))
                .arg(intercepted_resolv("outrig_test").len().to_string())
        };

        // Holding what the install wrote: the undo fires.
        std::fs::write(&target, intercepted_resolv("outrig_test")).expect("install");
        run_step(restore(ORIGINAL), None)
            .await
            .expect("the restore must run");
        assert_eq!(std::fs::read(&target).expect("restored"), ORIGINAL);

        // Holding something else -- a replacement container behind a reused
        // pid, or a resolver something changed after interception: left alone.
        std::fs::write(&target, b"nameserver 9.9.9.9\n").expect("third party");
        run_step(restore(ORIGINAL), None)
            .await
            .expect("the guard makes this a no-op, not a failure");
        assert_eq!(
            std::fs::read(&target).expect("untouched"),
            b"nameserver 9.9.9.9\n",
            "an undo must not clobber a resolver it did not install"
        );
    }

    /// A terminal newline added or removed is byte-distinct state, and taking
    /// it back would overwrite it. Command substitution strips trailing
    /// newlines, so without a sentinel inside it the two compare equal.
    #[tokio::test]
    async fn a_restore_sees_a_trailing_newline_added_or_removed() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

        for changed in [
            format!("{}\n", intercepted_resolv("outrig_test")),
            intercepted_resolv("outrig_test")
                .trim_end_matches('\n')
                .to_string(),
        ] {
            std::fs::write(&target, &changed).expect("write");
            run_step(
                Cmd::new("/bin/sh")
                    .args(["-c"])
                    .arg(script.clone())
                    .arg("_")
                    .arg(OsString::from_vec(ORIGINAL.to_vec()))
                    .arg(intercepted_resolv("outrig_test"))
                    .arg(intercepted_resolv("outrig_test").len().to_string()),
                None,
            )
            .await
            .expect("the guard makes this a no-op, not a failure");

            assert_eq!(
                std::fs::read_to_string(&target).expect("untouched"),
                changed,
                "a file differing only in its terminal newline is still a \
                 different file: {changed:?}"
            );
        }
    }

    /// A byte the shell cannot carry is still a byte in the file. Shells
    /// differ on what command substitution does with an embedded NUL, so the
    /// guard checks the file's own byte count as well as its text.
    #[tokio::test]
    async fn a_restore_sees_a_nul_added_after_it_installed() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

        // What the install wrote, with a NUL inserted into it.
        let mut tampered = intercepted_resolv("outrig_test").into_bytes();
        tampered.insert(0, 0);
        std::fs::write(&target, &tampered).expect("tamper");

        run_step(
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script)
                .arg("_")
                .arg(OsString::from_vec(ORIGINAL.to_vec()))
                .arg(intercepted_resolv("outrig_test"))
                .arg(intercepted_resolv("outrig_test").len().to_string()),
            None,
        )
        .await
        .expect("the guard makes this a no-op, not a failure");

        assert_eq!(
            std::fs::read(&target).expect("untouched"),
            tampered,
            "a resolver carrying bytes the install never wrote is not this attach's"
        );
    }

    /// A resolver that cannot be read is not a resolver that belongs to
    /// someone else. Discarding the read's status made the two look alike, so
    /// the undo exited zero, teardown struck it off, and `detach` reported
    /// success over a container still pointing at a stopped listener.
    #[tokio::test]
    async fn an_unreadable_resolver_fails_the_undo_rather_than_skipping_it() {
        use std::os::unix::fs::PermissionsExt as _;
        if nix::unistd::Uid::effective().is_root() {
            // Root reads it regardless, so there is nothing to observe.
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");

        for script in [RESTORE_RESOLV_SCRIPT, REMOVE_RESOLV_SCRIPT] {
            std::fs::write(&target, intercepted_resolv("outrig_test")).expect("install");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000))
                .expect("make unreadable");
            let script =
                script.replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

            let outcome = run_step(
                Cmd::new("/bin/sh")
                    .args(["-c"])
                    .arg(script)
                    .arg("_")
                    .arg("nameserver 10.0.2.3\n")
                    .arg(intercepted_resolv("outrig_test"))
                    .arg(intercepted_resolv("outrig_test").len().to_string()),
                None,
            )
            .await;

            assert!(
                outcome.is_err(),
                "an unreadable resolver has to fail the undo, not retire it"
            );
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
                .expect("restore permissions");
        }
    }

    /// An absent resolver owes nothing, and must not be confused with one that
    /// is there and could not be read.
    #[tokio::test]
    async fn an_absent_resolver_retires_the_undo_quietly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

        run_step(
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script)
                .arg("_")
                .arg("nameserver 10.0.2.3\n")
                .arg(intercepted_resolv("outrig_test"))
                .arg(intercepted_resolv("outrig_test").len().to_string()),
            None,
        )
        .await
        .expect("an absent resolver is not a failure");
        assert!(!target.exists(), "and nothing is written in its place");
    }

    /// Neither undo may depend on a utility a container is not required to
    /// have. The obvious spelling used `cmp`, which a minimal image need not
    /// ship: a missing one exits 127, `|| exit 0` reads that as "not ours",
    /// and the undo skips while `detach` reports success -- the resolver left
    /// pointing at a listener that has stopped.
    #[tokio::test]
    async fn neither_undo_needs_anything_beyond_a_shell_and_cat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        // A PATH holding only what the undos actually declare: `cat`, which
        // the snapshot already needs, and `rm`, which the absent-resolver undo
        // is. `wc` and `cmp` are not declared, and an undo that quietly
        // retires itself because one of them is missing is the failure this
        // is for.
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        for tool in ["cat", "rm"] {
            let real = ["/bin", "/usr/bin"]
                .into_iter()
                .map(|d| PathBuf::from(d).join(tool))
                .find(|p| p.exists())
                .unwrap_or_else(|| panic!("no {tool} to link"));
            std::os::unix::fs::symlink(&real, bin.join(tool)).expect("link tool");
        }

        for (script, expected) in [
            (RESTORE_RESOLV_SCRIPT, b"nameserver 10.0.2.3\n".to_vec()),
            (REMOVE_RESOLV_SCRIPT, Vec::new()),
        ] {
            std::fs::write(&target, intercepted_resolv("outrig_test")).expect("install");
            let script =
                script.replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));
            // Only what the snapshot already needs is on PATH: no `cmp`, no
            // `wc`. An undo that quietly retires itself because a utility is
            // missing is the failure this is for.
            let pared = format!("PATH={}; {script}", bin.display());
            run_step(
                Cmd::new("/bin/sh")
                    .args(["-c"])
                    .arg(pared)
                    .arg("_")
                    .arg(OsString::from_vec(b"nameserver 10.0.2.3\n".to_vec()))
                    .arg(intercepted_resolv("outrig_test"))
                    .arg(intercepted_resolv("outrig_test").len().to_string()),
                None,
            )
            .await
            .expect("the undo must run");

            if expected.is_empty() {
                assert!(!target.exists(), "the remove must have happened");
            } else {
                assert_eq!(std::fs::read(&target).expect("restored"), expected);
            }
        }
    }

    /// A resolver something has legitimately changed since is no longer the
    /// one this attach installed, and taking it back would discard that
    /// change. The marker says who installed it; the rest of the text says
    /// whether it is still what was installed, and both have to hold.
    #[tokio::test]
    async fn a_restore_keeps_a_change_made_after_it_was_installed() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

        for changed in [
            // A resolver manager pointing somewhere else, marker untouched.
            intercepted_resolv("outrig_test").replace("127.0.0.1", "9.9.9.9"),
            // An option added, marker untouched.
            format!("{}search added.test\n", intercepted_resolv("outrig_test")),
        ] {
            std::fs::write(&target, &changed).expect("write");
            run_step(
                Cmd::new("/bin/sh")
                    .args(["-c"])
                    .arg(script.clone())
                    .arg("_")
                    .arg(OsString::from_vec(ORIGINAL.to_vec()))
                    .arg(intercepted_resolv("outrig_test"))
                    .arg(intercepted_resolv("outrig_test").len().to_string()),
                None,
            )
            .await
            .expect("the guard makes this a no-op, not a failure");

            assert_eq!(
                std::fs::read_to_string(&target).expect("untouched"),
                changed,
                "an undo must not discard a change made after it installed: {changed:?}"
            );
        }
    }

    /// A pid reused by *another outrig-attached container* is the case a
    /// shared sentinel cannot see: every attachment installs the same resolver
    /// text, so the guard would pass and write the first container's resolver
    /// into the second. The sentinel carries the attach's own table name,
    /// which is unique to it.
    #[tokio::test]
    async fn a_restore_leaves_another_outrig_containers_resolver_alone() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\n";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("resolv.conf");
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));

        // The file holds what a *different* attachment installed.
        std::fs::write(&target, intercepted_resolv("outrig_other")).expect("other install");
        run_step(
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script)
                .arg("_")
                .arg(OsString::from_vec(ORIGINAL.to_vec()))
                .arg(intercepted_resolv("outrig_test"))
                .arg(intercepted_resolv("outrig_test").len().to_string()),
            None,
        )
        .await
        .expect("the guard makes this a no-op, not a failure");

        assert_eq!(
            std::fs::read(&target).expect("untouched"),
            intercepted_resolv("outrig_other").into_bytes(),
            "one attachment's undo must not reach another's container"
        );
    }

    /// The resolver a restore writes is the bytes the snapshot read, with
    /// nothing in between: they travel as an argument, so no quoting rule has
    /// to hold for the file to come back exactly as it was.
    #[test]
    fn a_restore_carries_the_original_resolver_bytes_verbatim() {
        let restore = restore_resolv_conf(std::process::id(), "outrig_test", present_snapshot());
        assert!(
            restore
                .args
                .iter()
                .any(|arg| arg.as_os_str().as_bytes() == ORIGINAL_RESOLV.as_bytes()),
            "the snapshot travels as its own argument, whole: {:#?}",
            restore.args
        );
    }

    /// Having no resolver file at all is a state too, and the one a bare `cat`
    /// could neither report nor put back: the restore for it removes the file
    /// the install created rather than leaving an empty one behind.
    #[test]
    fn a_container_with_no_resolver_file_is_restored_to_having_none() {
        let restore = restore_resolv_conf(std::process::id(), "outrig_test", b"0".to_vec());
        let rendered = restore.render();
        assert!(rendered.contains("rm -f /etc/resolv.conf"), "{rendered}");
    }

    /// A failed nft apply is a failed attach, and the resolver mutation that
    /// preceded it is still undone: the container gets its own file back even
    /// though the table never landed.
    #[tokio::test]
    async fn a_failed_nft_apply_still_restores_the_resolver() {
        let fake = FakeRunner {
            fail_on: Some("--echo --handle"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");

        let installed = install_interception(&run, &mut rollback, &target(), 4001, 4002).await;
        assert!(installed.is_err(), "the nft apply was injected to fail");
        rollback.undo_now(&run).await;

        let ran = fake.ran();
        assert_eq!(ran.len(), 5, "{ran:#?}");
        assert!(ran[3].contains("delete table inet outrig_test"), "{ran:#?}");
        assert!(ran[4].contains("nameserver 10.0.2.3"), "{ran:#?}");
    }

    /// Cancelled with the resolver install in flight. The undo log belongs to
    /// the caller, not to the dropped future, so the restore is still armed
    /// afterwards -- armed before the install ran, precisely so that a
    /// cancellation landing inside it cannot slip between the two.
    #[tokio::test]
    async fn cancelling_the_resolver_install_leaves_the_restore_armed() {
        // Only the write matches: the snapshot `printf`s too, but nothing
        // redirects into the file except the install this aims at.
        let fake = FakeRunner {
            hang_on: Some("> /etc/resolv.conf"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let target = target();
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        {
            let mut installing = Box::pin(install_interception(
                &run,
                &mut rollback,
                &target,
                4001,
                4002,
            ));
            assert!(
                futures_util::poll!(&mut installing).is_pending(),
                "the injected install never completes"
            );
        }

        let armed = rollback.armed();
        assert_eq!(armed.len(), 1, "{armed:#?}");
        assert!(armed[0].contains("nameserver 10.0.2.3"), "{armed:#?}");
    }

    /// Cancelled with the nft apply in flight: both mutations are armed, so
    /// the table is deleted whether or not the transaction landed and the
    /// resolver goes back either way.
    #[tokio::test]
    async fn cancelling_the_nft_apply_leaves_both_undos_armed() {
        let fake = FakeRunner {
            hang_on: Some("--echo --handle"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let target = target();
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        {
            let mut installing = Box::pin(install_interception(
                &run,
                &mut rollback,
                &target,
                4001,
                4002,
            ));
            assert!(
                futures_util::poll!(&mut installing).is_pending(),
                "the injected nft apply never completes"
            );
        }

        let armed = rollback.armed();
        assert_eq!(armed.len(), 2, "{armed:#?}");
        assert!(armed[0].contains("nameserver 10.0.2.3"), "{armed:#?}");
        assert!(
            armed[1].contains("delete table inet outrig_test"),
            "{armed:#?}"
        );
    }

    /// A container whose resolver was baked in at create time is neither read
    /// nor written, so there is nothing about it to put back.
    #[tokio::test]
    async fn a_dns_preconfigured_container_has_no_resolver_to_restore() {
        let fake = FakeRunner::default();
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        let target = Target {
            dns_preconfigured: true,
            ..target()
        };

        install_interception(&run, &mut rollback, &target, 4001, 4002)
            .await
            .expect("install interception");

        let ran = fake.ran();
        assert_eq!(ran.len(), 1, "{ran:#?}");
        assert!(ran[0].contains("nft --echo --handle -f"), "{ran:#?}");
        assert_eq!(rollback.armed().len(), 1, "{:#?}", rollback.armed());
    }

    /// The undo armed before the apply names the table, because there is no
    /// handle to name until the kernel has made it -- and the undo left armed
    /// afterwards names the handle, which is what a table recreated under this
    /// name by someone else in the namespace does not answer to.
    #[tokio::test]
    async fn the_table_undo_is_narrowed_to_the_handle_the_kernel_assigned() {
        let fake = FakeRunner::default();
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");

        let armed = rollback.armed();
        let undo = armed
            .iter()
            .find(|cmd| cmd.contains("delete table"))
            .unwrap_or_else(|| panic!("the table undo must stay armed: {armed:#?}"));
        assert!(
            undo.contains("delete table inet handle") && undo.contains(&FAKE_HANDLE.to_string()),
            "the undo must name the handle the create reported: {undo}"
        );
        assert!(
            !undo.contains("outrig_test"),
            "and by the handle *alone*: a name beside it is a second command \
             whose success says the name resolves somewhere, not that it \
             resolves to the table being deleted: {undo}"
        );
        // And it takes the namespace with it rather than trusting a check made
        // before `nsenter` resolved the pid.
        let NamespaceAnswer::Is(expected) = namespace_id(std::process::id()) else {
            panic!("this process has a namespace");
        };
        assert!(
            undo.contains("readlink /proc/self/ns/net")
                && undo.contains(&format!("net:[{}]", expected.1)),
            "the undo must check the namespace it lands in: {undo}"
        );
    }

    /// Cancelled *in* the apply, the undo is the name and nothing more --
    /// deliberately, and this is the one window in which that is so.
    ///
    /// The handle exists only in the output of the transaction that assigned
    /// it, so a cancellation that never sees that output has nothing narrower
    /// to arm. The alternatives are worse than the race they would close: arm
    /// nothing, and a container is left with a redirect to a listener that has
    /// stopped and nothing coming to remove it; or resolve the identity at
    /// removal time, which is the check-then-act this was built to avoid. The
    /// exposure is an actor holding `NET_ADMIN` in the container's own network
    /// namespace -- not granted by default -- who can also delete and recreate
    /// the table inside the interval between nft's commit and this undo being
    /// issued.
    #[tokio::test]
    async fn a_cancelled_apply_leaves_the_name_alone_armed_and_nothing_narrower() {
        let fake = FakeRunner {
            hang_on: Some("--echo --handle"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id()).expect("this process has a namespace");
        let target = target();
        {
            let installing = install_interception(&run, &mut rollback, &target, 4001, 4002);
            let cancelled = tokio::time::timeout(Duration::from_millis(50), installing).await;
            assert!(cancelled.is_err(), "the apply was injected to hang");
        }

        let armed = rollback.armed();
        assert!(
            armed
                .iter()
                .any(|cmd| cmd.contains("delete table inet outrig_test")),
            "what the apply may have committed is still owed a removal: {armed:#?}"
        );
        assert!(
            !armed.iter().any(|cmd| cmd.contains("handle")),
            "and there is no handle to have narrowed it with: {armed:#?}"
        );
    }

    /// Ownership comes from the transaction that made the table and from
    /// nowhere else, and with no ownership there is no removal -- not later,
    /// and not on the way out either. A delete that can only name its target
    /// is not this attach's to issue once the transaction has committed and
    /// made that name visible: a table put there in between is the one it
    /// would reach. What was made is reported as left behind instead, which is
    /// what makes it recoverable by hand.
    #[tokio::test]
    async fn a_table_no_handle_can_be_had_for_is_left_and_reported_rather_than_removed() {
        for echo in [
            // An nft too old for `--echo`, or one that stopped echoing.
            Vec::new(),
            // Echoed, but with no handle in it.
            b"create table inet outrig_test\n".to_vec(),
            // A handle, but for a table this attach did not create.
            b"create table inet someone_else # handle 4\n".to_vec(),
            // Not text at all.
            vec![0xff, 0xfe, 0x00],
        ] {
            let fake = FakeRunner {
                echo: Some(echo.clone()),
                ..Default::default()
            };
            let run = |cmd: Cmd| fake.run(cmd);
            let mut rollback =
                Rollback::new(std::process::id()).expect("this process has a namespace");

            let failed = install_interception(&run, &mut rollback, &target(), 4001, 4002)
                .await
                .expect_err("a table nothing can name exactly fails the attach");
            assert!(
                failed.to_string().contains("no handle"),
                "and says why: {failed} ({echo:?})"
            );

            assert!(
                !fake.ran().iter().any(|cmd| cmd.contains("delete table")),
                "no removal is issued by name at all: {:#?}",
                fake.ran()
            );
            let armed = rollback.armed();
            assert!(
                !armed.iter().any(|cmd| cmd.contains("delete table")),
                "and none is left armed for later: {armed:#?}"
            );

            // What it made is what the caller is told about.
            let residue = rollback.undo_now(&run).await;
            assert!(
                residue
                    .iter()
                    .any(|e| e.to_string().contains("still in the container")),
                "the table left behind has to reach the caller: {residue:#?}"
            );
        }
    }

    /// The handle is read from the echo of the transaction that created the
    /// table, and only for the table this attach made. Anything else, and the
    /// undo would be narrowed to something this attach does not own.
    #[test]
    fn a_handle_is_taken_only_from_this_attachs_own_create() {
        let echo = b"create table inet outrig_test # handle 4\n\
                     # new generation 2 by process 111 (nft)\n";
        assert_eq!(table_handle_in(echo, "outrig_test"), Some(4));

        // Another table in the same echo is not this one.
        assert_eq!(table_handle_in(echo, "outrig_other"), None);
        // An echo that says nothing about handles -- an nft too old for
        // `--echo`, or one whose output moved -- leaves the undo unnarrowed
        // rather than guessing.
        assert_eq!(
            table_handle_in(b"create table inet outrig_test\n", "outrig_test"),
            None
        );
        assert_eq!(table_handle_in(b"", "outrig_test"), None);
    }

    /// A `Rollback` that is dropped rather than discharged still runs what it
    /// is holding. There is no runtime here on purpose: this is the path a
    /// destructor takes, and a rollback that needed a task to spawn on would
    /// not survive the runtime it was cancelled with.
    #[test]
    fn a_dropped_rollback_still_issues_its_undo_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolver = dir.path().join("resolver");
        let table = dir.path().join("table");

        {
            let mut rollback =
                Rollback::new(std::process::id()).expect("this process has a namespace");
            rollback.arm(touch(&resolver));
            rollback.arm(touch(&table));
        }

        wait_for(&resolver);
        wait_for(&table);
    }

    /// Detaching a container that has already exited is a success: its
    /// namespace went with it, so there is no table left to delete and no
    /// resolv.conf left worth restoring.
    ///
    /// The same check is what stops an undo entering a *different* container's
    /// namespace. The pid is only how `nsenter` gets there; what is compared
    /// is the namespace instance the undos were armed against, so a pid handed
    /// on to something else reads the same as a pid whose process is gone --
    /// which is the answer that matters, because acting there is worse than
    /// not acting at all.
    #[tokio::test]
    async fn an_undo_whose_namespace_is_not_the_one_it_was_armed_against_does_nothing() {
        let fake = FakeRunner {
            fail_on: Some("podman"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        // A live pid -- this process -- recorded against a namespace that is
        // not the one behind it. No nsfs entry is device 0 inode 0, so this is
        // "the namespace these undos are for is not the one at that pid now",
        // whether it exited or was handed on.
        let mut rollback = Rollback {
            pid: std::process::id(),
            netns: (0, 0),
            undo: Vec::new(),
            left_behind: Vec::new(),
        };
        rollback.arm(write_resolv_conf(
            std::process::id(),
            "outrig_test",
            ORIGINAL_RESOLV.as_bytes().to_vec(),
        ));

        let failures = rollback.undo_now(&run).await;

        assert!(failures.is_empty(), "{failures:#?}");
        assert!(fake.ran().is_empty(), "{:#?}", fake.ran());
    }

    /// An answer the kernel would not give is not "the namespace is gone".
    /// Reading it that way discharged every armed undo -- the redirect table
    /// stays installed, `detach` returns `Ok(())`, and the container keeps
    /// sending through a rule aimed at a listener that has stopped. Only a
    /// namespace that is provably absent ends an obligation.
    #[test]
    fn only_a_namespace_that_is_gone_ends_an_obligation() {
        let still_there = || false;
        assert!(matches!(
            classify_namespace(Ok((5, 4_026_531_833)), still_there),
            NamespaceAnswer::Is((5, 4_026_531_833))
        ));
        // The two ways the kernel says the task went: the link is absent, or
        // the lookup behind it lost the race with an exit.
        assert!(matches!(
            classify_namespace(Err(io::Error::from(io::ErrorKind::NotFound)), still_there),
            NamespaceAnswer::Gone
        ));
        assert!(matches!(
            classify_namespace(Err(io::Error::from_raw_os_error(libc::ESRCH)), still_there),
            NamespaceAnswer::Gone
        ));
        // Refused permission is whichever the pid's own directory says: gone
        // if the task went while being looked at, unknown if it is simply not
        // ours to look at.
        let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(
            classify_namespace(Err(denied()), || true),
            NamespaceAnswer::Gone
        ));
        assert!(matches!(
            classify_namespace(Err(denied()), || false),
            NamespaceAnswer::Unknown
        ));
        // Everything else: the question was not answered, which is not the
        // same as answered "no".
        for kind in [
            io::ErrorKind::OutOfMemory,
            io::ErrorKind::Interrupted,
            io::ErrorKind::Other,
        ] {
            assert!(
                matches!(
                    classify_namespace(Err(io::Error::from(kind)), || true),
                    NamespaceAnswer::Unknown
                ),
                "{kind:?} says nothing about whether the namespace is there"
            );
        }
    }

    /// The destructor asks the same question as the awaited path. A detached
    /// undo is the one that runs latest and so the one most likely to find the
    /// pid moved on, and it is also the one nothing is left to report from.
    #[test]
    fn a_dropped_rollback_whose_namespace_moved_on_issues_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("issued");
        {
            let mut rollback = Rollback {
                pid: std::process::id(),
                netns: (0, 0),
                undo: Vec::new(),
                left_behind: Vec::new(),
            };
            rollback.arm(touch(&marker));
        }
        // Long enough that a detached command would have run: the reaper
        // starts it as soon as it is handed over.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !marker.exists(),
            "a destructor must not carry an undo into a namespace that is not \
             the one it was armed against"
        );
    }

    /// A namespace that cannot be identified is a refusal to arm anything,
    /// asked before the first mutation: an undo that could not later tell its
    /// namespace from a stranger's is one that must not run, and the moment to
    /// discover that is while there is still nothing to undo.
    #[test]
    fn a_rollback_refuses_a_namespace_it_cannot_identify() {
        // No process can hold pid 0, so nothing names a namespace here.
        let refused = Rollback::new(0).expect_err("pid 0 has no namespace");
        assert!(
            refused.to_string().contains("could not be identified"),
            "{refused}"
        );
    }

    // -- Ending the connections a detach owes (R3) and holding no state for
    // -- the ones it does not (R4). These drive `handle_tcp` and `dns_loop`
    // -- over real loopback sockets: no container, no root, no nft.

    fn alive_tasks() -> usize {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    }

    fn allow_all_policy() -> Arc<CompiledNetworkPolicy> {
        Arc::new(compiled(NetworkPolicy::allow_all()))
    }

    /// Denies everything. `SO_ORIGINAL_DST` on a connection nothing redirected
    /// reports the accepting listener's own address, so an allowed connection
    /// through a loop bound to that listener is bridged straight back into it
    /// -- each one accepted, bridged, and accepted again.
    fn deny_all_policy() -> Arc<CompiledNetworkPolicy> {
        Arc::new(compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .deny_host("*")
                .build()
                .expect("policy"),
        ))
    }

    fn empty_bindings() -> Bindings {
        Arc::new(Mutex::new(NameBindings::default()))
    }

    async fn audit_sink(dir: &Path) -> AuditSink {
        AuditSink::open(dir.join(NETWORK_LOG), "sid-test".to_string())
            .await
            .expect("open audit sink")
            .for_container("outrig-test")
    }

    /// Waits until `dir`'s audit log holds `want` records, which is what says
    /// that many connections have run far enough to write one. Counted by
    /// newline, so a line the writer is still appending is not one of them.
    async fn await_audit_records(dir: &Path, want: usize) {
        let path = dir.join(NETWORK_LOG);
        let waited = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let log = tokio::fs::read(&path).await.unwrap_or_default();
                if log.iter().filter(|byte| **byte == b'\n').count() >= want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "the audit log never reached {want} records");
    }

    /// The one record `dir`'s audit log holds, read as it is the instant it is
    /// asked for -- no polling, no retry, so a record written late is a
    /// failure rather than a slow pass.
    fn only_audit_record(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join(NETWORK_LOG)).expect("read audit log");
        let mut lines = text.lines();
        let record = lines
            .next()
            .expect("an audit record for the cut connection");
        assert_eq!(lines.next(), None, "exactly one connection was made");
        serde_json::from_str(record).expect("audit record json")
    }

    /// One accepted connection, wired the way the accept loop wires one: the
    /// client end stays with the test, the other end is what `handle_tcp` is
    /// handed.
    async fn accepted_connection() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr"))
            .await
            .expect("connect");
        let (intercepted, orig) = listener.accept().await.expect("accept");
        (client, intercepted, orig)
    }

    /// A destination that accepts one connection and hands the server end
    /// back, standing in for whatever the redirect displaced.
    async fn upstream_once() -> (SocketAddr, tokio::task::JoinHandle<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let accepted = tokio::spawn(async move { listener.accept().await.expect("accept").0 });
        (addr, accepted)
    }

    /// A live bridged connection and the tasks holding it: the client end, the
    /// upstream server end, and the connection's own `JoinSet`, which is what
    /// a detach ends it through.
    struct LiveConnection {
        client: TcpStream,
        upstream: TcpStream,
        tasks: JoinSet<()>,
        cancel: CancellationToken,
    }

    /// Brings up one connection through `handle_tcp` and returns once bytes
    /// have demonstrably crossed it in both directions, so a test that then
    /// cancels is cancelling something that was working.
    async fn live_connection(dir: &Path) -> LiveConnection {
        let (mut client, intercepted, orig) = accepted_connection().await;
        let (dst, upstream) = upstream_once().await;
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(handle_tcp(
            intercepted,
            orig,
            dst,
            audit_sink(dir).await,
            empty_bindings(),
            allow_all_policy(),
            cancel.clone(),
        ));

        // Written before the upstream is accepted: this is the client's
        // opening burst, which the sniff reads and the bridge forwards.
        client.write_all(b"before\n").await.expect("write before");
        let mut upstream = upstream.await.expect("upstream accepted");
        let mut sent = [0u8; 7];
        upstream
            .read_exact(&mut sent)
            .await
            .expect("the bridge should carry the opening bytes");
        assert_eq!(&sent, b"before\n");

        // And back the other way, so both byte counters have something on
        // them by the time anything cancels this.
        upstream.write_all(b"down!\n").await.expect("write down");
        let mut received = [0u8; 6];
        client
            .read_exact(&mut received)
            .await
            .expect("the bridge should carry the upstream's bytes");
        assert_eq!(&received, b"down!\n");

        LiveConnection {
            client,
            upstream,
            tasks,
            cancel,
        }
    }

    /// Cancelling an attachment cuts the connections it accepted, and it is
    /// cut by the time the call that ends them returns -- not eventually.
    #[tokio::test]
    async fn a_bridged_connection_stops_carrying_bytes_once_its_attachment_is_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let LiveConnection {
            mut client,
            mut upstream,
            mut tasks,
            cancel,
        } = live_connection(dir.path()).await;

        cancel.cancel();
        let failures = stop_tasks(&mut tasks).await;
        assert!(failures.is_empty(), "{failures:#?}");

        // Asked after the call that ended it returned, rather than waited for:
        // nothing written now can reach upstream, because nothing is left to
        // carry it and the socket that would have went with the task.
        let _ = client.write_all(b"after\n").await;
        let mut after = [0u8; 6];
        let crossed = tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut after)).await;
        assert!(
            matches!(crossed, Ok(Ok(0)) | Ok(Err(_))),
            "no bytes may cross a cut connection, got {crossed:?}"
        );
    }

    /// A cut connection still owes its audit record, and owes it *before* its
    /// task joins -- a detach that returned first would be reporting a
    /// connection whose record had not been written.
    #[tokio::test]
    async fn a_cut_connection_has_its_audit_record_by_the_time_its_task_joins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let LiveConnection {
            mut tasks, cancel, ..
        } = live_connection(dir.path()).await;

        cancel.cancel();
        stop_tasks(&mut tasks).await;

        let record = only_audit_record(dir.path());
        assert_eq!(record["outrig.container"], "outrig-test");
        // The bytes that had already crossed are in the record, both
        // directions: they are credited as they move, so the cut cannot erase
        // them.
        assert_eq!(record["orig_bytes"], 7);
        assert_eq!(record["resp_bytes"], 6);
    }

    /// A connection cut before its client ever spoke is recorded as what its
    /// address alone earned, with no bytes and no asserted identity. Nothing
    /// it might have claimed was ever read, and nothing was ever forwarded,
    /// so that is the whole truth about it.
    #[tokio::test]
    async fn a_connection_cut_before_its_client_spoke_is_recorded_by_address_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_client, intercepted, orig) = accepted_connection().await;
        let (dst, _upstream) = upstream_once().await;
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(handle_tcp(
            intercepted,
            orig,
            dst,
            audit_sink(dir.path()).await,
            empty_bindings(),
            allow_all_policy(),
            cancel.clone(),
        ));
        // The client says nothing, so the connection is parked in the sniff
        // read with no evidence yet gathered.
        tokio::task::yield_now().await;

        cancel.cancel();
        stop_tasks(&mut tasks).await;

        let record = only_audit_record(dir.path());
        assert_eq!(record["outrig.action"], "allow");
        assert_eq!(record["outrig.rule"], "default");
        assert_eq!(record["outrig.host"], serde_json::Value::Null);
        assert_eq!(record["orig_bytes"], 0);
        assert_eq!(record["resp_bytes"], 0);
        assert_eq!(record["conn_state"], "S0");
    }

    /// A connection parked with nothing to wake it -- neither peer speaks,
    /// neither closes -- is *terminated* by the cancel, not left running
    /// unwatched. The client's read returning EOF the moment the stop call
    /// returns is the proof: the socket went with the task's frame, which
    /// only happens if the task is gone.
    #[tokio::test]
    async fn a_connection_parked_with_nothing_to_wake_it_is_terminated_not_detached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let LiveConnection {
            mut client,
            upstream,
            mut tasks,
            cancel,
        } = live_connection(dir.path()).await;
        // Held open and silent for the rest of the test: the bridge has
        // nothing to copy and no timer to expire.
        let _upstream = upstream;

        cancel.cancel();
        stop_tasks(&mut tasks).await;

        let mut buf = [0u8; 1];
        let eof = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
        assert!(
            matches!(eof, Ok(Ok(0)) | Ok(Err(_))),
            "the interceptor's end of a cut connection must be gone, got {eof:?}"
        );
        assert!(tasks.is_empty());
    }

    /// The backstop for a task that never looks at its token: it is aborted
    /// *and joined*, so a detach can say the connections are gone rather than
    /// merely unwatched. A `JoinHandle` dropped after a timeout would detach
    /// its task instead, which is the bug this shape exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_task_that_ignores_cancellation_is_aborted_and_joined() {
        let (held, released) = tokio::sync::oneshot::channel::<()>();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            // Only dropped when this task's frame is destroyed.
            let _held = held;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let failures = stop_tasks(&mut tasks).await;

        assert!(
            matches!(
                failures.as_slice(),
                [OutrigError::NetworkTasksAborted { .. }]
            ),
            "{failures:#?}"
        );
        assert!(tasks.is_empty());
        // Bounded so an implementation that detached rather than aborted
        // fails here as an assertion instead of hanging the suite: the
        // receiver of a sender still held by a live task simply never wakes.
        let ended = tokio::time::timeout(Duration::from_secs(5), released)
            .await
            .expect("an aborted task's frame must be destroyed, not left running");
        assert!(
            ended.is_err(),
            "the task must be gone, not merely unwatched"
        );
    }

    /// A forward waits out `DNS_TIMEOUT` per resolver, well past the grace a
    /// detach allows. The loop has to abandon one in flight, or every detach
    /// racing a lookup would abort the task and report the abort.
    #[tokio::test]
    async fn the_dns_loop_does_not_outlast_a_cancellation_during_a_forward() {
        // Receives the query and never answers it.
        let blackhole = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind blackhole");
        let resolver = blackhole.local_addr().expect("blackhole addr");

        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind dns listener");
        let listener_addr = socket.local_addr().expect("dns addr");
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(dns_loop(
            socket,
            vec![resolver],
            empty_bindings(),
            cancel.clone(),
        ));

        let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
        client
            .send_to(
                &dns_packet(0x1234, DNS_QUERY_FLAGS, "example.test", &[]),
                listener_addr,
            )
            .await
            .expect("send query");

        // The forward is demonstrably in flight once the resolver has it.
        let mut forwarded = [0u8; 512];
        tokio::time::timeout(Duration::from_secs(5), blackhole.recv_from(&mut forwarded))
            .await
            .expect("the query should reach the resolver")
            .expect("receive the forwarded query");

        cancel.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(1), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        assert!(
            stopped.is_ok(),
            "a cancelled dns loop must not wait out {DNS_TIMEOUT:?}"
        );
    }

    /// Connections that have finished do not stay in the set the accept loop
    /// keeps them in. A set only ever spawned into holds one handle per
    /// connection the attachment has ever served.
    #[tokio::test]
    async fn finished_connections_do_not_pile_up_in_the_accept_loop_set() {
        let mut conns = JoinSet::new();
        for _ in 0..64 {
            conns.spawn(async {});
        }

        // Bounded, so a reap that reaps nothing fails the assertion below
        // rather than spinning here.
        for _ in 0..64 {
            tokio::task::yield_now().await;
            reap_finished(&mut conns);
            if conns.is_empty() {
                break;
            }
        }

        assert!(
            conns.is_empty(),
            "{} finished connections still held",
            conns.len()
        );
    }

    /// The same claim against the loop rather than the helper: it is the loop
    /// that has to keep taking finished connections back out, and a test that
    /// calls `reap_finished` itself cannot tell whether the loop still does.
    #[tokio::test]
    async fn the_accept_loop_keeps_taking_finished_connections_back() {
        const SERVED: usize = 64;
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = audit_sink(dir.path()).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let cancel = CancellationToken::new();
        let conn_cancel = cancel.child_token();
        // Held here rather than by the loop, which is the whole reason the
        // accepting half takes it by reference.
        let mut conns = JoinSet::new();

        let token = cancel.clone();
        let log = dir.path().to_path_buf();
        let clients = tokio::spawn(async move {
            // Cancels however this task ends, rather than only where it used
            // to: the gate below gives up by panicking, and a connect can
            // fail, and either would leave the loop parked in `accept` with
            // nothing coming to end it -- the test would hang rather than
            // report what went wrong, since the failure is not observable
            // until `accept_into` returns.
            let _ends_the_loop = token.drop_guard();
            for served in 1..=SERVED {
                // Closed at once, so each connection finishes on its own and
                // leaves its handle behind for the loop to take back.
                drop(TcpStream::connect(addr).await.expect("connect"));
                // Waited out by its own record rather than by the clock. A
                // connection writes one before its task ends, and the audit
                // writer is a queue of one on a runtime this test shares with
                // it -- so a sleep here says nothing about how many
                // connections are still in flight, and a machine slow enough
                // to lag the writer would leave a whole burst of them for the
                // assertion below to count.
                await_audit_records(&log, served).await;
            }
        });

        let (live, _idle) = mpsc::channel(1);
        accept_into(
            &listener,
            &audit,
            &empty_bindings(),
            &deny_all_policy(),
            &cancel,
            &conn_cancel,
            &mut conns,
            &live,
        )
        .await;
        clients.await.expect("clients");

        // Not zero: the loop reaps on its way in to an accept, so the
        // connection it served last is still held either way, and the drain
        // that follows `accept_into` in production is what collects it. What
        // the gate above buys is that only the last one or two can be --
        // every earlier connection had a whole further connection's worth of
        // the loop to be taken back in. The claim is that the carrier does
        // not grow with the number of connections served: without the reap
        // all SERVED of them are still here.
        assert!(
            conns.len() < 8,
            "{} of {SERVED} finished connections were never taken back out of the carrier",
            conns.len()
        );
    }

    /// One attachment serving many connections leaves nothing alive for the
    /// ones it has already closed.
    #[tokio::test]
    async fn many_closed_connections_leave_no_tasks_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = audit_sink(dir.path()).await;
        let sink = TcpListener::bind("127.0.0.1:0").await.expect("bind sink");
        let dst = sink.local_addr().expect("sink addr");
        let draining = tokio::spawn(async move {
            while let Ok((stream, _)) = sink.accept().await {
                drop(stream);
            }
        });
        let baseline = alive_tasks();

        let cancel = CancellationToken::new();
        let mut conns = JoinSet::new();
        for _ in 0..32 {
            let (client, intercepted, orig) = accepted_connection().await;
            // Closed at once, so the connection runs to its natural end.
            drop(client);
            conns.spawn(handle_tcp(
                intercepted,
                orig,
                dst,
                audit.clone(),
                empty_bindings(),
                allow_all_policy(),
                cancel.clone(),
            ));
        }
        while conns.join_next().await.is_some() {}

        assert_eq!(
            alive_tasks(),
            baseline,
            "32 closed connections left tasks behind"
        );
        draining.abort();
    }

    /// The accept loop does not return until the connections it started
    /// have. Their audit records are the proof: a loop that returned early
    /// would drop the set holding them, and an aborted connection writes
    /// nothing at all.
    #[tokio::test]
    async fn the_accept_loop_waits_for_the_connections_it_started() {
        const HELD: usize = 4;
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(tcp_accept_loop(
            listener,
            audit_sink(dir.path()).await,
            empty_bindings(),
            allow_all_policy(),
            cancel.clone(),
            mpsc::channel(1).0,
        ));

        // Silent clients, so every connection parks in the sniff read and is
        // still live -- and still unrecorded -- when the cancel lands. The
        // settle is a small fraction of `SNIFF_TIMEOUT`, which is the window
        // they are all parked in.
        let mut clients = Vec::new();
        for _ in 0..HELD {
            clients.push(TcpStream::connect(addr).await.expect("connect"));
        }
        tokio::time::sleep(SNIFF_TIMEOUT / 5).await;

        cancel.cancel();
        stop_tasks(&mut tasks).await;

        let text = std::fs::read_to_string(dir.path().join(NETWORK_LOG)).expect("read audit log");
        let records = text.lines().filter(|line| !line.trim().is_empty()).count();
        assert_eq!(
            records, HELD,
            "every connection the loop started owes a record before it returns"
        );
    }

    /// Repeated attach/detach cycles leave nothing behind: what a detach ends,
    /// it ends for good, so a long session does not accumulate one set of
    /// loops per attachment it ever had.
    #[tokio::test]
    async fn repeated_attach_detach_cycles_leave_no_tasks_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = audit_sink(dir.path()).await;
        let baseline = alive_tasks();

        for _ in 0..8 {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let dns = UdpSocket::bind("127.0.0.1:0").await.expect("bind dns");
            let cancel = CancellationToken::new();
            let mut tasks = JoinSet::new();
            tasks.spawn(tcp_accept_loop(
                listener,
                audit.clone(),
                empty_bindings(),
                allow_all_policy(),
                cancel.clone(),
                mpsc::channel(1).0,
            ));
            tasks.spawn(dns_loop(dns, Vec::new(), empty_bindings(), cancel.clone()));

            cancel.cancel();
            let failures = stop_tasks(&mut tasks).await;
            assert!(failures.is_empty(), "{failures:#?}");
        }

        assert_eq!(
            alive_tasks(),
            baseline,
            "eight attach/detach cycles left tasks behind"
        );
    }

    #[test]
    fn nft_rules_redirect_tcp_and_dns_but_skip_loopback() {
        let rules = nft_rules("outrig_test", 44123, 44124);
        let lines: Vec<&str> = rules.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some("create table inet outrig_test"),
            "`create` rather than `add`: it fails on a table that already \
             exists, which is what makes deleting one afterwards safe"
        );
        assert!(
            lines[1..]
                .iter()
                .all(|line| line.starts_with("add chain ") || line.starts_with("add rule ")),
            "every declaration is its own top-level command. Nesting them in a \
             `create table {{ ... }}` block parses, exits zero, and installs an \
             empty table on nft 1.0.9 -- an interceptor that redirects nothing. \
             Asserting only that the text contains each rule cannot tell the \
             two apart, which is why it did not: {rules}"
        );
        assert!(rules.contains("add rule inet outrig_test output ip daddr 127.0.0.0/8 return"));
        assert!(rules.contains("add rule inet outrig_test output ip6 daddr ::1 return"));
        assert!(
            rules.contains("add rule inet outrig_test output meta l4proto tcp redirect to :44123")
        );
        assert!(rules.contains("add rule inet outrig_test output udp dport 53 redirect to :44124"));
    }

    #[test]
    fn resolver_parser_reads_nameserver_lines_only() {
        let resolvers = parse_resolvers(
            "\
# generated file
search example.test
nameserver 127.0.0.53 # local stub
nameserver 2001:4860:4860::8888
options edns0
",
        );

        assert_eq!(
            resolvers,
            vec![
                SocketAddr::from(([127, 0, 0, 53], 53)),
                SocketAddr::new("2001:4860:4860::8888".parse().expect("ipv6"), 53),
            ]
        );
    }

    #[test]
    fn host_resolvers_prefer_systemd_upstream_when_primary_is_stub() {
        let primary = vec![SocketAddr::from(([127, 0, 0, 53], 53))];
        let upstream = vec![
            SocketAddr::from(([127, 0, 0, 54], 53)),
            SocketAddr::from(([172, 20, 232, 252], 53)),
        ];

        assert_eq!(
            select_host_resolvers(primary, upstream),
            vec![SocketAddr::from(([172, 20, 232, 252], 53))]
        );
    }

    #[test]
    fn host_resolvers_keep_primary_when_it_has_upstream_nameserver() {
        let primary = vec![SocketAddr::from(([10, 0, 2, 3], 53))];
        let upstream = vec![SocketAddr::from(([172, 20, 232, 252], 53))];

        assert_eq!(select_host_resolvers(primary.clone(), upstream), primary);
    }

    #[test]
    fn http_host_sniff_strips_port() {
        let assertion = sniff_client_bytes(b"GET / HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .expect("http sniff");
        assert_eq!(assertion.service(), "http");
        assert_eq!(assertion.asserted_host(), Some("example.com"));
        assert_eq!(assertion.sni, None);
    }

    #[test]
    fn tls_sniff_reads_the_server_name_extension() {
        let assertion =
            sniff_client_bytes(&tls_client_hello("registry.npmjs.org")).expect("tls sniff");
        assert_eq!(assertion.service(), "ssl");
        assert_eq!(assertion.sni.as_deref(), Some("registry.npmjs.org"));
        assert_eq!(assertion.asserted_host(), Some("registry.npmjs.org"));
    }

    #[test]
    fn audit_record_uses_zeek_conn_field_names() {
        let record = AuditRecord::new(
            "20260513T000000-abcd",
            "outrig-test",
            AuditEvent {
                opened: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(125),
                orig: addr("10.0.2.100:50123"),
                dst: addr("93.184.216.34:443"),
                resolved: resolved(&["example.com"]),
                assertion: ClientAssertion::tls("example.com".to_string()),
                service: "ssl",
                bytes_tx: 517,
                bytes_rx: 1298,
                decision: PolicyDecision::allow_default(),
            },
        );
        let json = serde_json::to_value(record).expect("record json");

        assert_eq!(json["ts"], 1_700_000_000.0);
        assert!(json["uid"].as_str().is_some_and(|uid| uid.starts_with('C')));
        assert_eq!(json["id.orig_h"], "10.0.2.100");
        assert_eq!(json["id.orig_p"], 50123);
        assert_eq!(json["id.resp_h"], "93.184.216.34");
        assert_eq!(json["id.resp_p"], 443);
        assert_eq!(json["proto"], "tcp");
        assert_eq!(json["service"], "ssl");
        assert_eq!(json["duration"], 0.125);
        assert_eq!(json["orig_bytes"], 517);
        assert_eq!(json["resp_bytes"], 1298);
        assert_eq!(json["conn_state"], "SF");
        assert_eq!(json["missed_bytes"], 0);
        assert_eq!(json["server_name"], "example.com");
        assert_eq!(json["outrig.session_id"], "20260513T000000-abcd");
        assert_eq!(json["outrig.container"], "outrig-test");
        assert_eq!(json["outrig.host"], "example.com");
        assert_eq!(json["outrig.action"], "allow");
        assert_eq!(json["outrig.rule"], "default");
        assert!(json.get("host").is_none());
        assert!(json.get("bytes_tx").is_none());
        assert!(json.get("duration_ms").is_none());
    }

    #[test]
    fn audit_record_says_where_its_host_came_from() {
        fn record(resolved: ResolvedNames, assertion: ClientAssertion) -> serde_json::Value {
            let record = AuditRecord::new(
                "sid",
                "outrig-test",
                AuditEvent {
                    opened: UNIX_EPOCH,
                    duration: Duration::from_millis(1),
                    orig: addr("10.0.2.100:50123"),
                    dst: addr("203.0.113.66:443"),
                    service: assertion.service(),
                    resolved,
                    assertion,
                    bytes_tx: 0,
                    bytes_rx: 0,
                    decision: PolicyDecision::allow_default(),
                },
            );
            serde_json::to_value(record).expect("record json")
        }

        let claimed = record(
            ResolvedNames::default(),
            ClientAssertion::tls("evil.example".to_string()),
        );
        assert_eq!(claimed["outrig.host"], "evil.example");
        assert_eq!(claimed["outrig.host_source"], "asserted");

        let looked_up = record(resolved(&["allowed.example"]), ClientAssertion::default());
        assert_eq!(looked_up["outrig.host"], "allowed.example");
        assert_eq!(looked_up["outrig.host_source"], "resolved");

        let unknown = record(ResolvedNames::default(), ClientAssertion::default());
        assert!(unknown.get("outrig.host").is_none());
        assert!(unknown.get("outrig.host_source").is_none());
    }

    #[test]
    fn audit_record_writes_deny_decision_with_zero_bytes() {
        let record = AuditRecord::new(
            "20260513T000000-abcd",
            "outrig-test",
            AuditEvent {
                opened: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(1),
                orig: addr("10.0.2.100:50123"),
                dst: addr("93.184.216.34:443"),
                resolved: ResolvedNames::default(),
                assertion: ClientAssertion::tls("example.com".to_string()),
                service: "ssl",
                bytes_tx: 0,
                bytes_rx: 0,
                decision: PolicyDecision {
                    action: NetworkAction::Deny,
                    rule: "deny[0]".to_string(),
                },
            },
        );
        let json = serde_json::to_value(record).expect("record json");

        assert_eq!(json["orig_bytes"], 0);
        assert_eq!(json["resp_bytes"], 0);
        assert_eq!(json["conn_state"], "S0");
        assert_eq!(json["outrig.action"], "deny");
        assert_eq!(json["outrig.rule"], "deny[0]");
    }

    #[test]
    fn a_client_asserted_name_can_still_deny() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Allow)
                .allow_host("*")
                .deny_host("example.com")
                .build()
                .expect("policy"),
        );
        let decision = policy.decide(
            addr("93.184.216.34:443"),
            &ResolvedNames::default(),
            &ClientAssertion::tls("example.com".to_string()),
        );

        assert_eq!(decision.action, NetworkAction::Deny);
        assert_eq!(decision.rule, "deny[0]");
    }

    #[test]
    fn policy_matches_ip_cidr_and_ports() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("*.npmjs.org")
                .allow_host("10.0.0.0/8")
                .allow_host_port("2001:db8::1", 443)
                .build()
                .expect("policy"),
        );

        let npm = policy.decide(
            addr("104.16.0.1:443"),
            &resolved(&["registry.npmjs.org"]),
            &ClientAssertion::default(),
        );
        let cidr = policy.decide(
            addr("10.2.3.4:22"),
            &ResolvedNames::default(),
            &ClientAssertion::default(),
        );
        let ipv6 = policy.decide(
            addr("[2001:db8::1]:443"),
            &ResolvedNames::default(),
            &ClientAssertion::default(),
        );
        let ipv6_wrong_port = policy.decide(
            addr("[2001:db8::1]:80"),
            &ResolvedNames::default(),
            &ClientAssertion::default(),
        );

        assert_eq!(npm.action, NetworkAction::Allow);
        assert_eq!(npm.rule, "allow[0]");
        assert_eq!(cidr.action, NetworkAction::Allow);
        assert_eq!(cidr.rule, "allow[1]");
        assert_eq!(ipv6.action, NetworkAction::Allow);
        assert_eq!(ipv6.rule, "allow[2]");
        assert_eq!(ipv6_wrong_port.action, NetworkAction::Deny);
        assert_eq!(ipv6_wrong_port.rule, "default");
    }

    /// The audited bypass: an allowlisted name, an unrelated destination, and
    /// the name supplied by the client that wants to reach it.
    #[test]
    fn a_forged_sni_alone_does_not_grant_a_hostname_rule() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host_port("allowed.example", 443)
                .build()
                .expect("policy"),
        );

        let decision = policy.decide(
            addr("203.0.113.66:443"),
            &ResolvedNames::default(),
            &ClientAssertion::tls("allowed.example".to_string()),
        );

        assert_eq!(decision.action, NetworkAction::Deny);
        assert_eq!(decision.rule, "default");
    }

    /// The same claim through the other parser: a cleartext `Host:` header
    /// reaches the policy by a different path than a `ClientHello`.
    #[test]
    fn a_forged_host_header_alone_does_not_grant_a_hostname_rule() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host_port("allowed.example", 80)
                .build()
                .expect("policy"),
        );
        let assertion = sniff_client_bytes(&http_request("allowed.example")).expect("http sniff");

        let decision = policy.decide(
            addr("203.0.113.66:80"),
            &ResolvedNames::default(),
            &assertion,
        );

        assert_eq!(decision.action, NetworkAction::Deny);
        assert_eq!(decision.rule, "default");
    }

    #[test]
    fn resolved_identity_grants_a_hostname_rule() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host_port("allowed.example", 443)
                .build()
                .expect("policy"),
        );

        let decision = policy.decide(
            addr("203.0.113.66:443"),
            &resolved(&["allowed.example"]),
            &ClientAssertion::default(),
        );

        assert_eq!(decision.action, NetworkAction::Allow);
        assert_eq!(decision.rule, "allow[0]");
    }

    /// Shared hosting: one address, two names. Neither answer may depend on
    /// which lookup happened last, which the old `IpAddr -> String` cache
    /// could not represent.
    #[test]
    fn two_names_on_one_address_both_keep_their_meaning() {
        let shared = ip("203.0.113.66");
        let allowing = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("allowed.example")
                .build()
                .expect("policy"),
        );
        let denying = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Allow)
                .deny_host("denied.example")
                .build()
                .expect("policy"),
        );

        for order in [
            ["allowed.example", "denied.example"],
            ["denied.example", "allowed.example"],
        ] {
            let now = Instant::now();
            let mut bindings = NameBindings::default();
            for name in order {
                bindings.bind(shared, name, Duration::from_secs(60), now);
            }
            let names = bindings.names(shared, now);

            assert_eq!(
                allowing
                    .decide(
                        addr("203.0.113.66:443"),
                        &names,
                        &ClientAssertion::default()
                    )
                    .action,
                NetworkAction::Allow,
                "bound in order {order:?}"
            );
            assert_eq!(
                denying
                    .decide(
                        addr("203.0.113.66:443"),
                        &names,
                        &ClientAssertion::default()
                    )
                    .action,
                NetworkAction::Deny,
                "bound in order {order:?}"
            );
        }
    }

    #[test]
    fn a_binding_past_its_ttl_does_not_grant() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("allowed.example")
                .build()
                .expect("policy"),
        );
        let now = Instant::now();
        let mut bindings = NameBindings::default();
        bindings.bind(
            ip("203.0.113.66"),
            "allowed.example",
            Duration::from_secs(60),
            now,
        );

        let fresh = bindings.names(ip("203.0.113.66"), now + Duration::from_secs(59));
        let stale = bindings.names(ip("203.0.113.66"), now + Duration::from_secs(61));

        assert_eq!(
            policy
                .decide(
                    addr("203.0.113.66:443"),
                    &fresh,
                    &ClientAssertion::default()
                )
                .action,
            NetworkAction::Allow
        );
        assert_eq!(stale, ResolvedNames::default());
        assert_eq!(
            policy
                .decide(
                    addr("203.0.113.66:443"),
                    &stale,
                    &ClientAssertion::default()
                )
                .action,
            NetworkAction::Deny
        );
    }

    /// Bindings are created per attachment, so what one container resolved is
    /// not evidence for another's traffic.
    #[test]
    fn a_binding_one_attachment_earned_is_invisible_to_another() {
        let now = Instant::now();
        let mut first = NameBindings::default();
        let second = NameBindings::default();
        first.bind(
            ip("203.0.113.66"),
            "allowed.example",
            Duration::from_secs(60),
            now,
        );

        assert_eq!(
            first.names(ip("203.0.113.66"), now),
            resolved(&["allowed.example"])
        );
        assert_eq!(
            second.names(ip("203.0.113.66"), now),
            ResolvedNames::default()
        );
    }

    /// A glob with no letter in it describes addresses, so it keeps matching
    /// the destination address's text; one with a letter needs a resolved
    /// name.
    #[test]
    fn address_shaped_globs_still_match_the_destination_address() {
        let wildcard = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("*")
                .build()
                .expect("policy"),
        );
        let ssh = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Allow)
                .deny_host_port("*", 22)
                .build()
                .expect("policy"),
        );
        let prefix = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("10.0.*")
                .build()
                .expect("policy"),
        );
        let named = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("*.npmjs.org")
                .build()
                .expect("policy"),
        );
        assert_eq!(
            decide_bare(&wildcard, "203.0.113.66:443").action,
            NetworkAction::Allow
        );
        assert_eq!(
            decide_bare(&ssh, "203.0.113.66:22").action,
            NetworkAction::Deny
        );
        assert_eq!(
            decide_bare(&prefix, "10.0.1.2:443").action,
            NetworkAction::Allow
        );
        assert_eq!(
            decide_bare(&prefix, "10.9.1.2:443").action,
            NetworkAction::Deny
        );
        assert_eq!(
            named
                .decide(
                    addr("104.16.0.1:443"),
                    &ResolvedNames::default(),
                    &ClientAssertion::tls("registry.npmjs.org".to_string())
                )
                .action,
            NetworkAction::Deny,
            "a name-shaped glob must not grant on a client-asserted name"
        );
        assert_eq!(
            prefix
                .decide(
                    addr("203.0.113.66:443"),
                    &resolved(&["10.0.attacker.example"]),
                    &ClientAssertion::default()
                )
                .action,
            NetworkAction::Deny,
            "an address glob describes addresses, so a registrable name that \
             matches it must not grant"
        );
    }

    /// An address rule is about the destination, so claiming an in-range
    /// address in a `Host:` header cannot grant one either.
    #[test]
    fn ip_and_cidr_allows_ignore_a_client_asserted_address() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("10.0.0.0/8")
                .allow_host("192.0.2.7")
                .build()
                .expect("policy"),
        );

        for claimed in ["10.1.2.3", "192.0.2.7"] {
            let decision = policy.decide(
                addr("198.51.100.9:443"),
                &ResolvedNames::default(),
                &ClientAssertion::http(claimed.to_string()),
            );
            assert_eq!(decision.action, NetworkAction::Deny, "claimed {claimed}");
        }
        assert_eq!(
            policy
                .decide(
                    addr("10.1.2.3:443"),
                    &ResolvedNames::default(),
                    &ClientAssertion::default()
                )
                .action,
            NetworkAction::Allow
        );
    }

    #[test]
    fn dns_response_must_come_from_the_resolver_the_query_went_to() {
        let query = asked("example.com");
        let response = dns_packet(
            0x1234,
            DNS_RESPONSE_FLAGS,
            "example.com",
            &[Rr::A("example.com", [93, 184, 216, 34], 60)],
        );
        let resolver = addr("192.0.2.53:53");

        assert!(query.answered_by(resolver, resolver, &response));
        assert!(!query.answered_by(resolver, addr("198.51.100.9:53"), &response));
    }

    #[test]
    fn dns_response_must_echo_the_transaction_id_and_the_question() {
        let query = asked("example.com");
        let resolver = addr("192.0.2.53:53");
        let answer = [Rr::A("example.com", [93, 184, 216, 34], 60)];

        let wrong_id = dns_packet(0x4321, DNS_RESPONSE_FLAGS, "example.com", &answer);
        let wrong_question = dns_packet(0x1234, DNS_RESPONSE_FLAGS, "other.example", &answer);

        assert!(!query.answered_by(resolver, resolver, &wrong_id));
        assert!(!query.answered_by(resolver, resolver, &wrong_question));
    }

    #[test]
    fn a_query_reflected_back_is_not_an_answer() {
        let raw = dns_packet(0x1234, DNS_QUERY_FLAGS, "example.com", &[]);
        let resolver = addr("192.0.2.53:53");

        assert!(
            !dns_query(&raw)
                .expect("query")
                .answered_by(resolver, resolver, &raw)
        );
    }

    #[test]
    fn truncated_and_error_responses_bind_nothing() {
        let answer = [Rr::A("example.com", [93, 184, 216, 34], 60)];
        let ok = dns_packet(1, DNS_RESPONSE_FLAGS, "example.com", &answer);
        let truncated = dns_packet(1, DNS_RESPONSE_FLAGS | 0x0200, "example.com", &answer);
        let nxdomain = dns_packet(1, DNS_RESPONSE_FLAGS | 0x0003, "example.com", &answer);

        assert!(dns_response_is_bindable(&ok));
        assert!(!dns_response_is_bindable(&truncated));
        assert!(!dns_response_is_bindable(&nxdomain));
    }

    /// A wire label may hold any byte, including `.`. Decoding one into a
    /// presentation name would let an authority for `attacker.example` mint
    /// `allowed.evil.attacker.example` and bind any address under
    /// `allow = ["allowed.*"]`, with no client assertion involved.
    #[test]
    fn a_label_carrying_a_dot_binds_nothing() {
        let packet =
            dns_packet_with_raw_name(&["allowed.evil", "attacker", "example"], [203, 0, 113, 66]);
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("allowed.*")
                .build()
                .expect("policy"),
        );

        let now = Instant::now();
        let bindings = bind_all("allowed.evil.attacker.example", &packet, now);

        assert!(
            dns_question(&packet).is_none(),
            "a name that cannot be written as a hostname must not parse"
        );
        assert_eq!(
            policy
                .decide(
                    addr("203.0.113.66:443"),
                    &bindings.names(ip("203.0.113.66"), now),
                    &ClientAssertion::default()
                )
                .action,
            NetworkAction::Deny,
            "a forged label must not grant the glob it was crafted to match"
        );
    }

    /// A length byte with reserved high bits can claim more than the 63 a
    /// label may hold.
    #[test]
    fn an_over_long_label_binds_nothing() {
        let long = "a".repeat(MAX_DNS_LABEL + 1);
        let packet = dns_packet_with_raw_name(&[&long, "example"], [203, 0, 113, 66]);

        assert!(dns_question(&packet).is_none());
        assert!(dns_bindings(&format!("{long}.example"), &packet).is_empty());

        let fits = "a".repeat(MAX_DNS_LABEL);
        let ok = dns_packet_with_raw_name(&[&fits, "example"], [203, 0, 113, 66]);
        assert_eq!(
            dns_bindings(&format!("{fits}.example"), &ok),
            vec![(ip("203.0.113.66"), Duration::from_secs(60))],
            "a label at the limit is still a label"
        );
    }

    /// A record the policy cannot name is ineligible on its own; it must not
    /// take the rest of the answer section with it, or a resolver that puts
    /// one first would turn a hostname allow into a denial.
    #[test]
    fn an_unnameable_owner_does_not_hide_the_records_after_it() {
        let packet = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "allowed.example",
            &[
                Rr::A("bad label", [198, 51, 100, 7], 60),
                Rr::A("allowed.example", [203, 0, 113, 66], 60),
            ],
        );

        assert_eq!(
            dns_bindings("allowed.example", &packet),
            vec![(ip("203.0.113.66"), Duration::from_secs(60))]
        );
    }

    #[test]
    fn answer_records_owned_by_an_unrelated_name_bind_nothing() {
        let packet = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "allowed.example",
            &[Rr::A("unrelated.example", [203, 0, 113, 66], 60)],
        );

        assert!(dns_bindings("allowed.example", &packet).is_empty());
    }

    #[test]
    fn a_cname_chain_binds_the_addresses_at_its_end() {
        let packet = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "www.example",
            &[
                Rr::Cname("www.example", "edge.cdn.example"),
                Rr::A("edge.cdn.example", [203, 0, 113, 10], 300),
            ],
        );

        assert_eq!(
            dns_bindings("www.example", &packet),
            vec![(ip("203.0.113.10"), Duration::from_secs(300))]
        );
    }

    /// An authority for one name cannot mint a binding for another by
    /// aliasing to it: the addresses bind under the queried name only.
    #[test]
    fn an_alias_does_not_bind_the_name_it_points_at() {
        let packet = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "attacker.example",
            &[
                Rr::Cname("attacker.example", "allowed.example"),
                Rr::A("allowed.example", [203, 0, 113, 66], 60),
            ],
        );
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Deny)
                .allow_host("allowed.example")
                .build()
                .expect("policy"),
        );
        let now = Instant::now();
        let bindings = bind_all("attacker.example", &packet, now);

        let decision = policy.decide(
            addr("203.0.113.66:443"),
            &bindings.names(ip("203.0.113.66"), now),
            &ClientAssertion::default(),
        );

        assert_eq!(decision.action, NetworkAction::Deny);
    }

    #[test]
    fn a_broken_or_looping_cname_chain_binds_nothing() {
        let broken = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "www.example",
            &[Rr::Cname("www.example", "missing.example")],
        );
        let looping = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "a.example",
            &[
                Rr::Cname("a.example", "b.example"),
                Rr::Cname("b.example", "a.example"),
                Rr::A("c.example", [203, 0, 113, 66], 60),
            ],
        );

        assert!(dns_bindings("www.example", &broken).is_empty());
        assert!(dns_bindings("a.example", &looping).is_empty());
    }

    /// Per-record TTLs, not one per response: two answers with different TTLs
    /// stop granting at different times.
    #[test]
    fn each_answer_record_keeps_its_own_ttl() {
        let packet = dns_packet(
            1,
            DNS_RESPONSE_FLAGS,
            "example.test",
            &[
                Rr::A("example.test", [203, 0, 113, 1], 60),
                Rr::Aaaa(
                    "example.test",
                    [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                    600,
                ),
            ],
        );
        let bound = dns_bindings("example.test", &packet);
        assert_eq!(
            bound,
            vec![
                (ip("203.0.113.1"), Duration::from_secs(60)),
                (ip("2001:db8::1"), Duration::from_secs(600)),
            ]
        );

        let now = Instant::now();
        let mut bindings = NameBindings::default();
        for (address, ttl) in bound {
            bindings.bind(address, "example.test", ttl, now);
        }
        let later = now + Duration::from_secs(120);

        assert_eq!(
            bindings.names(ip("203.0.113.1"), later),
            ResolvedNames::default()
        );
        assert_eq!(
            bindings.names(ip("2001:db8::1"), later),
            resolved(&["example.test"])
        );
    }

    /// A TTL of zero would otherwise expire before the connection it was
    /// looked up for, and a generous one would pin a name for the session.
    #[test]
    fn binding_lifetimes_are_clamped() {
        let now = Instant::now();
        let mut bindings = NameBindings::default();
        bindings.bind(ip("203.0.113.1"), "brief.test", Duration::ZERO, now);
        bindings.bind(
            ip("203.0.113.2"),
            "eternal.test",
            Duration::from_secs(86_400),
            now,
        );

        assert_eq!(
            bindings.names(
                ip("203.0.113.1"),
                now + MIN_BINDING_TTL - Duration::from_secs(1)
            ),
            resolved(&["brief.test"])
        );
        assert_eq!(
            bindings.names(
                ip("203.0.113.2"),
                now + MAX_BINDING_TTL + Duration::from_secs(1)
            ),
            ResolvedNames::default()
        );
    }

    /// A container that resolves in a loop must not grow the map without
    /// bound.
    #[test]
    fn bindings_are_capped() {
        let now = Instant::now();
        let mut bindings = NameBindings::default();
        for n in 0..(MAX_BOUND_ADDRESSES + 16) {
            let octets = (n as u32).to_be_bytes();
            let address = IpAddr::V4(Ipv4Addr::new(10, octets[1], octets[2], octets[3]));
            bindings.bind(address, "flood.test", Duration::from_secs(60), now);
        }

        assert!(bindings.by_ip.len() <= MAX_BOUND_ADDRESSES);
    }

    /// The interceptor waits out its whole timeout for a datagram that
    /// actually answers the query, so an off-path host that guessed the
    /// ephemeral port cannot decide what a name resolves to.
    #[tokio::test]
    async fn forward_dns_ignores_datagrams_that_do_not_answer_the_query() {
        let resolver = UdpSocket::bind("127.0.0.1:0").await.expect("bind resolver");
        let attacker = UdpSocket::bind("127.0.0.1:0").await.expect("bind attacker");
        let resolver_addr = resolver.local_addr().expect("resolver addr");

        let query = dns_packet(0x1234, DNS_QUERY_FLAGS, "example.test", &[]);
        let forged = dns_packet(
            0x1234,
            DNS_RESPONSE_FLAGS,
            "example.test",
            &[Rr::A("example.test", [203, 0, 113, 66], 60)],
        );
        let stale = dns_packet(
            0x4321,
            DNS_RESPONSE_FLAGS,
            "example.test",
            &[Rr::A("example.test", [203, 0, 113, 67], 60)],
        );
        let genuine = dns_packet(
            0x1234,
            DNS_RESPONSE_FLAGS,
            "example.test",
            &[Rr::A("example.test", [198, 51, 100, 7], 60)],
        );

        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let (_, client) = resolver.recv_from(&mut buf).await.expect("recv query");
            let _ = attacker.send_to(&forged, client).await;
            let _ = resolver.send_to(&stale, client).await;
            let _ = resolver.send_to(&genuine, client).await;
        });

        let parsed = dns_query(&query).expect("query");
        let response = forward_dns(&query, Some(&parsed), &[resolver_addr])
            .await
            .expect("dns exchange");

        assert!(dns_response_is_bindable(&response));
        assert_eq!(
            dns_bindings("example.test", &response),
            vec![(ip("198.51.100.7"), Duration::from_secs(60))]
        );
    }

    /// A query the interceptor cannot parse is still forwarded, and its
    /// answer comes straight back. Waiting out the timeout for an answer that
    /// can never be recognized would stall every query behind it.
    #[tokio::test]
    async fn an_unparsable_query_still_gets_its_answer() {
        let resolver = UdpSocket::bind("127.0.0.1:0").await.expect("bind resolver");
        let resolver_addr = resolver.local_addr().expect("resolver addr");
        // Two questions, so `dns_question` refuses it.
        let mut query = dns_packet(0x1234, DNS_QUERY_FLAGS, "example.test", &[]);
        query[5] = 2;
        let reply = b"whatever the resolver said".to_vec();
        let sent = reply.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let (_, client) = resolver.recv_from(&mut buf).await.expect("recv query");
            let _ = resolver.send_to(&sent, client).await;
        });

        assert!(dns_query(&query).is_none());
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            forward_dns(&query, None, &[resolver_addr]),
        )
        .await
        .expect("forward_dns must not wait out the timeout")
        .expect("dns exchange");

        assert_eq!(response, reply);
    }

    /// Lateness exists only in the timed read: a client that says nothing
    /// inside the window asserts nothing at decision time, whichever parser
    /// its bytes would have reached.
    #[tokio::test(start_paused = true)]
    async fn identity_arriving_after_the_sniff_window_is_not_part_of_the_decision() {
        for bytes in [
            tls_client_hello("evil.example"),
            http_request("evil.example"),
        ] {
            let (mut interceptor_side, mut client_side) = tokio::io::duplex(64 * 1024);
            let writer = tokio::spawn(async move {
                tokio::time::sleep(SNIFF_TIMEOUT * 2).await;
                let _ = client_side.write_all(&bytes).await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            });

            let sniffed = sniff_client_stream(&mut interceptor_side).await;

            assert_eq!(sniffed, Sniffed::default());
            writer.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn identity_arriving_inside_the_sniff_window_is_read() {
        let (mut interceptor_side, mut client_side) = tokio::io::duplex(64 * 1024);
        let hello = tls_client_hello("registry.npmjs.org");
        let writer = tokio::spawn(async move {
            tokio::time::sleep(SNIFF_TIMEOUT / 2).await;
            let _ = client_side.write_all(&hello).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let sniffed = sniff_client_stream(&mut interceptor_side).await;

        assert_eq!(sniffed.assertion.sni.as_deref(), Some("registry.npmjs.org"));
        assert!(!sniffed.initial.is_empty());
        writer.abort();
    }

    /// Two loopback pairs standing in for the container side and the upstream
    /// side of one bridged connection.
    async fn bridged_pair() -> (TcpStream, TcpStream, TcpStream, TcpStream) {
        let client_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind client");
        let upstream_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let client_addr = client_listener.local_addr().expect("client addr");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");

        let client = TcpStream::connect(client_addr)
            .await
            .expect("connect client");
        let (bridged_client, _) = client_listener.accept().await.expect("accept client");
        let upstream = TcpStream::connect(upstream_addr)
            .await
            .expect("connect upstream");
        let (upstream_server, _) = upstream_listener.accept().await.expect("accept upstream");

        (client, bridged_client, upstream, upstream_server)
    }

    fn allowed_outcome() -> ConnOutcome {
        ConnOutcome {
            assertion: ClientAssertion::default(),
            service: "-",
            decision: PolicyDecision::allow_default(),
            bytes_tx: 0,
            bytes_rx: 0,
        }
    }

    /// Fork 2: a name withheld until after the sniff window still has to be
    /// able to deny, and none of the bytes carrying it may reach upstream.
    #[tokio::test]
    async fn a_late_client_assertion_tears_the_bridge_down() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Allow)
                .deny_host("evil.example")
                .build()
                .expect("policy"),
        );
        let (mut client, mut bridged_client, mut upstream, mut upstream_server) =
            bridged_pair().await;
        let mut outcome = allowed_outcome();
        let resolved = ResolvedNames::default();

        // The server greets first, so bytes are already on their way to the
        // container when the late name arrives to deny the connection.
        const GREETING: &[u8] = b"220 service ready\r\n";
        upstream_server
            .write_all(GREETING)
            .await
            .expect("write greeting");

        let hello = tls_client_hello("evil.example");
        let writer = tokio::spawn(async move {
            // Reading the greeting first makes the deny strictly later than
            // the downstream copy that delivered it.
            let mut seen = vec![0u8; GREETING.len()];
            let _ = client.read_exact(&mut seen).await;
            let _ = client.write_all(&hello).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        bridge(
            &mut bridged_client,
            &mut upstream,
            &[],
            Some(LateRecheck {
                dst: addr("203.0.113.66:443"),
                resolved: &resolved,
                policy: &policy,
            }),
            &mut outcome,
        )
        .await;

        assert_eq!(outcome.decision.action, NetworkAction::Deny);
        assert_eq!(outcome.decision.rule, "deny[0]");
        assert_eq!(outcome.assertion.sni.as_deref(), Some("evil.example"));
        assert_eq!(outcome.bytes_tx, 0);
        assert_eq!(
            outcome.bytes_rx,
            GREETING.len() as u64,
            "bytes the container already received belong in the record"
        );

        drop(upstream);
        let mut seen = Vec::new();
        upstream_server
            .read_to_end(&mut seen)
            .await
            .expect("read upstream");
        assert!(
            seen.is_empty(),
            "no client bytes may reach a denied upstream"
        );
        writer.abort();
    }

    #[tokio::test]
    async fn a_late_client_assertion_that_does_not_deny_is_forwarded() {
        let policy = compiled(
            NetworkPolicy::builder()
                .default_action(NetworkAction::Allow)
                .deny_host("evil.example")
                .build()
                .expect("policy"),
        );
        let (mut client, mut bridged_client, mut upstream, mut upstream_server) =
            bridged_pair().await;
        let mut outcome = allowed_outcome();
        let resolved = ResolvedNames::default();

        let hello = tls_client_hello("fine.example");
        let expected = hello.clone();
        tokio::spawn(async move {
            let _ = client.write_all(&hello).await;
            let _ = client.shutdown().await;
        });
        let reader = tokio::spawn(async move {
            let mut seen = Vec::new();
            let _ = upstream_server.read_to_end(&mut seen).await;
            seen
        });

        bridge(
            &mut bridged_client,
            &mut upstream,
            &[],
            Some(LateRecheck {
                dst: addr("203.0.113.66:443"),
                resolved: &resolved,
                policy: &policy,
            }),
            &mut outcome,
        )
        .await;

        assert_eq!(outcome.decision.action, NetworkAction::Allow);
        assert_eq!(outcome.assertion.sni.as_deref(), Some("fine.example"));
        assert_eq!(outcome.bytes_tx, expected.len() as u64);
        assert_eq!(reader.await.expect("upstream reader"), expected);
    }

    #[tokio::test]
    async fn audit_sink_stamps_container_per_handle() {
        fn event() -> AuditEvent {
            AuditEvent {
                opened: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(1),
                orig: addr("10.0.2.100:50123"),
                dst: addr("93.184.216.34:443"),
                resolved: ResolvedNames::default(),
                assertion: ClientAssertion::default(),
                service: "-",
                bytes_tx: 0,
                bytes_rx: 0,
                decision: PolicyDecision::allow_default(),
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(NETWORK_LOG);
        let sink = AuditSink::open(path.clone(), "sid-1".to_string())
            .await
            .expect("open sink");
        write_audit(&sink.for_container("outrig-a"), event()).await;
        write_audit(&sink.for_container("outrig-b"), event()).await;

        let text = std::fs::read_to_string(path).expect("read audit log");
        let containers: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("record json"))
            .collect();
        assert_eq!(containers.len(), 2);
        for (record, container) in containers.iter().zip(["outrig-a", "outrig-b"]) {
            assert_eq!(record["outrig.session_id"], "sid-1");
            assert_eq!(record["outrig.container"], container);
        }
    }
}
