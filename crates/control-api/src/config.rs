//! The `entangled run <file>.toml` configuration format (backlog MVP-1201).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to parse VM config: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Smallest guest this machine is willing to build.
pub const MIN_MEMORY_MIB: u64 = 128;

/// Largest guest this machine is willing to build: 64 GiB.
///
/// RAM up to 3072 MiB (`machine_x86::layout::MMIO_HOLE_START`) sits below the
/// 32-bit MMIO hole; anything above that continues at 4 GiB as a second memory
/// region (the high-RAM split — `vmm_core::create_guest_memory` and
/// `machine_x86::e820_map` agree on the shape). The 64 GiB ceiling is a sanity
/// bound, not an architectural one: a typo'd `memory_mib` should be a typed
/// config error before it becomes a 2 TiB `mmap`.
///
/// `apps/entangled` has the test that keeps this consistent with the machine
/// crate; control-api deliberately does not depend on it.
pub const MAX_MEMORY_MIB: u64 = 65536;

/// Fewest vCPUs [`default_vcpus`] gives a new machine: what every installed
/// profile got before the default was derived, and still the floor on a small
/// host.
pub const MIN_DEFAULT_VCPUS: u32 = 2;

/// Most vCPUs [`default_vcpus`] gives a new machine. Not a machine limit (the
/// MP table and the MADT describe up to 254, and a profile may say up to 64):
/// 8 is the largest count measured (ADR-0004, "installed profiles"): the
/// installed GNOME desktop and a test kernel on WHP, with the same boot
/// written for KVM (`boot-tests`, `acpi`). There it
/// still scaled parallel work (6.3x one CPU on 8 jobs) and cost the idle
/// desktop nothing; nothing past it has been measured, and a desktop guest
/// wider than 8 would mostly be taking cores from the VMM's own threads.
pub const MAX_DEFAULT_VCPUS: u32 = 8;

/// The vCPUs a newly installed machine gets when nobody chose a number: half
/// the host's logical CPUs, clamped to
/// [`MIN_DEFAULT_VCPUS`]..=[`MAX_DEFAULT_VCPUS`].
///
/// Half, because each vCPU is a host thread that is busy whenever the guest
/// is, and the VMM's own threads (the GPU renderer and its ring workers, the
/// display, the block and network workers) and the host's desktop need the
/// other half: a GNOME desktop on the GPU keeps the VMM process at 2.6-3.2
/// cores beside its vCPUs (ADR-0004, the zero-copy amendment). `0` — a host
/// that could not say — gets the floor.
///
/// Both surfaces call this: `entangled install` for the profile it writes,
/// and the manager for its wizard's default, so a machine made either way on
/// one host gets the same number.
pub fn default_vcpus(host_logical_cpus: usize) -> u32 {
    let half = u32::try_from(host_logical_cpus / 2).unwrap_or(u32::MAX);
    half.clamp(MIN_DEFAULT_VCPUS, MAX_DEFAULT_VCPUS)
}

/// [`default_vcpus`] for the host this process runs on
/// (`std::thread::available_parallelism`, which honours a CPU affinity mask
/// and, inside WSL, is the WSL VM's processor count).
pub fn host_default_vcpus() -> u32 {
    default_vcpus(std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Which virtio transport the VM's devices sit on. Defaults to `mmio`, so
    /// every profile written before the pci transport existed still describes
    /// exactly the machine it used to.
    #[serde(default)]
    pub transport: VirtioTransport,
    pub boot: BootSection,
    #[serde(default, rename = "disk")]
    pub disks: Vec<DiskSection>,
    /// Optional installer/live medium (an ISO), attached read-only as the last
    /// virtio-blk device — after every `[[disk]]`, so it never shifts the disks'
    /// guest-visible names. `uefi` mode only: the point of a CD-ROM is that the
    /// *firmware* boots it, and a direct-linux guest that merely wants the ISO's
    /// bytes should say what it means with a read-only `[[disk]]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdrom: Option<CdromSection>,
    pub network: Option<NetworkSection>,
    #[serde(default)]
    pub display: DisplaySection,
    /// The guest's sound card (backlog GAME-2102). Off by default — see
    /// [`SoundSection`].
    #[serde(default)]
    pub sound: SoundSection,
    /// The guest's gamepad (backlog GAME-2104). Off by default — see
    /// [`GamepadSection`].
    #[serde(default)]
    pub gamepad: GamepadSection,
}

/// The virtio transport a VM's devices are attached to (EPIC 3 / EPIC 19).
///
/// Devices themselves are transport-agnostic, so this changes only how the guest
/// *finds* them — and how much of the guest has to cooperate:
///
/// * `mmio` needs `virtio_mmio.device=` clauses on the kernel command line and a
///   kernel built with `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`. Nothing enumerates:
///   the host tells the guest where to look. This is the MVP default and what
///   every existing profile means.
/// * `pci` needs nothing on the command line — the guest walks the bus — but does
///   need `CONFIG_VIRTIO_PCI`. It is the only transport a UEFI firmware can use:
///   EDK2's CloudHv build ships `VirtioPciDeviceDxe` and no virtio-MMIO driver at
///   all (ADR-0003), so booting an installer ISO requires it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VirtioTransport {
    /// virtio-mmio slots announced on the kernel command line.
    #[default]
    Mmio,
    /// virtio-pci functions on the PCI root bus.
    Pci,
}

impl VirtioTransport {
    pub fn is_pci(self) -> bool {
        matches!(self, Self::Pci)
    }
}

