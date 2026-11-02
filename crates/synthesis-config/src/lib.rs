//! Layered configuration, `config.json/v1` validation and credential resolution
//! (docs/specs/config.md). Faithful port of Node `config.js`, `configschema.js`,
//! `providercfg.js` and the read side of `auth.js`.
//!
//! Policy: only `SYNT_*` environment variables are read for layering and credentials;
//! no writes of credentials; no keychain.

mod auth;
mod config;
mod env;
mod json;
mod provider;
mod schema;

pub use auth::*;
pub use config::*;
pub use env::*;
pub use provider::*;
pub use schema::*;
