//! edge-shared — types, JWT verification, revocation helpers shared across edge crates.
//!
//! This crate is the dependency leaf — no other edge crate may depend on it cyclically.

pub mod errors;
pub mod jwt;
pub mod revocation;
pub mod types;

pub use errors::EdgeError;
pub use revocation::HttpRevocationOracle;
