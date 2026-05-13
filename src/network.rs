//! Per-session network audit plumbing.
//!
//! Audit mode is intentionally opt-in. When enabled, OutRig installs a small
//! nftables nat table in the session container's network namespace and keeps
//! host-side listener sockets in that namespace. The accepted sockets carry
//! the original destination metadata; upstream connections are opened from the
//! host namespace, so OutRig's own traffic is not routed back through the
//! interceptor.

use std::collections::BTreeMap;
use std::fs::File as StdFile;
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use rand::RngCore;
use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::process::{self, Cmd, Transcript};

const NETWORK_LOG: &str = "network.jsonl";
const SO_ORIGINAL_DST: libc::c_int = 80;
const SNIFF_TIMEOUT: Duration = Duration::from_millis(750);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type DnsCache = Arc<Mutex<BTreeMap<IpAddr, String>>>;

#[derive(Debug)]
pub struct NetworkInterceptor {
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    cleanup: Cleanup,
    disposed: bool,
}

impl NetworkInterceptor {
    pub async fn start(container: &Container, log_dir: &Path, session_id: &str) -> Result<Self> {
        require_tool("nft")?;
        require_tool("nsenter")?;

        let pid = container_pid(container).await?;
        let sockets = bind_interceptor_sockets(pid)?;
        let tcp_port = sockets.tcp.local_addr()?.port();
        let dns_port = sockets.dns.local_addr()?.port();

        tokio::fs::create_dir_all(log_dir).await?;
        let audit = AuditSink::open(
            log_dir.join(NETWORK_LOG),
            session_id.to_string(),
            container.name.clone(),
        )
        .await?;
        let dns_cache = Arc::new(Mutex::new(BTreeMap::new()));
        let cancel = CancellationToken::new();
        let cleanup = Cleanup {
            pid,
            table: nft_table_name(session_id),
            transcript: container.transcript(),
        };

        install_audit_resolv_conf(container).await?;
        apply_nft_rules(&cleanup, tcp_port, dns_port).await?;

        let tasks = vec![
            tokio::spawn(tcp_accept_loop(
                sockets.tcp,
                audit.clone(),
                dns_cache.clone(),
                cancel.clone(),
            )),
            tokio::spawn(dns_loop(sockets.dns, dns_cache, cancel.clone())),
        ];

        Ok(Self {
            cancel,
            tasks,
            cleanup,
            disposed: false,
        })
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, task).await;
        }
        if let Err(e) = self.cleanup.delete_table().await {
            tracing::warn!(target: "outrig::network", "network cleanup failed: {e}");
        }
        self.disposed = true;
    }
}

