//! virtio devices on the PCI root bus (EPIC 19), the second transport.
//!
//! The mmio counterpart is [`crate::virtio::VirtioMmioBus`]; this module is the
//! same job for [`virtio_core::PciTransport`], and the differences are all
//! consequences of PCI being *enumerable*:
//!
//! * **No kernel command line.** A virtio-mmio device only exists because a
//!   `virtio_mmio.device=` clause told the guest where to look, which is why
//!   [`crate::virtio::VirtioMmioBus::attach`] must preserve caller order — the
//!   clause order is the probe order and therefore decides `/dev/vda` versus
//!   `/dev/vdb`. Here the guest walks the bus, so device *numbers* (dense from
//!   `00:01.0` upwards) take that role instead. Caller order is still preserved,
//!   for the same reason.
//! * **Two address spaces per device.** Configuration accesses arrive as port
//!   I/O on `0xcf8`/`0xcfc` and are served by [`crate::pci::PciRoot`]; register
//!   accesses arrive as MMIO inside the device's BAR window. Both are dispatched
//!   from [`crate::bus::MachineBus`].
//! * **The driver decides when the device decodes.** Until the guest sets the
//!   memory-space-enable bit in the command register, the BAR window is not
//!   claimed at all — [`crate::pci::PciRoot::locate_mmio`] enforces that, so a
//!   pre-`pci_enable_device` access reads zeroes rather than reaching a device.
//!
//! # Interrupts
//!
//! Every function is wired for **both** mechanisms and the driver picks.
//!
//! ## MSI-X — the default, and what a Linux guest actually uses
//!
//! The function publishes an MSI-X capability with `queues + 1` vectors, its table
//! and PBA in the same BAR (`virtio_core::msix`), and delivers through
//! [`crate::msi::KvmMsiSink`]: one `KVM_SIGNAL_MSI` per interrupt, carrying the
//! address and data the driver programmed. No pin, no routing, nothing for a
//! firmware to clobber. The capability is omitted only when the host cannot
//! deliver MSI at all ([`crate::msi::KvmMsiSink::is_supported`]) or when
//! [`PciInterruptMode::IntxOnly`] is asked for.
//!
//! Two guest-owned registers therefore have to reach the transport, and both do it
//! without this module interpreting them: the message control register through
//! [`crate::pci::ConfigSpace::mirror_dword`], and a *change* to it through
//! [`Self::io_write`](VirtioPciBus::io_write), which tells the transport to release
//! anything the pending-bit array remembers.
//!
//! ## INTx — the fallback, and where a driver starts and ends
//!
//! One IOAPIC pin per device, from [`layout::PCI_FIRST_IRQ`], published to the
//! guest in the `interrupt_line` config register and raised through a KVM irqfd
//! — the same mechanism, and the same known limitation, as the mmio bus: the
//! injection is an **edge** on an ISA-style pin, not a level-triggered PCI
//! `INTA#`. Three consequences, all deliberate, and all of which MSI-X retires:
//!
//! * pins are never shared. One device per pin means the ISR byte does not have
//!   to deassert anything, which an edge injection could not model anyway.
//!   [`crate::pci::MAX_PCI_DEVICES`] and the pin space bound each other.
//! * `interrupt_line` is read-only, because EDK2's `PciBusDxe` scribbles over it
//!   and this machine has no platform driver that could put the real value back
//!   (see [`crate::pci::ConfigSpace::with_interrupt`]).
//! * without ACPI `_PRT` or a `$PIR` table Linux takes a PCI device's IRQ straight
//!   from `interrupt_line` and warns about a buggy MP table; `crate::mptable`
//!   routes those ISA pins to the IOAPIC, and that pairing is the only reason it
//!   works.
//!
//! INTx stays wired for every function, and must: it is where a driver starts,
//! where it stays if MSI-X allocation fails, and where an unbind
//! (`pci_free_irq_vectors`) puts it back. `INTX_DISABLE` is honoured too —
//! [`IntxLine`] checks the flag the config space maintains, so `pci_intx(dev, 0)`
//! really does stop the injections — and while MSI-X is enabled the transport
//! never raises the line at all (`virtio_core::msix`).
//!
//! # Two hosts, one bus (EPIC 17 phase 4)
//!
//! The same split [`crate::virtio::VirtioMmioBus`] got in phase 3, one transport
//! later. Everything above is host-neutral — the config space, the BAR decode,
//! the capability records, the MSI-X table semantics — and only the three wiring
//! primitives differ:
//!
//! * [`VirtioPciBus::attach`] / [`VirtioPciBus::attach_with`] — KVM: an irqfd per
//!   INTx line, `KVM_SIGNAL_MSI` per MSI-X message, an ioeventfd per queue.
//! * [`VirtioPciBus::attach_userspace`] — a host with no in-kernel irqchip (WHP):
//!   INTx through [`crate::irqchip::UserspaceIrqChip`]'s IOAPIC, MSI-X through
//!   [`crate::msi::UserspaceMsiSink`] (the architectural decode plus one
//!   `InterruptDelivery::request`), and every kick inline on the vCPU thread.
//!
//! The rebasing dance [`VirtioPciBus::reconcile_notify`] does for KVM does not
//! exist on the userspace path, and not because it is unfinished: with no
//! ioeventfds there is nothing registered at an absolute address. A kick is an
//! MMIO exit decoded through [`crate::pci::PciRoot::locate_mmio`] against the
//! BAR's *current* base on every access, so the notification area follows a BAR
//! move by construction.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use kvm_ioctls::VmFd;
use thiserror::Error;
use virtio_core::interrupt::{InterruptError, IrqLine, MsiSink};
use virtio_core::pci as vpci;
use virtio_core::transport::TransportError;
use virtio_core::{GuestMem, PciTransport, VirtioDevice};

use crate::irqchip::{IrqChipError, UserspaceIrqChip};
#[cfg(target_os = "linux")]
use crate::irqfd::{IrqFdError, IrqFdLine};
use crate::layout;
#[cfg(target_os = "linux")]
use crate::msi::KvmMsiSink;
use crate::msi::UserspaceMsiSink;
#[cfg(target_os = "linux")]
use crate::notify::{DeviceNotifier, NotifyAddressing, NotifyError, QueueNotifyMode};
use crate::pci::{ConfigSpace, PciError, PciRoot};
use virtio_core::msix;

#[derive(Debug, Error)]
pub enum VirtioPciAttachError {
    #[cfg(target_os = "linux")]
    #[error("failed to wire the interrupt line for PCI slot {slot}: {source}")]
    Irq {
        slot: usize,
        #[source]
        source: IrqFdError,
    },

