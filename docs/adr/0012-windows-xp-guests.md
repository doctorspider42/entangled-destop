# ADR-0012: Windows XP guests — the legacy PC this machine never was

Status: **proposed** (2026-11-11); nothing implemented, nothing booted.
Extends ADR-0008 (Windows guests), which reached Windows 10, 11 and 7 through
UEFI, virtio and MSI. Branch: `winxp-guests`, cut from `windows-guests`.

## Objective

`entangled run` boots the untouched Windows XP Professional SP3 x86 CD
(`en_windows_xp_professional_with_service_pack_3_x86_cd_vl_x14-73974.iso`,
supplied by the user), installs it to a disk, and boots the installed system
to a desktop in the window with keyboard, mouse, disk and network, on the
Windows/WHP host first and on Linux/KVM after. 2D only.

## Context: why ADR-0008 does not carry over

ADR-0008's machine is UEFI firmware, virtio devices with Microsoft- or Red
Hat-signed drivers, and MSI for every interrupt. Windows XP x86 can use none
of the three:

| ADR-0008 relies on | Windows XP x86 |
|---|---|
| UEFI (`bootmgfw.efi`, GOP framebuffer) | Has no EFI loader. The CD boots through El Torito no-emulation (`SETUPLDR.BIN`, 4 sectors at LBA 345 on this ISO, load segment `0x7c0`), and `NTLDR` lives on `int 10h`/`13h`/`15h`/`16h` |
| AHCI + ATAPI for the installation media, `viostor` for the disk | No in-box AHCI driver; a text-mode Setup driver needs a floppy (F6). In-box: PCI IDE (`pciide`/`atapi`) |
| MSI / MSI-X on every PCI function (the DSDT has no `_PRT`) | No MSI at all (it arrived with Vista). Every PCI function interrupts through INTx, level-triggered, routed by `_PRT` or the PIRQ table |
| virtio-gpu (`viogpudo`, our WDDM driver) | No virtio-gpu driver exists for XP's display model (XPDM). In-box: `vga.sys`, which drives real VGA registers and VBE through `int 10h` |
| virtio-input | No driver. In-box: `i8042prt` — PS/2 keyboard **and mouse** |
| virtio-net (`netkvm`) | Old virtio-win releases carry an XP `netkvm` for the *legacy* virtio-pci interface over INTx, which our modern-only transport does not offer. In-box: RTL8139, AMD PCnet |
| HD Audio (`hda`, in-box since Vista) | In-box: AC'97, ES1371 |
| Hyper-V reference clock | Ignored; XP's clock is the PIT or the RTC, its counter the PM timer |

What does carry over: guest memory, the vCPU run loops, the userspace
8259/8254/IOAPIC (`machine_x86::irqchip`), the RTC with its periodic
interrupt, the ACPI tables and PM block, the 8042 keyboard, `ignore_msrs`,
the PCI root bus, the ATAPI command set in `crates/ahci`, the VRAM
`ShmWindow` and its presenter, the user-mode NAT, and the whole lifecycle
(reset, quiesce, snapshot).

## What was found in the code (2026-11-11)

- `BootMode` is `DirectLinux | Uefi`. There is no path that enters a
  firmware at the reset vector in real mode *and* gives it BIOS services.
  The reset-vector state itself exists (`vmm_core::vcpu`, `uefi_boot::rom`).
- **`WHvEmulatorTryMmioEmulation` refuses 16-bit real mode**
  (`crates/vmm-core/tests/whp_smoke.rs`, "internal emulation failure"). Port
  I/O from real mode works (every smoke guest uses it). So on WHP, real-mode
  code cannot touch trapped memory: a BIOS must do its MMIO in 32-bit mode,
  and the VGA text buffer at `0xb8000` has to be ordinary RAM read by the
  presenter rather than a trap.
- `0xa0000–0xfffff` is laid out for EDK2 (ACPI tables at `0xe0000`, the MP
  table at `0xf0000`, PVH hand-off at `0xd0000`). A BIOS owns
  `0xc0000–0xfffff` (option ROMs, its own image, the tables it copies).
- There is no level-triggered INTx anywhere: `machine_x86::pci` mirrors
  `INTX_DISABLE` but nothing raises a pin, and ADR-0008 says so twice.
- `machine_x86::i8042` has a keyboard and a mouse port that is "stuck".
- No VGA registers are emulated: `framebuffer.rs` *absorbs* `0x3c0–0x3df`.

## Decision

A second x86 machine profile, `[machine] chipset = "legacy"` (name open),
selected by a third boot mode `mode = "bios"`. The default machine stays
byte-identical; a test pins it, as ADR-0008 did for the Linux CPUID policy.
Each piece below is its own change with a test that fails before it.

### 1. A BIOS (the blocker, and the open question)

Three ways to get `int 10h/13h/15h/16h/1Ah`, El Torito and an E820 map:

- **(a) SeaBIOS + SeaVGABIOS**, built from pinned sources and fetched with a
  pinned digest like `CLOUDHV.fd`. LGPLv3. It runs *in the guest*; nothing
  links it, and CLAUDE.md's copyleft rule covers host code. But a release
  would carry (or point at) an LGPL binary, which is a licensing choice
  only the project owner makes. It expects a machine description from the
  host: the `fw_cfg` port interface (`0x510/0x511` + DMA) for the E820 map,
  the ACPI tables and the boot order. That interface is a published
  de-facto standard like Bochs DISPI; no QEMU code is involved. It does its
  hardware work in 32-bit mode, which suits the WHP limit above. It also
  already boots from IDE, AHCI and virtio-blk.
