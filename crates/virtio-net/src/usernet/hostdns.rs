//! Name resolution through the **host's own resolver** (2026-10-12).
//!
//! The relay in `super` forwards a guest's DNS datagrams to one upstream
//! server. That breaks exactly where users notice: a corporate VPN with split
//! DNS (Windows NRPT rules, systemd-resolved per-link domains) answers internal
//! names only through the host's resolver *API*, never through whichever server
//! a relay picked, and a name in the host's `hosts` file is not in DNS at all.
//! VirtualBox has a "host resolver" mode for this reason.
//!
//! So `A` and `AAAA` questions — what every lookup a browser, `apt` or `ssh`
//! makes begins with — are answered here, by `getaddrinfo` on a small pool of
//! host threads, and turned back into a DNS reply. Everything else (MX, TXT,
//! SRV, PTR, an EDNS query with options, anything unusual) still goes to the
//! upstream relay, which now points at the host's first DNS server instead of
//! a public one ([`host_dns_servers`]).
//!
//! **The guest is untrusted.** [`parse_question`] accepts only a plain
//! standard query with one question, every read bounds-checked, no
//! compression pointers, labels of letters, digits, `-` and `_` — anything
//! else is not parsed but relayed, so nothing odd ever reaches `getaddrinfo`.
//! The pool is a fixed number of threads behind a bounded queue: a guest that
//! floods queries fills the queue and the rest are relayed, never more threads.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resolver threads. `getaddrinfo` blocks, sometimes for seconds on a dead
/// server, so lookups run beside the network pump, never on it.
const WORKERS: usize = 4;
/// Questions waiting for a worker. Past it a question is relayed instead.
const QUEUE: usize = 64;
/// TTL on the answers this makes: short, so a VPN coming up is noticed soon.
const ANSWER_TTL: u32 = 30;
/// Answers per reply, which keeps every reply well inside one frame.
const MAX_ANSWERS: usize = 16;
/// A lookup slower than this is answered nowhere: the guest has retried long
/// since, and its retry is queued like any other question.
const STALE: Duration = Duration::from_secs(10);

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_OPT: u16 = 41;
const CLASS_IN: u16 = 1;

/// One question this module can answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The guest's transaction id.
    pub id: u16,
    /// Recursion desired, echoed back.
    pub rd: bool,
    /// The name, dotted, without the trailing dot.
    pub name: String,
    /// [`TYPE_A`] or [`TYPE_AAAA`].
    pub qtype: u16,
    /// The question section exactly as the guest sent it: echoed verbatim,
    /// which is what a stub resolver checks its answer against.
    pub section: Vec<u8>,
}

/// Reads a standard query with one `A`/`AAAA` question for a plain host name;
/// `None` for anything else (which the relay handles).
pub fn parse_question(msg: &[u8]) -> Option<Question> {
    let header = msg.get(..12)?;
    let word = |i: usize| u16::from_be_bytes([header[i], header[i + 1]]);
    let id = word(0);
    let flags = word(2);
    let (qd, an, ns, ar) = (word(4), word(6), word(8), word(10));
    // QR = 0 (a query), OPCODE = 0 (standard), one question, no answers.
    if flags & 0x8000 != 0 || (flags >> 11) & 0xf != 0 || qd != 1 || an != 0 || ns != 0 || ar > 1 {
        return None;
    }
    let mut at = 12usize;
    let mut name = String::new();
    loop {
        let len = usize::from(*msg.get(at)?);
        at += 1;
        if len == 0 {
            break;
        }
        // No compression pointers (0xc0) or reserved label types in a query.
        if len > 63 {
            return None;
        }
        let label = msg.get(at..at + len)?;
        if !label
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
        {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(label).ok()?);
        if name.len() > 253 {
            return None;
        }
        at += len;
    }
    if name.is_empty() {
        return None;
    }
    let tail = msg.get(at..at + 4)?;
    let qtype = u16::from_be_bytes([tail[0], tail[1]]);
    let qclass = u16::from_be_bytes([tail[2], tail[3]]);
    if qclass != CLASS_IN || !(qtype == TYPE_A || qtype == TYPE_AAAA) {
        return None;
    }
    let section = msg.get(12..at + 4)?.to_vec();
    at += 4;
    // At most one additional record, and only a bare EDNS OPT (no options:
    // a client asking for DNSSEC or a cookie gets the real server instead).
    if ar == 1 {
        let opt = msg.get(at..at + 11)?;
        let rtype = u16::from_be_bytes([opt[1], opt[2]]);
        let rdlen = u16::from_be_bytes([opt[9], opt[10]]);
        if opt[0] != 0 || rtype != TYPE_OPT || rdlen != 0 {
            return None;
        }
    }
    Some(Question {
        id,
        rd: flags & 0x0100 != 0,
        name,
        qtype,
        section,
    })
}

