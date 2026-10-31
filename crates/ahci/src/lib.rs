//! An AHCI SATA controller with ATAPI optical drives (ADR-0008 §4).
//!
//! Windows PE has no virtio driver, so it cannot see a virtio-blk "CD-ROM":
//! not the installation ISO, not the virtio-win drivers, not an answer file.
//! It does have an in-box AHCI driver (`storahci`) and a CD-ROM class driver,
//! so ISOs on this controller are visible to it from the first instruction,
//! and the installation runs from an untouched Windows ISO. The firmware's
//! own `SataControllerDxe`/`AtaAtapiPassThru`/`ScsiDiskDxe` boot from it too.
//!
//! Portable device logic only: [`hba`] is the register file and command
//! engine, [`atapi`] the drive. The PCI function around it (identity, ABAR in
//! BAR 5, the interrupt) is the machine's.

pub mod atapi;
pub mod hba;

pub use atapi::{Atapi, Reply, Sense, BLOCK_SIZE};
pub use hba::{Ahci, Raise, ABAR_SIZE, MAX_PORTS};

/// PCI identity: Intel ICH9 AHCI, the controller every in-box driver knows.
pub const VENDOR_ID: u16 = 0x8086;
/// See [`VENDOR_ID`].
pub const DEVICE_ID: u16 = 0x2922;
/// Class 01 (mass storage), subclass 06 (SATA), programming interface 01
/// (AHCI 1.0).
pub const CLASS_AHCI: u32 = 0x0106_0100;
/// The BAR that carries ABAR, as the AHCI specification fixes it.
pub const ABAR_INDEX: u8 = 5;
