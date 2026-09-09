//! A real guest gets a DHCP lease from the user-mode NAT and fetches something
//! over TCP through it — on **both** hosts (backlog WHP-1704).
//!
//! usernet is load-bearing now: it is the default network on Windows, the only
//! one an `entangled install` has there, and the path a Fedora install pulls
//! nineteen hundred packages over. Its two known bugs were both found by a
//! stalled install rather than by a test, so this is the acceptance that would
//! have caught them, driven through the real binary on whichever hypervisor the
//! host has.
//!
//! Four things are asserted, and each fails differently:
//!
//! * **The lease.** The kernel is booted with `ip=dhcp` and nothing else — no
//!   client in the initramfs, no static address — so `IP-Config: Got DHCP
//!   answer` in the console is a real DISCOVER/OFFER/REQUEST/ACK against the
//!   NAT's own server, and the address it reports must be the one the server
//!   was built to hand out. Unit tests have covered that exchange since phase 3;
//!   nothing until now covered a *kernel* performing it.
//! * **The datapath.** The guest's probe (`entangled.netprobe=dhcp,…`) connects
//!   out through the NAT to a listener this test runs on one of the host's own
//!   routable addresses, and checks its greeting comes back byte for byte. Only
//!   returned bytes prove RX: frames queued to the guest, the interrupt raised,
//!   the buffers completed.
//! * **The teardown.** The listener writes the echo and closes **in the same
//!   breath**, which is the shape that raced during WHP phase 4 — the FIN
//!   chasing the echoed bytes through the NAT's teardown, measured beating them
//!   to the guest about one run in four. `whp_usernet.rs` works around it by
//!   holding its half open until the guest closes first; this test does not, so
//!   a regression shows up as `echo-mismatch` rather than as nothing at all.
//! * **The flow table.** The guest closes, and the host end must see EOF —
//!   evidence the half-close was propagated, which is the fix for the leak that
//!   stalled `debian-installer` at "Loading additional components".
//!
//! The listener sits on a routable host address because that is what the NAT
//! may legitimately reach: loopback and the guest's own segment are refused by
//! design, exactly so a guest cannot reach host-local services. Self-skips
//! without such an address, without a hypervisor, or without the guest
//! artifacts.
//!
//! ```bash
//! cargo test -p entangled --test usernet_guest -- --nocapture
//! ```

#![cfg(any(target_os = "linux", windows))]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_entangled");

/// The bootstrap kernel reaches its ready marker in a few seconds; `ip=dhcp`
/// adds the kernel's own autoconfiguration, which retries for a while if the
/// first DISCOVER is lost. Generous enough for a debug build under load.
const DEADLINE: Duration = Duration::from_secs(120);

/// What the guest's probe sends and expects back.
const GREETING: &[u8] = b"ENTANGLED_NETPROBE ping\n";

/// The kernel's IP autoconfiguration announcing a lease. The one line that
/// distinguishes "a DHCP server answered" from "the address was configured".
const DHCP_MARKER: &str = "IP-Config: Got DHCP answer";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

/// Kernel and initramfs, or `None` with a note.
///
/// The **bootstrap** kernel specifically, with no fallback to the Debian
/// installer kernel the other tests accept: this test needs `CONFIG_VIRTIO_NET`
/// and `CONFIG_IP_PNP_DHCP` *built in*, and in the installer kernel virtio-net
/// is a module its own initrd loads. With that kernel there is no `eth0` at all
/// by the time init runs, and the failure — `netprobe no-address` with no
/// `IP-Config` line anywhere — looks exactly like a broken NAT.
fn artifacts() -> Option<(PathBuf, PathBuf)> {
    let root = repo_root();
    let kernel = root.join("artifacts/bootstrap/vmlinuz");
    let initramfs = root.join("artifacts/tests/test-initramfs.cpio.gz");
    if kernel.is_file() && initramfs.is_file() {
        return Some((kernel, initramfs));
    }
    eprintln!(
        "skipping: guest artifacts missing — this test needs the bootstrap kernel \
         (bash guest/bootstrap-kernel/build.sh) and scripts/build-test-initramfs.sh"
    );
    None
}

fn hypervisor_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        if !Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm is not present");
            return false;
        }
        true
    }
    #[cfg(windows)]
    {
        match Command::new(BIN).arg("doctor").output() {
            Ok(out) if out.status.success() => true,
            _ => {
                eprintln!("skipping: the Windows Hypervisor Platform is not available");
                false
            }
        }
    }
}

/// An address of this host a NAT'ed guest may legitimately reach: the one the
/// default route would use. The UDP "connect" sends nothing — it only asks the
/// stack which local address it would pick.
fn routable_host_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if !addr.ip().is_loopback() && !addr.ip().is_unspecified() => {
            Some(*addr.ip())
        }
        _ => None,
    }
}

/// A scratch directory that goes away with the test.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("entangled-usernet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the echo listener saw, once the guest is done with it.
#[derive(Debug, Default)]
struct EchoResult {
    echoed: usize,
    /// The guest's half-close arriving as end-of-file on the host stream. False
    /// means the NAT never propagated the guest's FIN — the flow leak.
    saw_eof: bool,
}

