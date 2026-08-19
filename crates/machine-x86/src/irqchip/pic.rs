//! A userspace 8259A PIC pair, enough that Linux's probe does not wedge
//! (backlog WHP-1703).
//!
//! # Why this exists at all when nothing is wired to it
//!
//! This machine routes every interrupt through the IOAPIC: the MADT sets
//! `PCAT_COMPAT` (a dual-8259 is present) and overrides ISA IRQ 0 onto GSI 2, and
//! Linux with a MADT programs the IOAPIC and leaves the 8259s masked. So no
//! device's line reaches the PIC and the PIC's `INTR` output is wired to nothing.
//!
//! What does matter is `probe_8259A()`, which runs regardless:
//!
//! ```text
//! outb(0xff, 0xa1);          // mask all of the slave
//! outb(0xfb, 0x21);          // mask all of the master except the cascade
//! new_val = inb(0x21);
//! if (new_val != 0xfb) { pr_info("Using NULL legacy PIC"); legacy_pic = &null_legacy_pic; }
//! ```
//!
//! An unclaimed ISA port floats high, so with no PIC at all `inb(0x21)` reads
//! `0xff`, Linux installs `null_legacy_pic`, `nr_legacy_irqs()` becomes 0 and the
//! machine silently changes shape: `check_timer()` is skipped, the PIT clockevent
//! is not registered and the guest ends up depending entirely on the local APIC
//! timer. That may well boot, but it is a *different* machine from the one the
//! MADT describes and from the one the KVM backend presents, and the divergence
//! would only show up as a timekeeping bug much later. So: claim the ports,
//! answer the probe, keep the two backends' guests identical.
//!
//! # Model
//!
//! Both chips' initialisation sequence (ICW1..ICW4) and operation command words,
//! the interrupt mask register, and the `OCW3` read select that decides whether
//! `0x20`/`0xa0` reads report the IRR or the ISR. Plus the ELCR pair at
//! `0x4d0`/`0x4d1`, which Linux reads to learn each ISA line's trigger mode.
//!
//! **No delivery.** [`Pic8259::has_input`] is always false because nothing raises
//! a PIC input line; there is deliberately no path from here to a vCPU. If a
//! future device ever needs 8259-routed ExtINT delivery (a guest that boots
//! without ACPI, say), this is where the IRR/ISR priority resolution and the
//! `LINT0` injection would go — the register state it would need is already here.

/// Ports the PIC pair owns: command/data of each chip, and the ELCR pair.
pub const PIC_MASTER_COMMAND: u16 = 0x20;
pub const PIC_MASTER_DATA: u16 = 0x21;
pub const PIC_SLAVE_COMMAND: u16 = 0xa0;
pub const PIC_SLAVE_DATA: u16 = 0xa1;
pub const ELCR_MASTER: u16 = 0x4d0;
pub const ELCR_SLAVE: u16 = 0x4d1;

/// ICW1: bit 4 marks the start of an initialisation sequence.
const ICW1_INIT: u8 = 0x10;
/// ICW1 bit 0: an ICW4 will follow.
const ICW1_ICW4: u8 = 0x01;
/// ICW1 bit 1: single mode, i.e. no ICW3.
const ICW1_SINGLE: u8 = 0x02;

/// OCW3 selects itself with bit 3 set, bit 4 clear.
const OCW3_SELECT: u8 = 0x08;
/// OCW3 bit 1: the read register command is meaningful.
const OCW3_READ_REGISTER: u8 = 0x02;
/// OCW3 bit 0: read the ISR rather than the IRR.
const OCW3_READ_ISR: u8 = 0x01;

/// Where a chip is in its initialisation sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitStep {
    /// Not initialising: a data-port write sets the mask.
    Idle,
    /// Expecting ICW2 (the vector base).
    Icw2,
    /// Expecting ICW3 (cascade wiring).
    Icw3,
    /// Expecting ICW4 (mode flags).
    Icw4,
}

#[derive(Debug, Clone, Copy)]
struct PicChip {
    /// Interrupt mask register. Reset masks everything, like a BIOS leaves it.
    imr: u8,
    /// Interrupt request register — always 0 here: nothing drives an input.
    irr: u8,
    /// In-service register — always 0 for the same reason.
    isr: u8,
    /// Vector base from ICW2.
    vector_base: u8,
    /// Cascade wiring from ICW3.
    cascade: u8,
    /// Mode flags from ICW4 (auto-EOI, 8086 mode).
    icw4: u8,
    step: InitStep,
    /// Whether ICW4 is expected, from ICW1 bit 0.
    expect_icw4: bool,
    /// Whether ICW3 is expected, from ICW1 bit 1 (cleared = cascaded).
    expect_icw3: bool,
    /// Set by OCW3: a command-port read reports the ISR instead of the IRR.
    read_isr: bool,
    /// Edge/level control register byte for this chip's eight lines.
    elcr: u8,
}