    #[error("failed to wire the IOAPIC line for PCI slot {slot}: {source}")]
    IrqChip {
        slot: usize,
        #[source]
        source: IrqChipError,
    },

    #[error("PCI slot {slot}: {source}")]
    Transport {
        slot: usize,
        #[source]
        source: TransportError,
    },

    #[error("PCI slot {slot}: {source}")]
    Bus {
        slot: usize,
        #[source]
        source: PciError,
    },

    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Notify(#[from] NotifyError),
}

/// Environment variable that forces an interrupt mode, for the INTx regression
/// boot and for working around a host where MSI delivery misbehaves.
pub const PCI_INTERRUPT_MODE_ENV: &str = "ENTANGLED_PCI_MSIX";

/// Which interrupt mechanisms a virtio-pci function publishes.
///
/// Not "which one it uses": a function with MSI-X still has its INTx line, and
/// the driver decides per bring-up. This only says whether the MSI-X capability
/// is there to be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PciInterruptMode {
    /// Publish the MSI-X capability alongside INTx (the default).
    #[default]
    Msix,
    /// Publish no MSI-X capability, so the driver has nothing but INTx — the
    /// state every virtio-pci boot was in before MSI-X existed, kept so the INTx
    /// path stays a tested path rather than a plausible one.
    IntxOnly,
}

impl PciInterruptMode {
    /// Reads [`PCI_INTERRUPT_MODE_ENV`]; anything unrecognised keeps the default.
    pub fn from_env() -> Self {
        match std::env::var(PCI_INTERRUPT_MODE_ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    var = PCI_INTERRUPT_MODE_ENV,
                    value = %value,
                    "unrecognised virtio-pci interrupt mode, using the default"
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Parses the accepted spellings; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if ["1", "on", "yes", "msix"]
            .iter()
            .any(|v| value.eq_ignore_ascii_case(v))
        {
            Some(Self::Msix)
        } else if ["0", "off", "no", "intx"]
            .iter()
            .any(|v| value.eq_ignore_ascii_case(v))
        {
            Some(Self::IntxOnly)
        } else {
            None
        }
    }

    pub fn is_msix(self) -> bool {
        matches!(self, Self::Msix)
    }
}

/// A device's INTx line: a host interrupt line gated on the guest not having
/// set `INTX_DISABLE`.
///
/// The inner line is a KVM irqfd today ([`crate::irqfd::IrqFdLine`]), so
/// triggering injects the interrupt entirely inside the kernel; it is an
/// `Arc<dyn IrqLine>` so the WHP IOAPIC line slots in unchanged when virtio
/// reaches Windows (EPIC 17 phase 3). See the module docs for why this is an
/// edge on an ISA-style pin rather than a level-triggered `INTA#`.
pub struct IntxLine {
    line: Arc<dyn IrqLine>,
    /// Mirrors the command register's `INTX_DISABLE` bit, maintained by the
    /// config space. Checked on every injection rather than snapshotted, because
    /// a driver may disable INTx at any time — while switching to polling, or on
    /// its way to being unbound.
    enabled: Arc<AtomicBool>,
}

impl IrqLine for IntxLine {
    fn trigger(&self) -> Result<(), InterruptError> {
        if !self.enabled.load(Ordering::Acquire) {
            // Not an error: the driver asked for silence. The ISR bit stays set,
            // so a driver that polls still sees why the device wanted attention.
            tracing::trace!("INTx is disabled by the guest; not raising the line");
            return Ok(());
        }
        self.line.trigger()
    }
}

/// One attached virtio-pci device.
pub struct VirtioPciSlot {
    /// PCI device number on bus 0 (`00:<device>.0`).
    pub device_number: u8,
    /// Guest physical base of the device's BAR window, as the host assigned it.
    pub bar_base: u64,
    /// GSI the device's INTx line is wired to. Still wired under MSI-X: it is the
    /// fallback the driver starts on and returns to.
    pub irq: u32,
    /// MSI-X vectors this function publishes (`queues + 1`), or 0 when it
    /// publishes no MSI-X capability at all.
    pub msix_vectors: u16,
    /// The transport, shared with every vCPU thread that may take an exit into
    /// the BAR and with the device's queue worker thread.
    pub transport: Arc<Mutex<PciTransport>>,
    /// Present when this device's queue kicks are served by ioeventfds and a
    /// worker thread; `None` means every kick runs inline on the vCPU.
    #[cfg(target_os = "linux")]
    notifier: Option<DeviceNotifier<PciTransport>>,
}

impl VirtioPciSlot {
    /// The queue-notify offload for this device, if it has one.
    #[cfg(target_os = "linux")]
    pub fn notifier(&self) -> Option<&DeviceNotifier<PciTransport>> {
        self.notifier.as_ref()
    }
}

/// The machine's PCI bus: configuration space, BAR address decoding, and the
/// virtio transports behind it.
pub struct VirtioPciBus {
    /// Configuration space for every function, host bridge included. Behind a
    /// `Mutex` because a configuration access latches `CONFIG_ADDRESS`, i.e.
    /// even a read mutates, and any vCPU can make one.
    root: Mutex<PciRoot>,
    slots: Vec<VirtioPciSlot>,
    #[cfg(target_os = "linux")]
    mode: QueueNotifyMode,
    interrupts: PciInterruptMode,
}

/// One function's host-neutral pieces, built by [`VirtioPciBus::attach_function`]
/// and wired to a host by whichever constructor called it.
struct BuiltFunction {
    device_number: u8,
    msix_vectors: u16,
    device_type: virtio_core::DeviceType,
    transport: Arc<Mutex<PciTransport>>,
}

impl VirtioPciBus {
    /// A bus with the host bridge and no devices.
    pub fn empty() -> Self {
        Self {
            root: Mutex::new(PciRoot::new()),
            slots: Vec::new(),
            #[cfg(target_os = "linux")]
            mode: QueueNotifyMode::Synchronous,
            interrupts: PciInterruptMode::default(),
        }
    }

    /// Places `devices` on consecutive PCI device numbers starting at `00:01.0`,
    /// registering one irqfd per device, an MSI-X capability where the host can
    /// deliver MSI, and (by default) one queue-notify ioeventfd per queue.
    #[cfg(target_os = "linux")]
    pub fn attach(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
    ) -> Result<Self, VirtioPciAttachError> {
        Self::attach_with(vm, mem, devices, QueueNotifyMode::from_env())
    }

    /// [`Self::attach`] with an explicit queue-notify mode (benchmarks, tests).
    #[cfg(target_os = "linux")]
    pub fn attach_with(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        mode: QueueNotifyMode,
    ) -> Result<Self, VirtioPciAttachError> {
        Self::attach_with_interrupts(vm, mem, devices, mode, PciInterruptMode::from_env())
    }

