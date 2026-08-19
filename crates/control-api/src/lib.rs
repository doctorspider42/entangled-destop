//! Control surface shared by frontends (backlog EPIC 12). The CLI is the
//! first consumer; the crate exists so a future GUI talks to the same model.

mod config;

pub use config::{
    BootMode, BootSection, CdromSection, ConfigError, DiskSection, DisplaySection, NetworkBackend,
    NetworkSection, VirtioTransport, VmConfig, MAX_MEMORY_MIB, MIN_MEMORY_MIB,
};