impl Drop for NetworkInterceptor {
    fn drop(&mut self) {
        if self.disposed {
            return;
        }
        self.cancel.cancel();
        self.cleanup.spawn_detached_delete();
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

    fn spawn_detached_delete(&self) {
        let mut cmd = std::process::Command::new("nsenter");
        let _ = cmd
            .arg("-t")
            .arg(self.pid.to_string())
            .args(["-U", "-n", "nft", "delete", "table", "inet"])
            .arg(&self.table)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[derive(Clone)]
struct AuditSink {
    file: Arc<AsyncMutex<tokio::fs::File>>,
    session_id: String,
    container: String,
}

impl AuditSink {
    async fn open(path: PathBuf, session_id: String, container: String) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(Self {
            file: Arc::new(AsyncMutex::new(file)),
            session_id,
            container,
        })
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
    #[serde(rename = "outrig.action")]
    outrig_action: &'static str,
    #[serde(rename = "outrig.rule")]
    outrig_rule: &'static str,
}

impl AuditRecord {
    fn new(session_id: &str, container: &str, event: AuditEvent) -> Self {
        let host = event.sniff.host.unwrap_or_default();
        Self {
            ts: zeek_timestamp(event.opened),
            uid: zeek_uid(),
            id_orig_h: event.orig.ip().to_string(),
            id_orig_p: event.orig.port(),
            id_resp_h: event.dst.ip().to_string(),
            id_resp_p: event.dst.port(),
            proto: "tcp",
            service: event.sniff.service,
            duration: event.duration.as_secs_f64(),
            orig_bytes: event.bytes_tx,
            resp_bytes: event.bytes_rx,
            conn_state: if event.bytes_rx == 0 { "S0" } else { "SF" },
            local_orig: true,
            local_resp: false,
            missed_bytes: 0,
            server_name: event.sniff.sni,
            outrig_session_id: session_id.to_string(),
            outrig_container: container.to_string(),
            outrig_host: host,
            outrig_action: "allow",
            outrig_rule: "default",
        }
    }
}

#[derive(Debug, Clone)]
struct AuditEvent {
    opened: SystemTime,
    duration: Duration,
    orig: SocketAddr,
    dst: SocketAddr,
    sniff: Sniff,
    bytes_tx: u64,
    bytes_rx: u64,
}

fn zeek_timestamp(ts: SystemTime) -> f64 {
    ts.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

fn zeek_uid() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sniff {
    service: &'static str,
    host: Option<String>,
    sni: Option<String>,
}

async fn tcp_accept_loop(
    listener: TcpListener,
    audit: AuditSink,
    dns_cache: DnsCache,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(handle_tcp(stream, peer, audit.clone(), dns_cache.clone()));
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

async fn handle_tcp(
    mut client: TcpStream,
    orig: SocketAddr,
    audit: AuditSink,
    dns_cache: DnsCache,
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
    let mut bytes_tx = 0;
    let mut bytes_rx = 0;
    let mut sniff = Sniff {
        service: "-",
        host: cached_host(&dns_cache, dst.ip()),
        sni: None,
    };

    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(e)) => {
            tracing::warn!(target: "outrig::network", "connect upstream {dst} failed: {e}");
            write_audit(
                &audit,
                AuditEvent {
                    opened,
                    duration: started.elapsed(),
                    orig,
                    dst,
                    sniff,
                    bytes_tx,
                    bytes_rx,
                },
            )
            .await;
            return;
        }
        Err(e) => {
            tracing::warn!(target: "outrig::network", "connect upstream {dst} timed out: {e}");
            write_audit(
                &audit,
                AuditEvent {
                    opened,
                    duration: started.elapsed(),
                    orig,
                    dst,
                    sniff,
                    bytes_tx,
                    bytes_rx,
                },
            )
            .await;
            return;
        }
    };

    if dst.port() == 22 {
        let mut buf = vec![0; 1024];
        if let Ok(Ok(n)) = tokio::time::timeout(SNIFF_TIMEOUT, upstream.read(&mut buf)).await
            && n > 0
        {
            if buf[..n].starts_with(b"SSH-") {
                sniff.service = "ssh";
            }
            bytes_rx += n as u64;
            if let Err(e) = client.write_all(&buf[..n]).await {
                tracing::debug!(target: "outrig::network", "ssh banner write failed: {e}");
                write_audit(
                    &audit,
                    AuditEvent {
                        opened,
                        duration: started.elapsed(),
                        orig,
                        dst,
                        sniff,
                        bytes_tx,
                        bytes_rx,
                    },
                )
                .await;
                return;
            }
        }
    } else {
        let mut buf = vec![0; 16 * 1024];
        if let Ok(Ok(n)) = tokio::time::timeout(SNIFF_TIMEOUT, client.read(&mut buf)).await
            && n > 0
        {
            sniff = sniff_client_bytes(&buf[..n]).unwrap_or(sniff);
            if sniff.host.is_none() {
                sniff.host = cached_host(&dns_cache, dst.ip());
            }
            bytes_tx += n as u64;
            if let Err(e) = upstream.write_all(&buf[..n]).await {
                tracing::debug!(target: "outrig::network", "initial upstream write failed: {e}");
                write_audit(
                    &audit,
                    AuditEvent {
                        opened,
                        duration: started.elapsed(),
                        orig,
                        dst,
                        sniff,
                        bytes_tx,
                        bytes_rx,
                    },
                )
                .await;
                return;
            }
        }
    }

    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((tx, rx)) => {
            bytes_tx += tx;
            bytes_rx += rx;
        }
        Err(e) => {
            tracing::debug!(target: "outrig::network", "tcp bridge {dst} ended with error: {e}");
        }
    }

    write_audit(
        &audit,
        AuditEvent {
            opened,
            duration: started.elapsed(),
            orig,
            dst,
            sniff,
            bytes_tx,
            bytes_rx,
        },
    )
    .await;
}

async fn write_audit(audit: &AuditSink, event: AuditEvent) {
    let record = AuditRecord::new(&audit.session_id, &audit.container, event);
    if let Err(e) = audit.write(&record).await {
        tracing::warn!(target: "outrig::network", "network audit write failed: {e}");
    }
}

async fn dns_loop(socket: UdpSocket, cache: DnsCache, cancel: CancellationToken) {
    let resolvers = host_resolvers();
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            received = socket.recv_from(&mut buf) => {
                let Ok((n, peer)) = received else {
                    break;
                };
                let query = buf[..n].to_vec();
                let query_name = dns_query_name(&query);
                tracing::debug!(
                    target: "outrig::network",
                    "dns query from {peer}: {:?}",
                    query_name
                );
                let socket_ref = &socket;
                match forward_dns(&query, &resolvers).await {
                    Ok(response) => {
                        if let Some(host) = query_name {
                            cache_dns_response(&cache, &host, &response);
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

async fn forward_dns(query: &[u8], resolvers: &[SocketAddr]) -> io::Result<Vec<u8>> {
    let mut last_err = None;
    for resolver in resolvers {
        let bind_addr = if resolver.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        let socket = UdpSocket::bind(bind_addr).await?;
        if let Err(e) = socket.send_to(query, resolver).await {
            last_err = Some(e);
            continue;
        }
        let mut buf = vec![0u8; 4096];
        match tokio::time::timeout(DNS_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => return Ok(buf[..n].to_vec()),
            Ok(Err(e)) => last_err = Some(e),
            Err(e) => last_err = Some(io::Error::new(io::ErrorKind::TimedOut, e)),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no DNS resolvers configured")))
}

fn host_resolvers() -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some(raw) = line.strip_prefix("nameserver").map(str::trim) else {
                continue;
            };
            if let Ok(ip) = raw.parse::<IpAddr>() {
                out.push(SocketAddr::new(ip, 53));
            }
        }
    }
    if out.is_empty() {
        out.push(SocketAddr::from(([1, 1, 1, 1], 53)));
    }
    out
}

async fn container_pid(container: &Container) -> Result<u32> {
    let output = process::run_capture_logged(
        Cmd::new("podman")
            .args(["inspect", "--format", "{{.State.Pid}}"])
            .arg(&container.name),
        "podman",
        container.transcript().as_ref(),
    )
    .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let pid = text.trim().parse::<u32>().map_err(|e| {
        OutrigError::Configuration(format!(
            "podman inspect {} returned invalid pid: {e}",
            container.name
        ))
    })?;
    if pid == 0 {
        return Err(OutrigError::Configuration(format!(
            "container {:?} has no running network namespace",
            container.name
        )));
    }
    Ok(pid)
}

async fn install_audit_resolv_conf(container: &Container) -> Result<()> {
    process::run_capture_logged(
        Cmd::new("podman")
            .args(["exec", "--user=0:0"])
            .arg(&container.name)
            .args([
                "sh",
                "-c",
                "printf 'nameserver 127.0.0.1\noptions ndots:0\n' > /etc/resolv.conf",
            ]),
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
        "network audit mode requires `{name}` on PATH"
    )))
}

fn bind_interceptor_sockets(pid: u32) -> Result<InterceptorSockets> {
    let user_ns = StdFile::open(format!("/proc/{pid}/ns/user"))?;
    let net_ns = StdFile::open(format!("/proc/{pid}/ns/net"))?;
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
    let mut sv = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, sv.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }

    let child = unsafe { libc::fork() };
    if child == -1 {
        close_fd(sv[0]);
        close_fd(sv[1]);
        return Err(io::Error::last_os_error());
    }

    if child == 0 {
        close_fd(sv[0]);
        let status = child_bind_and_send_fds(sv[1], user_ns, net_ns);
        unsafe { libc::_exit(status) };
    }

    close_fd(sv[1]);
    let received = recv_fds(sv[0]);
    close_fd(sv[0]);

    let mut status = 0;
    let _ = unsafe { libc::waitpid(child, &mut status, 0) };

    let fds = received?;
    if fds.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network namespace helper did not return listener sockets",
        ));
    }
    Ok((fds[0], fds[1]))
}

