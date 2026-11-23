# ADR-0012: Windows XP guests — the legacy PC this machine never was

Status: **proposed** (2026-11-11); phases X0 and X1 done (2026-11-12): the
untouched XP CD boots through a BIOS of our own into text-mode Setup, which
draws its screens, takes keys and sees the disk. Setup formats
the disk and copies its files; GUI-mode Setup, after the restart, is not
reached yet.
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

A third boot mode, `boot.mode = "bios"`, selects a BIOS-era machine: the
BIOS of §1, the 8259 pair delivering, and ACPI tables shaped for a guest of
that era. The default machine stays byte-identical: everything below is
switched by the boot mode, and tests pin the default tables. Each piece is
its own change with a test that fails before it.

### 1. A BIOS of our own, served by the host

Decided 2026-11-11 by the project owner: no LGPL firmware, so not SeaBIOS
(which would also have needed the `fw_cfg` interface and a pinned binary),
and not EDK2's CSM (removed upstream, and a CSM needs a 16-bit core, which
is SeaBIOS again). The BIOS is `crates/pc-bios`, and it is not written the
way a BIOS usually is:

- **The ROM is a few hundred bytes.** Every interrupt vector points at a
  five-byte stub that pushes its number and jumps to one routine; the
  routine saves the registers on the guest's stack and writes `SS`, then
  `SP`, to two I/O ports (`0x4e0`, `0x4e2`). The machine code is assembled
  in Rust (`pc_bios::rom`), next to the comment that names each byte, so
  there is no 16-bit toolchain in the build.
- **The services run in the host.** On that port write `pc_bios::Bios`
  reads the saved registers out of guest memory, does the service — an
  `int 13h` read is a file read into guest memory, `int 10h` writes two
  bytes at `B800:0000`, `int 15h/E820` returns `machine_x86::e820_map` —
  writes the registers back, and the routine pops them and `iret`s. They
  are ordinary Rust with ordinary unit tests, on both hosts.
- **Nothing in real mode touches a device**, which is what the WHP limit
  above needed: the host writes text memory, and the drives are read from
  their image files with no controller in between.
- The interface between the two halves is guest memory and two ports, so
  it goes through `ExitHandler` as it is: no backend API changed for it.

What it owes a guest is found the way ADR-0008 found things: by running
the guest. `RUST_LOG=pc_bios=trace` prints every call with its registers,
and a service that is not there is reported once (`debug`).

It takes exactly one interrupt, the timer's (below). The keys come from
the 8042, which the machine drains into the BIOS before each call.

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

VGA, IDE, the mouse, the NIC: each a `reset()`, a `Quiesce` gate
where a host thread touches guest memory (the IDE DMA worker, the
presenter), and a save/load pair or a named snapshot refusal until it has
one (ADR-0005, ADR-0006). All guest-supplied lengths, PRDs and register
indexes are bounded and go through `vm-memory`.

## Phases

| Phase | Contents | Acceptance (WHP first) |
|---|---|---|
| **X0 — the BIOS speaks** (done) | `mode = "bios"`, the BIOS of §1, the 8259's interrupt, the legacy ACPI tables | the XP CD's loader runs and starts the XP kernel |
| **X1 — a screen and a CD** (done) | VGA text mode, IDE + ATAPI | the XP CD boots to the blue "Windows XP Professional Setup" screen and takes keys |
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

## Amendment, 2026-11-12: X0 on WHP, and what the first boot took

`boot.mode = "bios"` boots the untouched XP SP3 CD on the Windows/WHP host:
the El Torito loader, `SETUPLDR.BIN` and `NTDETECT.COM` run on the BIOS of
§1, the blue "Windows Setup" screen and "Setup is starting Windows" are
drawn from text memory in the window, about 770 `int 13h` reads load the
kernel and the boot drivers, and the XP kernel starts and runs for a few
seconds. It ends at **stop 0x7B, INACCESSIBLE_BOOT_DEVICE**: there is no
disk controller XP has a driver for, which is phase X1. One vCPU, 512 MiB,
`ignore_msrs = true`; the machine still carries its virtio devices and the
AHCI controller, which XP ignores.

What stood between the first call and that stop, in the order it was found:

1. **The loader waits for a timer tick.** `SETUPLDR` hooks `int 1Ch` and
   spins until the BIOS's timer interrupt calls it, to calibrate its delay
   loop. "A BIOS that takes no interrupts" lasted one evening. The 8259 pair
   was a register model with no delivery (`irqchip::pic`: "No delivery");
   it now resolves priority and answers the acknowledge cycle, and
   `UserspaceIrqChip::enable_legacy_interrupts` wires the 8254 to its IRQ 0.
   It is off for every machine but a BIOS one, so no other guest sees a
   second timer interrupt. On WHP the vector goes in as a pending
   external-interrupt event when the boot CPU can take it, with an
   interrupt-window notification when it cannot
   (`WhpVcpu::offer_ext_interrupt`); two defaulted methods on `ExitHandler`
   and one on `InterruptDelivery` carry it, and KVM, whose in-kernel 8259
   delivers by itself, implements none of them. The ROM's `int 08h` handler
   counts the tick at `0040:006C`, calls `int 1Ch` and ends the interrupt,
   on the interrupted code's stack: the loader's hook rewrites the return
   address the interrupt pushed, so the frame has to be exactly where the
   CPU left it.
2. **XP's loader follows the RSDT.** The tables are XSDT-only; with
   `RsdtAddress` 0 the loader read a table length out of the interrupt
   vectors at physical address 4 and tried to map 3.75 GiB.
   `acpi::write_rsdt` adds an RSDT beside the XSDT.
3. **Stop 0xA5 (2): a root bridge window above 4 GiB.** `PCI0._CRS` named
   the 64-bit aperture, which a 32-bit XP cannot represent. The tables of a
   BIOS machine (`acpi::write_legacy`) leave it out.
4. **Stop 0x7E in `acpi.sys`, a null interrupt-routing interface.** XP
   reserves a device's interrupt when it enumerates it, and its ACPI driver
   asks the PCI driver how to route only once the root bridge has started.
   The keyboard (`PS2K`, IRQ 1) was the bridge's sibling under `\_SB`, so
   it was enumerated first. In the legacy tables it is the bridge's child,
   as on a real board, and the bridge has a hard-wired `_PRT` for the
   bus's INTx lines. (The `_PRT` describes wiring the host does not honour
   yet: level-triggered INTx is still §4.)

Not needed after all, at least not yet: `fw_cfg`, a separate "legacy
chipset" switch (the boot mode selects everything above), and the PM
timer's `TMR_STS` — added while chasing stop 0xA5, kept because it is what
ACPI specifies, and not what XP was waiting for.

**Reading a stop that cannot be seen.** XP draws its blue screen in a VGA
graphics mode this machine does not have, then sits in the HAL polling the
power button. `ENTANGLED_WHP_SAMPLE_PAUSE=1` prints, when the VM is paused,
the vCPU's registers, the code at its RIP and — for a Windows kernel,
32-bit ones included now — the stop code and parameters from
`KiBugCheckData`, with the memory the parameters point at. The full exit
trace is no substitute here: it slows this guest enough that the loader
never reaches the kernel. The faulting driver was named by searching the
CD's own driver files for the code bytes at the fault address.

Not done: KVM (the legacy tables, the BIOS and the boot plan build there
and the in-kernel 8259 needs nothing, but no BIOS guest has run on it); a
snapshot of a BIOS machine (refused by name); `int 10h` beyond text mode;
`int 13h` for floppies; the timer tick's midnight rollover.

## Amendment, 2026-11-12: X1 — text-mode Setup, on an IDE controller and a VGA

The same profile now reaches "Welcome to Setup." about a minute after
start, in XP's own font; Enter shows the licence, F8 accepts it, and the
partition screen lists "8190 MB Disk 0 at Id 0 on bus 0 on atapi [MBR]".
What that took:

**An IDE controller** (`crates/ide`, `machine_x86::ide_function`). Two
channels at the fixed ISA ports on IRQ 14 and 15, the profile's `[[disk]]`s
on the primary and its `[[optical]]` drives on the secondary, two of each
at most. ATA disks are new; the ATAPI drive is `ahci::Atapi`, whose packet
command set the controller reaches through the ATA PACKET protocol instead
of an AHCI FIS, so §3's "split the packet layer" needed no change to that
crate. A BIOS machine gets this controller *instead of* virtio-blk and the
AHCI controller: its guest has a driver for neither, and one image must not
sit behind two controllers.

- **PIO only**, on the vCPU thread: no bus master, so no thread of its own
  and no `Quiesce` gate to take. The identify data offers no DMA mode and a
  command that asks for one is aborted. §3's bus-master DMA is still owed,
  and is what phase X2's file copy will want.
