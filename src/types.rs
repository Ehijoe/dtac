//! Module containing common data structures and types used in DTAC.

use std::collections::HashMap;
use std::net::IpAddr;

/// Represents the incoming request data needed by the router and cache.
#[derive(Debug, Clone)]
pub struct RequestData {
    /// The IP address of the client making the request.
    pub ip_address: IpAddr,
    /// The requested path (e.g., "/api/data").
    pub path: String,
    /// The HTTP method used for the request (e.g., "GET", "POST").
    pub method: String,
    /// The HTTP headers associated with the request.
    pub headers: HashMap<String, String>,
}