    /// [`Self::attach_with`] with an explicit interrupt mode as well, which is how
    /// the INTx acceptance boot keeps testing INTx now that a Linux guest would
    /// otherwise always choose MSI-X.
    #[cfg(target_os = "linux")]
    pub fn attach_with_interrupts(
        vm: Arc<VmFd>,
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        mode: QueueNotifyMode,
        interrupts: PciInterruptMode,
    ) -> Result<Self, VirtioPciAttachError> {
        // One probe for the whole bus: whether a function publishes MSI-X must be
        // settled before any driver can walk the capability list, and a capability
        // whose interrupts silently vanish is worse than no capability.
        let msi: Option<Arc<dyn MsiSink>> =
            match (interrupts.is_msix(), KvmMsiSink::is_supported(&vm)) {
                (true, true) => Some(Arc::new(KvmMsiSink::new(Arc::clone(&vm)))),
                (true, false) => {
                    tracing::warn!(
                        "this host has no KVM_CAP_SIGNAL_MSI; virtio-pci functions will \
                     publish no MSI-X capability and drivers will use INTx"
                    );
                    None
                }
                (false, _) => {
                    tracing::info!(
                        var = PCI_INTERRUPT_MODE_ENV,
                        "MSI-X disabled by request; virtio-pci functions are INTx only"
                    );
                    None
                }
            };
        // Built up as we go so that an error part way through drops the slots
        // already created, which stops their workers and deassigns their fds.
        let mut bus = Self {
            root: Mutex::new(PciRoot::new()),
            slots: Vec::with_capacity(devices.len()),
            mode,
            interrupts,
        };
        for (slot, mut device) in devices.into_iter().enumerate() {
            let bar_base = layout::pci_bar_slot(slot as u64);
            // Not `first + slot`: the pins that skips are ones this machine's own
            // legacy devices own — pin 8 is the RTC, which Linux will not share,
            // and pin 13 is the ACPI SCI (see `layout::VIRTIO_IRQS`).
            let gsi = layout::virtio_irq(slot).ok_or(VirtioPciAttachError::Bus {
                slot,
                source: PciError::BusFull,
            })?;

            let irqfd = IrqFdLine::new(&vm, gsi)
                .map_err(|source| VirtioPciAttachError::Irq { slot, source })?;

            // Handed over before the device disappears into its transport; the
            // real waker (the worker's queue-0 eventfd) does not exist yet, so
            // it is filled in below (see `virtio_core::DeferredWaker`).
            let waker = virtio_core::DeferredWaker::new();
            device.set_host_waker(Arc::clone(&waker) as Arc<dyn virtio_core::HostWaker>);

            let built = bus.attach_function(
                slot,
                device,
                &mem,
                Arc::new(irqfd),
                msi.clone(),
                bar_base,
                gsi,
            )?;

            let notifier = if mode.is_offloaded() {
                // Every queue has its own notification address, so the offload
                // needs no datamatch (see `crate::notify`).
                let addressing = NotifyAddressing::PerQueue {
                    base: bar_base.saturating_add(vpci::NOTIFY_CFG_OFFSET),
                    stride: u64::from(vpci::NOTIFY_OFF_MULTIPLIER),
                };
                DeviceNotifier::attach(Arc::clone(&vm), slot, addressing, &built.transport)?
            } else {
                None
            };
            if let Some(host_waker) = notifier.as_ref().and_then(|n| n.waker()) {
                waker.install(host_waker);
            }

            tracing::info!(
                slot,
                device = ?built.device_type,
                address = format_args!("00:{:02x}.0", built.device_number),
                bar = format_args!("{bar_base:#x}"),
                irq = gsi,
                msix_vectors = built.msix_vectors,
                offloaded_queues = notifier.as_ref().map_or(0, |n| n.offloaded_queues().len()),
                "attached virtio-pci device"
            );
            bus.slots.push(VirtioPciSlot {
                device_number: built.device_number,
                bar_base,
                irq: gsi,
                msix_vectors: built.msix_vectors,
                transport: built.transport,
                notifier,
            });
        }
        Ok(bus)
    }

    /// Places `devices` on consecutive PCI device numbers on a host whose
    /// hypervisor has **no in-kernel interrupt controllers** — WHP (EPIC 17
    /// phase 4). The PCI peer of
    /// [`crate::virtio::VirtioMmioBus::attach_userspace`], differing from
    /// [`Self::attach_with_interrupts`] in exactly the three host primitives:
    ///
    /// * INTx lines come from the [`UserspaceIrqChip`]'s IOAPIC instead of
    ///   irqfds — same `Arc<dyn IrqLine>`, same pins, same `interrupt_line`
    ///   register value;
    /// * MSI-X messages go through [`UserspaceMsiSink`] — the architectural
    ///   address/data decode plus one `InterruptDelivery::request` — instead of
    ///   `KVM_SIGNAL_MSI`. Same capability bytes, same table semantics;
    /// * there is no ioeventfd, so every kick is a full exit served inline on
    ///   the vCPU thread — and *because* every kick is decoded against the
    ///   BAR's current base ([`crate::pci::PciRoot::locate_mmio`]), the
    ///   ioeventfd rebasing dance does not exist here rather than being missed.
    ///
    /// Portable on purpose: it compiles and is exercised on Linux too, which is
    /// what keeps its unit tests running in CI on both hosts.
    pub fn attach_userspace(
        mem: Arc<GuestMem>,
        devices: Vec<Box<dyn VirtioDevice>>,
        irqchip: &UserspaceIrqChip,
        interrupts: PciInterruptMode,
    ) -> Result<Self, VirtioPciAttachError> {
        let msi: Option<Arc<dyn MsiSink>> = if interrupts.is_msix() {
            Some(Arc::new(UserspaceMsiSink::new(
                irqchip.interrupt_delivery(),
            )))
        } else {
            tracing::info!(
                var = PCI_INTERRUPT_MODE_ENV,
                "MSI-X disabled by request; virtio-pci functions are INTx only"
            );
            None
        };
        let mut bus = Self {
            root: Mutex::new(PciRoot::new()),
            slots: Vec::with_capacity(devices.len()),
            #[cfg(target_os = "linux")]
            mode: QueueNotifyMode::Synchronous,
            interrupts,
        };
        for (slot, device) in devices.into_iter().enumerate() {
            let bar_base = layout::pci_bar_slot(slot as u64);
            let gsi = layout::virtio_irq(slot).ok_or(VirtioPciAttachError::Bus {
                slot,
                source: PciError::BusFull,
            })?;
            let line = irqchip
                .virtio_line(slot)
                .map_err(|source| VirtioPciAttachError::IrqChip { slot, source })?;

            let built =
                bus.attach_function(slot, device, &mem, line, msi.clone(), bar_base, gsi)?;
            tracing::info!(
                slot,
                device = ?built.device_type,
                address = format_args!("00:{:02x}.0", built.device_number),
                bar = format_args!("{bar_base:#x}"),
                irq = gsi,
                msix_vectors = built.msix_vectors,
                "attached virtio-pci device on the userspace irqchip (synchronous kicks)"
            );
            bus.slots.push(VirtioPciSlot {
                device_number: built.device_number,
                bar_base,
                irq: gsi,
                msix_vectors: built.msix_vectors,
                transport: built.transport,
                #[cfg(target_os = "linux")]
                notifier: None,
            });
        }
        Ok(bus)
    }

