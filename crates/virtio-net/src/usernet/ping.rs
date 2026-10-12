//! `ping` past the gateway (2026-10-13), without privileges on either host.
//!
//! A raw ICMP socket needs administrator rights on Windows and `CAP_NET_RAW`
//! on Linux, which is why the NAT answered only a ping of the gateway until
//! now. Both hosts have an unprivileged way to send an echo and wait for its
//! answer, and that is all a guest's `ping` needs:
//!
//! * **Windows:** `IcmpSendEcho` (iphlpapi) — any user, any destination.
//! * **Linux:** an ICMP *datagram* socket (`socket(AF_INET, SOCK_DGRAM,
//!   IPPROTO_ICMP)`), allowed when the user's group is inside
//!   `net.ipv4.ping_group_range` — the default on Ubuntu and Fedora. Where it
//!   is not, the socket is refused once, logged once, and pings past the
//!   gateway stay unanswered as before.
//!
//! Each echo blocks for up to [`ECHO_TIMEOUT`], so they run on a small pool of
//! worker threads ([`WORKERS`]) behind a bounded queue ([`MAX_IN_FLIGHT`]):
//! a guest that floods pings has its excess dropped, which is what a real
//! router under load does to ICMP. The workers start on the first ping, so a
//! guest that never pings costs nothing. The answer goes back to the guest as
//! an echo reply from the address it pinged, with its own identifier,
//! sequence number and payload.

use std::net::Ipv4Addr;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Echoes waiting or in progress at once.
pub const MAX_IN_FLIGHT: usize = 32;
/// Worker threads (each blocks on one echo at a time).
pub const WORKERS: usize = 2;
/// How long one echo waits for its answer.
pub const ECHO_TIMEOUT: Duration = Duration::from_secs(3);
/// The largest echo payload carried (what fits one guest frame).
pub const MAX_ECHO_DATA: usize = 1472 - 8;

/// One echo the guest asked for.
pub(super) struct Echo {
    pub dst: Ipv4Addr,
    pub ident: u16,
    pub seq: u16,
    pub data: Vec<u8>,
}

pub(super) struct Pinger {
    jobs: SyncSender<Echo>,
    answers: Receiver<Echo>,
    dropped: u64,
}

impl Pinger {
    pub(super) fn new() -> Option<Self> {
        let (jobs, job_rx) = mpsc::sync_channel::<Echo>(MAX_IN_FLIGHT);
        let (answer_tx, answers) = mpsc::channel::<Echo>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for n in 0..WORKERS {
            let job_rx = Arc::clone(&job_rx);
            let answer_tx = answer_tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("usernet-ping-{n}"))
                .spawn(move || worker(&job_rx, &answer_tx));
            if let Err(error) = spawned {
                tracing::warn!(%error, "cannot start a ping worker");
                return None;
            }
        }
        Some(Self {
            jobs,
            answers,
            dropped: 0,
        })
    }

    /// Queues an echo; one over [`MAX_IN_FLIGHT`] is dropped.
    pub(super) fn submit(&mut self, echo: Echo) {
        match self.jobs.try_send(echo) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped = self.dropped.saturating_add(1);
            }
        }
    }

    /// An answered echo, if one is ready.
    pub(super) fn poll(&mut self) -> Option<Echo> {
        self.answers.try_recv().ok()
    }

    pub(super) fn dropped(&self) -> u64 {
        self.dropped
    }
}

fn worker(jobs: &Mutex<Receiver<Echo>>, answers: &mpsc::Sender<Echo>) {
    let mut host = host::Echoer::new();
    loop {
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        // The router (and with it the sender) is gone: the VM is ending.
        let Ok(job) = job else { return };
        let Some(echoer) = host.as_mut() else {
            continue;
        };
        if let Some(data) = echoer.echo(job.dst, job.seq, &job.data, ECHO_TIMEOUT) {
            let answered = Echo { data, ..job };
            if answers.send(answered).is_err() {
                return;
            }
        }
    }
}

#[cfg(windows)]
mod host {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::IpHelper::{
        IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY,
    };

    pub struct Echoer(HANDLE);

    impl Echoer {
        pub fn new() -> Option<Self> {
            // SAFETY: no arguments; the handle is checked before use and closed
            // in Drop.
            match unsafe { IcmpCreateFile() } {
                Ok(handle) if !handle.is_invalid() => Some(Self(handle)),
                _ => {
                    tracing::warn!("IcmpCreateFile failed; pings past the gateway stay unanswered");
                    None
                }
            }
        }

