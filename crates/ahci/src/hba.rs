//! The AHCI 1.3 host bus adapter: the register file behind ABAR (BAR 5) and
//! the command engine that walks the guest's command lists.
//!
//! What it models is what a firmware's `AtaAtapiPassThru`, Windows' in-box
//! `storahci` and Linux's `ahci` need for ATAPI optical drives: the generic
//! and per-port registers, COMRESET and HBA reset, command slots executed when
//! `PxCI` is written, the D2H register FIS in the received-FIS area, and the
//! interrupt rule (`PxIS & PxIE` sets the port's `IS` bit; `GHC.IE` gates the
//! line). Commands run to completion synchronously, in the order of their
//! slot bits; there is no NCQ (`CAP.SNCQ` clear).
//!
//! Every address here comes from the guest. Guest memory is read and written
//! only through `vm-memory`'s checked API, a PRD table is walked at most
//! [`MAX_PRDT_ENTRIES`] entries and [`MAX_TRANSFER`] bytes per command, and a
//! command the device cannot make sense of completes with an error status
//! rather than a panic.

use std::sync::Arc;

use virtio_core::GuestMem;
use vm_memory::{Bytes, GuestAddress};

use crate::atapi::{Atapi, Reply, Sense, BLOCK_SIZE};

/// Size of the ABAR register window.
pub const ABAR_SIZE: u32 = 0x1000;
/// Ports this HBA implements (each may or may not have a drive).
pub const MAX_PORTS: usize = 4;
/// Command slots per port.
const SLOTS: u32 = 32;
/// PRD entries walked per command at most.
pub const MAX_PRDT_ENTRIES: usize = 1024;
/// Bytes moved per command at most.
pub const MAX_TRANSFER: u64 = 64 << 20;
/// Largest piece of a read staged in host memory at once.
const READ_CHUNK: usize = 1 << 20;

/// ATAPI device signature in `PxSIG`.
const SIG_ATAPI: u32 = 0xeb14_0101;
/// `PxSSTS`: device present and PHY communication established, Gen1, active.
const SSTS_PRESENT: u32 = 0x113;

// Generic host control registers.
const CAP: u64 = 0x00;
const GHC: u64 = 0x04;
const IS: u64 = 0x08;
const PI: u64 = 0x0c;
const VS: u64 = 0x10;
const PORTS: u64 = 0x100;
const PORT_SIZE: u64 = 0x80;

const GHC_HR: u32 = 1 << 0;
const GHC_IE: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;

// Port registers (offsets inside a port's window).
const P_CLB: u64 = 0x00;
const P_CLBU: u64 = 0x04;
const P_FB: u64 = 0x08;
const P_FBU: u64 = 0x0c;
const P_IS: u64 = 0x10;
const P_IE: u64 = 0x14;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SCTL: u64 = 0x2c;
const P_SERR: u64 = 0x30;
const P_SACT: u64 = 0x34;
const P_CI: u64 = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_CLO: u32 = 1 << 3;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
/// The `PxCMD` bits software owns; the rest are read-only or reflect state.
const CMD_WRITABLE: u32 = CMD_ST | (1 << 1) | (1 << 2) | CMD_FRE | (0xf << 28);

const IS_DHRS: u32 = 1 << 0;
const IS_TFES: u32 = 1 << 30;

/// ATA status bits.
const ATA_DRDY: u8 = 0x40;
const ATA_DSC: u8 = 0x10;
const ATA_ERR: u8 = 0x01;
/// ATA error register: command aborted.
const ATA_ABRT: u8 = 0x04;

/// How the HBA raises its interrupt; the machine decides MSI or INTx.
pub type Raise = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Default, Clone, Copy)]
struct PortRegs {
    clb: u64,
    fb: u64,
    is: u32,
    ie: u32,
    cmd: u32,
    tfd: u32,
    sctl: u32,
    serr: u32,
    ci: u32,
}

struct Port {
    drive: Option<Atapi>,
    regs: PortRegs,
}

impl Port {
    fn power_on(&mut self) {
        self.regs = PortRegs {
            tfd: if self.drive.is_some() {
                u32::from(ATA_DRDY | ATA_DSC)
            } else {
                0x7f
            },
            ..PortRegs::default()
        };
    }

