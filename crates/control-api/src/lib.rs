//! Control surface shared by frontends (backlog EPIC 12). The CLI is the
//! first consumer; the crate exists so a future GUI talks to the same model.

mod config;
pub mod control;
pub mod wsl;
/// Getting a Linux engine into WSL: the pinned download, the digest check and
/// the copy into the distribution. Behind the `engine-install` feature, which
/// only the two binaries that perform it turn on (see Cargo.toml).
#[cfg(feature = "engine-install")]
pub mod wsl_engine;

pub use config::{
    default_vcpus, host_default_vcpus, BootMode, BootSection, CdromSection, ConfigError,
    DiskSection, DisplaySection, GamepadBackend, GamepadSection, GpuRenderer, NetworkBackend,
    NetworkSection, SoundBackend, SoundSection, VirglIsolation, VirtioTransport, VmConfig,
    DEFAULT_HOST_VISIBLE_MIB, DEFAULT_REFRESH_HZ, MAX_DEFAULT_VCPUS, MAX_GAMEPAD_PLAYERS,
    MAX_HOST_VISIBLE_MIB, MAX_MEMORY_MIB, MAX_REFRESH_HZ, MIN_DEFAULT_VCPUS, MIN_HOST_VISIBLE_MIB,
    MIN_MEMORY_MIB, MIN_REFRESH_HZ,
};