impl std::fmt::Display for VirtioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mmio => f.write_str("mmio"),
            Self::Pci => f.write_str("pci"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BootMode {
    /// Direct bzImage + initramfs load — the MVP mode (ADR-0001 §3).
    DirectLinux,
    /// Boot a UEFI firmware image, which then finds its own bootloader
    /// (EPIC 18, [ADR-0003](../../docs/adr/0003-uefi-firmware.md)).
    Uefi,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BootSection {
    pub mode: BootMode,
    /// `direct-linux` only: the kernel `bzImage` the host loads itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<PathBuf>,
    /// `direct-linux` only: initramfs loaded after the kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<PathBuf>,
    /// `uefi` only: the firmware image (a PVH ELF such as `CLOUDHV.fd`, or a
    /// flash image entered through the reset vector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<PathBuf>,
    /// `uefi` only: the VM's non-volatile UEFI variable store (UEFI-1804).
    ///
    /// One file per VM, created erased on first use and then owned by the guest
    /// firmware: `BootOrder`, the `Boot####` entries `grub-install` writes, and
    /// (if the firmware is built with secure boot) the key database. Without it
    /// the firmware keeps variables in RAM and an installed system loses its
    /// boot entry every time the VM stops — which is why `entangled install`
    /// always writes this key for a UEFI profile.
    ///
    /// The firmware image itself stays shared and pristine: it is never written
    /// through this path (see `machine_x86::pflash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvram: Option<PathBuf>,
    /// Kernel command line. Meaningless in `uefi` mode — the firmware and the
    /// guest bootloader own the command line there.
    #[serde(default)]
    pub cmdline: String,
}

impl BootSection {
    /// The kernel image, for `direct-linux` profiles. Validation guarantees it
    /// is present in that mode; this accessor keeps the error typed for
    /// callers that build a `BootSection` by hand.
    pub fn require_kernel(&self) -> Result<&PathBuf, ConfigError> {
        self.kernel.as_ref().ok_or_else(|| {
            ConfigError::Invalid("boot.kernel is required for mode = \"direct-linux\"".into())
        })
    }

    /// The firmware image, for `uefi` profiles.
    pub fn require_firmware(&self) -> Result<&PathBuf, ConfigError> {
        self.firmware.as_ref().ok_or_else(|| {
            ConfigError::Invalid("boot.firmware is required for mode = \"uefi\"".into())
        })
    }
}

impl Default for BootSection {
    /// A direct-Linux section with nothing chosen yet. Exists so that adding a
    /// key to this struct does not have to be threaded through every caller
    /// that builds one by hand (`entangled install` builds four).
    fn default() -> Self {
        Self {
            mode: BootMode::DirectLinux,
            kernel: None,
            initramfs: None,
            firmware: None,
            nvram: None,
            cmdline: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiskSection {
    pub path: PathBuf,
    #[serde(default)]
    pub writable: bool,
}

/// `[cdrom]` — one optional installer/live medium (UEFI-1803's machinery as a
/// first-class config key rather than a hand-written `[[disk]]` pair).
///
/// Always read-only — there is deliberately no `writable` key to get wrong: the
/// medium's value is that its provenance was verified
/// (`scripts/fetch-ubuntu-iso.sh`), and a VMM that can scribble on it destroys
/// exactly that.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CdromSection {
    pub path: PathBuf,
}

/// How the guest's virtio-net device reaches a real network (EPIC 5, WHP-1704).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkBackend {
    /// A host TAP interface (`scripts/setup-tap.sh`). Linux only — Windows has
    /// no TAP, and the drivers that would provide one are GPL (ADR-0002); the
    /// run path reports that rather than this crate, which validates the same
    /// config on every host.
    Tap,
    /// User-mode NAT inside the `entangled` process (smoltcp): DHCP, DNS relay
    /// and outbound TCP with no host interface, no `CAP_NET_ADMIN` and no
    /// administrator. The only backend on Windows, and the rootless option on
    /// Linux.
    Usernet,
}

impl std::fmt::Display for NetworkBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tap => f.write_str("tap"),
            Self::Usernet => f.write_str("usernet"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    pub backend: NetworkBackend,
    /// The host TAP interface. Required by `backend = "tap"`, meaningless (and
    /// therefore refused) for `backend = "usernet"`, whose segment lives inside
    /// the process and touches no host interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Optional fixed MAC ("52:00:…"); derived from the VM name when absent.
    /// Every machine `entangled install` creates has one written
    /// ([`new_machine_mac`]), so it survives a rename; a profile without one
    /// keeps the name-derived address it always had.
    pub mac: Option<String>,
}

/// What a newly created machine is networked with when nobody chose: the
/// user-mode NAT, on both hosts (ADR-0002, the installed-network amendment).
/// It is the one backend that needs nothing from the host — no interface made
/// as root, no DHCP server beside it, no administrator — and the only one
/// Windows has; TAP stays one flag away (`--network tap`) on Linux.
pub const DEFAULT_NEW_MACHINE_NETWORK: NetworkBackend = NetworkBackend::Usernet;

/// The MAC a new machine is given, written into its profile once and then
/// never derived again.
///
/// Locally administered and unicast (`52:…`, the prefix
/// `virtio_net::MacAddr::derive` uses), and a hash of the machine's name *and*
/// its disk: two machines on one host never share a disk, so two profiles
/// that happen to share a name (`ubuntu` in two VM directories) still get two
/// addresses — which matters on a TAP bridge, where both are on one segment.
/// Deterministic rather than random so a reinstall onto the same disk keeps
/// the address its guest's network configuration may have been keyed to.
pub fn new_machine_mac(vm_name: &str, disk: &Path) -> String {
    // FNV-1a; no cryptographic requirement, only spread.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let disk = disk.to_string_lossy();
    for byte in b"entangled-mac\0"
        .iter()
        .chain(vm_name.as_bytes())
        .chain(b"\0")
        .chain(disk.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    let h = hash.to_le_bytes();
    format_mac([0x52, h[0], h[1], h[2], h[3], h[4]])
}

/// `aa:bb:cc:dd:ee:ff`, lower case — the spelling a profile carries.
pub fn format_mac(octets: [u8; 6]) -> String {
    octets
        .iter()
        .map(|o| format!("{o:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parses a profile's `mac`: six colon-separated hex octets, unicast and not
/// all zeroes — the only addresses a NIC may have. Refused at load rather than
/// at `run`, so the manager's editor shows the same error before a start does.
pub fn parse_mac(text: &str) -> Result<[u8; 6], ConfigError> {
    let invalid = |why: &str| {
        ConfigError::Invalid(format!(
            "network.mac '{text}' {why}: expected six hex octets such as \"52:54:00:12:34:56\""
        ))
    };
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 6 {
        return Err(invalid("does not have six octets"));
    }
    let mut octets = [0u8; 6];
    for (octet, part) in octets.iter_mut().zip(&parts) {
        if part.is_empty() || part.len() > 2 {
            return Err(invalid("is not hexadecimal"));
        }
        *octet = u8::from_str_radix(part, 16).map_err(|_| invalid("is not hexadecimal"))?;
    }
    if octets[0] & 0x01 != 0 {
        return Err(invalid("is a multicast address, which no NIC may have"));
    }
    if octets == [0; 6] {
        return Err(invalid("is all zeroes"));
    }
    Ok(octets)
}

impl NetworkSection {
    /// The `[network]` a machine `entangled install` creates is given:
    /// `backend` (with `interface` for TAP) and a [`new_machine_mac`], so the
    /// guest's NIC keeps one address for the life of the machine.
    pub fn for_new_machine(
        backend: NetworkBackend,
        interface: Option<String>,
        vm_name: &str,
        disk: &Path,
    ) -> Self {
        Self {
            backend,
            interface: match backend {
                NetworkBackend::Tap => interface,
                // Refused by validation for this backend: the segment lives
                // inside the process and touches no host interface.
                NetworkBackend::Usernet => None,
            },
            mac: Some(new_machine_mac(vm_name, disk)),
        }
    }

    /// The configured MAC as octets, `None` when the profile names none.
    pub fn mac_octets(&self) -> Result<Option<[u8; 6]>, ConfigError> {
        self.mac.as_deref().map(parse_mac).transpose()
    }

    /// The TAP interface name, for `backend = "tap"` callers. Validation
    /// guarantees it is present for that backend; this accessor keeps the error
    /// typed for callers that build a section by hand.
    pub fn require_interface(&self) -> Result<&str, ConfigError> {
        self.interface.as_deref().ok_or_else(|| {
            ConfigError::Invalid("network.interface is required for backend = \"tap\"".into())
        })
    }
}

/// Where the host 3D renderer runs (ADR-0004's GPU-012 amendment).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VirglIsolation {
    /// In a dedicated child process, so a crash inside the host GL stack
    /// degrades the VM to 2D instead of killing it. The default: the concrete
    /// failure it prevents (WSLg's mesa D3D12 driver segfaulting after a few
    /// minutes of compositing) takes the whole VM down otherwise.
    #[default]
    Process,
    /// virglrenderer `dlopen`ed into the VMM itself — one less copy per
    /// transfer and per flushed rect, at the price of sharing a process with
    /// the host GL driver. For a host whose GL stack is trusted.
    InProcess,
}

impl std::fmt::Display for VirglIsolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process => f.write_str("process"),
            Self::InProcess => f.write_str("in-process"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct DisplaySection {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// 3D acceleration (ADR-0004): offer `VIRTIO_GPU_F_VIRGL` backed by a
    /// host virglrenderer. Off by default; a host that cannot bring the
    /// renderer up fails `entangled run` rather than silently booting 2D.
    pub virgl: bool,
    /// Whether that renderer runs in its own process (GPU-012). Ignored when
    /// `virgl` is false.
    pub virgl_isolation: VirglIsolation,
    /// The GPU desktop (ADR-0004, "how a user turns it on"): this VMM's own
    /// Venus renderer, which executes the guest's Vulkan on the host's
    /// Vulkan device — and with it, through Mesa's Zink in the guest, its
    /// OpenGL and its compositor. Both hosts; the host needs a Vulkan device
    /// with `VK_EXT_external_memory_host` (`entangled doctor` says), or the
    /// run fails before the guest boots rather than silently booting 2D.
    ///
    /// One renderer per virtio-gpu device, so `venus` and [`Self::virgl`]
    /// together are refused. Off by default, and **not written when off**:
    /// a profile saved by this build must still load in an engine built
    /// before the key existed (the manager drives a WSL engine that may be
    /// an older release, and every section here denies unknown fields).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub venus: bool,
    /// The refresh rate the virtual monitor's EDID advertises, in Hz
    /// (GAME-2105).
    ///
    /// Not decoration: the guest's compositor schedules against it, and a
    /// frame whose work overruns one period lands in the next one. At the
    /// physical-monitor default of 60 Hz that turns any frame costing more
    /// than 16.7 ms into exactly 30 fps; a virtual display has no scanout to
    /// be honest about, so raising this makes the quantum finer. Measured
    /// before and after in `docs/adr/0004-virtio-gpu-3d.md`.
    pub refresh_hz: u32,
    /// Where the virtio-gpu device mirrors its frame statistics as JSON
    /// (GAME-2105). `entangled run --frame-stats <PATH>` sets it; the
    /// per-window `info` log happens either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_stats: Option<PathBuf>,
    /// Size of the virtio-gpu host-visible window, in MiB: the part of the
    /// device's shared-memory BAR a 3D renderer maps its blobs into (ADR-0004,
    /// the capacity amendment). Absent means the renderer's own default
    /// ([`DEFAULT_HOST_VISIBLE_MIB`] for the Venus renderer). Every rendering
    /// client in the guest keeps its command rings and every host-visible
    /// allocation it maps in this window, so a desktop of GPU clients needs
    /// hundreds of MiB of it. Guest-visible: it is the BAR's size.
    ///
    /// A power of two from [`MIN_HOST_VISIBLE_MIB`] to [`MAX_HOST_VISIBLE_MIB`]:
    /// a PCI BAR is a power of two anyway, and the machine caps one device's
    /// shared-memory BAR at 4 GiB. It costs guest-physical address space, not
    /// host memory — nothing is mapped until the guest maps a blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_visible_mib: Option<u32>,
    /// Device-local (VRAM) memory every Venus client of the guest together
    /// may allocate on each device-local heap of the host GPU, in MiB
    /// (ADR-0004, the resource-exhaustion amendment). Absent means the
    /// renderer's default: three quarters of each heap, which leaves the
    /// host's own desktop the rest. Never more than the heap, whatever it
    /// says; one client may hold three quarters of it, and that is the heap
    /// size the guest is shown.
    ///
    /// From [`MIN_GPU_MEMORY_MIB`] to [`MAX_GPU_MEMORY_MIB`]. Host memory,
    /// not guest address space: it is what a hostile or runaway guest can
    /// take of the host's VRAM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mib: Option<u32>,
}

/// Bounds on [`DisplaySection::refresh_hz`], mirroring `virtio_gpu::edid`'s
/// own limits — this crate must validate the same profile on every host, so
/// the numbers live here rather than behind a dependency on the device crate.
pub const MIN_REFRESH_HZ: u32 = 24;
pub const MAX_REFRESH_HZ: u32 = 240;
/// What a profile that says nothing gets: what a physical monitor would.
pub const DEFAULT_REFRESH_HZ: u32 = 60;

/// Bounds on [`DisplaySection::host_visible_mib`]. The top is
/// `machine_x86::layout::MAX_SHM_BAR_BYTES`, restated here because this crate
/// validates the same profile on every host without the machine crate.
pub const MIN_HOST_VISIBLE_MIB: u32 = 64;
pub const MAX_HOST_VISIBLE_MIB: u32 = 4096;
/// Bounds on [`DisplaySection::gpu_memory_mib`]: a desktop needs a few
/// hundred MiB, and no GPU a host has is 1 TiB.
pub const MIN_GPU_MEMORY_MIB: u32 = 256;
pub const MAX_GPU_MEMORY_MIB: u32 = 1 << 20;

/// The Venus renderer's window when a profile does not say
/// (`virtio_gpu::venus::renderer::VENUS_HOST_VISIBLE_BYTES`, which a test in
/// `entangled` holds equal to this).
pub const DEFAULT_HOST_VISIBLE_MIB: u32 = 4096;

impl Default for DisplaySection {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
            virgl: false,
            virgl_isolation: VirglIsolation::default(),
            venus: false,
            refresh_hz: DEFAULT_REFRESH_HZ,
            frame_stats: None,
            host_visible_mib: None,
            gpu_memory_mib: None,
        }
    }
}

/// Which host renderer a VM's virtio-gpu device gets — the one answer the
/// `[display]` switches `virgl` and `venus` add up to. The profile keeps the
/// two booleans (every profile written before Venus has `virgl`, and an old
/// engine must keep reading new profiles); this is what the run path, the
/// manager and the tests reason with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuRenderer {
    /// No 3D: the device scans out what the guest draws on its CPU.
    #[default]
    TwoD,
    /// OpenGL through the host's virglrenderer (Linux hosts only).
    Virgl,
    /// Vulkan, and OpenGL through Zink, on this VMM's Venus renderer over the
    /// host's Vulkan device (both hosts).
    Venus,
}

impl GpuRenderer {
    pub const ALL: [GpuRenderer; 3] = [GpuRenderer::TwoD, GpuRenderer::Virgl, GpuRenderer::Venus];
}

impl std::fmt::Display for GpuRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TwoD => "2d",
            Self::Virgl => "virgl",
            Self::Venus => "venus",
        })
    }
}