fn child_bind_and_send_fds(sock: RawFd, user_ns: RawFd, net_ns: RawFd) -> i32 {
    if setns_raw(user_ns, libc::CLONE_NEWUSER).is_err() {
        return 1;
    }
    unsafe {
        let _ = libc::setgid(0);
        let _ = libc::setuid(0);
    }
    if setns_raw(net_ns, libc::CLONE_NEWNET).is_err() {
        return 1;
    }

    let tcp = match std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) {
        Ok(listener) => listener,
        Err(_) => return 1,
    };
    let dns = match std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 53))) {
        Ok(socket) => socket,
        Err(_) => return 1,
    };

    match send_fds(sock, &[tcp.as_raw_fd(), dns.as_raw_fd()]) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn setns_raw(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    let rc = unsafe { libc::setns(fd, nstype) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn send_fds(sock: RawFd, fds: &[RawFd]) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = vec![0u8; cmsg_space(std::mem::size_of_val(fds))];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len();

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = cmsg_len(std::mem::size_of_val(fds));
        std::ptr::copy_nonoverlapping(
            fds.as_ptr().cast::<u8>(),
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            std::mem::size_of_val(fds),
        );
        msg.msg_controllen = (*cmsg).cmsg_len;
        if libc::sendmsg(sock, &msg, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fds(sock: RawFd) -> io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = vec![0u8; cmsg_space(std::mem::size_of::<[RawFd; 2]>())];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len();

    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n == -1 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "network namespace helper exited without returning sockets",
        ));
    }

    let mut out = Vec::new();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "network namespace helper returned no socket rights",
            ));
        }
        let data_len = (*cmsg).cmsg_len.saturating_sub(cmsg_len(0));
        let count = data_len / std::mem::size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        for i in 0..count {
            out.push(*data.add(i));
        }
    }
    Ok(out)
}