- **(b) A BIOS of our own** (Rust `no_std` + a real-mode stub). Clean
  licence, exactly the services XP uses, and it can read our tables
  directly. Cost: weeks before `SETUPLDR` prints anything, and every
  `int 13h`/`int 10h` corner XP touches is found by bugcheck.
- **(c) EDK2 with a CSM.** Upstream removed the CSM, and a CSM needs a
  16-bit core, which is SeaBIOS again. Not an option.

Recommendation: **(a)** to reach a desktop and learn what XP needs, keeping
(b) possible later because the host side (`fw_cfg`, the devices) is the
same for both. **This needs the owner's yes before any binary is fetched.**

### 2. Real VGA

One PCI function of class `0300` with: the sequencer, CRTC, graphics
controller, attribute controller and DAC at `0x3c0–0x3df`; text mode 3
rendered from `0xb8000` with the 8×16 font plane; planar modes (`12h`,
640×480×16, which `bootvid` and GUI-mode Setup draw in) with write modes
0–3 behind a trap on `0xa0000–0xaffff`; and the Bochs DISPI registers the
machine already has, so SeaVGABIOS (or our own `int 10h`) offers VBE linear
modes and `vga.sys` gets true colour after the install. VRAM stays the
existing `ShmWindow`; the presenter gains two more sources (text, planar).

### 3. PCI IDE

A PIIX3-shaped function (`8086:7010`, legacy ports `0x1f0/0x3f6`,
`0x170/0x376`, IRQ 14/15, bus-master DMA in BAR 4): an ATA disk over the
existing RAW backend and an ATAPI drive reusing `crates/ahci`'s MMC set
(the packet layer is transport-independent; the FIS/PRDT half is not, and
that split is the first refactor). IRQ 14/15 are ISA, edge-triggered, so
this does not wait for §4.

### 4. Level-triggered INTx with a `_PRT`

The IOAPIC and PIC get level semantics with EOI re-evaluation; the bus
gets four PIRQ lines; the DSDT gets `_PRT` (and the `$PIR` table a non-ACPI
HAL wants). On KVM this is the irqfd-with-resample path. Needed by the NIC
and sound, and it also repairs what ADR-0008 worked around with forced MSI.

### 5. The rest of the legacy set

- A PS/2 mouse on the 8042's second port (IRQ 12). Relative only: the
  window grabs the pointer. An absolute pointer needs a guest driver, later.
- An in-box NIC: RTL8139 (`10ec:8139`) into the existing net backends.
- Sound: AC'97 or ES1371 into `AudioSink`. Last.
- ACPI as XP reads it: the tables are ACPI 2.0+ with a 64-bit XSDT, which XP
  accepts, but its 2001 interpreter is stricter about AML than anything
  tested so far. Expect `0xA5` stops, read as ADR-0008 read them.
- CPU: a 32-bit guest needs nothing new from WHP on AMD. Intel hosts need
  unrestricted guest for real mode (every VT-x CPU WHP supports has it).
  On a current CPU XP wants `ignore_msrs`, as Windows 10 did.

### 6. What every new device owes

VGA, IDE, `fw_cfg`, the mouse, the NIC: each a `reset()`, a `Quiesce` gate
where a host thread touches guest memory (the IDE DMA worker, the
presenter), and a save/load pair or a named snapshot refusal until it has
one (ADR-0005, ADR-0006). All guest-supplied lengths, PRDs and register
indexes are bounded and go through `vm-memory`.

## Phases

| Phase | Contents | Acceptance (WHP first) |
|---|---|---|
| **X0 — the BIOS speaks** | `mode = "bios"`, the legacy memory layout, `fw_cfg`, the BIOS decision of §1 | the BIOS banner and "no bootable device" on ttyS0 |
| **X1 — a screen and a CD** | VGA text mode, IDE + ATAPI | the XP CD boots to the blue "Windows XP Professional Setup" screen and takes keys |
| **X2 — Setup runs** | ATA disk, bus-master DMA, planar VGA | text-mode Setup formats and copies; GUI-mode Setup draws; the mouse moves |
| **X3 — a desktop** | whatever the first boots stop on (ACPI, MSRs, timers), VBE modes | the installed system reaches the desktop, 10 boots without a stop, shutdown ends the run |
| **X4 — network and sound** | level INTx + `_PRT`, RTL8139, AC'97 | the guest reaches the internet through usernet; a sound plays |
| **X5 — install command** | `install windows` recognising an XP CD: `WINNT.SIF` on a floppy image or a second CD, a product key the user supplies | unattended install from the untouched ISO |

X0–X3 are the decision this ADR asks for.

## Out of scope

- 3D, the WDDM drivers, Venus: XP's driver model predates all of it.
- 64-bit XP, Windows 2000/9x — likely neighbours once X3 holds, untested.
- Activation. The ISO is a volume-licence medium; the key is the user's.
- Guest additions (absolute pointer, resize, clipboard): after X4, and they
  need a 32-bit XP driver or agent of our own.

## Risks, in the order they are expected to bite

1. Real-mode code touching trapped memory on WHP (§ "What was found"). If a
   BIOS or `NTLDR` writes to `0xa0000` from real mode, the answer is a small
   decoder of our own for the handful of `mov`/`stos`/`movs` forms, not a
   different hypervisor API.
2. Planar VGA is an exit per byte written. GUI-mode Setup will be slow; how
   slow is unknown until X2.
3. The legacy layout and the EDK2 layout both want `0xc0000–0xfffff`; the
   split has to be per boot mode without forking `layout.rs` into two files
   nobody can compare.
4. KVM cannot be tested on this machine at all (no `/dev/kvm` in WSL).
