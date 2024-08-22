use std::net::SocketAddr;

pub const DEVSERVER_ADDR_ENV: &str = "DIOXUS_DEVSERVER_ADDR";
pub const FULLSTACK_ADDRESS_ENV: &str = "DIOXUS_FULLSTACK_ADDRESS";

/// when targetting ios, we need to set a prefix to the argument such that it gets picked up by simctl
pub const IOS_DEVSERVER_ADDR_ENV: &str = "SIMCTL_CHILD_DEVSERVER_ADDR";

/// Get the address of the devserver
pub fn devserver_addr() -> Option<SocketAddr> {
    std::env::var(DEVSERVER_ADDR_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
}

pub fn fullstack_address() -> Option<SocketAddr> {
    std::env::var(FULLSTACK_ADDRESS_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
}