    fn ssts(&self) -> u32 {
        // DET reads 0 during a COMRESET (SCTL.DET = 1).
        if self.drive.is_some() && self.regs.sctl & 0xf != 1 {
            SSTS_PRESENT
        } else {
            0
        }
    }
}

/// How one command ends.
struct Outcome {
    status: u8,
    error: u8,
    transferred: u32,
}

impl Outcome {
    fn good(transferred: u32) -> Self {
        Self {
            status: ATA_DRDY | ATA_DSC,
            error: 0,
            transferred,
        }
    }

    fn abort() -> Self {
        Self {
            status: ATA_DRDY | ATA_DSC | ATA_ERR,
            error: ATA_ABRT,
            transferred: 0,
        }
    }

    /// An ATAPI CHECK CONDITION: the sense key goes in the error register.
    fn check(sense: Sense) -> Self {
        Self {
            status: ATA_DRDY | ATA_DSC | ATA_ERR,
            error: sense.key << 4,
            transferred: 0,
        }
    }
}

/// The HBA.
pub struct Ahci {
    ghc: u32,
    is: u32,
    ports: Vec<Port>,
    mem: Option<Arc<GuestMem>>,
    raise: Option<Raise>,
}

impl std::fmt::Debug for Ahci {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ahci")
            .field("ports", &self.ports.len())
            .field("ghc", &format_args!("{:#x}", self.ghc))
            .finish_non_exhaustive()
    }
}

impl Ahci {
    /// An HBA with one port per drive (at most [`MAX_PORTS`]).
    pub fn new(drives: Vec<Atapi>) -> Self {
        let mut ports: Vec<Port> = drives
            .into_iter()
            .take(MAX_PORTS)
            .map(|drive| Port {
                drive: Some(drive),
                regs: PortRegs::default(),
            })
            .collect();
        if ports.is_empty() {
            ports.push(Port {
                drive: None,
                regs: PortRegs::default(),
            });
        }
        let mut ahci = Self {
            ghc: GHC_AE,
            is: 0,
            ports,
            mem: None,
            raise: None,
        };
        ahci.reset();
        ahci
    }

    /// Gives the HBA guest memory and its interrupt.
    pub fn connect(&mut self, mem: Arc<GuestMem>, raise: Raise) {
        self.mem = Some(mem);
        self.raise = Some(raise);
    }

    /// Power-on (ADR-0005): every register back, drives still inserted.
    pub fn reset(&mut self) {
        self.ghc = GHC_AE;
        self.is = 0;
        for port in &mut self.ports {
            port.power_on();
        }
    }

    fn cap(&self) -> u32 {
        // S64A, SAM (AHCI only), Gen1, NCS = 31, NP = ports - 1.
        (1 << 31) | (1 << 18) | (1 << 20) | ((SLOTS - 1) << 8) | (self.ports.len() as u32 - 1)
    }

    fn pi(&self) -> u32 {
        (1u32 << self.ports.len()) - 1
    }

