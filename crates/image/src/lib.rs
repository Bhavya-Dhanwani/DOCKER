//! OCI images: parse references, pull from a registry, keep layers deduplicated on disk.
mod add;
mod archive;
mod auth;
pub mod dockerfile;
mod error;
mod http;
mod ignore;
mod info;
mod layer;
mod pack;
mod pull;
mod push;
mod reference;
mod remote;
mod search;
mod store;

pub use error::{Error, Result};
pub use add::{extract_archive, fetch_url, is_url, url_filename};
pub use archive::{load, save};
pub use auth::{login, logout};
pub use ignore::{expand_glob, expand_with, Ignore};
pub use info::{history, inspect, HistoryRow};
pub use pull::{pull, pull_for};
pub use search::{search, Hit};
#[cfg(target_os = "linux")]
pub use layer::unpack_rootless;
pub use push::push;
pub use reference::{normalize_registry, Reference};
pub use store::{dir_size, Health, ImageMeta, LayerRef, Store};