    /// Builds and attaches one function's host-neutral pieces: the MSI-X table
    /// size, the configuration space, the [`IntxLine`] gate, the transport and
    /// the config-space attachment to the root. Shared by both constructors so
    /// a function's guest-visible identity cannot depend on which host wired it.
    #[allow(clippy::too_many_arguments)]
    fn attach_function(
        &mut self,
        slot: usize,
        device: Box<dyn VirtioDevice>,
        mem: &Arc<GuestMem>,
        inner_line: Arc<dyn IrqLine>,
        msi: Option<Arc<dyn MsiSink>>,
        bar_base: u64,
        gsi: u32,
    ) -> Result<BuiltFunction, VirtioPciAttachError> {
        // How many MSI-X vectors this function needs: one per queue plus one
        // for configuration changes. A device with more queues than the table
        // region can hold gets no capability rather than a table too small for
        // its own queues — INTx still works, and the warning says why.
        let queues = device.queue_max_sizes().len();
        let table_size = msi.as_ref().and_then(|_| {
            msix::table_size_for(queues).or_else(|| {
                tracing::warn!(
                    slot,
                    queues,
                    max = msix::MAX_MSIX_VECTORS,
                    "device has too many queues for an MSI-X table; publishing INTx only"
                );
                None
            })
        });

        // The config space is built first so its INTx flag can gate the
        // line the transport is about to be handed.
        let (mut config, msix_cap_at) =
            Self::build_config_space(slot, bar_base, gsi, device.as_ref(), table_size)?;
        let line = Arc::new(IntxLine {
            line: inner_line,
            enabled: config.intx_flag(),
        });

        let device_type = device.device_type();
        let transport = match (msi, table_size) {
            (Some(sink), Some(_)) => {
                PciTransport::with_msix(slot, device, Arc::clone(mem), line, sink)
            }
            _ => PciTransport::new(slot, device, Arc::clone(mem), line),
        }
        .map_err(|source| VirtioPciAttachError::Transport { slot, source })?;

        // The transport owns the message-control value it reads on every
        // interrupt; the config space keeps it up to date with what the guest
        // wrote. Nothing here interprets the bits.
        if let (Some(at), Some(handle)) = (msix_cap_at, transport.msix_control_handle()) {
            config.mirror_dword(at, handle);
        }
        let msix_vectors = transport.msix_table_size();
        let transport = Arc::new(Mutex::new(transport));

        let device_number = match self.root.lock() {
            Ok(mut root) => root
                .attach(config, slot)
                .map_err(|source| VirtioPciAttachError::Bus { slot, source })?,
            // Only reachable if a previous panic poisoned it, which cannot
            // have happened yet: nothing else holds this lock during setup.
            Err(_) => {
                return Err(VirtioPciAttachError::Bus {
                    slot,
                    source: PciError::BusFull,
                })
            }
        };
        Ok(BuiltFunction {
            device_number,
            msix_vectors,
            device_type,
            transport,
        })
    }

    /// Builds one device's PCI configuration space: the modern virtio identity,
    /// the single memory BAR the host has reserved for it, its INTx line, the four
    /// capability records that tell a driver where everything is, and — when
    /// `msix_table_size` is given — an MSI-X capability.
    ///
    /// Returns the configuration space and the byte offset the MSI-X record landed
    /// at, which is the register whose value has to reach the transport.
    ///
    /// The identity comes from the transport module rather than from here — the
    /// bus knows about type-0 headers, not about virtio.
    ///
    /// Deliberately an associated function: it needs no bus state, and that is
    /// what lets the tests below check the bytes a firmware matches on without
    /// KVM, an eventfd or a real device.
    fn build_config_space(
        slot: usize,
        bar_base: u64,
        gsi: u32,
        device: &dyn VirtioDevice,
        msix_table_size: Option<u16>,
    ) -> Result<(ConfigSpace, Option<u8>), VirtioPciAttachError> {
        let bus_error = |source| VirtioPciAttachError::Bus { slot, source };
        // The aperture and the BAR size are both host constants far below 4 GiB,
        // and the GSI is one of a handful of low pins.
        let base = u32::try_from(bar_base).map_err(|_| {
            bus_error(PciError::MisalignedBar {
                base: u32::MAX,
                size: 0,
            })
        })?;
        let size = u32::try_from(vpci::VIRTIO_PCI_BAR_SIZE).unwrap_or(u32::MAX);

        let mut config = ConfigSpace::type0(
            vpci::VIRTIO_PCI_VENDOR_ID,
            vpci::device_id(device.device_type()),
            vpci::class_code(device.device_type()),
            vpci::VIRTIO_PCI_REVISION,
        )
        // Linux reads the virtio *vendor* id out of the PCI subsystem vendor;
        // EDK2's Virtio10Dxe refuses anything with a subsystem *device* id below
        // 0x40, which is what the spec asks a non-transitional device to publish.
        .with_subsystem(
            vpci::VIRTIO_PCI_SUBSYSTEM_VENDOR_ID,
            vpci::VIRTIO_PCI_SUBSYSTEM_DEVICE_ID,
        )
        .with_memory_bar(vpci::VIRTIO_PCI_BAR_INDEX, base, size)
        .map_err(bus_error)?
        .with_interrupt(
            vpci::VIRTIO_PCI_INTERRUPT_PIN,
            u8::try_from(gsi).unwrap_or(u8::MAX),
        );
        for record in vpci::capability_records() {
            config.add_capability(&record).map_err(bus_error)?;
        }
        // MSI-X last, so the four virtio records keep the offsets every existing
        // test and log line names — and COMMON_CFG stays the list head, which is
        // what decides whether Linux's modern driver binds at all.
        let msix_cap_at = match msix_table_size {
            Some(table_size) => Some(
                config
                    .add_capability_writable(
                        &msix::capability_record(table_size),
                        // Only the enable and function-mask bits of message
                        // control are the guest's; the table size in the same
                        // halfword must stay read-only.
                        &[msix::MSIX_CONTROL_WRITE_MASK],
                    )
                    .map_err(bus_error)?,
            ),
            None => None,
        };
        Ok((config, msix_cap_at))
    }