/// The reply to `question` given what the host's resolver said: its addresses,
/// or `None` when it could not resolve the name (answered NXDOMAIN).
pub fn reply(question: &Question, resolved: Option<&[IpAddr]>) -> Vec<u8> {
    let records: Vec<&IpAddr> = resolved
        .unwrap_or(&[])
        .iter()
        .filter(|ip| match question.qtype {
            TYPE_A => ip.is_ipv4(),
            _ => ip.is_ipv6(),
        })
        .take(MAX_ANSWERS)
        .collect();
    let rcode: u16 = if resolved.is_none() { 3 } else { 0 };
    let flags: u16 = 0x8000 | (u16::from(question.rd) << 8) | 0x0080 | rcode;
    let mut out = Vec::with_capacity(12 + question.section.len() + records.len() * 28);
    for word in [question.id, flags, 1, records.len() as u16, 0, 0] {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(&question.section);
    for ip in records {
        // The name: a pointer to the question's, at offset 12.
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&question.qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&ANSWER_TTL.to_be_bytes());
        match ip {
            IpAddr::V4(v4) => {
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.extend_from_slice(&16u16.to_be_bytes());
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

struct Job {
    guest_port: u16,
    question: Question,
    queued: Instant,
}

/// The pool: questions in, `(guest port, reply)` out.
pub(super) struct HostResolver {
    jobs: Option<SyncSender<Job>>,
    answers: Option<Receiver<(u16, Vec<u8>)>>,
    pending: usize,
    /// How names are looked up; `getaddrinfo` except in tests.
    lookup: Lookup,
}

/// A host lookup: the name's addresses, or `None` if it does not resolve.
pub(super) type Lookup = Arc<dyn Fn(&str) -> Option<Vec<IpAddr>> + Send + Sync>;

/// `getaddrinfo`, through the standard library.
pub(super) fn system_lookup() -> Lookup {
    Arc::new(|name: &str| {
        // getaddrinfo lists an address once per socket type (stream, datagram,
        // raw): keep the first of each, in the resolver's order.
        (name, 0u16).to_socket_addrs().ok().map(|addrs| {
            let mut unique: Vec<IpAddr> = Vec::new();
            for ip in addrs.map(|a| a.ip()) {
                if !unique.contains(&ip) {
                    unique.push(ip);
                }
            }
            unique
        })
    })
}

impl HostResolver {
    pub(super) fn new(lookup: Lookup) -> Self {
        Self {
            jobs: None,
            answers: None,
            pending: 0,
            lookup,
        }
    }

    /// Starts the workers on first use, so a VM that never resolves anything
    /// never has them.
    fn start(&mut self) -> Option<&SyncSender<Job>> {
        if self.jobs.is_none() {
            let (job_tx, job_rx) = mpsc::sync_channel::<Job>(QUEUE);
            let (answer_tx, answer_rx) = mpsc::channel();
            let job_rx = Arc::new(Mutex::new(job_rx));
            for n in 0..WORKERS {
                let job_rx = Arc::clone(&job_rx);
                let answer_tx = answer_tx.clone();
                let lookup = Arc::clone(&self.lookup);
                let spawned = std::thread::Builder::new()
                    .name(format!("usernet-dns-{n}"))
                    .spawn(move || loop {
                        let job = match job_rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        if job.queued.elapsed() > STALE {
                            continue;
                        }
                        let resolved = lookup(&job.question.name);
                        tracing::debug!(
                            name = %job.question.name,
                            qtype = job.question.qtype,
                            found = resolved.as_ref().map_or(0, Vec::len),
                            "usernet: resolved through the host"
                        );
                        let message = reply(&job.question, resolved.as_deref());
                        if answer_tx.send((job.guest_port, message)).is_err() {
                            return;
                        }
                    });
                if let Err(error) = spawned {
                    tracing::warn!(%error, "cannot start a host DNS worker");
                }
            }
            self.jobs = Some(job_tx);
            self.answers = Some(answer_rx);
        }
        self.jobs.as_ref()
    }

    /// Takes the query if this module answers it; `false` sends it to the
    /// relay (not a question for here, or the queue is full).
    pub(super) fn submit(&mut self, guest_port: u16, payload: &[u8]) -> bool {
        let Some(question) = parse_question(payload) else {
            return false;
        };
        let Some(jobs) = self.start() else {
            return false;
        };
        match jobs.try_send(Job {
            guest_port,
            question,
            queued: Instant::now(),
        }) {
            Ok(()) => {
                self.pending += 1;
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.jobs = None;
                false
            }
        }
    }

    /// One finished answer, if any.
    pub(super) fn poll(&mut self) -> Option<(u16, Vec<u8>)> {
        let answer = self.answers.as_ref()?.try_recv().ok()?;
        self.pending = self.pending.saturating_sub(1);
        Some(answer)
    }

    /// Whether answers are still to come (a stale one is never answered, so
    /// this can stay true for [`STALE`] after a hang; the pump only uses it
    /// to keep polling).
    pub(super) fn is_busy(&self) -> bool {
        self.pending > 0
    }
}

/// The host's DNS servers, IPv4, in the order the host would use them: where
/// the relay sends what the host resolver does not answer. Empty when they
/// cannot be read, and the caller falls back to a public resolver.
pub fn host_dns_servers() -> Vec<SocketAddr> {
    imp::servers()
}

#[cfg(target_os = "linux")]
mod imp {
    use std::net::{IpAddr, SocketAddr};

    /// `nameserver` lines of `/etc/resolv.conf`. A local stub
    /// (`127.0.0.53`, systemd-resolved) is kept: the relay runs on the host,
    /// where the stub is reachable.
    pub(super) fn servers() -> Vec<SocketAddr> {
        let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") else {
            return Vec::new();
        };
        parse_resolv_conf(&text)
    }

    pub(super) fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("nameserver"))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter_map(|addr| addr.parse::<IpAddr>().ok())
            .filter(IpAddr::is_ipv4)
            .map(|ip| SocketAddr::new(ip, 53))
            .collect()
    }
}

#[cfg(windows)]
mod imp {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;

    /// Every up adapter's IPv4 DNS servers, lowest interface metric first —
    /// which puts a VPN's servers (VPN adapters take a low metric) ahead of
    /// the LAN's, as Windows itself orders them.
    pub(super) fn servers() -> Vec<SocketAddr> {
        let mut size: u32 = 16 * 1024;
        for _ in 0..4 {
            // u64 elements keep the buffer 8-byte aligned, as the structure
            // needs; `size` is in bytes.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            // SAFETY: `head` points at `size` writable, suitably aligned bytes
            // that outlive the call; the flags only skip data.
            let rc = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC.0),
                    GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_FRIENDLY_NAME,
                    None,
                    Some(head),
                    &mut size,
                )
            };
            if rc == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if rc != 0 {
                return Vec::new();
            }
            let mut found: Vec<(u32, Ipv4Addr)> = Vec::new();
            let mut adapter = head.cast_const();
            while !adapter.is_null() {
                // SAFETY: `adapter` is the head of, or a `Next` link in, the
                // list Windows wrote into `buf`, which is still alive.
                let a = unsafe { &*adapter };
                if a.OperStatus == IfOperStatusUp {
                    let mut dns = a.FirstDnsServerAddress.cast_const();
                    while !dns.is_null() {
                        // SAFETY: a `FirstDnsServerAddress`/`Next` link of the
                        // same list, inside `buf`.
                        let d = unsafe { &*dns };
                        let sa = d.Address.lpSockaddr;
                        if !sa.is_null()
                            && d.Address.iSockaddrLength as usize >= size_of::<SOCKADDR_IN>()
                        {
                            // SAFETY: a non-null sockaddr of at least
                            // `SOCKADDR_IN`'s length; the family is read
                            // before the address is interpreted.
                            let family = unsafe { (*sa).sa_family };
                            if family == AF_INET {
                                // SAFETY: an `AF_INET` sockaddr is a
                                // `SOCKADDR_IN`, and its length was checked.
                                let sin = unsafe { &*sa.cast::<SOCKADDR_IN>() };
                                // SAFETY: `S_un` is a union of views of the
                                // same four bytes; `S_addr` is network order.
                                let raw = unsafe { sin.sin_addr.S_un.S_addr };
                                found.push((a.Ipv4Metric, Ipv4Addr::from(u32::from_be(raw))));
                            }
                        }
                        dns = d.Next.cast_const();
                    }
                }
                adapter = a.Next.cast_const();
            }
            found.sort_by_key(|(metric, _)| *metric);
            let mut out: Vec<SocketAddr> = Vec::new();
            for (_, ip) in found {
                let addr = SocketAddr::new(IpAddr::V4(ip), 53);
                if !out.contains(&addr) {
                    out.push(addr);
                }
            }
            return out;
        }
        Vec::new()
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub(super) fn servers() -> Vec<std::net::SocketAddr> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// `dig example.com A` as glibc sends it: RD set, one question, an EDNS
    /// OPT with no options.
    fn query(name: &str, qtype: u16, edns: bool) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, u8::from(edns)];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        if edns {
            q.extend_from_slice(&[0, 0, 41, 0x04, 0xd0, 0, 0, 0, 0, 0, 0]);
        }
        q
    }

    #[test]
    fn a_plain_a_query_is_parsed() {
        let q = parse_question(&query("intranet.corp.example", TYPE_A, true)).expect("parsed");
        assert_eq!(q.id, 0x1234);
        assert!(q.rd);
        assert_eq!(q.name, "intranet.corp.example");
        assert_eq!(q.qtype, TYPE_A);
    }

    #[test]
    fn anything_unusual_is_left_to_the_relay() {
        // MX, PTR and TXT go upstream.
        for qtype in [15u16, 12, 16] {
            assert!(parse_question(&query("example.com", qtype, false)).is_none());
        }
        // A response, a compression pointer, odd bytes, truncation.
        let mut response = query("example.com", TYPE_A, false);
        response[2] |= 0x80;
        assert!(parse_question(&response).is_none());
        let mut pointer = query("example.com", TYPE_A, false);
        pointer[12] = 0xc0;
        assert!(parse_question(&pointer).is_none());
        let mut odd = query("exa mple.com", TYPE_A, false);
        odd.truncate(odd.len());
        assert!(parse_question(&odd).is_none());
        let full = query("example.com", TYPE_A, false);
        for cut in 0..full.len() {
            assert!(parse_question(&full[..cut]).is_none(), "cut at {cut}");
        }
        // An EDNS OPT with options (a cookie, DNSSEC) gets the real server.
        let mut cookie = query("example.com", TYPE_A, true);
        let n = cookie.len();
        cookie[n - 1] = 4;
        assert!(parse_question(&cookie).is_none());
    }

    #[test]
    fn the_reply_echoes_the_question_and_carries_the_matching_family() {
        let q = parse_question(&query("host.example", TYPE_A, false)).expect("parsed");
        let addrs = [
            IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 1, 2, 4)),
        ];
        let r = reply(&q, Some(&addrs));
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 2, "two A records");
        assert_eq!(&r[12..12 + q.section.len()], q.section.as_slice());
        let first = 12 + q.section.len();
        assert_eq!(&r[first..first + 2], &[0xc0, 0x0c]);
        assert_eq!(&r[first + 12..first + 16], &[10, 1, 2, 3]);

        let q6 = parse_question(&query("host.example", TYPE_AAAA, false)).expect("parsed");
        let r6 = reply(&q6, Some(&addrs));
        assert_eq!(u16::from_be_bytes([r6[6], r6[7]]), 1, "one AAAA record");
    }

    #[test]
    fn an_unresolvable_name_is_nxdomain_and_an_empty_family_is_nodata() {
        let q = parse_question(&query("nope.invalid", TYPE_A, false)).expect("parsed");
        assert_eq!(reply(&q, None)[3] & 0x0f, 3, "NXDOMAIN");
        let v6_only = [IpAddr::V6(Ipv6Addr::LOCALHOST)];
        let r = reply(&q, Some(&v6_only));
        assert_eq!(r[3] & 0x0f, 0);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0, "no A records");
    }

    #[test]
    fn the_pool_answers_through_the_lookup_and_relays_what_it_cannot() {
        let lookup: Lookup = Arc::new(|name: &str| {
            (name == "vpn.internal").then(|| vec![IpAddr::V4(Ipv4Addr::new(10, 9, 8, 7))])
        });
        let mut pool = HostResolver::new(lookup);
        assert!(
            !pool.submit(53000, &query("example.com", 15, false)),
            "MX relayed"
        );
        assert!(pool.submit(53001, &query("vpn.internal", TYPE_A, false)));
        assert!(pool.is_busy());
        let deadline = Instant::now() + Duration::from_secs(5);
        let answer = loop {
            if let Some(a) = pool.poll() {
                break a;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(answer.0, 53001);
        assert!(answer.1.ends_with(&[10, 9, 8, 7]));
        assert!(!pool.is_busy());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolv_conf_nameservers_are_read_in_order() {
        let text = "# generated\nnameserver 10.0.0.1\nsearch corp\nnameserver ::1\nnameserver 127.0.0.53\n";
        assert_eq!(
            imp::parse_resolv_conf(text),
            vec![
                "10.0.0.1:53".parse::<SocketAddr>().unwrap(),
                "127.0.0.53:53".parse::<SocketAddr>().unwrap()
            ]
        );
    }
}