/// The acceptance: lease, then TCP, through a NAT that has to survive a peer
/// closing on top of its own reply.
#[test]
fn a_guest_leases_an_address_and_fetches_over_tcp_through_the_nat() {
    if !hypervisor_available() {
        return;
    }
    let Some((kernel, initramfs)) = artifacts() else {
        return;
    };
    let Some(host_ip) = routable_host_address() else {
        eprintln!("skipping: this host has no routable IPv4 address for the NAT to connect to");
        return;
    };

    // ---- the host side the guest will talk to ----
    let listener = TcpListener::bind((host_ip, 0)).expect("bind the echo listener");
    let port = listener.local_addr().expect("listener address").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let stop = Arc::new(AtomicBool::new(false));
    let echo_stop = Arc::clone(&stop);
    let echo = std::thread::spawn(move || -> EchoResult {
        let mut result = EchoResult::default();
        let stream = loop {
            if echo_stop.load(Ordering::Acquire) {
                return result;
            }
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    eprintln!("[usernet_guest] accept failed: {e}");
                    return result;
                }
            }
        };
        let mut stream = stream;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut buf = [0u8; 256];
        let mut closed = false;
        loop {
            if echo_stop.load(Ordering::Acquire) {
                return result;
            }
            match stream.read(&mut buf) {
                Ok(0) => {
                    result.saw_eof = true;
                    return result;
                }
                Ok(n) if !closed => {
                    if stream.write_all(&buf[..n]).is_err() {
                        return result;
                    }
                    result.echoed += n;
                    if buf[..n].contains(&b'\n') {
                        let _ = stream.flush();
                        // The racy shape, deliberately: the FIN goes out in the
                        // same breath as the echo rather than after the guest
                        // has closed first. A half-close rather than a full one
                        // only so the read half stays open to observe the
                        // guest's own FIN below — the bytes and the FIN leave
                        // together either way, which is what raced.
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        closed = true;
                    }
                }
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return result,
            }
        }
    });

    // ---- the VM ----
    let scratch = Scratch::new();
    let profile = scratch.0.join("usernet.toml");
    // `ip=dhcp` and no address of its own: the kernel's own client has to talk
    // to the NAT's DHCP server, and a profile's `ip=` clause wins over the one
    // the backend would otherwise append.
    std::fs::write(
        &profile,
        format!(
            "name = \"usernet-probe\"\n\
             memory_mib = 256\n\
             vcpus = 1\n\
             transport = \"pci\"\n\n\
             [boot]\n\
             mode = \"direct-linux\"\n\
             kernel = {kernel:?}\n\
             initramfs = {initramfs:?}\n\
             cmdline = \"console=ttyS0 panic=1 reboot=k ip=dhcp \
             entangled.netprobe=dhcp,192.168.74.1,{host_ip}:{port}\"\n\n\
             [network]\n\
             backend = \"usernet\"\n\n\
             [display]\n\
             width = 640\n\
             height = 480\n"
        ),
    )
    .expect("write profile");

    eprintln!("[usernet_guest] echo listener on {host_ip}:{port}");
    let mut child = Command::new(BIN)
        .args([
            "run",
            "--headless",
            profile.to_str().expect("a UTF-8 profile path"),
        ])
        .current_dir(repo_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn entangled run");

    let console = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    for stream in [
        child.stdout.take().map(Reader::Out),
        child.stderr.take().map(Reader::Err),
    ]
    .into_iter()
    .flatten()
    {
        let console = Arc::clone(&console);
        readers.push(std::thread::spawn(move || stream.pump(&console)));
    }

    let start = Instant::now();
    let mut outcome = None;
    while start.elapsed() < DEADLINE {
        let text = console.lock().expect("console").clone();
        // Only *complete* lines: a marker that is still being written arrives
        // truncated, and a truncated `VMHOST_TEST_OK netprobe … echoed=24` is
        // indistinguishable from a failure.
        if let Some(line) = text
            .split_inclusive('\n')
            .filter(|l| l.ends_with('\n'))
            .find(|l| l.contains("VMHOST_TEST_OK netprobe") || l.contains("VMHOST_TEST_FAIL"))
        {
            outcome = Some(line.trim_end().to_string());
            break;
        }
        if text.contains("Kernel panic") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let text = console.lock().expect("console").clone();
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, Ordering::Release);
    for reader in readers {
        let _ = reader.join();
    }
    let result = echo.join().expect("the echo thread");

    let log = std::env::temp_dir().join(format!("entangled-usernet-{}.log", std::process::id()));
    let _ = std::fs::write(&log, &text);

    let outcome = outcome.unwrap_or_else(|| {
        panic!(
            "the guest never reported a netprobe result within {:?}; console at {}",
            DEADLINE,
            log.display()
        )
    });
    eprintln!("[usernet_guest] {outcome}");

    assert!(
        text.contains(DHCP_MARKER),
        "the kernel must have got its address from the NAT's DHCP server, not from thin air; \
         console at {}",
        log.display()
    );
    assert!(
        outcome.contains("VMHOST_TEST_OK netprobe"),
        "the guest's probe failed: {outcome}; console at {}",
        log.display()
    );
    assert!(
        outcome.contains("ip=192.168.74.15/24"),
        "the lease must be the address the NAT's DHCP server offers: {outcome}"
    );
    assert!(
        outcome.contains(&format!("echoed={}", GREETING.len())),
        "the whole greeting must have come back through the NAT: {outcome}"
    );
    assert_eq!(
        result.echoed,
        GREETING.len(),
        "the host end must have seen the whole greeting"
    );
    assert!(
        result.saw_eof,
        "the guest's close must reach the host as end-of-file — without it the NAT leaks \
         one flow per connection, which is what stalled a Debian install"
    );
}

/// Reading a child's stdout and stderr into one transcript.
enum Reader {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Reader {
    fn pump(self, console: &Mutex<String>) {
        let mut source: Box<dyn Read> = match self {
            Reader::Out(out) => Box::new(out),
            Reader::Err(err) => Box::new(err),
        };
        let mut buffer = [0u8; 4096];
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if let Ok(mut console) = console.lock() {
                        console.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    }
                }
            }
        }
    }
}
