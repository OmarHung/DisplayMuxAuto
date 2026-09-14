mod capabilities;
mod domain;
#[cfg(any(target_os = "macos", test))]
mod edid;
mod error;
mod network;
mod port;
mod service;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

pub use domain::{
    DestinationHost, DiscoveredPeer, DisplayInput, DisplayMuxProfile, MonitorDescriptor,
    MonitorFingerprint, MonitorId, MonitorResolution, ResolutionSource, SwitchMode, SwitchOutcome,
};
pub use error::DisplayMuxError;
pub use network::{
    AgentAction, AgentClient, AgentDisplayRoute, AgentRequest, AgentResponse, AgentServer,
    MacAddress, MdnsPeerDiscovery, PeerEndpoint, WakeTarget, AGENT_PROTOCOL_VERSION,
    DEFAULT_AGENT_PORT,
};
pub use port::{MonitorControl, PeerDiscovery};
pub use service::DisplayMuxService;
