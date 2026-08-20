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
use std::fs::File as StdFile;
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
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
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{
    NetworkAction, NetworkEntry, NetworkHostPattern, NetworkPolicy, parse_network_host_pattern,
};
use crate::container::Container;
use crate::error::{IoPathExt, OutrigError, Result};
use crate::nsfork;
use crate::process::{self, Cmd, Transcript};

const NETWORK_LOG: &str = "network.jsonl";

/// Resolver the interceptor requires inside every attached container: DNS to
/// the loopback listener, `ndots:0` so bare names resolve without
/// search-domain expansion. Installed by `podman exec` on running containers
/// ([`install_audit_resolv_conf`]) and baked in via `podman create --dns` for
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

/// Per-container interception state: the container's accept loops and the
/// handle that deletes its nft table. Sockets live inside the container's
/// namespaces, so every attachment owns its own listeners and loops.
#[derive(Debug)]
struct Attachment {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    cleanup: Cleanup,
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

        let cleanup = Cleanup {
            pid,
            table: self.table.clone(),
            transcript: container.transcript(),
        };

        // A dns-preconfigured container had the loopback resolver baked in
        // via `podman create --dns` (`podman exec` cannot reach it before
        // start); everything else gets the exec-based install.
        if !container.dns_preconfigured() {
            install_audit_resolv_conf(container).await?;
        }
        apply_nft_rules(&cleanup, tcp_port, dns_port).await?;

        let bindings: Bindings = Arc::new(Mutex::new(NameBindings::default()));
        let cancel = self.cancel.child_token();
        let tasks = vec![
            tokio::spawn(tcp_accept_loop(
                sockets.tcp,
                self.audit.for_container(name),
                bindings.clone(),
                self.policy.clone(),
                cancel.clone(),
            )),
            tokio::spawn(dns_loop(sockets.dns, bindings, cancel.clone())),
        ];

        self.attachments.insert(
            name.to_string(),
            Attachment {
                cancel,
                tasks,
                cleanup,
            },
        );
        Ok(())
    }

    /// Detaches one container: cancels its loops and deletes its nft table
    /// without disturbing other attachments. Takes the container name rather
    /// than a [`Container`] so an already-dead container can still be
    /// detached (the nft delete against its defunct pid fails harmlessly).
    /// The container's `/etc/resolv.conf` is left pointing at the loopback
    /// listener; detach is intended to run just before the container stops.
    pub async fn detach(&mut self, container: &str) -> Result<()> {
        let attachment = self.attachments.remove(container).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container {container:?} is not attached to the network interceptor"
            ))
        })?;
        teardown_attachment(attachment).await;
        Ok(())
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        // Attachments live in disjoint namespaces, so their grace periods and
        // nft deletes can overlap.
        futures_util::future::join_all(
            std::mem::take(&mut self.attachments)
                .into_values()
                .map(teardown_attachment),
        )
        .await;
    }
}

impl Drop for NetworkInterceptor {
    fn drop(&mut self) {
        self.cancel.cancel();
        for attachment in self.attachments.values() {
            attachment.cleanup.spawn_detached_delete();
        }
    }
}

async fn teardown_attachment(attachment: Attachment) {
    attachment.cancel.cancel();
    for task in attachment.tasks {
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, task).await;
    }
    if let Err(e) = attachment.cleanup.delete_table().await {
        tracing::warn!(target: "outrig::network", "network cleanup failed: {e}");
    }
}

#[derive(Debug)]
struct InterceptorSockets {
    tcp: TcpListener,
    dns: UdpSocket,
}

#[derive(Debug, Clone)]
struct Cleanup {
    pid: u32,
    table: String,
    transcript: Option<Transcript>,
}

impl Cleanup {
    async fn delete_table(&self) -> Result<()> {
        let _ = process::try_capture_logged(
            nsenter_nft(self.pid)
                .args(["delete", "table", "inet"])
                .arg(&self.table),
            "network",
            self.transcript.as_ref(),
        )
        .await?;
        Ok(())
    }

    /// The `Drop` form of [`Self::delete_table`]: a destructor cannot await an
    /// `nsenter`, so the command is detached and its reap handed to
    /// [`crate::supervise`].
    fn spawn_detached_delete(&self) {
        // Issued once: this selects a namespace by pid, and a pid is reused.
        // A retry landing after the container exited would enter whatever
        // holds that pid now and delete a table belonging to it.
        crate::supervise::detach_cleanup(
            nsenter_nft(self.pid)
                .args(["delete", "table", "inet"])
                .arg(&self.table),
            crate::supervise::Reissue::Once,
        );
    }
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

async fn tcp_accept_loop(
    listener: TcpListener,
    audit: AuditSink,
    bindings: Bindings,
    policy: Arc<CompiledNetworkPolicy>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(handle_tcp(
                            stream,
                            peer,
                            audit.clone(),
                            bindings.clone(),
                            policy.clone(),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(target: "outrig::network", "tcp accept failed: {e}");
                        break;
                    }
                }
            }
        }
    }
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

