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
//! attachment's own DNS listener validated for an address ([`ResolvedNames`])
//! are the only thing that may *grant* a hostname rule; the name a client
//! writes into a `Host:` header or a TLS `ClientHello` ([`ClientAssertion`])
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
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{
    NetworkAction, NetworkEntry, NetworkHostPattern, NetworkPolicy, parse_network_host_pattern,
};
use crate::container::Container;
use crate::error::{IoPathExt, NetworkTeardownCause, NetworkTeardownFailure, OutrigError, Result};
use crate::nsfork;
use crate::process::{self, Cmd, Transcript};
use crate::supervise::{Reissue, detach_cleanup};

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
    table: String,
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
            table: nft_table_name(session_id),
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

        let target = Target {
            name: name.to_string(),
            pid,
            table: self.table.clone(),
            dns_preconfigured: container.dns_preconfigured(),
        };
        let transcript = container.transcript();
        let run = |cmd: Cmd| run_step(cmd, transcript.clone());

        // The undo log is this frame's, so a caller that drops this future
        // while a command is in flight drops it too, and its destructor puts
        // the container back without needing a runtime to do it.
        let mut rollback = Rollback::new(pid);
        if let Err(e) = install_interception(&run, &mut rollback, &target, tcp_port, dns_port).await
        {
            for failure in rollback.undo_now(&run).await {
                tracing::warn!(target: "outrig::network", "rolling back attach: {failure}");
            }
            return Err(e);
        }

        let bindings: Bindings = Arc::new(Mutex::new(NameBindings::default()));
        let cancel = self.cancel.child_token();
        let mut tasks = JoinSet::new();
        tasks.spawn(tcp_accept_loop(
            sockets.tcp,
            self.audit.for_container(name),
            bindings.clone(),
            self.policy.clone(),
            cancel.clone(),
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
                cancel,
                tasks,
                rollback,
                transcript,
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
        teardown_result(failures.into_iter().flatten().collect())
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
        mut tasks,
        mut rollback,
        transcript,
    } = attachment;
    cancel.cancel();
    let mut failures = stop_tasks(&mut tasks).await;
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
    /// A container whose resolver was baked in by `podman create --dns` has
    /// no resolv.conf to snapshot and none to put back.
    dns_preconfigured: bool,
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
    /// The container's init pid, used only to ask whether the namespace these
    /// commands would enter still exists.
    pid: u32,
    /// Undo commands in the order their mutations were made, discharged in
    /// reverse.
    undo: Vec<Cmd>,
}

