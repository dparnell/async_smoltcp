use std::result::Result;
use std::sync::Arc;
use std::time::Duration;

pub enum VpnConnectionError {
    Unknown(String),
    NoHost { s: String },
    NoInternet,
}

pub enum VpnDisconnectionError {
    Unknown(String),
    NotRunning,
}

pub type VpnPacket = Vec<u8>;

pub type PhySend = Arc<dyn Fn(Vec<u8>) -> Result<(), ()> + Send + Sync>;
pub type PhyReceive = Arc<dyn Fn(Option<Duration>) -> Option<Vec<u8>> + Send + Sync>;
pub type VpnConnect = Arc<dyn Fn() -> Result<(), VpnConnectionError> + Send + Sync>;
pub type VpnDisconnect = Arc<dyn Fn() -> Result<(), VpnDisconnectionError> + Send + Sync>;

pub enum PhySendError{
    Unknown(String)
}
pub enum PhyReceiveError{
    NoDataAvailable,
    Unknown(String)
}

pub enum VpnEvent {
    Disconnected,
    Connected
}

//TODO: evaluate if needed to implement set_phy_receive_c and phy_receive_c
pub trait VpnClient {
    fn set_username(&mut self, s: Option<&str>);
    fn set_password(&mut self, s: Option<&str>);
    //fn set_vpn_connect(&mut self, f: VpnConnect);
    //fn set_vpn_disconnect(&mut self, f: VpnDisconnect);
    fn vpn_connect(&mut self) -> Result<(), VpnConnectionError>;
    fn vpn_disconnect(&mut self) -> Result<(), VpnDisconnectionError>;
    //fn set_phy_send(&mut self, f: PhySend);
    //fn set_phy_receive(&mut self, f: PhyReceive);
    fn phy_receive(&mut self, timeout: Option<Duration>, f: &mut dyn FnMut(&[u8])) -> std::result::Result<(), PhyReceiveError>;
    //TODO: change Err return
    fn phy_send(&self, vpn_packet: &[u8]) -> std::result::Result<(), PhySendError> ;
    //fn set_on_produce(&self, f: Arc<dyn Fn(VpnPacket)>);
    //fn set_on_receive(&mut self, f: Arc<dyn Fn()-> VpnPacket>);
}