- The PCI function is an identity only (class `0101`, compatibility mode,
  no BARs, no interrupt pin), with a vendor id no chipset driver claims, so
  Windows binds its standard dual-channel driver by class. An Intel PIIX id
  would have brought `intelide.sys` and its timing registers.
- **The root bridge has to own the ports.** The first run with the
  controller still ended in 0x7B, and the exit statistics said why without
  a debugger: XP never touched `0x1f0`. `PCI0._CRS` named only the
  configuration ports and the VGA range, and XP gives a PCI function no
  port its bridge does not pass down. The legacy tables now give the bridge
  the whole I/O space on both sides of `0xcf8`, as a PC's do.

**A VGA register file** (`machine_x86::vga`). XP's `vga.sys` probes the
graphics controller, programs every register, loads a font and moves the
cursor through the CRT controller. The registers at `0x3c0..0x3df` are
modelled; display memory is not. Text mode is drawn from what the registers
say — geometry, cursor, the palette through the attribute controller and
the DAC — over the character cells at `B800:0000`, which are plain guest
RAM, and with the font the guest loaded, which lands in that same RAM at
`A000:0000` while the sequencer selects plane 2 and is read back from
there. That is honest for text and wrong for anything planar: the boot
screen and GUI-mode Setup need the four planes and the write modes behind a
trap (X2), and until then a graphics mode leaves the last text frame up.
The presenter (`bios_text_present`) draws the BIOS's screen until the guest
programs the CRT controller and the VGA's after.

Keys reach Setup through the 8042 and XP's `i8042prt`, on IOAPIC pin 1: by
then the HAL has masked the 8259s, and the BIOS no longer drains the
controller because nothing calls the BIOS.

**Further than X1 asked.** Driven over the control channel (Enter, F8,
Enter, a format choice), Setup partitioned the 8 GiB disk, formatted it as
NTFS — a full format, about five minutes — and copied its files: 95 % of
the copy was done when the fifteen-minute run was stopped by its own
timeout, at 1 500–1 800 port exits a second. So the text-mode half of X2
works on PIO as it is. The restart into GUI-mode Setup has not been tried,
and it will stop at the VGA: that is where the planar modes begin.

Not done: KVM (builds; IRQ 14/15 are irqfds there; nothing run); bus-master
DMA; a snapshot of a BIOS machine; the PCI identity of the VGA (XP's
`VgaSave` finds it at the legacy ports; a PnP display driver will want a
class-`0300` function); `[[disk]]`/`[[optical]]` beyond two each.

## Amendment, 2026-11-12: past text mode — what GUI-mode Setup and an unattended install took

Everything here was measured on WHP with the untouched XP Professional SP3
volume-licence CD. KVM builds and passes its unit tests; nothing below has
run on it.

**Planar VGA (X2).** GUI-mode Setup draws in mode 12h, sixteen colours in
four bit planes, and reads its own pixels back. `machine_x86::vga_planes`
is that memory — write modes 0 to 3, read modes 0 and 1, chain-4 and
odd/even — behind a new `vmm_core::hv::RamTrap`, which takes a sub-range of
guest RAM out of the hypervisor's map so that every access to
`0xA0000..0xBFFFF` exits. One thing it cost: WHP's instruction emulator
wrote "RAM" directly, trap or no trap, and Setup's screen stayed black
until `ExitHandler::ram_is_trapped` sent those accesses to the device. KVM
offers no trap yet. The throughput is what it sounds like — eighteen to
twenty-two thousand exits a second while Setup paints — and it is the
reason GUI-mode Setup takes about forty minutes here.

**The BIOS sets standard modes.** NTLDR asks `int 10h AX=0012h` for the
boot menu and the progress bar; the BIOS now carries register tables for
modes 03h, 12h and 13h and programs the VGA with them.

**VBE over the boot framebuffer.** `int 10h AH=4Fh` (4F00–4F03, modes from
0x140, 32 bpp, linear) is answered from the Bochs-DISPI framebuffer device,
whose VRAM a BIOS machine places below 4 GiB (`low_vram_base`) because
XP's `_CRS` has no 64-bit window. XP calls the BIOS from virtual-8086 mode
with paging on, so a BIOS call's memory accesses walk the guest's page
tables (`pc_bios::Paging`).