    /// Guest read inside ABAR. Unclaimed offsets read zero.
    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let aligned = offset & !3;
        let value = self.read_dword(aligned).to_le_bytes();
        let start = (offset - aligned) as usize;
        for (i, byte) in data.iter_mut().enumerate() {
            if let Some(&b) = value.get(start + i) {
                *byte = b;
            }
        }
    }

    fn read_dword(&self, offset: u64) -> u32 {
        match offset {
            CAP => self.cap(),
            GHC => self.ghc,
            IS => self.is,
            PI => self.pi(),
            VS => 0x0001_0300,
            o if o >= PORTS => {
                let index = ((o - PORTS) / PORT_SIZE) as usize;
                let Some(port) = self.ports.get(index) else {
                    return 0;
                };
                let r = &port.regs;
                match (o - PORTS) % PORT_SIZE {
                    P_CLB => r.clb as u32,
                    P_CLBU => (r.clb >> 32) as u32,
                    P_FB => r.fb as u32,
                    P_FBU => (r.fb >> 32) as u32,
                    P_IS => r.is,
                    P_IE => r.ie,
                    P_CMD => {
                        let mut cmd = r.cmd;
                        if cmd & CMD_ST != 0 {
                            cmd |= CMD_CR;
                        }
                        if cmd & CMD_FRE != 0 {
                            cmd |= CMD_FR;
                        }
                        cmd
                    }
                    P_TFD => r.tfd,
                    P_SIG => {
                        if port.drive.is_some() {
                            SIG_ATAPI
                        } else {
                            0xffff_ffff
                        }
                    }
                    P_SSTS => port.ssts(),
                    P_SCTL => r.sctl,
                    P_SERR => r.serr,
                    P_SACT => 0,
                    P_CI => r.ci,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// Guest write inside ABAR. Narrow writes are merged into their dword;
    /// write-1-to-clear registers only see the bytes actually written.
    pub fn mmio_write(&mut self, offset: u64, data: &[u8]) {
        let aligned = offset & !3;
        let start = (offset - aligned) as usize;
        if data.is_empty() || start + data.len() > 4 {
            return;
        }
        let mut bytes = [0u8; 4];
        let mut mask = [0u8; 4];
        bytes[start..start + data.len()].copy_from_slice(data);
        mask[start..start + data.len()].fill(0xff);
        let value = u32::from_le_bytes(bytes);
        let mask = u32::from_le_bytes(mask);
        let merged = (self.read_dword(aligned) & !mask) | value;
        self.write_dword(aligned, merged, value);
    }

    /// `merged` is the register with the written bytes replaced; `written`
    /// has only the written bits (for write-1-to-clear registers).
    fn write_dword(&mut self, offset: u64, merged: u32, written: u32) {
        match offset {
            GHC => {
                if merged & GHC_HR != 0 {
                    self.reset();
                    return;
                }
                self.ghc = GHC_AE | (merged & GHC_IE);
                self.update_interrupt(0);
            }
            IS => self.is &= !written,
            o if o >= PORTS => {
                let index = ((o - PORTS) / PORT_SIZE) as usize;
                if index >= self.ports.len() {
                    return;
                }
                self.write_port(index, (o - PORTS) % PORT_SIZE, merged, written);
            }
            _ => {}
        }
    }

    fn write_port(&mut self, index: usize, register: u64, merged: u32, written: u32) {
        let Some(port) = self.ports.get_mut(index) else {
            return;
        };
        let r = &mut port.regs;
        match register {
            P_CLB => r.clb = (r.clb & !0xffff_ffff) | u64::from(merged & !0x3ff),
            P_CLBU => r.clb = (r.clb & 0xffff_ffff) | (u64::from(merged) << 32),
            P_FB => r.fb = (r.fb & !0xffff_ffff) | u64::from(merged & !0xff),
            P_FBU => r.fb = (r.fb & 0xffff_ffff) | (u64::from(merged) << 32),
            P_IS => {
                r.is &= !written;
                if r.is & r.ie == 0 {
                    self.is &= !(1 << index);
                }
            }
            P_IE => {
                r.ie = merged;
                self.update_interrupt(index);
            }
            P_CMD => {
                let was = r.cmd;
                r.cmd = merged & CMD_WRITABLE;
                // CLO clears BSY/DRQ and then itself.
                if merged & CMD_CLO != 0 {
                    r.tfd &= !0x88;
                }
                if was & CMD_ST != 0 && r.cmd & CMD_ST == 0 {
                    r.ci = 0;
                }
                let receives = was & CMD_FRE == 0 && r.cmd & CMD_FRE != 0;
                let started = r.cmd & CMD_ST != 0;
                if receives {
                    self.post_signature(index);
                }
                if started {
                    self.run_commands(index);
                }
            }
            P_SCTL => {
                let was = r.sctl;
                r.sctl = merged & 0xfff;
                // COMRESET: DET 1 then 0. The device comes back with its
                // signature, as after power-on.
                if was & 0xf == 1 && r.sctl & 0xf == 0 {
                    port.power_on();
                    port.regs.sctl = merged & 0xfff;
                    port.regs.serr |= 1 << 26;
                    self.post_signature(index);
                }
            }
            P_SERR => r.serr &= !written,
            P_CI if r.cmd & CMD_ST != 0 => {
                r.ci |= merged;
                self.run_commands(index);
            }
            _ => {}
        }
    }

    /// The initial D2H register FIS a device sends after reset, carrying its
    /// signature, when the port receives FISes.
    fn post_signature(&mut self, index: usize) {
        let Some(port) = self.ports.get(index) else {
            return;
        };
        if port.drive.is_none() || port.regs.cmd & CMD_FRE == 0 {
            return;
        }
        let mut fis = [0u8; 20];
        fis[0] = 0x34;
        fis[2] = ATA_DRDY | ATA_DSC;
        fis[4] = 0x01; // LBA low
        fis[5] = 0x14; // LBA mid
        fis[6] = 0xeb; // LBA high
        fis[12] = 0x01; // sector count
        let fb = port.regs.fb;
        self.write_guest(fb + 0x40, &fis);
    }

    fn write_guest(&self, addr: u64, data: &[u8]) -> bool {
        match &self.mem {
            Some(mem) => mem.write_slice(data, GuestAddress(addr)).is_ok(),
            None => false,
        }
    }

    fn read_guest(&self, addr: u64, data: &mut [u8]) -> bool {
        match &self.mem {
            Some(mem) => mem.read_slice(data, GuestAddress(addr)).is_ok(),
            None => false,
        }
    }

    /// Runs every slot set in `PxCI`, lowest first.
    fn run_commands(&mut self, index: usize) {
        loop {
            let Some(port) = self.ports.get(index) else {
                return;
            };
            if port.regs.cmd & CMD_ST == 0 || port.regs.ci == 0 {
                return;
            }
            let slot = port.regs.ci.trailing_zeros();
            let outcome = self.execute(index, slot);
            self.complete(index, slot, &outcome);
        }
    }

    fn execute(&mut self, index: usize, slot: u32) -> Outcome {
        let Some(port) = self.ports.get(index) else {
            return Outcome::abort();
        };
        let header_addr = port.regs.clb + u64::from(slot) * 32;
        let mut header = [0u8; 32];
        if !self.read_guest(header_addr, &mut header) {
            return Outcome::abort();
        }
        let dw0 = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let ctba = u64::from_le_bytes([
            header[8], header[9], header[10], header[11], header[12], header[13], header[14],
            header[15],
        ]) & !0x7f;
        let prdtl = ((dw0 >> 16) as usize).min(MAX_PRDT_ENTRIES);
        let mut cfis = [0u8; 20];
        if !self.read_guest(ctba, &mut cfis) || cfis[0] != 0x27 {
            return Outcome::abort();
        }
        let prdt = self.read_prdt(ctba + 0x80, prdtl);
        let outcome = match cfis[2] {
            // IDENTIFY PACKET DEVICE
            0xa1 => match self.ports.get(index).and_then(|p| p.drive.as_ref()) {
                Some(drive) => {
                    let data = drive.identify_packet();
                    Outcome::good(self.scatter(&prdt, &data))
                }
                None => Outcome::abort(),
            },
            // PACKET
            0xa0 => {
                let mut cdb = [0u8; 16];
                if !self.read_guest(ctba + 0x40, &mut cdb) {
                    return Outcome::abort();
                }
                self.packet(index, &cdb, &prdt)
            }
            // NOP, DEVICE RESET, SET FEATURES, IDLE IMMEDIATE, CHECK POWER
            // MODE: accepted, nothing to do.
            0x00 | 0x08 | 0xef | 0xe1 | 0xe5 => Outcome::good(0),
            // IDENTIFY DEVICE (which an ATAPI device aborts, sending the host
            // to IDENTIFY PACKET DEVICE) and everything else.
            _ => Outcome::abort(),
        };
        // PRDBC: bytes transferred.
        self.write_guest(header_addr + 4, &outcome.transferred.to_le_bytes());
        outcome
    }

    fn packet(&mut self, index: usize, cdb: &[u8; 16], prdt: &[(u64, u64)]) -> Outcome {
        let Some(drive) = self.ports.get_mut(index).and_then(|p| p.drive.as_mut()) else {
            return Outcome::abort();
        };
        match drive.packet(cdb) {
            Reply::Done => Outcome::good(0),
            Reply::Check(sense) => Outcome::check(sense),
            Reply::Data(data) => Outcome::good(self.scatter(prdt, &data)),
            Reply::Read { lba, blocks } => self.read_blocks(index, prdt, lba, blocks),
        }
    }

    /// The PRD table as (address, length) pairs, bounded by
    /// [`MAX_PRDT_ENTRIES`]; an unreadable entry ends the table.
    fn read_prdt(&self, base: u64, entries: usize) -> Vec<(u64, u64)> {
        let mut prdt = Vec::with_capacity(entries);
        for i in 0..entries as u64 {
            let mut entry = [0u8; 16];
            if !self.read_guest(base + i * 16, &mut entry) {
                break;
            }
            let addr = u64::from_le_bytes([
                entry[0], entry[1], entry[2], entry[3], entry[4], entry[5], entry[6], entry[7],
            ]);
            let dbc = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]);
            prdt.push((addr, u64::from(dbc & 0x3f_ffff) + 1));
        }
        prdt
    }

    /// Copies `data` into the PRD regions in order. Returns bytes copied.
    fn scatter(&self, prdt: &[(u64, u64)], data: &[u8]) -> u32 {
        let mut done = 0usize;
        for &(addr, len) in prdt {
            if done >= data.len() {
                break;
            }
            let take = (len as usize).min(data.len() - done);
            if !self.write_guest(addr, &data[done..done + take]) {
                break;
            }
            done += take;
        }
        done as u32
    }

    /// Streams `blocks` blocks from `lba` into the PRD regions without
    /// staging the whole transfer.
    fn read_blocks(&mut self, index: usize, prdt: &[(u64, u64)], lba: u64, blocks: u64) -> Outcome {
        let total = (blocks * BLOCK_SIZE).min(MAX_TRANSFER);
        let mut offset = lba * BLOCK_SIZE;
        let mut done = 0u64;
        let mut staging = vec![0u8; READ_CHUNK];
        for &(addr, len) in prdt {
            let mut region_done = 0u64;
            while region_done < len && done < total {
                let take = (len - region_done).min(total - done).min(READ_CHUNK as u64) as usize;
                let read = {
                    let Some(drive) = self.ports.get(index).and_then(|p| p.drive.as_ref()) else {
                        return Outcome::abort();
                    };
                    drive.read_at(&mut staging[..take], offset)
                };
                if read.is_err() {
                    if let Some(drive) = self.ports.get_mut(index).and_then(|p| p.drive.as_mut()) {
                        drive.set_sense(Sense::READ_ERROR);
                    }
                    return Outcome::check(Sense::READ_ERROR);
                }
                if !self.write_guest(addr + region_done, &staging[..take]) {
                    return Outcome::abort();
                }
                region_done += take as u64;
                done += take as u64;
                offset += take as u64;
            }
            if done >= total {
                break;
            }
        }
        Outcome::good(done as u32)
    }

    /// Finishes slot `slot`: the D2H register FIS, `PxTFD`, `PxIS`, `PxCI`
    /// and the interrupt.
    fn complete(&mut self, index: usize, slot: u32, outcome: &Outcome) {
        let fb = {
            let Some(port) = self.ports.get_mut(index) else {
                return;
            };
            port.regs.ci &= !(1 << slot);
            port.regs.tfd = u32::from(outcome.status) | (u32::from(outcome.error) << 8);
            port.regs.is |= IS_DHRS;
            if outcome.status & ATA_ERR != 0 {
                port.regs.is |= IS_TFES;
            }
            (port.regs.cmd & CMD_FRE != 0).then_some(port.regs.fb)
        };
        if let Some(fb) = fb {
            let mut fis = [0u8; 20];
            fis[0] = 0x34;
            fis[1] = 0x40; // interrupt
            fis[2] = outcome.status;
            fis[3] = outcome.error;
            self.write_guest(fb + 0x40, &fis);
        }
        self.update_interrupt(index);
    }

    /// Sets the port's `IS` bit when an enabled port interrupt is pending,
    /// and raises the line on a new one.
    fn update_interrupt(&mut self, index: usize) {
        let Some(port) = self.ports.get(index) else {
            return;
        };
        let bit = 1 << index;
        if port.regs.is & port.regs.ie == 0 {
            return;
        }
        let new = self.is & bit == 0;
        self.is |= bit;
        if new && self.ghc & GHC_IE != 0 {
            if let Some(raise) = &self.raise {
                raise();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vm_memory::GuestMemoryMmap;

    const MEM: u64 = 1 << 20;
    const CLB: u64 = 0x1_0000;
    const FB: u64 = 0x2_0000;
    const CTBA: u64 = 0x3_0000;
    const BUF: u64 = 0x4_0000;

    struct Harness {
        ahci: Ahci,
        mem: Arc<GuestMem>,
        raised: Arc<AtomicUsize>,
        _image: std::path::PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self._image);
        }
    }

    fn harness(blocks: u64) -> Harness {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "entangled-ahci-{}-{}.iso",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut image = vec![0u8; (blocks * BLOCK_SIZE) as usize];
        for (i, byte) in image.iter_mut().enumerate() {
            *byte = (i / BLOCK_SIZE as usize) as u8;
        }
        std::fs::write(&path, image).unwrap();
        let drive = Atapi::open(&path, "T").unwrap();
        let mem = Arc::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM as usize)]).unwrap(),
        );
        let raised = Arc::new(AtomicUsize::new(0));
        let mut ahci = Ahci::new(vec![drive]);
        let counter = Arc::clone(&raised);
        ahci.connect(
            Arc::clone(&mem),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        Harness {
            ahci,
            mem,
            raised,
            _image: path,
        }
    }

    impl Harness {
        fn write(&mut self, offset: u64, value: u32) {
            self.ahci.mmio_write(offset, &value.to_le_bytes());
        }
        fn read(&self, offset: u64) -> u32 {
            let mut data = [0u8; 4];
            self.ahci.mmio_read(offset, &mut data);
            u32::from_le_bytes(data)
        }
        fn start(&mut self) {
            self.write(PORTS + P_CLB, CLB as u32);
            self.write(PORTS + P_FB, FB as u32);
            self.write(PORTS + P_IE, u32::MAX);
            self.write(GHC, GHC_AE | GHC_IE);
            self.write(PORTS + P_CMD, CMD_FRE | CMD_ST);
        }
        /// Builds a command in slot 0 and issues it.
        fn issue(&mut self, command: u8, cdb: Option<&[u8]>, buf_len: u32) {
            let mut header = [0u8; 32];
            let flags = 5u32 | if cdb.is_some() { 1 << 5 } else { 0 } | (1 << 16);
            header[0..4].copy_from_slice(&flags.to_le_bytes());
            header[8..16].copy_from_slice(&CTBA.to_le_bytes());
            self.mem.write_slice(&header, GuestAddress(CLB)).unwrap();
            let mut cfis = [0u8; 20];
            cfis[0] = 0x27;
            cfis[1] = 0x80;
            cfis[2] = command;
            self.mem.write_slice(&cfis, GuestAddress(CTBA)).unwrap();
            if let Some(cdb) = cdb {
                let mut acmd = [0u8; 16];
                acmd[..cdb.len()].copy_from_slice(cdb);
                self.mem
                    .write_slice(&acmd, GuestAddress(CTBA + 0x40))
                    .unwrap();
            }
            let mut prd = [0u8; 16];
            prd[0..8].copy_from_slice(&BUF.to_le_bytes());
            prd[12..16].copy_from_slice(&(buf_len - 1).to_le_bytes());
            self.mem
                .write_slice(&prd, GuestAddress(CTBA + 0x80))
                .unwrap();
            self.write(PORTS + P_CI, 1);
        }
        fn buffer(&self, len: usize) -> Vec<u8> {
            let mut data = vec![0u8; len];
            self.mem.read_slice(&mut data, GuestAddress(BUF)).unwrap();
            data
        }
    }

    #[test]
    fn identifies_as_an_ahci_hba_with_an_atapi_device() {
        let h = harness(4);
        assert_ne!(h.read(CAP) & (1 << 18), 0, "AHCI only");
        assert_eq!(h.read(PI), 1);
        assert_eq!(h.read(VS), 0x0001_0300);
        assert_eq!(h.read(PORTS + P_SIG), SIG_ATAPI);
        assert_eq!(h.read(PORTS + P_SSTS), SSTS_PRESENT);
        assert_ne!(h.read(GHC) & GHC_AE, 0);
    }

    #[test]
    fn identify_packet_device_fills_the_buffer_and_interrupts() {
        let mut h = harness(4);
        h.start();
        h.issue(0xa1, None, 512);
        assert_eq!(h.read(PORTS + P_CI), 0, "slot completed");
        assert_eq!(&h.buffer(2), &0x85c0u16.to_le_bytes());
        assert_ne!(h.read(PORTS + P_IS) & IS_DHRS, 0);
        assert_eq!(h.read(IS), 1);
        assert_eq!(h.raised.load(Ordering::Relaxed), 1);
        // The D2H FIS landed in the received-FIS area.
        let mut fis = [0u8; 4];
        h.mem.read_slice(&mut fis, GuestAddress(FB + 0x40)).unwrap();
        assert_eq!(fis[0], 0x34);
    }

    #[test]
    fn a_packet_read_streams_blocks_into_the_prdt() {
        let mut h = harness(8);
        h.start();
        h.issue(0xa0, Some(&[0x28, 0, 0, 0, 0, 3, 0, 0, 2]), 4096);
        let data = h.buffer(4096);
        assert!(data[..2048].iter().all(|&b| b == 3));
        assert!(data[2048..].iter().all(|&b| b == 4));
        let mut prdbc = [0u8; 4];
        h.mem.read_slice(&mut prdbc, GuestAddress(CLB + 4)).unwrap();
        assert_eq!(u32::from_le_bytes(prdbc), 4096);
    }

    #[test]
    fn identify_device_is_aborted_with_a_task_file_error() {
        let mut h = harness(1);
        h.start();
        h.issue(0xec, None, 512);
        let tfd = h.read(PORTS + P_TFD);
        assert_ne!(tfd & u32::from(ATA_ERR), 0);
        assert_eq!((tfd >> 8) as u8, ATA_ABRT);
        assert_ne!(h.read(PORTS + P_IS) & IS_TFES, 0);
    }

    #[test]
    fn interrupt_status_is_write_one_to_clear() {
        let mut h = harness(1);
        h.start();
        h.issue(0xa1, None, 512);
        h.write(PORTS + P_IS, u32::MAX);
        assert_eq!(h.read(PORTS + P_IS), 0);
        h.write(IS, 1);
        assert_eq!(h.read(IS), 0);
    }

    #[test]
    fn commands_wait_for_start_and_stop_clears_them() {
        let mut h = harness(1);
        h.write(PORTS + P_CLB, CLB as u32);
        h.write(PORTS + P_CI, 1);
        assert_eq!(h.read(PORTS + P_CI), 0, "ignored while stopped");
        h.write(PORTS + P_CMD, CMD_ST);
        assert_ne!(h.read(PORTS + P_CMD) & CMD_CR, 0);
        h.write(PORTS + P_CMD, 0);
        assert_eq!(h.read(PORTS + P_CMD) & CMD_CR, 0);
    }

    #[test]
    fn hba_reset_returns_to_power_on() {
        let mut h = harness(1);
        h.start();
        h.write(GHC, GHC_HR);
        assert_eq!(h.read(GHC), GHC_AE);
        assert_eq!(h.read(PORTS + P_CMD), 0);
        assert_eq!(h.read(PORTS + P_CLB), 0);
    }

    #[test]
    fn garbage_from_the_guest_never_panics() {
        let mut h = harness(1);
        h.start();
        // A command list pointing outside guest memory, an absurd PRDT.
        h.write(PORTS + P_CLB, 0xffff_fc00);
        h.write(PORTS + P_CLBU, 0xffff_ffff);
        h.write(PORTS + P_CI, u32::MAX);
        assert_eq!(h.read(PORTS + P_CI), 0);
        for offset in (0..u64::from(ABAR_SIZE)).step_by(3) {
            h.ahci.mmio_write(offset, &[0xff, 0xff]);
            let mut data = [0u8; 3];
            h.ahci.mmio_read(offset, &mut data);
        }
    }
}
