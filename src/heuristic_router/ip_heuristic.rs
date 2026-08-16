use std::net::IpAddr;
use tracing::trace;

/// Evaluates the IP address. 
/// Using `pub(super)` restricts this function to the parent module (`heuristic_router`).
pub(super) fn analyze_ip(ip: &IpAddr) -> f64 {
    trace!("Running IP heuristic on {}", ip);
    
    // Boilerplate heuristic: IPv4 gets a different base score than IPv6
    match ip {
        IpAddr::V4(_) => 10.0,
        IpAddr::V6(_) => 15.0,
    }
}