impl PicChip {
    const fn new() -> Self {
        Self {
            imr: 0xff,
            irr: 0,
            isr: 0,
            vector_base: 0,
            cascade: 0,
            icw4: 0,
            step: InitStep::Idle,
            expect_icw4: false,
            expect_icw3: false,
            read_isr: false,
            elcr: 0,
        }
    }

    /// Write to the command port (`0x20` / `0xa0`).
    fn write_command(&mut self, value: u8) {
        if value & ICW1_INIT != 0 {
            // ICW1 restarts the sequence and, per the datasheet, clears the mask.
            self.expect_icw4 = value & ICW1_ICW4 != 0;
            self.expect_icw3 = value & ICW1_SINGLE == 0;
            self.step = InitStep::Icw2;
            self.imr = 0;
            self.read_isr = false;
            return;
        }
        // OCW3. Only the read-register select has an observable effect here; the
        // special-mask and poll bits would need real delivery. Anything else is
        // OCW2 — EOI and rotate commands. With nothing ever in service there is
        // no state to clear, but accepting the write is what keeps
        // `init_8259A(auto_eoi)`'s trailing EOIs from looking like errors.
        if value & OCW3_SELECT != 0 && value & OCW3_READ_REGISTER != 0 {
            self.read_isr = value & OCW3_READ_ISR != 0;
        }
    }

    /// Write to the data port (`0x21` / `0xa1`).
    fn write_data(&mut self, value: u8) {
        match self.step {
            InitStep::Idle => self.imr = value,
            InitStep::Icw2 => {
                self.vector_base = value;
                self.step = if self.expect_icw3 {
                    InitStep::Icw3
                } else if self.expect_icw4 {
                    InitStep::Icw4
                } else {
                    InitStep::Idle
                };
            }
            InitStep::Icw3 => {
                self.cascade = value;
                self.step = if self.expect_icw4 {
                    InitStep::Icw4
                } else {
                    InitStep::Idle
                };
            }
            InitStep::Icw4 => {
                self.icw4 = value;
                self.step = InitStep::Idle;
            }
        }
    }

    fn read_command(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }

    /// The read `probe_8259A()` depends on: the mask register, verbatim.
    fn read_data(&self) -> u8 {
        self.imr
    }
}

/// The machine's master/slave 8259A pair.
pub struct Pic8259 {
    master: PicChip,
    slave: PicChip,
}

impl Default for Pic8259 {
    fn default() -> Self {
        Self::new()
    }
}

impl Pic8259 {
    pub const fn new() -> Self {
        Self {
            master: PicChip::new(),
            slave: PicChip::new(),
        }
    }

    /// True when `port` belongs to the PIC pair.
    pub fn contains(port: u16) -> bool {
        matches!(
            port,
            PIC_MASTER_COMMAND | PIC_MASTER_DATA | PIC_SLAVE_COMMAND | PIC_SLAVE_DATA
        ) || matches!(port, ELCR_MASTER | ELCR_SLAVE)
    }

    /// Whether any line is asserted on either chip. Always false — see the
    /// module docs. Exists so a caller that wants to assert "the PIC never has
    /// anything to deliver" can, instead of taking it on trust.
    pub fn has_input(&self) -> bool {
        self.master.irr != 0 || self.slave.irr != 0
    }

    pub fn io_write(&mut self, port: u16, value: u8) {
        match port {
            PIC_MASTER_COMMAND => self.master.write_command(value),
            PIC_MASTER_DATA => self.master.write_data(value),
            PIC_SLAVE_COMMAND => self.slave.write_command(value),
            PIC_SLAVE_DATA => self.slave.write_data(value),
            ELCR_MASTER => self.master.elcr = value,
            ELCR_SLAVE => self.slave.elcr = value,
            _ => {}
        }
    }