    /// Shares the VM's pause gate with every device on the bus (ADR-0005), so a
    /// device with a worker thread of its own parks with everything else.
    pub fn set_quiesce(&self, quiesce: Arc<virtio_core::Quiesce>) {
        for slot in &self.slots {
            match slot.transport.lock() {
                Ok(mut transport) => transport.set_quiesce(Arc::clone(&quiesce)),
                Err(_) => tracing::error!(
                    device = slot.device_number,
                    "virtio-pci transport lock is poisoned; this device will not pause"
                ),
            }
        }
        #[cfg(target_os = "linux")]
        for slot in &self.slots {
            if let Some(notifier) = slot.notifier() {
                notifier.set_quiesce(Arc::clone(&quiesce));
            }
        }
    }

    /// Machine reset (ADR-0005): every function's configuration space back to
    /// power-on, every transport and device back to power-on, and the
    /// queue-notify registrations rebuilt around the BARs' restored addresses.
    ///
    /// That last step is the one that is easy to miss and impossible to
    /// diagnose: an ioeventfd is registered at an *absolute* guest address
    /// derived from wherever the previous guest's BIOS put the BAR. Reset the
    /// config space without re-basing and the kicks of the next boot land at an
    /// address nothing is listening to — a device that enumerates, negotiates
    /// and then never completes a request.
    pub fn reset(&self) {
        for slot in &self.slots {
            match slot.transport.lock() {
                Ok(mut transport) => transport.power_on_reset(),
                Err(_) => tracing::error!(
                    device = slot.device_number,
                    "virtio-pci transport lock is poisoned; this device is not reset"
                ),
            }
        }
        match self.root.lock() {
            Ok(mut root) => root.reset(),
            Err(_) => {
                tracing::error!("PCI root lock is poisoned; configuration space is not reset")
            }
        }
        #[cfg(target_os = "linux")]
        self.reconcile_notify();
    }

    pub fn slots(&self) -> &[VirtioPciSlot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How queue kicks reach the devices on this bus.
    #[cfg(target_os = "linux")]
    pub fn notify_mode(&self) -> QueueNotifyMode {
        self.mode
    }

    /// Which interrupt mechanisms the functions on this bus publish.
    pub fn interrupt_mode(&self) -> PciInterruptMode {
        self.interrupts
    }

    /// Stops every queue worker thread and deassigns their ioeventfds.
    ///
    /// Idempotent, and also run from `Drop`, so "closing the VM leaves no device
    /// threads behind" holds even on an error path that never gets here. A bus
    /// whose kicks are synchronous owns no threads and no registrations, so
    /// there is nothing to undo.
    pub fn shutdown(&self) {
        #[cfg(target_os = "linux")]
        for slot in &self.slots {
            if let Some(notifier) = &slot.notifier {
                notifier.shutdown();
            }
        }
    }

    // ------------------------------------------------------------- dispatch

    /// True when `port` is a configuration-mechanism port.
    pub fn claims_port(port: u16) -> bool {
        PciRoot::contains(port)
    }

    /// Guest read from `0xcf8`/`0xcfc`.
    pub fn io_read(&self, port: u16, data: &mut [u8]) {
        match self.root.lock() {
            Ok(root) => root.io_read(port, data),
            Err(_) => {
                tracing::error!(
                    port = format_args!("{port:#x}"),
                    "PCI root lock is poisoned; reading all-ones"
                );
                data.fill(0xff);
            }
        }
    }

    /// Guest write to `0xcf8`/`0xcfc`.
    ///
    /// Two kinds of write are more than a register update:
    ///
    /// * one that moves a BAR window moves the queue notification area with it,
    ///   and the KVM ioeventfds registered inside it must follow — see
    ///   [`Self::reconcile_notify`];
    /// * one that changes the MSI-X message control register may have made
    ///   everything the pending-bit array remembers deliverable (enabling MSI-X,
    ///   or clearing the function mask), which only the transport can act on.
    pub fn io_write(&self, port: u16, data: &[u8]) {
        let write = match self.root.lock() {
            Ok(mut root) => root.io_write(port, data),
            Err(_) => {
                tracing::error!(
                    port = format_args!("{port:#x}"),
                    "PCI root lock is poisoned; dropping guest write"
                );
                return;
            }
        };
        // The root lock is released before either follow-up: `reconcile_notify`
        // takes it again to read the new windows, and neither may hold it while a
        // transport lock is taken (the queue workers hold those). On the
        // userspace path a decode change needs no follow-up at all: nothing is
        // registered at an absolute address, so there is nothing to move.
        #[cfg(target_os = "linux")]
        if write.decode_changed {
            self.reconcile_notify();
        }
        if write.mirror_changed {
            if let Some(owner) = write.owner {
                self.msix_control_changed(owner);
            }
        }
    }

    /// Tells one function's transport that its MSI-X message control register
    /// changed value.
    ///
    /// The transport decides what that means; this only delivers the news, which is
    /// what keeps [`crate::pci`] ignorant of MSI-X semantics.
    fn msix_control_changed(&self, owner: usize) {
        let Some(slot) = self.slots.get(owner) else {
            return;
        };
        match slot.transport.lock() {
            Ok(transport) => transport.msix_control_changed(),
            // A dropped notification here costs at most one delayed interrupt,
            // which the next signal on that vector delivers; taking the VM down
            // over it would cost the whole guest.
            Err(_) => tracing::error!(
                slot = owner,
                "virtio-pci transport lock is poisoned; not releasing pending MSI-X vectors"
            ),
        }
    }

    /// Re-points every device's queue-notify ioeventfds at wherever its BAR now
    /// decodes.
    ///
    /// Called after any configuration write that changed *some* function's decode
    /// state, and deliberately over **all** slots rather than just the one that
    /// changed. That is what makes a permutation of BAR addresses resolvable:
    /// EDK2's `PciBusDxe` hands out our own aperture slots in reverse order, so
    /// the first device to be enabled wants an address a *different* device's
    /// stale registration still owns. KVM refuses that registration, the queue
    /// stays on the synchronous path for the moment, and this sweep picks it up
    /// as soon as the other device has moved away. Bounded work: at most
    /// [`crate::pci::MAX_PCI_DEVICES`] slots × [`vpci::MAX_NOTIFY_QUEUES`]-capped
    /// queues, no allocation per slot, and it converges because each device's BAR
    /// only moves when the guest moves it.
    #[cfg(target_os = "linux")]
    fn reconcile_notify(&self) {
        // One pass to release addresses that are no longer ours, then one to
        // claim the new ones — in that order, or two devices swapping windows
        // could never both succeed.
        let mut targets = [None; crate::pci::MAX_PCI_DEVICES];
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(target) = targets.get_mut(index) else {
                break;
            };
            let window = match self.root.lock() {
                Ok(root) => root.bar_window_of(index, vpci::VIRTIO_PCI_BAR_INDEX),
                Err(_) => {
                    tracing::error!("PCI root lock is poisoned; not reconciling notify addresses");
                    return;
                }
            };
            let Some(notifier) = slot.notifier() else {
                continue;
            };
            match window {
                // The BAR decodes somewhere; that is where the kicks will land.
                Some((bar_base, _)) => {
                    let notify_base = bar_base.saturating_add(vpci::NOTIFY_CFG_OFFSET);
                    if notifier.notify_base() != Some(notify_base) {
                        notifier.unregister(&slot.transport);
                        *target = Some(notify_base);
                    }
                }
                // Memory decoding is off, or the BAR is parked at 0. Nothing can
                // reach the device through MMIO either way, so the registrations
                // are released — keeping them would let a later device's kicks be
                // swallowed by this one's stale address.
                None => notifier.unregister(&slot.transport),
            }
        }
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(Some(notify_base)) = targets.get(index).copied() else {
                continue;
            };
            if let Some(notifier) = slot.notifier() {
                notifier.rebase(notify_base, &slot.transport);
            }
        }
    }

