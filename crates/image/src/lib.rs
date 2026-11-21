//! OCI images: parse references, pull from a registry, keep layers deduplicated on disk.
mod add;
mod archive;
mod auth;
pub mod dockerfile;
mod error;
mod http;
mod ignore;
mod layer;
mod pack;
mod pull;
mod push;
mod reference;
mod remote;
mod store;

pub use error::{Error, Result};
pub use add::{extract_archive, fetch_url, is_url, url_filename};
pub use archive::{load, save};
pub use auth::{login, logout};
pub use ignore::Ignore;
pub use pull::pull;
pub use push::push;
pub use reference::{normalize_registry, Reference};
pub use store::{dir_size, Health, ImageMeta, LayerRef, Store};
