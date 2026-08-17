//! Bounded-memory tracker for IP states.

use std::net::IpAddr;

/// State for a single IP address.
#[derive(Debug, Clone, Copy, Default)]
pub struct IpState {
    /// Timestamp (in seconds) when this IP was last seen.
    pub last_seen: u64,
    /// EWMA Velocity, stored with 10 fractional bits (i.e. * 1024).
    pub velocity: u32,
    /// Flag indicating whether this IP is suspected of missing auxiliary assets.
    pub missing_auxiliary_assets: bool,
}

/// SpaceSaver tracks recent IPs in bounded memory, evicting the lowest-volume
/// "mouse" IPs when full.
#[derive(Debug, Clone, Copy)]
pub struct SpaceSaver {
    // Implementation details (HashMap + BTreeSet) omitted for now.
}

impl SpaceSaver {
    /// Creates a new, empty SpaceSaver.
    pub fn new() -> Self {
        Self {}
    }

    /// Fetches the state for `client_ip`. If it doesn't exist, adds it.
    /// If memory is full, evicts the lowest-volume IP.
    pub async fn get_or_add_and_evict(&self, _client_ip: IpAddr) -> IpState {
        // TODO: Implement SpaceSaver logic
        IpState::default()
    }

    /// Updates the state for the given `client_ip`.
    pub async fn update(&self, _client_ip: IpAddr, _state: IpState) {
        // TODO: Implement SpaceSaver state update
    }
}