    /// Decodes `addr` into the slot whose BAR claims it and the offset inside
    /// that BAR. `None` when no *enabled* BAR covers it.
    fn locate(&self, addr: u64) -> Option<(&VirtioPciSlot, u64)> {
        let (owner, bar, offset) = match self.root.lock() {
            Ok(root) => root.locate_mmio(addr)?,
            Err(_) => {
                tracing::error!(
                    addr = format_args!("{addr:#x}"),
                    "PCI root lock is poisoned; not decoding"
                );
                return None;
            }
        };
        // Only BAR 0 exists on a virtio function, but a device could in
        // principle grow another one; refusing rather than assuming keeps the
        // dispatch honest.
        if bar != vpci::VIRTIO_PCI_BAR_INDEX {
            return None;
        }
        Some((self.slots.get(owner)?, offset))
    }

    /// Guest MMIO read inside some device's BAR window.
    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        let Some((slot, offset)) = self.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.read_bar(offset, data),
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-pci transport lock is poisoned; reading zeroes"
            ),
        }
    }

    /// Guest MMIO write inside some device's BAR window.
    pub fn mmio_write(&self, addr: u64, data: &[u8]) {
        let Some((slot, offset)) = self.locate(addr) else {
            return;
        };
        match slot.transport.lock() {
            Ok(mut transport) => transport.write_bar(offset, data),
            Err(_) => tracing::error!(
                addr = format_args!("{addr:#x}"),
                "virtio-pci transport lock is poisoned; dropping guest write"
            ),
        }
    }
}

impl Drop for VirtioPciBus {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci;

    /// The host's aperture slots and the transport's BAR must be the same size,
    /// or a device would decode into its neighbour's window (too large) or leave
    /// part of its own register block unreachable (too small).
    #[test]
    fn the_bar_slot_size_matches_the_transport() {
        assert_eq!(layout::PCI_MMIO_SLOT_SIZE, vpci::VIRTIO_PCI_BAR_SIZE);
        assert_eq!(
            layout::PCI_MMIO_SLOTS as usize,
            pci::MAX_PCI_DEVICES - 1,
            "one aperture slot per device, and the host bridge has no BAR"
        );
    }

    /// The identity a driver matches on comes from `virtio_core::pci`, so the
    /// config space and the transport cannot disagree — this pins the values
    /// that reach `lspci` and Linux's `vp_modern_probe`.
    #[test]
    fn the_identity_registers_are_the_transports_own() {
        for (kind, device_id, class) in [
            (virtio_core::DeviceType::Net, 0x1041u16, 0x0200_0000u32),
            (virtio_core::DeviceType::Block, 0x1042, 0x0180_0000),
            (virtio_core::DeviceType::Gpu, 0x1050, 0x0380_0000),
            (virtio_core::DeviceType::Input, 0x1052, 0x0980_0000),
        ] {
            assert_eq!(vpci::device_id(kind), device_id, "{kind:?} device id");
            assert_eq!(vpci::class_code(kind), class, "{kind:?} class code");
        }
    }

    #[test]
    fn an_empty_bus_still_has_a_host_bridge() {
        let bus = VirtioPciBus::empty();
        assert!(bus.is_empty());
        assert!(bus.locate(layout::PCI_MMIO_BASE).is_none());
        // The host bridge answers, which is what makes Linux believe in the bus.
        let address = 0x8000_0000u32;
        bus.io_write(pci::CONFIG_ADDRESS_PORT, &address.to_le_bytes());
        let mut id = [0u8; 4];
        bus.io_read(pci::CONFIG_DATA_PORT, &mut id);
        assert_eq!(u32::from_le_bytes(id) & 0xffff, 0x8086);
    }

    /// A device that exists only to be asked what it is.
    struct IdentityOnly(virtio_core::DeviceType);

