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
//! **Delivery is off unless the machine asks for it.** A guest that boots
//! through a BIOS (ADR-0012) runs with the 8259s as its only interrupt
//! controller until its kernel programs the IOAPIC: Windows XP's loader waits
//! for the timer tick on IRQ 0 before it does anything else. For such a
//! machine `UserspaceIrqChip::enable_legacy_interrupts` wires the 8254 to
//! [`Pic8259::pulse`], and the chip pair resolves priority
//! ([`Pic8259::pending`]) and answers the acknowledge cycle
//! ([`Pic8259::acknowledge`]) for the run loop that injects the vector. On
//! every other machine nothing pulses a line and [`Pic8259::has_input`]
//! stays false, exactly as before.
//!
//! Modelled for delivery: edge-triggered requests, the mask, fully nested
//! priority, the cascade on IR 2, auto-EOI, and the specific and
//! non-specific EOI commands. Not modelled: priority rotation, special mask
//! mode, poll mode, level-triggered lines.

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

/// ICW4 bit 1: the chip ends an interrupt at its acknowledge.
const ICW4_AUTO_EOI: u8 = 0x02;
/// The master's input the slave's output is wired to.
const CASCADE_LINE: u8 = 2;

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

    fn save_state(&self) -> crate::state::SavedPicChip {
        crate::state::SavedPicChip {
            imr: self.imr,
            irr: self.irr,
            isr: self.isr,
            vector_base: self.vector_base,
            cascade: self.cascade,
            icw4: self.icw4,
            step: match self.step {
                InitStep::Idle => 0,
                InitStep::Icw2 => 1,
                InitStep::Icw3 => 2,
                InitStep::Icw4 => 3,
            },
            expect_icw4: self.expect_icw4,
            expect_icw3: self.expect_icw3,
            read_isr: self.read_isr,
            elcr: self.elcr,
        }
    }

    fn load_state(
        &mut self,
        state: &crate::state::SavedPicChip,
    ) -> Result<(), crate::state::StateError> {
        self.step = match state.step {
            0 => InitStep::Idle,
            1 => InitStep::Icw2,
            2 => InitStep::Icw3,
            3 => InitStep::Icw4,
            other => {
                return Err(crate::state::StateError::BadValue {
                    what: "8259 initialisation step",
                    value: u64::from(other),
                })
            }
        };
        self.imr = state.imr;
        self.irr = state.irr;
        self.isr = state.isr;
        self.vector_base = state.vector_base;
        self.cascade = state.cascade;
        self.icw4 = state.icw4;
        self.expect_icw4 = state.expect_icw4;
        self.expect_icw3 = state.expect_icw3;
        self.read_isr = state.read_isr;
        self.elcr = state.elcr;
        Ok(())
    }

    /// Write to the command port (`0x20` / `0xa0`).
    fn write_command(&mut self, value: u8) {
        if value & ICW1_INIT != 0 {
            // ICW1 restarts the sequence and, per the datasheet, clears the mask.
            self.expect_icw4 = value & ICW1_ICW4 != 0;
            self.expect_icw3 = value & ICW1_SINGLE == 0;
            self.step = InitStep::Icw2;
            self.imr = 0;
            self.irr = 0;
            self.isr = 0;
            self.read_isr = false;
            return;
        }
        if value & OCW3_SELECT != 0 {
            // OCW3. Only the read-register select is modelled; the
            // special-mask and poll bits are not.
            if value & OCW3_READ_REGISTER != 0 {
                self.read_isr = value & OCW3_READ_ISR != 0;
            }
            return;
        }
        // OCW2: end of interrupt. Non-specific ends the highest-priority line
        // in service, specific names its line. The rotate commands are
        // accepted and do nothing.
        match value & 0xe0 {
            0x20 => self.isr &= self.isr.wrapping_sub(1),
            0x60 => self.isr &= !(1 << (value & 7)),
            _ => {}
        }
    }

    /// The line this chip would interrupt for: the highest-priority one that
    /// is requested and unmasked, unless a line of that priority or higher is
    /// still in service. `extra` is a request from outside the chip's own
    /// register — the slave's output on the master's cascade input.
    fn pending_line(&self, extra: u8) -> Option<u8> {
        let requests = (self.irr | extra) & !self.imr;
        if requests == 0 {
            return None;
        }
        let line = requests.trailing_zeros() as u8;
        let at_or_above = (1u16 << (line + 1)) - 1;
        (u16::from(self.isr) & at_or_above == 0).then_some(line)
    }

    /// The acknowledge cycle for `line`: its request is consumed, it is in
    /// service (unless the chip ends interrupts by itself), and the vector
    /// is the base ICW2 set plus the line.
    fn acknowledge(&mut self, line: u8) -> u8 {
        self.irr &= !(1 << line);
        if self.icw4 & ICW4_AUTO_EOI == 0 {
            self.isr |= 1 << line;
        }
        (self.vector_base & 0xf8) | line
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

    /// Machine reset (ADR-0005): both chips back to the un-initialised state a
    /// power-on 8259 pair is in, half-way through no ICW sequence.
    ///
    /// Cheap to get wrong in an invisible way: a guest's `init_8259A()` writes
    /// ICW1..ICW4 in order, and a chip that thinks it is already three words
    /// into a sequence interprets the first of those as data.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Both chips, for a snapshot (ADR-0006).
    pub fn save_state(&self) -> crate::state::SavedPic {
        crate::state::SavedPic {
            master: self.master.save_state(),
            slave: self.slave.save_state(),
        }
    }

    /// Puts them back.
    pub fn load_state(
        &mut self,
        state: &crate::state::SavedPic,
    ) -> Result<(), crate::state::StateError> {
        self.master.load_state(&state.master)?;
        self.slave.load_state(&state.slave)
    }

    /// Whether any line is requested on either chip. False on every machine
    /// that has not enabled legacy interrupts — see the module docs — so a
    /// caller that wants to assert "the PIC never has anything to deliver"
    /// can, instead of taking it on trust.
    pub fn has_input(&self) -> bool {
        self.master.irr != 0 || self.slave.irr != 0
    }

    /// One edge on interrupt line `irq` (0–15): the request is latched until
    /// it is acknowledged.
    pub fn pulse(&mut self, irq: u8) {
        match irq {
            0..=7 => self.master.irr |= 1 << irq,
            8..=15 => self.slave.irr |= 1 << (irq - 8),
            _ => {}
        }
    }

    fn cascade_request(&self) -> u8 {
        u8::from(self.slave.pending_line(0).is_some()) << CASCADE_LINE
    }

    /// The pair's `INTR` output: there is an interrupt the CPU should take.
    pub fn pending(&self) -> bool {
        self.master.pending_line(self.cascade_request()).is_some()
    }

    /// The acknowledge cycle. Returns the vector of the interrupt now in
    /// service, or `None` when nothing is pending.
    pub fn acknowledge(&mut self) -> Option<u8> {
        let cascade = self.cascade_request();
        let line = self.master.pending_line(cascade)?;
        if line == CASCADE_LINE && self.master.irr & (1 << CASCADE_LINE) == 0 {
            // The request came through the cascade: the slave names the
            // vector, and the master holds the cascade line in service.
            let slave_line = self.slave.pending_line(0)?;
            if self.master.icw4 & ICW4_AUTO_EOI == 0 {
                self.master.isr |= 1 << CASCADE_LINE;
            }
            return Some(self.slave.acknowledge(slave_line));
        }
        Some(self.master.acknowledge(line))
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

    /// The pair as a PC BIOS leaves it: vectors 08h and 70h, the slave on
    /// IR 2, 8086 mode, and `mask` on the master.
    fn initialised(master_mask: u8, slave_mask: u8) -> Pic8259 {
        let mut pic = Pic8259::new();
        for (port, value) in [
            (PIC_MASTER_COMMAND, 0x11),
            (PIC_SLAVE_COMMAND, 0x11),
            (PIC_MASTER_DATA, 0x08),
            (PIC_SLAVE_DATA, 0x70),
            (PIC_MASTER_DATA, 0x04),
            (PIC_SLAVE_DATA, 0x02),
            (PIC_MASTER_DATA, 0x01),
            (PIC_SLAVE_DATA, 0x01),
            (PIC_MASTER_DATA, master_mask),
            (PIC_SLAVE_DATA, slave_mask),
        ] {
            pic.io_write(port, value);
        }
        pic
    }

    #[test]
    fn an_unmasked_edge_is_delivered_once_with_its_vector() {
        let mut pic = initialised(0xfe, 0xff);
        assert!(!pic.pending());
        pic.pulse(0);
        assert!(pic.pending());
        assert_eq!(pic.acknowledge(), Some(0x08));
        // In service now: the request is consumed.
        assert!(!pic.pending());
        assert_eq!(pic.acknowledge(), None);
    }

    #[test]
    fn a_masked_line_waits_for_the_mask_to_open() {
        let mut pic = initialised(0xff, 0xff);
        pic.pulse(0);
        assert!(!pic.pending());
        pic.io_write(PIC_MASTER_DATA, 0xfe);
        assert!(pic.pending());
    }

    #[test]
    fn a_line_in_service_holds_back_itself_and_lower_priorities_until_eoi() {
        let mut pic = initialised(0x00, 0xff);
        pic.pulse(1);
        assert_eq!(pic.acknowledge(), Some(0x09));
        // A second edge on the same line, and one on a lower-priority line.
        pic.pulse(1);
        pic.pulse(3);
        assert!(!pic.pending());
        // A higher-priority line still gets through.
        pic.pulse(0);
        assert_eq!(pic.acknowledge(), Some(0x08));
        // Non-specific EOI ends the highest priority in service: line 0.
        pic.io_write(PIC_MASTER_COMMAND, 0x20);
        assert!(!pic.pending(), "line 1 is still in service");
        pic.io_write(PIC_MASTER_COMMAND, 0x20);
        assert_eq!(pic.acknowledge(), Some(0x09));
        // A specific EOI names its line.
        pic.io_write(PIC_MASTER_COMMAND, 0x61);
        assert_eq!(pic.acknowledge(), Some(0x0b));
    }

    #[test]
    fn a_slave_line_comes_through_the_cascade_with_the_slaves_vector() {
        let mut pic = initialised(0xfb, 0xfe);
        pic.pulse(8);
        assert!(pic.pending());
        assert_eq!(pic.acknowledge(), Some(0x70));
        assert!(!pic.pending());
        // Both chips need their EOI before line 8 can interrupt again.
        pic.pulse(8);
        pic.io_write(PIC_SLAVE_COMMAND, 0x20);
        assert!(!pic.pending(), "the master still holds the cascade");
        pic.io_write(PIC_MASTER_COMMAND, 0x20);
        assert_eq!(pic.acknowledge(), Some(0x70));
        // With the cascade input masked on the master, nothing arrives.
        let mut pic = initialised(0xff, 0xfe);
        pic.pulse(8);
        assert!(!pic.pending());
    }

    #[test]
    fn nothing_is_pending_on_a_pair_nobody_pulsed() {
        let pic = initialised(0x00, 0x00);
        assert!(!pic.pending());
        assert!(!pic.has_input());
    }

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
