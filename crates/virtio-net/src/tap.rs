//! The TAP backend (backlog MVP-503/505/506), Linux only.
//!
//! One `/dev/net/tun` descriptor attached with `TUNSETIFF` to a named TAP
//! interface in `IFF_TAP | IFF_NO_PI` mode: raw Ethernet frames, no 4-byte
//! packet-info prefix, so what the device reads and writes is exactly what
//! travels on the wire minus the virtio-net header.
//!
//! # Privileges (CAP_NET_ADMIN)
//!
//! `TUNSETIFF` needs `CAP_NET_ADMIN` **when it has to create** the interface.
//! It does *not* when the interface already exists and is owned by the calling
//! user, which is the deployment VMHost targets: an administrator runs
//! `scripts/setup-tap.sh --user "$USER"` once (that script also does the
//! bridging/NAT and IP forwarding), and from then on `vmhost run` attaches to
//! `vmhost0` unprivileged. `vmhost doctor` reports which of the two situations
//! it is in.
//!
//! # Lifetime
//!
//! The descriptor is owned by the backend and closed on drop. A TAP interface
//! this process created (no `TUNSETPERSIST`) disappears with it, so closing the
//! VM leaves no interface behind; a persistent interface created by the setup
//! script survives, detached and down. Device *reset* deliberately keeps the
//! descriptor open — a driver is allowed to reset and re-initialise, and
//! re-creating the interface mid-VM would need privileges the VMM may not have.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};
use vmm_sys_util::ioctl::ioctl_with_ref;
use vmm_sys_util::ioctl_iow_nr;

use crate::backend::{NetBackend, NetError, Readiness};

const TUN_PATH: &str = "/dev/net/tun";

/// `IFNAMSIZ` — interface names are at most 15 bytes plus the NUL terminator.
pub const MAX_IFNAME_LEN: usize = 16;

/// The `ifreq` union is 24 bytes wide (`struct ifmap` is the largest member);
/// VMHost only ever writes its first two bytes, `ifru_flags`.
const IFREQ_UNION_LEN: usize = 24;

// `TUNSETIFF` = `_IOW('T', 202, int)`, from `linux/if_tun.h`.
ioctl_iow_nr!(TUNSETIFF, 'T' as u32, 202, ::std::os::raw::c_int);

/// `struct ifreq` as `TUNSETIFF` reads it.
///
/// Hand-rolled instead of `libc::ifreq` so that setting the flags does not need
/// an `unsafe` union write and does not depend on `libc`'s union field naming.
#[repr(C)]
struct IfReq {
    ifr_name: [u8; MAX_IFNAME_LEN],
    ifr_ifru: [u8; IFREQ_UNION_LEN],
}

impl IfReq {
    /// `name` must already have passed [`validate_ifname`].
    fn new(name: &str, flags: i16) -> Self {
        let mut ifr_name = [0u8; MAX_IFNAME_LEN];
        for (slot, byte) in ifr_name.iter_mut().zip(name.as_bytes()) {
            *slot = *byte;
        }
        let mut ifr_ifru = [0u8; IFREQ_UNION_LEN];
        ifr_ifru[..2].copy_from_slice(&flags.to_ne_bytes());
        Self { ifr_name, ifr_ifru }
    }
}

/// Checks an interface name against `IFNAMSIZ` and the kernel's rules, before
/// it reaches an ioctl. Config comes from the host operator, not the guest, but
/// a typo must produce a typed error rather than a silently truncated name.
pub fn validate_ifname(name: &str) -> Result<(), NetError> {
    let invalid = name.is_empty()
        || name.len() >= MAX_IFNAME_LEN
        || name == "."
        || name == ".."
        || name
            .bytes()
            .any(|b| b == 0 || b == b'/' || b == b':' || b.is_ascii_whitespace());
    if invalid {
        return Err(NetError::InterfaceName {
            name: name.to_owned(),
            max: MAX_IFNAME_LEN - 1,
        });
    }
    Ok(())
}

/// A TAP interface, ready to carry Ethernet frames.
#[derive(Debug)]
pub struct TapBackend {
    fd: OwnedFd,
    /// Written by [`NetBackend::wake`] so a blocked `poll` returns at once on
    /// device reset. Polled alongside the TAP descriptor.
    wake: EventFd,
    ifname: String,
    label: String,
}

impl TapBackend {
    /// Attaches to the TAP interface `ifname`, creating it if it does not exist
    /// and the process has `CAP_NET_ADMIN`.
    pub fn open(ifname: &str) -> Result<Self, NetError> {
        validate_ifname(ifname)?;

        let flags = i16::try_from(libc::IFF_TAP | libc::IFF_NO_PI).unwrap_or(0);
        // SAFETY: a NUL-terminated literal path and plain flag bits; `open` only
        // reads the string and returns a descriptor or -1.
        let raw = unsafe {
            libc::open(
                c"/dev/net/tun".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(NetError::Open {
                path: TUN_PATH.to_owned(),
                source: io::Error::last_os_error(),
            });
        }
        // SAFETY: `raw` is a fresh, valid descriptor this call owns exclusively;
        // wrapping it hands that ownership to `OwnedFd`, which closes it once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let request = IfReq::new(ifname, flags);
        // SAFETY: `fd` is a live `/dev/net/tun` descriptor and `request` is a
        // fully initialised `struct ifreq`, which is what TUNSETIFF expects to
        // read. The kernel writes back the chosen name, which we ignore.
        let rc = unsafe { ioctl_with_ref(&fd, TUNSETIFF(), &request) };
        if rc < 0 {
            return Err(NetError::Attach {
                ifname: ifname.to_owned(),
                source: io::Error::last_os_error(),
            });
        }

        let wake = EventFd::new(EFD_NONBLOCK).map_err(|source| NetError::Wakeup { source })?;
        tracing::info!(ifname, "attached to TAP interface");
        Ok(Self {
            fd,
            wake,
            ifname: ifname.to_owned(),
            label: format!("tap:{ifname}"),
        })
    }

    /// Name of the attached interface.
    pub fn ifname(&self) -> &str {
        &self.ifname
    }
}

impl NetBackend for TapBackend {
    fn name(&self) -> &str {
        &self.label
    }