impl Rollback {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            undo: Vec::new(),
        }
    }

    /// Take responsibility for `undo`, before the mutation it reverses runs.
    /// Every undo here is idempotent, so arming one that turns out never to
    /// have been needed costs nothing -- while arming afterwards would lose
    /// whichever mutation a cancellation landed in the middle of.
    fn arm(&mut self, undo: Cmd) {
        self.undo.push(undo);
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
        if !container_alive(self.pid) {
            self.undo.clear();
            return Vec::new();
        }
        let mut failures = Vec::new();
        while let Some(cmd) = self.undo.last().cloned() {
            let outcome = run(cmd).await;
            self.undo.pop();
            if let Err(e) = outcome {
                failures.push(e);
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
        if !container_alive(self.pid) {
            return;
        }
        for cmd in self.undo.drain(..).rev() {
            // Issued once: these select a container by name and a namespace
            // by pid, and both identities are reusable. A retry landing after
            // the container exited would rewrite some other container's file,
            // or delete a table in some other namespace.
            detach_cleanup(cmd, Reissue::Once);
        }
    }
}

/// Whether the container the undo commands target still exists. One that has
/// exited took its namespace -- and with it the nft table, and any point in
/// restoring its resolv.conf -- along with it, so there is nothing left to
/// undo and nothing to report.
fn container_alive(pid: u32) -> bool {
    // The network namespace, not the pid. A pid entry outlives what these
    // commands need it for -- a zombie init still has one -- and the namespace
    // is the thing `nsenter` enters and the thing the container's `/etc` lives
    // beside, so its absence is what actually means "there is nothing left to
    // undo".
    Path::new(&format!("/proc/{pid}/ns/net")).exists()
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
        let snapshot = run(read_resolv_conf(&target.name)).await?;
        rollback.arm(restore_resolv_conf(&target.name, snapshot));
        run(install_resolv_conf(&target.name)).await?;
    }

    let mut rules = tempfile::NamedTempFile::new()?;
    rules.write_all(nft_rules(&target.table, tcp_port, dns_port).as_bytes())?;
    rules.as_file_mut().sync_all()?;
    // `nft -f` applies as one kernel transaction, so the table this deletes
    // either exists whole or was never created.
    rollback.arm(delete_nft_table(target.pid, &target.table));
    run(nsenter_nft(target.pid).arg("-f").arg(rules.path())).await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct AuditSink {
    file: Arc<AsyncMutex<tokio::fs::File>>,
    session_id: String,
    container: String,
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
        Ok(Self {
            file: Arc::new(AsyncMutex::new(file)),
            session_id,
            container: String::new(),
        })
    }

    /// A handle writing to the same file whose records are stamped with
    /// `container`.
    fn for_container(&self, container: &str) -> Self {
        Self {
            container: container.to_string(),
            ..self.clone()
        }
    }

    async fn write(&self, record: &AuditRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record)
            .map_err(|e| OutrigError::Configuration(format!("encoding network audit: {e}")))?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
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
) {
    loop {
        reap_finished(conns);
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => match original_dst(&stream) {
                        Ok(dst) => {
                            conns.spawn(handle_tcp(
                                stream,
                                peer,
                                dst,
                                audit.clone(),
                                bindings.clone(),
                                policy.clone(),
                                conn_cancel.clone(),
                            ));
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
        tracing::warn!(target: "outrig::network", "network audit write failed: {e}");
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
fn read_resolv_conf(container: &str) -> Cmd {
    Cmd::new("podman")
        .args(["exec", "--user=0:0"])
        .arg(container)
        .args([
            "sh",
            "-c",
            "if [ -e /etc/resolv.conf ]; then printf 1; cat /etc/resolv.conf; \
             else printf 0; fi",
        ])
}

/// The command that puts back whatever [`read_resolv_conf`] found: the file
/// with exactly its old bytes, or its absence.
fn restore_resolv_conf(container: &str, snapshot: Vec<u8>) -> Cmd {
    match snapshot.split_first() {
        Some((&RESOLV_PRESENT, original)) => write_resolv_conf(container, original.to_vec()),
        _ => remove_resolv_conf(container),
    }
}

fn remove_resolv_conf(container: &str) -> Cmd {
    Cmd::new("podman")
        .args(["exec", "--user=0:0"])
        .arg(container)
        .args(["rm", "-f", "/etc/resolv.conf"])
}

fn install_resolv_conf(container: &str) -> Cmd {
    write_resolv_conf(
        container,
        format!("nameserver {INTERCEPT_DNS_NAMESERVER}\noptions {INTERCEPT_DNS_OPTION}\n").into(),
    )
}

/// Writes `content` as the container's resolver.
///
/// `content` rides in as `sh`'s `$1` rather than being interpolated into the
/// script, so the bytes a restore puts back -- arbitrary, since they are
/// whatever that container happened to have -- never pass through quoting or
/// escaping at all. `printf '%s'` completes it: no escape processing, and no
/// newline of its own.
/// Writes the bytes handed in as `$1` to the resolver file. A `const` so a
/// test can run this exact script rather than a paraphrase of it -- the claim
/// is that it reproduces arbitrary bytes, and a copy of the script proves that
/// about the copy.
const RESTORE_RESOLV_SCRIPT: &str = "printf '%s' \"$1\" > /etc/resolv.conf";

fn write_resolv_conf(container: &str, content: Vec<u8>) -> Cmd {
    Cmd::new("podman")
        .args(["exec", "--user=0:0"])
        .arg(container)
        .args(["sh", "-c", RESTORE_RESOLV_SCRIPT, "_"])
        .arg(OsString::from_vec(content))
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

fn nsenter_nft(pid: u32) -> Cmd {
    Cmd::new("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .args(["-U", "-n", "nft"])
}

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
    format!("outrig_{suffix}")
}

fn nft_rules(table: &str, tcp_port: u16, dns_port: u16) -> String {
    format!(
        "\
table inet {table} {{
  chain output {{
    type nat hook output priority dstnat; policy accept;
    ip daddr 127.0.0.0/8 return
    ip6 daddr ::1 return
    meta l4proto tcp redirect to :{tcp_port}
    udp dport 53 redirect to :{dns_port}
  }}
}}
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
            dns_preconfigured: false,
        }
    }

    /// R5's mechanism, on the real command path. Every other teardown test
    /// injects failure at the runner, which means none of them would notice
    /// `run_step` losing its exit-status check -- and a teardown that quietly
    /// failed is the thing this reports.
    #[tokio::test]
    async fn a_non_zero_undo_reaches_the_caller() {
        let mut rollback = Rollback::new(std::process::id());
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
        let mut rollback = Rollback::new(std::process::id());
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

        // The production script, with only its redirect retargeted.
        let script = RESTORE_RESOLV_SCRIPT
            .replace("/etc/resolv.conf", target.to_str().expect("utf-8 tempdir"));
        run_step(
            Cmd::new("/bin/sh")
                .args(["-c"])
                .arg(script)
                .arg("_")
                .arg(OsString::from_vec(NASTY.to_vec())),
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
        let mut rollback = Rollback::new(std::process::id());

        install_interception(&run, &mut rollback, &target(), 4001, 4002)
            .await
            .expect("install interception");
        let failures = rollback.undo_now(&run).await;

        assert!(failures.is_empty(), "{failures:#?}");
        let ran = fake.ran();
        assert_eq!(ran.len(), 5, "{ran:#?}");
        assert!(ran[0].contains("cat /etc/resolv.conf"), "{ran:#?}");
        assert!(ran[1].contains("nameserver 127.0.0.1"), "{ran:#?}");
        assert!(ran[2].contains("nft -f"), "{ran:#?}");
        assert!(ran[3].contains("delete table inet outrig_test"), "{ran:#?}");
        assert!(ran[4].contains("nameserver 10.0.2.3"), "{ran:#?}");
        assert!(rollback.armed().is_empty(), "{:#?}", rollback.armed());
    }

    /// The resolver a restore writes is the bytes the snapshot read, with
    /// nothing in between: they travel as an argument, so no quoting rule has
    /// to hold for the file to come back exactly as it was.
    #[test]
    fn a_restore_carries_the_original_resolver_bytes_verbatim() {
        let restore = restore_resolv_conf("outrig-test", present_snapshot());
        assert_eq!(
            restore.args.last().expect("resolver content"),
            ORIGINAL_RESOLV
        );
    }

    /// Having no resolver file at all is a state too, and the one a bare `cat`
    /// could neither report nor put back: the restore for it removes the file
    /// the install created rather than leaving an empty one behind.
    #[test]
    fn a_container_with_no_resolver_file_is_restored_to_having_none() {
        let restore = restore_resolv_conf("outrig-test", b"0".to_vec());
        let rendered = restore.render();
        assert!(rendered.contains("rm -f /etc/resolv.conf"), "{rendered}");
    }

    /// A failed nft apply is a failed attach, and the resolver mutation that
    /// preceded it is still undone: the container gets its own file back even
    /// though the table never landed.
    #[tokio::test]
    async fn a_failed_nft_apply_still_restores_the_resolver() {
        let fake = FakeRunner {
            fail_on: Some("nft -f"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let mut rollback = Rollback::new(std::process::id());

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
        let mut rollback = Rollback::new(std::process::id());
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
            hang_on: Some("nft -f"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        let target = target();
        let mut rollback = Rollback::new(std::process::id());
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
        let mut rollback = Rollback::new(std::process::id());
        let target = Target {
            dns_preconfigured: true,
            ..target()
        };

        install_interception(&run, &mut rollback, &target, 4001, 4002)
            .await
            .expect("install interception");

        let ran = fake.ran();
        assert_eq!(ran.len(), 1, "{ran:#?}");
        assert!(ran[0].contains("nft -f"), "{ran:#?}");
        assert_eq!(rollback.armed().len(), 1, "{:#?}", rollback.armed());
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
            let mut rollback = Rollback::new(std::process::id());
            rollback.arm(touch(&resolver));
            rollback.arm(touch(&table));
        }

        wait_for(&resolver);
        wait_for(&table);
    }

    /// Detaching a container that has already exited is a success: its
    /// namespace went with it, so there is no table left to delete and no
    /// resolv.conf left worth restoring.
    #[tokio::test]
    async fn an_exited_container_has_nothing_left_to_undo() {
        let fake = FakeRunner {
            fail_on: Some("podman"),
            ..Default::default()
        };
        let run = |cmd: Cmd| fake.run(cmd);
        // No process can hold pid 0, so this stands in for a container whose
        // init is gone.
        let mut rollback = Rollback::new(0);
        rollback.arm(write_resolv_conf(
            "outrig-test",
            ORIGINAL_RESOLV.as_bytes().to_vec(),
        ));

        let failures = rollback.undo_now(&run).await;

        assert!(failures.is_empty(), "{failures:#?}");
        assert!(fake.ran().is_empty(), "{:#?}", fake.ran());
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
        let clients = tokio::spawn(async move {
            for _ in 0..64 {
                // Closed at once, so each connection finishes on its own and
                // leaves its handle behind for the loop to take back.
                drop(TcpStream::connect(addr).await.expect("connect"));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            token.cancel();
        });

        accept_into(
            &listener,
            &audit,
            &empty_bindings(),
            &deny_all_policy(),
            &cancel,
            &conn_cancel,
            &mut conns,
        )
        .await;
        clients.await.expect("clients");

        // Not zero: whichever connection finishes after the loop's last reap
        // is still held, and the drain that follows `accept_into` in
        // production is what collects it. The claim is that the carrier does
        // not grow with the number of connections served -- without the reap
        // all 64 are still here.
        assert!(
            conns.len() < 8,
            "{} of 64 finished connections were never taken back out of the carrier",
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
        assert!(rules.contains("table inet outrig_test"));
        assert!(rules.contains("ip daddr 127.0.0.0/8 return"));
        assert!(rules.contains("ip6 daddr ::1 return"));
        assert!(rules.contains("meta l4proto tcp redirect to :44123"));
        assert!(rules.contains("udp dport 53 redirect to :44124"));
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
