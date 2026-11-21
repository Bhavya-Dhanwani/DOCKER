//! Tiny container runtime. `platform` is the OS backend; everything else is portable.
pub mod cli;
pub mod env;
pub mod error;
pub mod networks;
pub mod registry;
pub mod security;
pub mod stats;
pub mod user;
pub mod volumes;
pub mod vm;

/// OS backend: `run`, `stats`, `stop`.
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
pub mod platform;

#[cfg(windows)]
#[path = "windows/mod.rs"]
pub mod platform;

#[cfg(not(any(target_os = "linux", windows)))]
#[path = "unsupported/mod.rs"]
pub mod platform;