        pub fn echo(
            &mut self,
            dst: Ipv4Addr,
            _seq: u16,
            data: &[u8],
            timeout: Duration,
        ) -> Option<Vec<u8>> {
            let request_len = u16::try_from(data.len()).ok()?;
            // One reply structure, its payload and the 8 bytes of an ICMP error
            // the API documents room for.
            let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 8 + 16];
            let addr = u32::from_ne_bytes(dst.octets());
            // SAFETY: the handle is live (Drop has not run); `data` is valid for
            // `request_len` bytes and `reply` for its whole length, both for the
            // duration of this synchronous call; no options pointer is passed.
            let count = unsafe {
                IcmpSendEcho(
                    self.0,
                    addr,
                    data.as_ptr().cast(),
                    request_len,
                    None,
                    reply.as_mut_ptr().cast(),
                    reply.len() as u32,
                    timeout.as_millis().min(u128::from(u32::MAX)) as u32,
                )
            };
            if count == 0 {
                return None;
            }
            // SAFETY: the API wrote at least one ICMP_ECHO_REPLY at the start of
            // `reply`, which is large enough for it; read unaligned because a
            // Vec<u8> promises no alignment.
            let header =
                unsafe { std::ptr::read_unaligned(reply.as_ptr().cast::<ICMP_ECHO_REPLY>()) };
            if header.Status != 0 {
                return None;
            }
            // The payload pointer points into `reply`; take it as an offset and
            // bound it by the buffer rather than trusting the pointer.
            let start = (header.Data as usize).checked_sub(reply.as_ptr() as usize)?;
            let end = start.checked_add(usize::from(header.DataSize))?;
            reply.get(start..end).map(<[u8]>::to_vec)
        }
    }

    impl Drop for Echoer {
        fn drop(&mut self) {
            // SAFETY: the handle came from IcmpCreateFile and is closed once.
            let _ = unsafe { IcmpCloseHandle(self.0) };
        }
    }
}

#[cfg(target_os = "linux")]
mod host {
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    pub struct Echoer(libc::c_int);

    impl Echoer {
        pub fn new() -> Option<Self> {
            // SAFETY: plain socket creation; the descriptor is checked and owned
            // by the returned value, closed in Drop.
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
            if fd < 0 {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "no unprivileged ICMP socket (net.ipv4.ping_group_range); pings past the \
                     gateway stay unanswered"
                );
                return None;
            }
            Some(Self(fd))
        }

        pub fn echo(
            &mut self,
            dst: Ipv4Addr,
            seq: u16,
            data: &[u8],
            timeout: Duration,
        ) -> Option<Vec<u8>> {
            // Type 8, code 0; the kernel fills in the identifier and checksum
            // of an ICMP datagram socket.
            let mut request = vec![8u8, 0, 0, 0, 0, 0];
            request.extend_from_slice(&seq.to_be_bytes());
            request.extend_from_slice(data);
            // SAFETY: an all-zero sockaddr_in is valid; the fields set below
            // make it the destination.
            let mut to: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            to.sin_family = libc::AF_INET as libc::sa_family_t;
            to.sin_addr.s_addr = u32::from_ne_bytes(dst.octets());
            // SAFETY: the descriptor is live; `request` and `to` are valid for
            // the lengths passed, for the duration of the call.
            let sent = unsafe {
                libc::sendto(
                    self.0,
                    request.as_ptr().cast(),
                    request.len(),
                    0,
                    (&to as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            if sent < 0 {
                return None;
            }
            let deadline = Instant::now() + timeout;
            let mut buf = vec![0u8; 2048];
            loop {
                let left = deadline.checked_duration_since(Instant::now())?;
                let mut pfd = libc::pollfd {
                    fd: self.0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ms = left.as_millis().clamp(1, 60_000) as libc::c_int;
                // SAFETY: one valid pollfd for the duration of the call.
                if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
                    return None;
                }
                // SAFETY: `buf` is writable for its length; no address is asked for.
                let n = unsafe {
                    libc::recv(
                        self.0,
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                let Ok(n) = usize::try_from(n) else {
                    continue;
                };
                // An echo reply to *this* request: the sequence and the payload
                // match (an older, timed-out one may still arrive).
                if n >= 8 && buf[0] == 0 && buf[6..8] == seq.to_be_bytes() && buf[8..n] == *data {
                    return Some(buf[8..n].to_vec());
                }
            }
        }
    }

    impl Drop for Echoer {
        fn drop(&mut self) {
            // SAFETY: the descriptor is owned by this value and closed once.
            unsafe { libc::close(self.0) };
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod host {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    pub struct Echoer;

    impl Echoer {
        pub fn new() -> Option<Self> {
            None
        }

        pub fn echo(&mut self, _: Ipv4Addr, _: u16, _: &[u8], _: Duration) -> Option<Vec<u8>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback answers pings on every host that has an unprivileged way to
    /// send them; where there is none (a Linux whose ping_group_range excludes
    /// us) the test says so and passes.
    #[test]
    fn a_ping_of_loopback_comes_back_with_its_payload() {
        let Some(mut echoer) = host::Echoer::new() else {
            eprintln!("skipping: no unprivileged ICMP on this host");
            return;
        };
        let got = echoer.echo(Ipv4Addr::LOCALHOST, 7, b"entangled", ECHO_TIMEOUT);
        assert_eq!(got.as_deref(), Some(&b"entangled"[..]));
    }

    #[test]
    fn the_queue_is_bounded() {
        let Some(mut pinger) = Pinger::new() else {
            return;
        };
        // TEST-NET-3 never answers, so every worker blocks and the queue fills.
        for seq in 0..(MAX_IN_FLIGHT as u16 + WORKERS as u16 + 8) {
            pinger.submit(Echo {
                dst: Ipv4Addr::new(203, 0, 113, 77),
                ident: 1,
                seq,
                data: vec![0; 8],
            });
        }
        assert!(pinger.dropped() >= 8);
    }
}
