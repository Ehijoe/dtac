//! Error module for the DTAC library.
//!
//! This module defines the core error types and a specialized result type
//! used throughout the dynamic traffic-aware cache system.

use thiserror::Error;

/// Represents all possible errors that can occur within the DTAC library.
#[derive(Error, Debug)]
pub enum DTACError {
    /// Error returned when an IP address string cannot be parsed into an `IpAddr`.
    #[error("Failed to parse IP address: {0}")]
    IpParseError(String),

    /// Error returned when the router configuration is invalid.
    #[error("Router configuration invalid: {0}")]
    ConfigError(String),

    /// Error returned when an ASN database or ASN list cannot be read.
    #[error("ASN database error: {0}")]
    AsnDbError(String),
}

/// A specialized `Result` type for DTAC operations.
pub type Result<T> = std::result::Result<T, DTACError>;