impl DisplaySection {
    /// The renderer these switches select. A section with both switches on
    /// is refused by validation; one built by hand that has both reads as
    /// [`GpuRenderer::Venus`], the newer of the two, but never reaches a run.
    pub fn gpu_renderer(&self) -> GpuRenderer {
        match (self.venus, self.virgl) {
            (true, _) => GpuRenderer::Venus,
            (false, true) => GpuRenderer::Virgl,
            (false, false) => GpuRenderer::TwoD,
        }
    }

    /// Sets the switches for `renderer`, leaving every other key —
    /// `virgl_isolation` and `host_visible_mib` included — as it was, so a
    /// round trip through another renderer keeps a deliberate choice.
    pub fn set_gpu_renderer(&mut self, renderer: GpuRenderer) {
        self.virgl = renderer == GpuRenderer::Virgl;
        self.venus = renderer == GpuRenderer::Venus;
    }
}

/// Which host audio backend a VM's virtio-snd device plays into.
///
/// `control-api` only parses the word; which backends exist on this host, and
/// what to do when the chosen one does not, is `virtio_sound::open_sink`'s
/// business — this crate validates the same profile on every OS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SoundBackend {
    /// The host's native backend if it is there, silence if it is not. Audio
    /// is never a reason a VM fails to boot.
    #[default]
    Auto,
    /// Silence, paced like a sound card. What a headless or CI run wants: the
    /// guest still enumerates a working card.
    Null,
    /// ALSA (Linux hosts). Reached by runtime `dlopen`, never linked — see
    /// `virtio_sound::alsa` for why. Fails loudly if libasound is missing.
    Alsa,
    /// WASAPI in shared mode (Windows hosts). Fails loudly if the machine has
    /// no default render endpoint.
    Wasapi,
}

impl std::fmt::Display for SoundBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Null => "null",
            Self::Alsa => "alsa",
            Self::Wasapi => "wasapi",
        })
    }
}

/// `[sound]` — a virtio-snd playback device for the guest (GAME-2102).
///
/// **Off by default**, for two reasons that are worth stating rather than
/// rediscovering. First, the card is attached *after* every other device, so
/// turning it on never renames `/dev/vda` or shifts a PCI device number — but
/// it does consume one of the eight slots on either bus
/// (`machine_x86::virtio::MAX_VIRTIO_SLOTS`, `machine_x86::pci::MAX_PCI_DEVICES`),
/// and a profile with several disks plus a CD-ROM is already close. Second, an
/// existing profile must keep describing exactly the machine it used to.
///
/// Enable it explicitly, the way `[display] virgl` is enabled:
///
/// ```toml
/// [sound]
/// enabled = true
/// # backend = "auto"   # auto | null | alsa | wasapi
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SoundSection {
    pub enabled: bool,
    /// Which host sink the device plays into. Meaningless while `enabled` is
    /// false, and therefore not refused there: a profile may keep its chosen
    /// backend across an on/off toggle.
    pub backend: SoundBackend,
}

impl Default for SoundSection {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: SoundBackend::Auto,
        }
    }
}

/// Which host mechanism a VM's gamepad reads real controllers through.
///
/// As with [`SoundBackend`], `control-api` only parses the word; which
/// mechanisms exist on this host is `virtio_input::gamepad::open_source`'s
/// business, so the same profile validates on every OS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GamepadBackend {
    /// The host's native mechanism if this OS has one, [`Self::Null`] if not.
    /// Never fails, and in particular never fails because no controller is
    /// plugged in — that is what hotplug is for.
    #[default]
    Auto,
    /// A pad the host never moves. The guest still enumerates a working
    /// joystick, which is what a headless or CI run wants and what the boot
    /// tests drive with synthetic events.
    Null,
    /// Linux hosts: `/dev/input/event*`, read directly.
    Evdev,
    /// Windows hosts: XInput. Spelled out because `kebab-case` would other-
    /// wise make the word `x-input`, which nobody would ever type.
    #[serde(rename = "xinput")]
    XInput,
}

impl std::fmt::Display for GamepadBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Null => "null",
            Self::Evdev => "evdev",
            Self::XInput => "xinput",
        })
    }
}

/// The most players one VM can have; mirrors `virtio_input::MAX_PLAYERS`
/// (XInput's own limit) and is bounded well below it by the slot budget.
pub const MAX_GAMEPAD_PLAYERS: u8 = 4;

