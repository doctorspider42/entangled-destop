//! Control surface shared by frontends (backlog EPIC 12). The CLI is the
//! first consumer; the crate exists so a future GUI talks to the same model.

mod config;
pub mod control;

pub use config::{
    BootMode, BootSection, CdromSection, ConfigError, DiskSection, DisplaySection, GamepadBackend,
    GamepadSection, NetworkBackend, NetworkSection, SoundBackend, SoundSection, VirglIsolation,
    VirtioTransport, VmConfig, DEFAULT_REFRESH_HZ, MAX_MEMORY_MIB, MAX_REFRESH_HZ, MIN_MEMORY_MIB,
    MIN_REFRESH_HZ,
};
