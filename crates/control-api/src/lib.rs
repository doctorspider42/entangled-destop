//! Control surface shared by frontends (backlog EPIC 12). The CLI is the
//! first consumer; the crate exists so a future GUI talks to the same model.

mod config;
pub mod control;
/// The refresh rate a new machine's virtual monitor gets: the host monitor's,
/// rounded and clamped ([`refresh::default_refresh_hz`]); the host query is
/// behind the `host-display` feature.
pub mod refresh;
pub mod wsl;
/// Getting a Linux engine into WSL: the pinned download, the digest check and
/// the copy into the distribution. Behind the `engine-install` feature, which
/// only the two binaries that perform it turn on (see Cargo.toml).
#[cfg(feature = "engine-install")]
pub mod wsl_engine;

pub use config::{
    default_vcpus, format_mac, host_default_vcpus, new_machine_mac, parse_mac, BootMode,
    BootSection, CdromSection, ConfigError, DiskSection, DisplaySection, GamepadBackend,
    GamepadSection, GpuRenderer, NetworkBackend, NetworkSection, SoundBackend, SoundSection,
    VirglIsolation, VirtioTransport, VmConfig, DEFAULT_HOST_VISIBLE_MIB,
    DEFAULT_NEW_MACHINE_NETWORK, DEFAULT_REFRESH_HZ, MAX_DEFAULT_VCPUS, MAX_GAMEPAD_PLAYERS,
    MAX_GPU_MEMORY_MIB, MAX_HOST_VISIBLE_MIB, MAX_MEMORY_MIB, MAX_REFRESH_HZ, MIN_DEFAULT_VCPUS,
    MIN_GPU_MEMORY_MIB, MIN_HOST_VISIBLE_MIB, MIN_MEMORY_MIB, MIN_REFRESH_HZ,
};