    pub fn io_read(&self, port: u16) -> u8 {
        match port {
            PIC_MASTER_COMMAND => self.master.read_command(),
            PIC_MASTER_DATA => self.master.read_data(),
            PIC_SLAVE_COMMAND => self.slave.read_command(),
            PIC_SLAVE_DATA => self.slave.read_data(),
            ELCR_MASTER => self.master.elcr,
            ELCR_SLAVE => self.slave.elcr,
            _ => 0xff,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason this module exists: `probe_8259A()` must conclude that a
    /// PIC is present, or the machine quietly loses `nr_legacy_irqs()` and with
    /// it `check_timer()` and the PIT clockevent.
    #[test]
    fn linux_probe_8259a_finds_a_pic() {
        let mut pic = Pic8259::new();
        // ~(1 << PIC_CASCADE_IR) with PIC_CASCADE_IR == 2.
        let probe_val = !(1u8 << 2);
        pic.io_write(PIC_SLAVE_DATA, 0xff);
        pic.io_write(PIC_MASTER_DATA, probe_val);
        assert_eq!(
            pic.io_read(PIC_MASTER_DATA),
            probe_val,
            "the mask must read back, or Linux installs null_legacy_pic"
        );
        assert_ne!(pic.io_read(PIC_MASTER_DATA), 0xff);
    }

    /// `init_8259A()` writes ICW1..ICW4 to both chips and then the mask. A chip
    /// that mistook ICW2 for a mask write would report a nonsense mask
    /// afterwards, which is exactly what the probe above would catch — in the
    /// wrong direction.
    #[test]
    fn full_init_sequence_ends_with_a_writable_mask() {
        let mut pic = Pic8259::new();
        // Master: ICW1 (cascade + ICW4), ICW2 vector 0x20, ICW3 slave on IR2,
        // ICW4 8086 mode.
        pic.io_write(PIC_MASTER_COMMAND, ICW1_INIT | ICW1_ICW4);
        pic.io_write(PIC_MASTER_DATA, 0x20);
        pic.io_write(PIC_MASTER_DATA, 1 << 2);
        pic.io_write(PIC_MASTER_DATA, 0x01);
        // Slave: same shape, vector 0x28, cascade identity 2.
        pic.io_write(PIC_SLAVE_COMMAND, ICW1_INIT | ICW1_ICW4);
        pic.io_write(PIC_SLAVE_DATA, 0x28);
        pic.io_write(PIC_SLAVE_DATA, 2);
        pic.io_write(PIC_SLAVE_DATA, 0x01);

        assert_eq!(pic.master.vector_base, 0x20);
        assert_eq!(pic.slave.vector_base, 0x28);
        assert_eq!(pic.master.cascade, 1 << 2);
        assert_eq!(pic.master.step, InitStep::Idle);
        assert_eq!(pic.slave.step, InitStep::Idle);

        // Now the masks Linux writes to shut both chips up.
        pic.io_write(PIC_MASTER_DATA, 0xff);
        pic.io_write(PIC_SLAVE_DATA, 0xff);
        assert_eq!(pic.io_read(PIC_MASTER_DATA), 0xff);
        assert_eq!(pic.io_read(PIC_SLAVE_DATA), 0xff);
    }

    /// An ICW1 with `SINGLE` set skips ICW3, so the byte after ICW2 is ICW4. A
    /// chip that always expected ICW3 would land one byte out of step and store
    /// the mask as a mode flag.
    #[test]
    fn single_mode_init_skips_icw3() {
        let mut pic = Pic8259::new();
        pic.io_write(PIC_MASTER_COMMAND, ICW1_INIT | ICW1_SINGLE | ICW1_ICW4);
        pic.io_write(PIC_MASTER_DATA, 0x20);
        pic.io_write(PIC_MASTER_DATA, 0x01); // ICW4
        assert_eq!(pic.master.step, InitStep::Idle);
        assert_eq!(pic.master.icw4, 0x01);
        pic.io_write(PIC_MASTER_DATA, 0x5a);
        assert_eq!(pic.io_read(PIC_MASTER_DATA), 0x5a);
    }

    #[test]
    fn ocw3_selects_between_irr_and_isr() {
        let mut pic = Pic8259::new();
        pic.io_write(
            PIC_MASTER_COMMAND,
            OCW3_SELECT | OCW3_READ_REGISTER | OCW3_READ_ISR,
        );
        assert_eq!(
            pic.io_read(PIC_MASTER_COMMAND),
            0,
            "ISR: nothing in service"
        );
        pic.io_write(PIC_MASTER_COMMAND, OCW3_SELECT | OCW3_READ_REGISTER);
        assert_eq!(
            pic.io_read(PIC_MASTER_COMMAND),
            0,
            "IRR: nothing requesting"
        );
    }

    /// Linux reads the ELCR to learn each ISA line's trigger mode; the value
    /// must be the one it wrote, not a floating 0xff that would make every line
    /// look level-triggered.
    #[test]
    fn elcr_round_trips() {
        let mut pic = Pic8259::new();
        pic.io_write(ELCR_MASTER, 0x0c);
        pic.io_write(ELCR_SLAVE, 0x30);
        assert_eq!(pic.io_read(ELCR_MASTER), 0x0c);
        assert_eq!(pic.io_read(ELCR_SLAVE), 0x30);
    }

    #[test]
    fn nothing_is_ever_pending() {
        let mut pic = Pic8259::new();
        pic.io_write(PIC_MASTER_DATA, 0x00); // unmask everything
        assert!(!pic.has_input());
    }

    #[test]
    fn port_ownership() {
        for port in [0x20u16, 0x21, 0xa0, 0xa1, 0x4d0, 0x4d1] {
            assert!(Pic8259::contains(port), "{port:#x} must be claimed");
        }
        for port in [0x1fu16, 0x22, 0x9f, 0xa2, 0x4cf, 0x4d2, 0x3f8] {
            assert!(!Pic8259::contains(port), "{port:#x} must not be claimed");
        }
    }
}
