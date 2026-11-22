//! memdusa-core: storage and domain logic for the memdusa agent memory &
//! execution service. Transport-agnostic; used by the HTTP API, the MCP
//! server and the CLI.

pub mod auth;
pub mod brief;
pub mod embed;
pub mod model;
pub mod redact;
pub mod retention;
pub mod review;
pub mod store;
pub mod transfer;

pub use brief::Brief;
pub use store::{Backend, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