fn cmsg_align(len: usize) -> usize {
    let align = std::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>()) + data_len
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
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

fn sniff_client_bytes(bytes: &[u8]) -> Option<Sniff> {
    if let Some(host) = http_host(bytes) {
        return Some(Sniff {
            service: "http",
            host: Some(host),
            sni: None,
        });
    }
    if let Some(sni) = tls_sni(bytes) {
        return Some(Sniff {
            service: "ssl",
            host: Some(sni.clone()),
            sni: Some(sni),
        });
    }
    None
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

fn cached_host(cache: &DnsCache, ip: IpAddr) -> Option<String> {
    cache.lock().ok()?.get(&ip).cloned()
}

fn cache_dns_response(cache: &DnsCache, host: &str, packet: &[u8]) {
    let ips = dns_answer_ips(packet);
    if ips.is_empty() {
        return;
    }
    if let Ok(mut cache) = cache.lock() {
        for ip in ips {
            cache.insert(ip, host.to_string());
        }
    }
}

fn dns_query_name(packet: &[u8]) -> Option<String> {
    if packet.len() < 12 {
        return None;
    }
    let (name, _) = read_dns_name(packet, 12, 0)?;
    Some(name)
}

fn dns_answer_ips(packet: &[u8]) -> Vec<IpAddr> {
    if packet.len() < 12 {
        return Vec::new();
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut off = 12;
    for _ in 0..qdcount {
        let Some((_, next)) = read_dns_name(packet, off, 0) else {
            return Vec::new();
        };
        off = next.saturating_add(4);
        if off > packet.len() {
            return Vec::new();
        }
    }

    let mut ips = Vec::new();
    for _ in 0..ancount {
        let Some((_, next)) = read_dns_name(packet, off, 0) else {
            break;
        };
        off = next;
        if off + 10 > packet.len() {
            break;
        }
        let rr_type = u16::from_be_bytes([packet[off], packet[off + 1]]);
        let rdlen = u16::from_be_bytes([packet[off + 8], packet[off + 9]]) as usize;
        off += 10;
        if off + rdlen > packet.len() {
            break;
        }
        match (rr_type, rdlen) {
            (1, 4) => {
                ips.push(IpAddr::V4(Ipv4Addr::new(
                    packet[off],
                    packet[off + 1],
                    packet[off + 2],
                    packet[off + 3],
                )));
            }
            (28, 16) => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&packet[off..off + 16]);
                ips.push(IpAddr::V6(Ipv6Addr::from(octets)));
            }
            _ => {}
        }
        off += rdlen;
    }
    ips
}

fn read_dns_name(packet: &[u8], mut off: usize, depth: usize) -> Option<(String, usize)> {
    if depth > 8 {
        return None;
    }
    let mut labels = Vec::new();
    let end;
    loop {
        let len = *packet.get(off)?;
        if len & 0b1100_0000 == 0b1100_0000 {
            let b2 = *packet.get(off + 1)?;
            let ptr = (((len & 0b0011_1111) as usize) << 8) | b2 as usize;
            let (suffix, _) = read_dns_name(packet, ptr, depth + 1)?;
            labels.push(suffix);
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
        labels.push(std::str::from_utf8(&packet[off..next]).ok()?.to_string());
        off = next;
    }
    Some((labels.join("."), end))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn http_host_sniff_strips_port() {
        let sniff = sniff_client_bytes(b"GET / HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .expect("http sniff");
        assert_eq!(sniff.service, "http");
        assert_eq!(sniff.host.as_deref(), Some("example.com"));
    }

    #[test]
    fn audit_record_uses_zeek_conn_field_names() {
        let record = AuditRecord::new(
            "20260513T000000-abcd",
            "outrig-test",
            AuditEvent {
                opened: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_millis(125),
                orig: "10.0.2.100:50123".parse().expect("orig addr"),
                dst: "93.184.216.34:443".parse().expect("dst addr"),
                sniff: Sniff {
                    service: "ssl",
                    host: Some("example.com".to_string()),
                    sni: Some("example.com".to_string()),
                },
                bytes_tx: 517,
                bytes_rx: 1298,
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
    fn dns_parser_caches_a_records_for_query_name() {
        let response = [
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x07, b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x01, 0x00,
            0x01, 0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04, 93, 184,
            216, 34,
        ];
        assert_eq!(dns_query_name(&response).as_deref(), Some("example.com"));
        assert_eq!(
            dns_answer_ips(&response),
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]
        );
    }
}
