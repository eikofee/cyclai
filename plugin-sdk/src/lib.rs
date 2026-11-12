//! Protocol between CyclAI and its activity-source plugins.
//!
//! A plugin is a small HTTP service (typically its own container) that holds
//! the credentials for one source and answers three requests from CyclAI:
//!
//! | Request | Answer |
//! |---|---|
//! | `GET /manifest` | [`Manifest`]: who it is, whether it is configured |
//! | `GET /activities?after=<epoch>` | [`ListResponse`]: activities started after that time (0 = all) |
//! | `GET /activities/{id}` | an [`Activity`] (JSON) or the original `.fit` (`application/vnd.ant.fit`); 404 when there is nothing to give |
//!
//! CyclAI only ever calls the plugin: a plugin has no access to CyclAI, its
//! database or its API. Each request carries an HMAC signature made with a
//! secret shared by CyclAI and that one plugin (see [`sign`]).

mod signing;
mod types;

pub use signing::{sign, verify, AuthError, MAX_SKEW_SECONDS, MIN_SECRET_LEN};
pub use types::*;

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub use client::{Client, ClientError};

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::{app, serve};

/// Bumped on a breaking change of the protocol; shown in [`Manifest::protocol`].
pub const PROTOCOL_VERSION: u32 = 1;

pub const HEADER_TIMESTAMP: &str = "x-cyclai-timestamp";
pub const HEADER_SIGNATURE: &str = "x-cyclai-signature";
pub const FIT_CONTENT_TYPE: &str = "application/vnd.ant.fit";

/// Size limits, so a misbehaving plugin cannot exhaust CyclAI's memory.
pub const MAX_REQUEST_BODY: usize = 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ACTIVITY_BYTES: usize = 64 * 1024 * 1024;