    fn write_frame(&self, frame: &[u8]) -> Result<usize, NetError> {
        if frame.is_empty() {
            return Ok(0);
        }
        // SAFETY: `frame` is a live slice of `frame.len()` initialised bytes;
        // `write` only reads from it.
        let written = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                frame.as_ptr().cast::<libc::c_void>(),
                frame.len(),
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                // The interface queue is full or the transient error is
                // retryable: the frame is dropped, exactly as a real NIC would
                // under congestion. Not a host failure.
                // EWOULDBLOCK is EAGAIN on Linux, hence the single arm.
                Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(0),
                _ => Err(NetError::Write {
                    backend: self.label.clone(),
                    source: error,
                }),
            };
        }
        // `write` returned a non-negative count, at most `frame.len()`.
        Ok(usize::try_from(written).unwrap_or(0))
    }

    fn read_frame(&self, buf: &mut [u8]) -> Result<Option<usize>, NetError> {
        if buf.is_empty() {
            return Ok(None);
        }
        // SAFETY: `buf` is a live mutable slice of `buf.len()` bytes; `read`
        // writes at most that many and never keeps the pointer.
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(None),
                _ => Err(NetError::Read {
                    backend: self.label.clone(),
                    source: error,
                }),
            };
        }
        // `read` returned a non-negative count, at most `buf.len()`.
        Ok(usize::try_from(read).ok())
    }

    fn wait_readable(&self, timeout: Duration) -> Result<Readiness, NetError> {
        let mut fds = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` is a live array of two initialised `pollfd`s and the
        // length passed matches it exactly.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            // A signal is not a failure: the caller re-polls.
            if error.raw_os_error() == Some(libc::EINTR) {
                return Ok(Readiness::TimedOut);
            }
            return Err(NetError::Poll {
                backend: self.label.clone(),
                source: error,
            });
        }
        if ready == 0 {
            return Ok(Readiness::TimedOut);
        }
        if fds[1].revents != 0 {
            // Drain the counter so the next wait blocks again. A failure here
            // only costs one extra wake-up.
            let _ = self.wake.read();
            return Ok(Readiness::WokenUp);
        }
        // POLLERR/POLLHUP are reported as readable so the following `read`
        // surfaces the real error instead of spinning here.
        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(Readiness::Readable);
        }
        Ok(Readiness::TimedOut)
    }

    fn wake(&self) -> Result<(), NetError> {
        self.wake
            .write(1)
            .map_err(|source| NetError::Wakeup { source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_names_are_validated() {
        for good in ["vmhost0", "tap0", "a", "eth-test123456"] {
            assert!(validate_ifname(good).is_ok(), "{good} must be accepted");
        }
        for bad in [
            "",
            "sixteencharname0",
            "with space",
            "with/slash",
            "with:colon",
            ".",
            "..",
            "nul\0inside",
        ] {
            assert!(
                matches!(
                    validate_ifname(bad),
                    Err(NetError::InterfaceName { max: 15, .. })
                ),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn ifreq_has_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<IfReq>(), 40);
        let req = IfReq::new("vmhost0", 0x0002 | 0x1000);
        assert_eq!(&req.ifr_name[..7], b"vmhost0");
        assert_eq!(req.ifr_name[7], 0, "name must be NUL terminated");
        assert_eq!(
            i16::from_ne_bytes([req.ifr_ifru[0], req.ifr_ifru[1]]),
            0x1002
        );
        assert!(req.ifr_ifru[2..].iter().all(|b| *b == 0));
    }

    #[test]
    fn tunsetiff_matches_the_kernel_constant() {
        // _IOW('T', 202, int) — a wrong value would silently talk to another
        // ioctl on some other driver.
        assert_eq!(TUNSETIFF(), 0x4004_54ca);
    }

    #[test]
    fn flag_bits_are_tap_without_packet_info() {
        assert_eq!(libc::IFF_TAP, 0x0002);
        assert_eq!(libc::IFF_NO_PI, 0x1000);
    }

    /// Opening a TAP interface needs either an existing interface owned by this
    /// user or CAP_NET_ADMIN; without them the test reports why it skipped.
    #[test]
    fn open_reports_a_typed_error_or_succeeds() {
        if !std::path::Path::new(TUN_PATH).exists() {
            eprintln!("skipping: {TUN_PATH} is absent (no TUN/TAP support in this kernel)");
            return;
        }
        match TapBackend::open("vmhostunit0") {
            Ok(tap) => {
                assert_eq!(tap.ifname(), "vmhostunit0");
                assert_eq!(tap.name(), "tap:vmhostunit0");
                // Nothing is pending on a fresh interface.
                assert_eq!(
                    tap.wait_readable(Duration::from_millis(10)).ok(),
                    Some(Readiness::TimedOut)
                );
                // …and a wake-up returns immediately.
                assert!(tap.wake().is_ok());
                assert_eq!(
                    tap.wait_readable(Duration::from_secs(5)).ok(),
                    Some(Readiness::WokenUp)
                );
            }
            Err(error) => {
                eprintln!("skipping: cannot open a TAP interface ({error})");
            }
        }
    }
}