    impl VirtioDevice for IdentityOnly {
        fn device_type(&self) -> virtio_core::DeviceType {
            self.0
        }
        fn queue_max_sizes(&self) -> &[u16] {
            &[256]
        }
        fn device_features(&self) -> u64 {
            virtio_core::VIRTIO_F_VERSION_1
        }
        fn ack_features(&mut self, _negotiated: u64) -> bool {
            true
        }
        fn read_config(&self, _offset: u64, _data: &mut [u8]) {}
        fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
        fn activate(
            &mut self,
            _resources: virtio_core::DeviceResources,
        ) -> Result<(), virtio_core::DeviceError> {
            Ok(())
        }
        fn notify(&mut self, _queue_index: u16) -> Result<(), virtio_core::DeviceError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    /// Everything EDK2's `Virtio10Dxe` gates on, read back out of the
    /// configuration space the bus actually builds
    /// (`OvmfPkg/Virtio10Dxe/Virtio10.c`, `Virtio10BindingSupported`):
    /// vendor `0x1af4`, device id in `0x1040..=0x107f`, revision ≥ 1,
    /// **subsystem device id ≥ 0x40**, and the capability-list status bit.
    ///
    /// The subsystem id was `0` until UEFI-1803 and is the whole reason the
    /// firmware enumerated our disks and then refused to drive them. Linux never
    /// reads that field, so no Linux boot test could have caught it — hence this
    /// one asserts the firmware's condition literally.
    #[test]
    fn the_config_space_satisfies_edk2s_virtio10_binding() {
        for kind in [
            virtio_core::DeviceType::Block,
            virtio_core::DeviceType::Net,
            virtio_core::DeviceType::Gpu,
            virtio_core::DeviceType::Input,
        ] {
            let device = IdentityOnly(kind);
            let (config, _) = config_space_of(&device, None);

            let id = config.read_dword(pci::reg::ID);
            assert_eq!(
                id & 0xffff,
                u32::from(vpci::VIRTIO_PCI_VENDOR_ID),
                "{kind:?}"
            );
            let device_id = id >> 16;
            assert!(
                (0x1040..=0x107f).contains(&device_id),
                "{kind:?} device id {device_id:#x} outside the modern virtio range"
            );

            let revision = config.read_dword(pci::reg::CLASS_REVISION) & 0xff;
            assert!(
                revision >= 1,
                "{kind:?} revision {revision} is transitional"
            );

            let subsystem = config.read_dword(pci::reg::SUBSYSTEM);
            assert_eq!(
                subsystem & 0xffff,
                u32::from(vpci::VIRTIO_PCI_SUBSYSTEM_VENDOR_ID),
                "{kind:?} subsystem vendor (Linux reads the virtio vendor here)"
            );
            let subsystem_device = subsystem >> 16;
            assert!(
                subsystem_device >= 0x40,
                "{kind:?} subsystem device id {subsystem_device:#x} < 0x40: \
                 Virtio10Dxe will not bind this device"
            );

            // `EFI_PCI_STATUS_CAPABILITY`, without which the firmware never
            // walks the list that locates the four virtio structures.
            assert_ne!(config.status() & (1 << 4), 0, "{kind:?} capability bit");
            assert_eq!(config.read_dword(pci::reg::CAP_POINTER) & 0xff, 0x40);

            // INTA#, so a driver believes there is a legacy interrupt at all.
            let interrupt = config.read_dword(pci::reg::INTERRUPT);
            assert_eq!(
                (interrupt >> 8) & 0xff,
                u32::from(vpci::VIRTIO_PCI_INTERRUPT_PIN)
            );
            assert_eq!(interrupt & 0xff, layout::PCI_FIRST_IRQ);
        }
    }

    /// One device's configuration space as the bus would build it, with or without
    /// an MSI-X capability, plus where that capability landed.
    fn config_space_of(
        device: &dyn VirtioDevice,
        msix_table_size: Option<u16>,
    ) -> (ConfigSpace, Option<u8>) {
        VirtioPciBus::build_config_space(
            0,
            layout::pci_bar_slot(0),
            layout::PCI_FIRST_IRQ,
            device,
            msix_table_size,
        )
        .expect("the host's own aperture and GSI are valid")
    }

    /// Walks the capability list the way a driver does — head from
    /// `CAP_POINTER`, then `cap_next` until 0 — and returns `(id, offset)` pairs.
    fn capability_list(config: &ConfigSpace) -> Vec<(u8, u8)> {
        let mut out = Vec::new();
        let mut at = (config.read_dword(pci::reg::CAP_POINTER) & 0xff) as u8;
        // Bounded by the header: a list that does not terminate must not hang a
        // test any more than it may hang a guest.
        while at >= pci::reg::FIRST_CAPABILITY && out.len() < pci::reg::DWORDS {
            let dword = config.read_dword(at);
            let shift = (at % 4) * 8;
            let id = (dword >> shift) as u8;
            let next = (dword >> (shift + 8)) as u8;
            out.push((id, at));
            if next == 0 {
                break;
            }
            at = next;
        }
        out
    }

    /// The MSI-X capability a driver finds: id `0x11`, the right table size, and
    /// table/PBA pointing at the BAR regions the transport actually decodes.
    #[test]
    fn the_msix_capability_describes_the_transports_own_regions() {
        let device = IdentityOnly(virtio_core::DeviceType::Block);
        let (config, at) = config_space_of(&device, Some(2));
        let at = at.expect("an MSI-X capability was requested");

        let list = capability_list(&config);
        assert_eq!(
            list.len(),
            5,
            "four virtio structure locators plus MSI-X: {list:?}"
        );
        assert_eq!(list[0].0, 0x09, "COMMON_CFG must stay the list head");
        assert_eq!(
            list.last().copied(),
            Some((msix::PCI_CAP_ID_MSIX, at)),
            "MSI-X is last: {list:?}"
        );

        // Message control: table size - 1 in bits 10:0, disabled and unmasked.
        let control = (config.read_dword(at + 2) >> 16) as u16;
        assert_eq!(control & msix::MSIX_CTRL_TABLE_SIZE_MASK, 1, "two vectors");
        assert_eq!(control & msix::MSIX_CTRL_ENABLE, 0);
        assert_eq!(control & msix::MSIX_CTRL_FUNCTION_MASK, 0);

        // Table and PBA: BIR 0, i.e. the one BAR, at the offsets the transport
        // decodes. If these two ever disagree, a driver programs a table nothing
        // reads and gets no interrupts at all.
        let table = config.read_dword(at + 4);
        let pba = config.read_dword(at + 8);
        assert_eq!(
            u64::from(table & 0x7),
            u64::from(vpci::VIRTIO_PCI_BAR_INDEX)
        );
        assert_eq!(u64::from(pba & 0x7), u64::from(vpci::VIRTIO_PCI_BAR_INDEX));
        assert_eq!(u64::from(table & !0x7), vpci::MSIX_TABLE_OFFSET);
        assert_eq!(u64::from(pba & !0x7), vpci::MSIX_PBA_OFFSET);
    }

