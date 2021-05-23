mod async_smoltcp;
mod virtual_tun;
mod smol_stack_virtual_tun;
mod socket;
mod vpn_client;
pub use async_smoltcp::*;
pub const DEFAULT_MTU: usize = 1500;