**PS/2 mouse.** The i8042 has its second port: a mouse on IRQ 12 (`PS2M`,
`PNP0F13`), fed relative motion converted from the host's absolute pointer.

**A floppy, for the answers.** `boot.floppy` is a 1.44 MB image the BIOS
serves as drive 0; `entangled install windows` on an XP ISO builds a FAT12
one holding `WINNT.SIF`, and text-mode Setup then asks nothing. The floppy
holds the product key and is deleted when the install ends.

**No virtio on a BIOS machine.** XP has no driver for any of it, and each
function was a "Found New Hardware" wizard waiting for a click nobody is
there to make. A BIOS machine gets the IDE controller, the VGA, the boot
framebuffer and the network card below — nothing else on the bus.

**RTL8139 (X4).** `crates/rtl8139` is the card XP has an in-box driver
for, on the `NetBackend` virtio-net already sends through, with its
registers in an I/O BAR (`ConfigSpace::with_io_bar`). Its `INTA#` is a
level and this machine's lines are edges, so the card raises an edge
whenever a cause becomes pending and an `EoiListener` relay raises one
more when the guest ends an interrupt with a cause still set.

**What an install with a made-up key proved.** With an answer floppy and
a placeholder key, Setup ran from the CD's boot sector to the "Your
Product Key" page of GUI mode with no input: text mode in about fifteen
minutes, then device installation. The page is where it should stop —
volume-licence media has no keyless path.

**Still open:** sound; bus-master DMA (the disk is PIO, and it shows);
a RAM trap and INTx resampling on KVM; snapshots of a BIOS machine, which
`entangled` refuses; a PCI identity for the VGA.

## Amendment, 2026-11-13: an installed XP, and the three things its desktop took

With a real volume-licence key, `entangled install windows` on the XP CD
ran from the boot sector to a powered-off, installed system in 51 minutes
with no input, and the system it left boots to a desktop with a network.
Measured on WHP; KVM still only builds.

**VBE carried GUI-mode Setup.** With no virtio functions on the bus and
VRAM below 4 GiB, XP's `vga.sys` took the BIOS's VBE modes and GUI-mode
Setup ran on the linear framebuffer, in colour, instead of through the
planar memory trap.

**ACPI power-off works.** Setup's own shutdown, and `shutdown -s` from
the desktop, end the run through S5.

**A BIOS enables PCI devices; ours did not, and XP noticed.** The first
desktop had three devices that would not start — our display adapter and
the RTL8139 with code 10 — and nothing wrong with either driver. A PC
BIOS's POST switches on I/O and memory decoding on every PCI function
and hands the system a live bus. This machine handed XP functions whose
command registers were zero. XP's PCI driver then treated their BAR
assignments as nobody's, moved every BAR to the top of the bridge's
window, and started the display driver with **bus-mastering on and memory
decoding still off** (command `0x0004`): the miniport mapped its register
page and read zeroes. The fix is the BIOS's job done at the bus:
`PciRoot::decode_from_power_on` makes every function of a BIOS machine
come out of reset — every reset — with I/O, memory and bus-master
enabled, at the address the host placed it. XP now keeps the BARs where
they are. (A UEFI machine is unchanged: its firmware does this itself,
and a function that decodes before firmware has placed it would be the
bug ADR-0005 warns about.)

**The RTL8139's EEPROM auto-load has to finish.** XP's in-box driver
writes `0x40` to `9346CR` — "reload the configuration from the EEPROM" —
and polls until the card clears the mode bits by itself. The model kept
them set; the driver read the register twenty thousand times and gave
up. The load is instant now. With that, DHCP, ping and DNS work from the
desktop through user-mode networking.

**Processors, the old way.** XP predates the `ACPI0007` processor device
and listed each as an unknown device; a BIOS machine's DSDT now declares
`Processor` objects under `\_PR`.

**How it was found.** Three devices failing at once pointed at the
machine, not a driver; `wmic path Win32_PnPEntity where
"ConfigManagerErrorCode<>0"` gave the codes; a debug console at port
`0xE9` (a byte at a time, lines into the host's log) let the miniport say
what it saw before it had mapped anything; and `machine_x86::pci=debug`
logs every configuration write, which is where command `0x0004` showed.

**Still open:** a root-enumerated COM1 left by Setup's hardware detection
conflicts with the ACPI one (code 12, harmless); sound; bus-master DMA;
KVM; snapshots; the PS/2 mouse has a cursor but has not been driven from
a host window in this work.