/// `[gamepad]` — virtio-input gamepads for the guest (GAME-2104).
///
/// **Off by default**, for exactly the two reasons `[sound]` is. Pads are
/// attached last, so turning them on never renames `/dev/vda` or shifts a PCI
/// device number — but each takes one of the eight slots on either bus
/// (`machine_x86::virtio::MAX_VIRTIO_SLOTS`, and eight functions plus the host
/// bridge in `machine_x86::pci::MAX_PCI_DEVICES`). And an existing profile
/// must keep describing exactly the machine it used to.
///
/// # The slot budget, spelled out
///
/// One pad is a device, not a feature flag: there is no way to put two
/// players on one virtio-input device, because one virtio-input device is one
/// evdev device and a game reads one controller per node. So `players = N`
/// costs N slots, and the eight break down like this:
///
/// | Device | Slots |
/// |---|---|
/// | disks (`[[disk]]`, one each) | 1+ |
/// | virtio-gpu | 1 |
/// | keyboard | 1 |
/// | tablet (absolute pointer) | 1 |
/// | network (`[network]`) | 0 or 1 |
/// | sound (`[sound]`) | 0 or 1 |
/// | gamepads (`players`) | 0..=4 |
///
/// A one-disk VM with a network card and sound has spent six, so **two
/// players fit and a CD-ROM or second disk then does not**. Drop the sound
/// card or the network and two players leave room for one more device;
/// three players need both dropped. Four never fits alongside sound and
/// network, and the bus says so by name and count when it is built rather
/// than failing obscurely later.
///
/// ```toml
/// [gamepad]
/// enabled = true
/// # players = 1       # 1..=4, one virtio slot each
/// # backend = "auto"  # auto | null | evdev | xinput
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct GamepadSection {
    pub enabled: bool,
    /// How many pads the guest gets, one virtio slot each. Meaningless while
    /// `enabled` is false, and validated whether or not it is: a profile that
    /// says `players = 9` is a mistake worth reporting even before the switch
    /// is flipped.
    pub players: u8,
    /// Which host mechanism the pads read. Meaningless while `enabled` is
    /// false and therefore not refused there, so a profile may keep its chosen
    /// backend across an on/off toggle.
    pub backend: GamepadBackend,
}

impl Default for GamepadSection {
    fn default() -> Self {
        Self {
            enabled: false,
            players: 1,
            backend: GamepadBackend::Auto,
        }
    }
}