    /// Only the enable and function-mask bits are the guest's. A guest that could
    /// write the table size would be describing entries the host never allocated.
    #[test]
    fn a_guest_may_write_the_msix_enable_bits_and_nothing_else_in_that_dword() {
        let device = IdentityOnly(virtio_core::DeviceType::Block);
        let (mut config, at) = config_space_of(&device, Some(3));
        let at = at.expect("an MSI-X capability was requested");
        let control = |c: &ConfigSpace| (c.read_dword(at + 2) >> 16) as u16;
        assert_eq!(control(&config) & msix::MSIX_CTRL_TABLE_SIZE_MASK, 2);

        // `pci_msix_clear_and_set_ctrl` writes the halfword at cap + 2.
        let mirror = Arc::new(std::sync::atomic::AtomicU32::new(0));
        config.mirror_dword(at, Arc::clone(&mirror));
        let changed = config.write_dword_bytes(at, 2, &msix::MSIX_CTRL_ENABLE.to_le_bytes());
        assert!(changed, "a mirrored register changed");
        assert_ne!(control(&config) & msix::MSIX_CTRL_ENABLE, 0);
        assert_eq!(
            control(&config) & msix::MSIX_CTRL_TABLE_SIZE_MASK,
            2,
            "the table size is read-only"
        );
        // …and the transport sees it, without this module interpreting the bits.
        assert_eq!(
            (mirror.load(Ordering::Acquire) >> 16) as u16 & msix::MSIX_CTRL_ENABLE,
            msix::MSIX_CTRL_ENABLE
        );

        // Writing every bit of the dword changes only those two.
        let before = config.read_dword(at);
        let _ = config.write_dword_bytes(at, 0, &0xffff_ffffu32.to_le_bytes());
        let after = config.read_dword(at);
        assert_eq!(
            after & !msix::MSIX_CONTROL_WRITE_MASK,
            before & !msix::MSIX_CONTROL_WRITE_MASK,
            "the capability id, next pointer and table size are read-only"
        );
        assert_eq!(control(&config) & msix::MSIX_CTRL_TABLE_SIZE_MASK, 2);
        assert_ne!(control(&config) & msix::MSIX_CTRL_FUNCTION_MASK, 0);

        // A write that changes nothing reports nothing, so the bus does not go
        // looking for pending vectors on every unrelated configuration access.
        assert!(!config.write_dword_bytes(pci::reg::HEADER_TYPE, 0, &[0x10]));
    }

    /// Without MSI-X the configuration space is byte for byte what it was before
    /// MSI-X existed: four capabilities, no fifth record, nothing writable added.
    #[test]
    fn an_intx_only_function_publishes_no_msix_capability() {
        let device = IdentityOnly(virtio_core::DeviceType::Block);
        let (config, at) = config_space_of(&device, None);
        assert_eq!(at, None);
        let list = capability_list(&config);
        assert_eq!(
            list.len(),
            4,
            "the four virtio structure locators: {list:?}"
        );
        assert!(
            !list.iter().any(|(id, _)| *id == msix::PCI_CAP_ID_MSIX),
            "no MSI-X record: {list:?}"
        );
    }

    /// **Every** region of the BAR follows the guest when it moves the BAR —
    /// including the two MSI-X ones — and nothing is left decoding at the old
    /// address.
    ///
    /// EDK2's `PciBusDxe` reassigns every BAR during resource allocation, so this
    /// is not a hypothetical. The queue-notification area needs host help to
    /// follow (its ioeventfds are registered at absolute addresses:
    /// `DeviceNotifier::rebase`, driven by `reconcile_notify` from the same
    /// `bar_window_of` this test reads). The MSI-X table and PBA need none — they
    /// are decoded through `locate_mmio` on every access — and this test is what
    /// says so rather than leaving it to be assumed.
    #[test]
    fn every_bar_region_including_msix_follows_a_guest_bar_move() {
        let device = IdentityOnly(virtio_core::DeviceType::Block);
        let (config, _) = config_space_of(&device, Some(2));
        let mut root = PciRoot::new();
        assert_eq!(root.attach(config, 0), Ok(1));

        let select = |root: &mut PciRoot, register: u8| {
            let address = 0x8000_0000u32 | (1 << 11) | u32::from(register & 0xfc);
            let _ = root.io_write(pci::CONFIG_ADDRESS_PORT, &address.to_le_bytes());
        };
        let write32 = |root: &mut PciRoot, register: u8, value: u32| {
            select(root, register);
            let _ = root.io_write(pci::CONFIG_DATA_PORT, &value.to_le_bytes());
        };

        write32(
            &mut root,
            pci::reg::COMMAND,
            u32::from(crate::pci::command::MEMORY_SPACE),
        );
        let regions = [
            ("common cfg", vpci::COMMON_CFG_OFFSET),
            ("notify", vpci::NOTIFY_CFG_OFFSET),
            ("device cfg", vpci::DEVICE_CFG_OFFSET),
            ("msix table", vpci::MSIX_TABLE_OFFSET),
            ("msix pba", vpci::MSIX_PBA_OFFSET),
            ("last byte", vpci::VIRTIO_PCI_BAR_SIZE - 1),
        ];

        // Somewhere else in the aperture — the reverse-order slot `PciBusDxe`
        // would hand out.
        let moved = layout::pci_bar_slot(layout::PCI_MMIO_SLOTS - 1);
        for (from, to) in [(layout::pci_bar_slot(0), moved), (moved, moved)] {
            if from != to {
                write32(
                    &mut root,
                    pci::reg::BAR0,
                    u32::try_from(to).expect("the aperture is below 4 GiB"),
                );
            }
            for (name, offset) in regions {
                assert_eq!(
                    root.locate_mmio(to + offset),
                    Some((0, vpci::VIRTIO_PCI_BAR_INDEX, offset)),
                    "{name} must decode at the BAR's current base"
                );
            }
            if from != to {
                assert_eq!(
                    root.locate_mmio(from + vpci::MSIX_TABLE_OFFSET),
                    None,
                    "the MSI-X table must not still answer at the old base"
                );
            }
        }
        // And the notify base the bus would rebase the ioeventfds to is derived
        // from the same window, so the two cannot drift apart.
        assert_eq!(
            root.bar_window_of(0, vpci::VIRTIO_PCI_BAR_INDEX),
            Some((moved, vpci::VIRTIO_PCI_BAR_SIZE))
        );
    }

    #[test]
    fn interrupt_mode_parsing_accepts_the_documented_spellings() {
        for on in ["1", "on", "ON", " msix ", "yes"] {
            assert_eq!(
                PciInterruptMode::parse(on),
                Some(PciInterruptMode::Msix),
                "{on}"
            );
        }
        for off in ["0", "off", "OFF", "intx", "no"] {
            assert_eq!(
                PciInterruptMode::parse(off),
                Some(PciInterruptMode::IntxOnly),
                "{off}"
            );
        }
        assert_eq!(PciInterruptMode::parse("maybe"), None);
        assert_eq!(PciInterruptMode::parse(""), None);
        assert!(
            PciInterruptMode::default().is_msix(),
            "MSI-X is the default"
        );
    }

    /// A read of an unclaimed BAR address must not touch a transport at all —
    /// this is the path a guest takes before `pci_enable_device`.
    #[test]
    fn undecoded_mmio_is_dropped() {
        let bus = VirtioPciBus::empty();
        let mut data = [0xffu8; 4];
        bus.mmio_read(layout::pci_bar_slot(0), &mut data);
        assert_eq!(data, [0xff; 4], "mmio_read leaves undecoded reads alone");
        bus.mmio_write(layout::pci_bar_slot(0), &[0; 4]);
    }
}