async fn handle_tcp(
    mut client: TcpStream,
    orig: SocketAddr,
    audit: AuditSink,
    bindings: Bindings,
    policy: Arc<CompiledNetworkPolicy>,
) {
    let opened = SystemTime::now();
    let started = Instant::now();
    let dst = match original_dst(&client) {
        Ok(dst) => dst,
        Err(e) => {
            tracing::warn!(target: "outrig::network", "SO_ORIGINAL_DST failed: {e}");
            return;
        }
    };

    let sniffed = if server_speaks_first(dst.port()) {
        Sniffed::default()
    } else {
        sniff_client_stream(&mut client).await
    };
    // Read after the sniff, not before: the window is up to `SNIFF_TIMEOUT`
    // long, and a lookup the container completes inside it is evidence this
    // connection is entitled to have weighed.
    let resolved = resolved_names(&bindings, dst.ip());

    let mut outcome = ConnOutcome {
        decision: policy.decide(dst, &resolved, &sniffed.assertion),
        service: sniffed.assertion.service(),
        assertion: sniffed.assertion,
        bytes_tx: 0,
        bytes_rx: 0,
    };
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
    let mut late = None;

    // Counted as they move rather than at the end. `try_join!` cancels the
    // sibling direction when one fails -- which the late-deny path does on
    // purpose -- and a returned total dies with it, so bytes the container
    // already sent or received would vanish from the audit record.
    let mut bytes_tx = 0u64;
    let mut bytes_rx = 0u64;
    let bridged = {
        let downstream = async {
            copy_counting(&mut upstream_rx, &mut client_tx, &mut bytes_rx).await?;
            let _ = client_tx.shutdown().await;
            io::Result::Ok(())
        };
        let upward = async {
            if !initial.is_empty() {
                write_counting(&mut upstream_tx, initial, &mut bytes_tx).await?;
            }
            if let Some(recheck) = recheck {
                // Scoped so the sniff buffer is not held for the life of the
                // copy loop below.
                let mut buf = vec![0; SNIFF_BUFFER];
                let n = client_rx.read(&mut buf).await?;
                if n > 0 {
                    let assertion = sniff_client_bytes(&buf[..n]).unwrap_or_default();
                    let decision = recheck
                        .policy
                        .decide(recheck.dst, recheck.resolved, &assertion);
                    // `recheck` is only set when the sniff window closed with
                    // no assertion, so an empty one here re-derives the
                    // decision already taken and has nothing to record.
                    if !assertion.is_empty() {
                        let denied = decision.action == NetworkAction::Deny;
                        late = Some((assertion, decision));
                        if denied {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "denied by network policy on late client identity",
                            ));
                        }
                    }
                    write_counting(&mut upstream_tx, &buf[..n], &mut bytes_tx).await?;
                }
            }
            copy_counting(&mut client_rx, &mut upstream_tx, &mut bytes_tx).await?;
            let _ = upstream_tx.shutdown().await;
            io::Result::Ok(())
        };
        tokio::try_join!(downstream, upward)
    };

    outcome.bytes_tx += bytes_tx;
    outcome.bytes_rx += bytes_rx;
    if let Err(e) = bridged {
        tracing::debug!(target: "outrig::network", "tcp bridge ended with error: {e}");
    }
    if let Some((assertion, decision)) = late {
        outcome.service = assertion.service();
        outcome.assertion = assertion;
        outcome.decision = decision;
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

async fn dns_loop(socket: UdpSocket, bindings: Bindings, cancel: CancellationToken) {
    let resolvers = host_resolvers();
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
                match forward_dns(&raw, query.as_ref(), &resolvers).await {
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

async fn install_audit_resolv_conf(container: &Container) -> Result<()> {
    process::run_capture_logged(
        Cmd::new("podman")
            .args(["exec", "--user=0:0"])
            .arg(container.name())
            .args(["sh", "-c"])
            .arg(format!(
                "printf 'nameserver {INTERCEPT_DNS_NAMESERVER}\noptions \
                 {INTERCEPT_DNS_OPTION}\n' > /etc/resolv.conf"
            )),
        "podman",
        container.transcript().as_ref(),
    )
    .await?;
    Ok(())
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

async fn apply_nft_rules(cleanup: &Cleanup, tcp_port: u16, dns_port: u16) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(nft_rules(&cleanup.table, tcp_port, dns_port).as_bytes())?;
    file.as_file_mut().sync_all()?;

    process::run_capture_logged(
        nsenter_nft(cleanup.pid).arg("-f").arg(file.path()),
        "network",
        cleanup.transcript.as_ref(),
    )
    .await?;
    Ok(())
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