impl VmConfig {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        let cfg: VmConfig = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Attaches (or replaces) the CD-ROM after parsing — the `--cdrom <iso>`
    /// path. Re-runs validation, because the combination rules (uefi mode, the
    /// pci transport) apply to the modified profile, not the one on disk.
    pub fn set_cdrom(&mut self, path: PathBuf) -> Result<(), ConfigError> {
        self.cdrom = Some(CdromSection { path });
        self.validate()
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let err = |m: String| Err(ConfigError::Invalid(m));
        if self.name.is_empty() {
            return err("name must not be empty".into());
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return err(format!(
                "memory_mib {} outside supported range {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB}",
                self.memory_mib
            ));
        }
        if !(1..=64).contains(&self.vcpus) {
            return err(format!(
                "vcpus {} outside supported range 1..=64",
                self.vcpus
            ));
        }
        if self.display.width == 0 || self.display.height == 0 {
            return err("display dimensions must be non-zero".into());
        }
        if !(1..=MAX_GAMEPAD_PLAYERS).contains(&self.gamepad.players) {
            return err(format!(
                "gamepad.players {} outside supported range 1..={MAX_GAMEPAD_PLAYERS}; \
                 each player is one virtio slot, and eight is all there are",
                self.gamepad.players
            ));
        }
        if !(MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&self.display.refresh_hz) {
            return err(format!(
                "display.refresh_hz {} outside supported range {MIN_REFRESH_HZ}..={MAX_REFRESH_HZ}",
                self.display.refresh_hz
            ));
        }
        if self.display.virgl && self.display.venus {
            return err(
                "display.virgl and display.venus are both true, but they are two \
                 different host renderers for the one virtio-gpu device: virgl serves \
                 OpenGL through the host's virglrenderer (Linux hosts), venus serves Vulkan \
                 — and OpenGL through Zink in the guest — on the host's Vulkan device. \
                 Keep one"
                    .into(),
            );
        }
        if let Some(mib) = self.display.host_visible_mib {
            if !mib.is_power_of_two()
                || !(MIN_HOST_VISIBLE_MIB..=MAX_HOST_VISIBLE_MIB).contains(&mib)
            {
                return err(format!(
                    "display.host_visible_mib {mib} must be a power of two from \
                     {MIN_HOST_VISIBLE_MIB} to {MAX_HOST_VISIBLE_MIB}: it is the size of a PCI \
                     BAR, and one device's shared-memory BAR is at most 4 GiB"
                ));
            }
        }
        if let Some(mib) = self.display.gpu_memory_mib {
            if !(MIN_GPU_MEMORY_MIB..=MAX_GPU_MEMORY_MIB).contains(&mib) {
                return err(format!(
                    "display.gpu_memory_mib {mib} must be from {MIN_GPU_MEMORY_MIB} to \
                     {MAX_GPU_MEMORY_MIB}: it is the host VRAM the guest's Vulkan may allocate \
                     per heap (never more than the heap itself)"
                ));
            }
        }
        // Per-backend network keys, same policy as the boot section: the wrong
        // key is refused rather than ignored, so a profile that names a TAP
        // interface under backend = "usernet" fails loudly instead of quietly
        // not using the interface its author configured.
        if let Some(network) = &self.network {
            match network.backend {
                NetworkBackend::Tap => {
                    if network.interface.is_none() {
                        return err("network.interface is required for backend = \"tap\"".into());
                    }
                }
                NetworkBackend::Usernet => {
                    if network.interface.is_some() {
                        return err("network.interface is only valid for backend = \"tap\"; \
                             the usernet segment lives inside the entangled process and uses \
                             no host interface"
                            .into());
                    }
                }
            }
            network.mac_octets()?;
        }
        // Per-mode boot keys: reject the *wrong* key instead of ignoring it,
        // so a profile that names a kernel under mode = "uefi" fails loudly
        // rather than booting something the author did not ask for.
        match self.boot.mode {
            BootMode::DirectLinux => {
                if self.cdrom.is_some() {
                    return err("cdrom is only valid for mode = \"uefi\": booting a CD-ROM \
                         means the firmware finds its bootloader, which direct-linux skips. \
                         To hand a direct-linux guest the ISO's bytes, use a [[disk]] with \
                         writable = false"
                        .into());
                }
                if self.boot.kernel.is_none() {
                    return err("boot.kernel is required for mode = \"direct-linux\"".into());
                }
                if self.boot.firmware.is_some() {
                    return err("boot.firmware is only valid for mode = \"uefi\"; \
                         direct-linux boots without firmware"
                        .into());
                }
                if self.boot.nvram.is_some() {
                    return err("boot.nvram is only valid for mode = \"uefi\"; \
                         a direct-linux guest has no UEFI variables to store"
                        .into());
                }
            }
            BootMode::Uefi => {
                if self.boot.firmware.is_none() {
                    return err("boot.firmware is required for mode = \"uefi\"".into());
                }
                if self.boot.kernel.is_some() || self.boot.initramfs.is_some() {
                    return err("boot.kernel/boot.initramfs are only valid for mode = \
                         \"direct-linux\"; in uefi mode the firmware loads the guest"
                        .into());
                }
                // A UEFI firmware cannot see a virtio-mmio device at all: EDK2's
                // CloudHv build ships VirtioPciDeviceDxe/Virtio10Dxe/VirtioBlkDxe
                // and no virtio-MMIO driver (ADR-0003). The combination is not
                // "slower" or "less featured", it is a firmware that boots
                // perfectly and then reports "No bootable option or device was
                // found" — which reads like a bug in the media, the ISO or the
                // block device, and sends the reader looking in four wrong
                // places. Refuse it while we still know why.
                if !self.transport.is_pci() && (!self.disks.is_empty() || self.cdrom.is_some()) {
                    let media = match (self.disks.len(), self.cdrom.is_some()) {
                        (0, _) => "the configured cdrom".to_string(),
                        (n, true) => format!("the {n} configured disk(s) and the cdrom"),
                        (n, false) => format!("the {n} configured disk(s)"),
                    };
                    return err(format!(
                        "mode = \"uefi\" needs transport = \"pci\": a UEFI firmware has no \
                         virtio-mmio driver, so {media} would be invisible to it and the \
                         boot would end at \"No bootable option or device was found\""
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the backlog (EPIC 12) must parse as-is.
    const BACKLOG_EXAMPLE: &str = r#"
name = "debian-demo"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"
initramfs = "artifacts/bootstrap/initrd.img"
cmdline = "console=ttyS0 root=/dev/vda1 rw"

[[disk]]
path = "images/debian.raw"
writable = true

[network]
backend = "tap"
interface = "entangled0"

[display]
width = 1920
height = 1080
scale = 1.0
"#;

    #[test]
    fn parses_backlog_example() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(cfg.name, "debian-demo");
        assert_eq!(cfg.boot.mode, BootMode::DirectLinux);
        assert_eq!(
            cfg.boot.require_kernel().unwrap(),
            &PathBuf::from("artifacts/bootstrap/vmlinuz")
        );
        assert_eq!(cfg.disks.len(), 1);
        assert!(cfg.disks[0].writable);
        assert_eq!(
            cfg.network.as_ref().unwrap().interface.as_deref(),
            Some("entangled0")
        );
        assert_eq!(cfg.display.width, 1920);
    }

    /// Half the host, never fewer than the old fixed 2 and never more than 8,
    /// with the host count injected so the rule is pinned on every machine.
    #[test]
    fn default_vcpus_is_half_the_host_clamped_to_2_through_8() {
        for (host, vcpus) in [
            (0, 2), // a host that could not say
            (1, 2),
            (2, 2),
            (4, 2),
            (5, 2),
            (6, 3),
            (8, 4),
            (12, 6),
            (16, 8),
            (24, 8), // this project's Windows host, a 12-core Threadripper
            (128, 8),
            (usize::MAX, 8),
        ] {
            assert_eq!(default_vcpus(host), vcpus, "{host} logical CPUs");
        }
        assert_eq!(default_vcpus(0), MIN_DEFAULT_VCPUS);
        assert_eq!(default_vcpus(usize::MAX), MAX_DEFAULT_VCPUS);
        // Whatever this host is, the answer is one a profile may carry.
        let here = host_default_vcpus();
        assert!((MIN_DEFAULT_VCPUS..=MAX_DEFAULT_VCPUS).contains(&here));
        let mut cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("parses");
        cfg.vcpus = MAX_DEFAULT_VCPUS;
        cfg.validate().expect("the ceiling is a valid profile");
    }

    /// A profile written before the gamepad existed must keep describing
    /// exactly the machine it used to: no pad, and therefore no seventh slot
    /// taken from a VM that was counting on it.
    #[test]
    fn a_gamepad_is_off_unless_a_profile_asks_for_it() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("the backlog example parses");
        assert!(!cfg.gamepad.enabled);
        assert_eq!(cfg.gamepad.backend, GamepadBackend::Auto);
        // Round-trips without inventing a section.
        let text = toml::to_string(&cfg).expect("serialises");
        assert_eq!(
            VmConfig::from_toml(&text).expect("round-trips").gamepad,
            cfg.gamepad
        );
    }

    /// `[display] host_visible_mib`: absent stays absent (a profile written
    /// before it keeps its bytes), a power of two inside the BAR cap is taken,
    /// anything else is refused by name.
    #[test]
    fn the_host_visible_window_is_a_power_of_two_inside_the_bar_cap() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("parses");
        assert_eq!(cfg.display.host_visible_mib, None);
        let text = toml::to_string(&cfg).expect("serialises");
        assert!(
            !text.contains("host_visible_mib"),
            "no key invented: {text}"
        );

        for mib in [MIN_HOST_VISIBLE_MIB, 256, 512, MAX_HOST_VISIBLE_MIB] {
            let text = format!("{BACKLOG_EXAMPLE}host_visible_mib = {mib}\n");
            let cfg = VmConfig::from_toml(&text).unwrap_or_else(|e| panic!("{mib}: {e}"));
            assert_eq!(cfg.display.host_visible_mib, Some(mib));
            let back = toml::to_string(&cfg).expect("serialises");
            assert_eq!(
                VmConfig::from_toml(&back).expect("round-trips").display,
                cfg.display
            );
        }
        for mib in [0, 32, 100, 768, 8192, u32::MAX] {
            let text = format!("{BACKLOG_EXAMPLE}host_visible_mib = {mib}\n");
            let err = VmConfig::from_toml(&text).expect_err("refused");
            assert!(err.to_string().contains("host_visible_mib"), "{mib}: {err}");
        }
        const {
            assert!(DEFAULT_HOST_VISIBLE_MIB.is_power_of_two());
            assert!(
                DEFAULT_HOST_VISIBLE_MIB >= MIN_HOST_VISIBLE_MIB
                    && DEFAULT_HOST_VISIBLE_MIB <= MAX_HOST_VISIBLE_MIB
            );
        };
    }

    /// `[display] gpu_memory_mib`: absent stays absent, inside its bounds is
    /// taken, anything else is refused by name.
    #[test]
    fn the_gpu_memory_cap_is_optional_and_bounded() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("parses");
        assert_eq!(cfg.display.gpu_memory_mib, None);
        let text = toml::to_string(&cfg).expect("serialises");
        assert!(!text.contains("gpu_memory_mib"), "no key invented: {text}");
        for mib in [MIN_GPU_MEMORY_MIB, 3000, 6144, MAX_GPU_MEMORY_MIB] {
            let text = format!("{BACKLOG_EXAMPLE}gpu_memory_mib = {mib}\n");
            let cfg = VmConfig::from_toml(&text).unwrap_or_else(|e| panic!("{mib}: {e}"));
            assert_eq!(cfg.display.gpu_memory_mib, Some(mib));
            let back = toml::to_string(&cfg).expect("serialises");
            assert_eq!(
                VmConfig::from_toml(&back).expect("round-trips").display,
                cfg.display
            );
        }
        for mib in [0, 255, MAX_GPU_MEMORY_MIB + 1, u32::MAX] {
            let text = format!("{BACKLOG_EXAMPLE}gpu_memory_mib = {mib}\n");
            let err = VmConfig::from_toml(&text).expect_err("refused");
            assert!(err.to_string().contains("gpu_memory_mib"), "{mib}: {err}");
        }
    }

    /// `[display] venus`: off unless asked for, never written while off (an
    /// older engine denies unknown keys), round-trips when on, and refused
    /// beside `virgl` — one device, one renderer.
    #[test]
    fn venus_is_an_opt_in_renderer_that_excludes_virgl() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("parses");
        assert!(!cfg.display.venus);
        assert_eq!(cfg.display.gpu_renderer(), GpuRenderer::TwoD);
        let text = toml::to_string_pretty(&cfg).expect("serialises");
        assert!(!text.contains("venus"), "no key invented while off: {text}");

        let on = format!("{BACKLOG_EXAMPLE}venus = true\n");
        let cfg = VmConfig::from_toml(&on).expect("venus = true parses");
        assert!(cfg.display.venus && !cfg.display.virgl);
        assert_eq!(cfg.display.gpu_renderer(), GpuRenderer::Venus);
        let text = toml::to_string_pretty(&cfg).expect("serialises");
        assert!(text.contains("venus = true"), "{text}");
        assert_eq!(VmConfig::from_toml(&text).expect("round-trips"), cfg);

        let virgl = format!("{BACKLOG_EXAMPLE}virgl = true\n");
        assert_eq!(
            VmConfig::from_toml(&virgl)
                .expect("virgl parses")
                .display
                .gpu_renderer(),
            GpuRenderer::Virgl
        );

        let both = format!("{BACKLOG_EXAMPLE}virgl = true\nvenus = true\n");
        let error = VmConfig::from_toml(&both).expect_err("both renderers refused");
        let ConfigError::Invalid(message) = error else {
            panic!("an Invalid, not a parse error: {error}");
        };
        assert!(
            message.contains("display.virgl") && message.contains("display.venus"),
            "names both keys: {message}"
        );

        // Not a string: a typo'd `venus = "vulkan"` (the old environment
        // variable's value) is a parse error, not a silent false.
        let word = format!("{BACKLOG_EXAMPLE}venus = \"vulkan\"\n");
        assert!(matches!(
            VmConfig::from_toml(&word),
            Err(ConfigError::Parse(_))
        ));
    }

    /// The setter moves only the two switches: isolation and the window
    /// survive a trip through another renderer.
    #[test]
    fn setting_a_renderer_keeps_every_other_display_key() {
        let mut display = DisplaySection {
            virgl_isolation: VirglIsolation::InProcess,
            host_visible_mib: Some(512),
            ..DisplaySection::default()
        };
        for renderer in GpuRenderer::ALL {
            display.set_gpu_renderer(renderer);
            assert_eq!(display.gpu_renderer(), renderer);
            assert!(!(display.virgl && display.venus));
            assert_eq!(display.virgl_isolation, VirglIsolation::InProcess);
            assert_eq!(display.host_visible_mib, Some(512));
        }
        assert_eq!(GpuRenderer::Venus.to_string(), "venus");
        assert_eq!(GpuRenderer::TwoD.to_string(), "2d");
    }

    #[test]
    fn the_gamepad_section_parses_round_trips_and_refuses_typos() {
        let text =
            BACKLOG_EXAMPLE.to_string() + "\n[gamepad]\nenabled = true\nbackend = \"null\"\n";
        let cfg = VmConfig::from_toml(&text).expect("parses");
        assert!(cfg.gamepad.enabled);
        assert_eq!(cfg.gamepad.backend, GamepadBackend::Null);
        assert_eq!(cfg.gamepad.backend.to_string(), "null");

        for (word, expected) in [
            ("auto", GamepadBackend::Auto),
            ("null", GamepadBackend::Null),
            ("evdev", GamepadBackend::Evdev),
            ("xinput", GamepadBackend::XInput),
        ] {
            let text =
                format!("{BACKLOG_EXAMPLE}\n[gamepad]\nenabled = true\nbackend = \"{word}\"\n");
            let cfg = VmConfig::from_toml(&text).unwrap_or_else(|e| panic!("{word}: {e}"));
            assert_eq!(cfg.gamepad.backend, expected);
            assert_eq!(cfg.gamepad.backend.to_string(), word);
        }

        // A backend that does not exist is a config error, not a silent
        // fallback — the same rule `[sound]` follows.
        let typo = BACKLOG_EXAMPLE.to_string() + "\n[gamepad]\nbackend = \"dinput\"\n";
        assert!(VmConfig::from_toml(&typo).is_err());
        // …and so is a field nobody implemented.
        let unknown = BACKLOG_EXAMPLE.to_string() + "\n[gamepad]\ndeadzone = 0.2\n";
        assert!(VmConfig::from_toml(&unknown).is_err());
    }

    /// One player unless a profile asks for more, and never more than the
    /// slot budget can hold.
    #[test]
    fn gamepad_players_defaults_to_one_and_is_bounded() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).expect("parses");
        assert_eq!(
            cfg.gamepad.players, 1,
            "a profile that says nothing gets one"
        );

        for players in 1..=MAX_GAMEPAD_PLAYERS {
            let text =
                format!("{BACKLOG_EXAMPLE}\n[gamepad]\nenabled = true\nplayers = {players}\n");
            let cfg = VmConfig::from_toml(&text).unwrap_or_else(|e| panic!("{players}: {e}"));
            assert_eq!(cfg.gamepad.players, players);
            // …and it survives a round trip through TOML.
            let text = toml::to_string(&cfg).expect("serialises");
            assert_eq!(
                VmConfig::from_toml(&text).expect("round-trips").gamepad,
                cfg.gamepad
            );
        }

        // Zero pads is not "no gamepad", it is a typo; so is a fifth player.
        for players in [0u8, MAX_GAMEPAD_PLAYERS + 1, 200] {
            let text =
                format!("{BACKLOG_EXAMPLE}\n[gamepad]\nenabled = true\nplayers = {players}\n");
            let error = VmConfig::from_toml(&text)
                .expect_err(&format!("players = {players} must be refused"));
            let ConfigError::Invalid(message) = error else {
                panic!("players = {players} must be an Invalid, not a parse error");
            };
            assert!(
                message.contains("gamepad.players") && message.contains("virtio slot"),
                "the message must say what the limit is about: {message}"
            );
        }

        // The count is validated even with the pad switched off: a profile
        // carrying `players = 9` is a mistake worth reporting before the
        // switch is flipped, not after.
        let text = BACKLOG_EXAMPLE.to_string() + "\n[gamepad]\nenabled = false\nplayers = 9\n";
        assert!(matches!(
            VmConfig::from_toml(&text),
            Err(ConfigError::Invalid(_))
        ));
    }

    /// A profile written before virtio-snd existed must keep describing
    /// exactly the machine it used to: no sound card.
    #[test]
    fn sound_is_off_unless_a_profile_asks_for_it() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert!(!cfg.sound.enabled);
        assert_eq!(cfg.sound.backend, SoundBackend::Auto);
    }

    #[test]
    fn the_sound_section_parses_round_trips_and_refuses_typos() {
        let text = BACKLOG_EXAMPLE.to_string() + "\n[sound]\nenabled = true\nbackend = \"null\"\n";
        let cfg = VmConfig::from_toml(&text).unwrap();
        assert!(cfg.sound.enabled);
        assert_eq!(cfg.sound.backend, SoundBackend::Null);
        assert_eq!(cfg.sound.backend.to_string(), "null");
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );

        // Every backend word the runtime knows must parse here too, or a
        // profile the GUI writes is unreadable by the CLI.
        for (word, expected) in [
            ("auto", SoundBackend::Auto),
            ("null", SoundBackend::Null),
            ("alsa", SoundBackend::Alsa),
            ("wasapi", SoundBackend::Wasapi),
        ] {
            let text =
                format!("{BACKLOG_EXAMPLE}\n[sound]\nenabled = true\nbackend = \"{word}\"\n");
            assert_eq!(
                VmConfig::from_toml(&text).unwrap().sound.backend,
                expected,
                "backend = {word}"
            );
        }

        // A typo is a hard error, not a silent fall back to auto: a VM that
        // quietly plays into nothing is the failure this catches.
        let typo = BACKLOG_EXAMPLE.to_string() + "\n[sound]\nbackend = \"pulse\"\n";
        assert!(matches!(
            VmConfig::from_toml(&typo),
            Err(ConfigError::Parse(_))
        ));
        let unknown = BACKLOG_EXAMPLE.to_string() + "\n[sound]\nvolume = 11\n";
        assert!(matches!(
            VmConfig::from_toml(&unknown),
            Err(ConfigError::Parse(_))
        ));
    }

    /// A profile written before the pci transport existed must keep meaning
    /// exactly what it meant: virtio-mmio.
    #[test]
    fn the_transport_defaults_to_mmio() {
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(cfg.transport, VirtioTransport::Mmio);
        assert!(!cfg.transport.is_pci());
    }

    #[test]
    fn the_transport_can_be_selected_and_round_trips() {
        let pci = BACKLOG_EXAMPLE.replace("vcpus = 2", "vcpus = 2\ntransport = \"pci\"");
        let cfg = VmConfig::from_toml(&pci).unwrap();
        assert_eq!(cfg.transport, VirtioTransport::Pci);
        assert!(cfg.transport.is_pci());
        assert_eq!(cfg.transport.to_string(), "pci");
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );

        // A typo is a hard error rather than a silent fall back to mmio: a VM
        // whose devices the guest cannot find looks like a device bug.
        let typo = BACKLOG_EXAMPLE.replace("vcpus = 2", "vcpus = 2\ntransport = \"pcie\"");
        assert!(matches!(
            VmConfig::from_toml(&typo),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn display_and_disks_are_optional() {
        let cfg = VmConfig::from_toml(
            r#"
name = "tiny"
memory_mib = 512
vcpus = 1
[boot]
mode = "direct-linux"
kernel = "vmlinuz"
"#,
        )
        .unwrap();
        assert_eq!(cfg.display, DisplaySection::default());
        assert!(cfg.disks.is_empty());
        assert!(cfg.network.is_none());
    }

    /// EPIC 18 / ADR-0003: a UEFI profile names a firmware image and no kernel.
    const UEFI_EXAMPLE: &str = r#"
name = "ubuntu-uefi"
memory_mib = 2560
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[[disk]]
path = "images/ubuntu.raw"
writable = true
"#;

    #[test]
    fn parses_uefi_profile() {
        let cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert_eq!(
            cfg.boot.require_firmware().unwrap(),
            &PathBuf::from("artifacts/firmware/CLOUDHV.fd")
        );
        assert!(cfg.boot.kernel.is_none());
        assert!(cfg.boot.require_kernel().is_err());
    }

    #[test]
    fn boot_keys_must_match_the_mode() {
        // uefi without firmware
        let no_fw = UEFI_EXAMPLE.replace(r#"firmware = "artifacts/firmware/CLOUDHV.fd""#, "");
        assert!(matches!(
            VmConfig::from_toml(&no_fw),
            Err(ConfigError::Invalid(_))
        ));
        // uefi *and* a kernel: ambiguous, refuse it
        let both = UEFI_EXAMPLE.replace(
            r#"mode = "uefi""#,
            "mode = \"uefi\"\nkernel = \"artifacts/bootstrap/vmlinuz\"",
        );
        assert!(matches!(
            VmConfig::from_toml(&both),
            Err(ConfigError::Invalid(_))
        ));
        // direct-linux with a firmware key
        let stray = BACKLOG_EXAMPLE.replace(
            r#"mode = "direct-linux""#,
            "mode = \"direct-linux\"\nfirmware = \"CLOUDHV.fd\"",
        );
        assert!(matches!(
            VmConfig::from_toml(&stray),
            Err(ConfigError::Invalid(_))
        ));
        // direct-linux without a kernel
        let no_kernel = BACKLOG_EXAMPLE.replace(r#"kernel = "artifacts/bootstrap/vmlinuz""#, "");
        assert!(matches!(
            VmConfig::from_toml(&no_kernel),
            Err(ConfigError::Invalid(_))
        ));
    }

    /// A UEFI profile has no `kernel`; the serializer must not choke on the
    /// `None` (bare `Option` in a TOML table is an error without `skip`).
    #[test]
    fn uefi_profile_round_trips_through_toml() {
        let cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !text.contains("kernel"),
            "unexpected kernel key in:\n{text}"
        );
        assert_eq!(VmConfig::from_toml(&text).unwrap(), cfg);
    }

    /// UEFI-1803: the ISO boot profile — firmware, the pci transport, a writable
    /// target as `/dev/vda` and the installer ISO read-only as `/dev/vdb`.
    const UBUNTU_ISO_EXAMPLE: &str = r#"
name = "ubuntu-uefi"
memory_mib = 2560
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[[disk]]
path = "/home/you/entangled-vms/ubuntu.raw"
writable = true

[[disk]]
path = "/home/you/.cache/entangled/ubuntu/26.04/ubuntu-26.04-live-server-amd64.iso"
writable = false
"#;

    #[test]
    fn parses_the_ubuntu_iso_profile() {
        let cfg = VmConfig::from_toml(UBUNTU_ISO_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert!(cfg.transport.is_pci());
        // Disk order is device order: 00:01.0 is /dev/vda, 00:02.0 is /dev/vdb.
        assert_eq!(cfg.disks.len(), 2);
        assert!(cfg.disks[0].writable, "the install target must be writable");
        assert!(
            !cfg.disks[1].writable,
            "the installer ISO must be attached read-only"
        );
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );
    }

    /// `writable` defaults to false, so an ISO section that simply omits the key
    /// is read-only rather than accidentally writable. This is the direction a
    /// default must fail in.
    #[test]
    fn a_disk_without_writable_is_read_only() {
        let cfg = VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("writable = false", "# no writable key here"),
        )
        .unwrap();
        assert!(!cfg.disks[1].writable);
    }

    /// A UEFI profile with disks on virtio-mmio describes a machine whose
    /// firmware cannot see its own boot media (ADR-0003: CloudHv ships no
    /// virtio-MMIO driver). Refused at parse time, because the symptom — "No
    /// bootable option or device was found" — looks like a media problem.
    #[test]
    fn uefi_with_disks_requires_the_pci_transport() {
        let mmio = UBUNTU_ISO_EXAMPLE.replace("transport = \"pci\"", "");
        let error = VmConfig::from_toml(&mmio).expect_err("mmio + uefi + disks must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("transport = \"pci\""), "{message}");

        // Explicit mmio is refused the same way as the default.
        assert!(matches!(
            VmConfig::from_toml(&UBUNTU_ISO_EXAMPLE.replace("\"pci\"", "\"mmio\"")),
            Err(ConfigError::Invalid(_))
        ));

        // But a firmware-only profile — no disks at all, which is how the
        // firmware bring-up boots to the Boot Manager (examples/uefi-firmware.toml)
        // — stays valid on the default transport: there is no media for the
        // missing driver to miss.
        let cfg = VmConfig::from_toml(
            r#"
name = "uefi-firmware"
memory_mib = 2048
vcpus = 1

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"
"#,
        )
        .expect("a diskless uefi profile is valid on mmio");
        assert!(cfg.disks.is_empty());
        assert_eq!(cfg.transport, VirtioTransport::Mmio);
    }

    /// Guests above 3 GiB are legal since the high-RAM split (the machine
    /// continues RAM at 4 GiB); the ceiling is a sanity bound at 64 GiB, and it
    /// must stay a typed config error, not a panic three crates away.
    #[test]
    fn memory_bounds_allow_the_high_ram_split_and_stop_at_the_sanity_cap() {
        assert_eq!(MAX_MEMORY_MIB, 65536, "64 GiB sanity bound");
        // The GNOME-desktop-sized guest that motivated the split.
        assert!(VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 4096")
        )
        .is_ok());
        // Exactly at the bound is fine; one MiB over is not.
        assert!(VmConfig::from_toml(
            &UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 65536")
        )
        .is_ok());
        let too_big = UBUNTU_ISO_EXAMPLE.replace("memory_mib = 2560", "memory_mib = 65537");
        let error = VmConfig::from_toml(&too_big).expect_err("65 GiB must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("supported range"), "{message}");
    }

    /// The cdrom section: a UEFI profile with `[cdrom]` and no disks at all is
    /// the generic "boot this ISO" machine (`entangled run --cdrom`).
    const CDROM_EXAMPLE: &str = r#"
name = "iso-boot"
memory_mib = 2560
vcpus = 2
transport = "pci"

[boot]
mode = "uefi"
firmware = "artifacts/firmware/CLOUDHV.fd"

[cdrom]
path = "/home/you/.cache/entangled/ubuntu/26.04/ubuntu-26.04-desktop-amd64.iso"
"#;

    #[test]
    fn a_cdrom_profile_parses_and_round_trips() {
        let cfg = VmConfig::from_toml(CDROM_EXAMPLE).unwrap();
        assert_eq!(cfg.boot.mode, BootMode::Uefi);
        assert!(cfg.disks.is_empty());
        let cdrom = cfg.cdrom.as_ref().expect("cdrom section");
        assert!(cdrom.path.to_string_lossy().ends_with(".iso"));
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );
        // And a profile without one serializes without the key.
        let plain = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        assert!(plain.cdrom.is_none());
        let text = toml::to_string_pretty(&plain).unwrap();
        assert!(!text.contains("cdrom"), "stray cdrom key in:\n{text}");
    }

    /// There is no `writable` key to get wrong: a cdrom section that tries to
    /// name one is refused at parse time (`deny_unknown_fields`).
    #[test]
    fn a_cdrom_cannot_be_made_writable() {
        let with_writable = CDROM_EXAMPLE.replace("[cdrom]", "[cdrom]\nwritable = true");
        assert!(matches!(
            VmConfig::from_toml(&with_writable),
            Err(ConfigError::Parse(_))
        ));
    }

    /// The same transport rule the disks obey: firmware cannot see virtio-mmio,
    /// so a cdrom on the default transport would boot to "no bootable option".
    #[test]
    fn a_cdrom_requires_uefi_and_the_pci_transport() {
        let mmio = CDROM_EXAMPLE.replace("transport = \"pci\"", "");
        let error = VmConfig::from_toml(&mmio).expect_err("mmio + cdrom must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("transport = \"pci\""), "{message}");
        assert!(message.contains("cdrom"), "{message}");

        // On direct-linux the section is a category error, and the message says
        // what to use instead.
        let direct =
            BACKLOG_EXAMPLE.replace("[network]", "[cdrom]\npath = \"/isos/x.iso\"\n\n[network]");
        let error = VmConfig::from_toml(&direct).expect_err("cdrom on direct-linux");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("uefi"), "{message}");
        assert!(message.contains("[[disk]]"), "{message}");
    }

    /// `set_cdrom` is the `--cdrom <iso>` path: it must re-validate, so a flag
    /// added to a profile the combination rules refuse fails like the profile
    /// would, not at boot time.
    #[test]
    fn set_cdrom_revalidates_the_modified_profile() {
        let mut cfg = VmConfig::from_toml(UEFI_EXAMPLE).unwrap();
        cfg.set_cdrom(PathBuf::from("/isos/x.iso")).unwrap();
        assert_eq!(
            cfg.cdrom.as_ref().unwrap().path,
            PathBuf::from("/isos/x.iso")
        );
        // Replacing an existing cdrom is allowed — the flag wins.
        cfg.set_cdrom(PathBuf::from("/isos/y.iso")).unwrap();
        assert_eq!(
            cfg.cdrom.as_ref().unwrap().path,
            PathBuf::from("/isos/y.iso")
        );

        let mut direct = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        let error = direct
            .set_cdrom(PathBuf::from("/isos/x.iso"))
            .expect_err("cdrom on a direct-linux profile");
        assert!(matches!(error, ConfigError::Invalid(_)), "{error}");
    }

    /// UEFI-1804: the profile `entangled install ubuntu` writes names an NVRAM
    /// file, and that key must round-trip and stay uefi-only.
    #[test]
    fn the_nvram_key_is_uefi_only_and_round_trips() {
        let with_nvram = UBUNTU_ISO_EXAMPLE.replace(
            r#"firmware = "artifacts/firmware/CLOUDHV.fd""#,
            "firmware = \"artifacts/firmware/CLOUDHV.fd\"\nnvram = \"/vms/ubuntu.nvram\"",
        );
        let cfg = VmConfig::from_toml(&with_nvram).unwrap();
        assert_eq!(cfg.boot.nvram, Some(PathBuf::from("/vms/ubuntu.nvram")));
        assert_eq!(
            VmConfig::from_toml(&toml::to_string_pretty(&cfg).unwrap()).unwrap(),
            cfg
        );

        // Absent is fine — that is a firmware boot with RAM-only variables.
        assert_eq!(
            VmConfig::from_toml(UBUNTU_ISO_EXAMPLE).unwrap().boot.nvram,
            None
        );

        // On direct-linux it is a mistake worth naming: nothing would ever read
        // the file, so a profile that names one is not describing what it thinks.
        let stray = BACKLOG_EXAMPLE.replace(
            r#"mode = "direct-linux""#,
            "mode = \"direct-linux\"\nnvram = \"/vms/x.nvram\"",
        );
        let error = VmConfig::from_toml(&stray).expect_err("nvram on direct-linux must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("boot.nvram"), "{message}");
    }

    /// WHP-1704: the user-mode NAT backend is a first-class config choice, and
    /// the section round-trips without an interface key.
    #[test]
    fn usernet_is_a_backend_and_needs_no_interface() {
        let usernet = BACKLOG_EXAMPLE.replace(
            "backend = \"tap\"\ninterface = \"entangled0\"",
            "backend = \"usernet\"",
        );
        let cfg = VmConfig::from_toml(&usernet).unwrap();
        let network = cfg.network.as_ref().unwrap();
        assert_eq!(network.backend, NetworkBackend::Usernet);
        assert_eq!(network.interface, None);
        assert!(network.require_interface().is_err());
        assert_eq!(network.backend.to_string(), "usernet");
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(!text.contains("interface"), "no interface key in:\n{text}");
        assert_eq!(VmConfig::from_toml(&text).unwrap(), cfg);
    }

    /// The wrong network key for the backend is refused, in both directions —
    /// same policy as the boot section's per-mode keys.
    #[test]
    fn network_keys_must_match_the_backend() {
        // usernet with a TAP interface: the author configured something the
        // backend would silently ignore.
        let stray = BACKLOG_EXAMPLE.replace("backend = \"tap\"", "backend = \"usernet\"");
        let error = VmConfig::from_toml(&stray).expect_err("usernet + interface must be refused");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("usernet"), "{message}");

        // tap without an interface: nothing to open.
        let missing = BACKLOG_EXAMPLE.replace("interface = \"entangled0\"", "");
        let error = VmConfig::from_toml(&missing).expect_err("tap without interface");
        let ConfigError::Invalid(message) = error else {
            panic!("expected a validation error, got {error:?}");
        };
        assert!(message.contains("network.interface"), "{message}");

        // The tap example still parses and still names its interface.
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(
            cfg.network.as_ref().unwrap().require_interface().unwrap(),
            "entangled0"
        );
    }

    #[test]
    fn rejects_nonsense() {
        assert!(VmConfig::from_toml("name = 3").is_err());
        let zero_mem = BACKLOG_EXAMPLE.replace("memory_mib = 2048", "memory_mib = 1");
        assert!(matches!(
            VmConfig::from_toml(&zero_mem),
            Err(ConfigError::Invalid(_))
        ));
        let typo = BACKLOG_EXAMPLE.replace("[display]", "[dispaly]");
        assert!(matches!(
            VmConfig::from_toml(&typo),
            Err(ConfigError::Parse(_))
        ));
    }

    /// A new machine's `[network]`, for each backend: usernet drops the
    /// interface (validation would refuse it), TAP keeps it, both carry a MAC
    /// — and each survives a TOML round trip exactly, as the installer writes
    /// and `run` reads it.
    #[test]
    fn a_new_machines_network_round_trips_for_each_backend() {
        let disk = Path::new("vms").join("work.raw");
        for (backend, interface) in [
            (NetworkBackend::Usernet, None),
            (NetworkBackend::Tap, Some("entangled0")),
        ] {
            let section = NetworkSection::for_new_machine(
                backend,
                Some("entangled0".to_string()),
                "work",
                &disk,
            );
            assert_eq!(section.backend, backend);
            assert_eq!(section.interface.as_deref(), interface, "{backend}");
            let mac = section.mac.clone().expect("a new machine has a MAC");
            assert_eq!(mac, new_machine_mac("work", &disk));

            let mut cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
            cfg.network = Some(section);
            let text = toml::to_string_pretty(&cfg).unwrap();
            assert!(text.contains(&format!("backend = \"{backend}\"")), "{text}");
            assert!(text.contains(&format!("mac = \"{mac}\"")), "{text}");
            assert_eq!(
                text.contains("interface ="),
                interface.is_some(),
                "{backend}: {text}"
            );
            assert_eq!(VmConfig::from_toml(&text).unwrap(), cfg, "{text}");
        }
        assert_eq!(DEFAULT_NEW_MACHINE_NETWORK, NetworkBackend::Usernet);
    }

    /// The MAC is stable (same machine, same address, every time), unique
    /// per machine (another name *or* another disk is another address), and
    /// always an address a NIC may have: locally administered, unicast.
    #[test]
    fn a_new_machines_mac_is_stable_unique_and_valid() {
        let disk = |dir: &str, file: &str| Path::new(dir).join(file);
        let a = new_machine_mac("ubuntu", &disk("vms", "ubuntu.raw"));
        assert_eq!(a, new_machine_mac("ubuntu", &disk("vms", "ubuntu.raw")));
        let mut seen = std::collections::HashSet::new();
        for (name, dir, file) in [
            ("ubuntu", "vms", "ubuntu.raw"),
            // Same name, another VM directory: two machines.
            ("ubuntu", "other", "ubuntu.raw"),
            ("ubuntu-2", "vms", "ubuntu.raw"),
            ("debian", "vms", "debian.raw"),
            ("gpu-desktop", "vms", "gpu-desktop.raw"),
        ] {
            let mac = new_machine_mac(name, &disk(dir, file));
            assert!(seen.insert(mac.clone()), "{name} in {dir}: {mac} repeats");
            let octets = parse_mac(&mac).expect("a valid MAC");
            assert_eq!(octets[0], 0x52, "{mac}: locally administered, unicast");
            assert_eq!(format_mac(octets), mac);
        }
        // A thousand machines in one directory, no two alike.
        let many: std::collections::HashSet<String> = (0..1000)
            .map(|i| new_machine_mac(&format!("vm{i}"), &disk("vms", &format!("vm{i}.raw"))))
            .collect();
        assert_eq!(many.len(), 1000);
    }

    /// A profile's MAC is checked at load: six hex octets, unicast, not zero.
    /// Upper case and an unpadded octet are fine; they are still an address.
    #[test]
    fn a_profiles_mac_is_validated_at_load() {
        let with_mac = |mac: &str| {
            BACKLOG_EXAMPLE.replace(
                "interface = \"entangled0\"",
                &format!("interface = \"entangled0\"\nmac = \"{mac}\""),
            )
        };
        for good in ["52:54:00:12:34:56", "52:54:00:AB:cd:1", "02:00:00:00:00:01"] {
            let cfg = VmConfig::from_toml(&with_mac(good)).expect(good);
            assert!(cfg.network.unwrap().mac_octets().unwrap().is_some());
        }
        for (bad, why) in [
            ("52:54:00:12:34", "six octets"),
            ("52:54:00:12:34:56:78", "six octets"),
            ("52:54:00:12:34:zz", "hexadecimal"),
            ("52:54:00:12:34:123", "hexadecimal"),
            ("52-54-00-12-34-56", "six octets"),
            ("53:54:00:12:34:56", "multicast"),
            ("ff:ff:ff:ff:ff:ff", "multicast"),
            ("00:00:00:00:00:00", "zeroes"),
        ] {
            let error = VmConfig::from_toml(&with_mac(bad)).expect_err(bad);
            let ConfigError::Invalid(message) = error else {
                panic!("{bad}: expected a validation error, got {error:?}");
            };
            assert!(message.contains(why), "{bad}: {message}");
            assert!(message.contains("network.mac"), "{bad}: {message}");
        }
        // No MAC at all is still a valid profile: `run` derives one from the
        // name, as it always has.
        let cfg = VmConfig::from_toml(BACKLOG_EXAMPLE).unwrap();
        assert_eq!(cfg.network.unwrap().mac_octets().unwrap(), None);
    }
}
