use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::{Arc, RwLock as StdRwLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::Mutex,
    time::timeout,
};

use crate::{
    DestinationHost, DiscoveredPeer, DisplayInput, DisplayMuxError, MonitorFingerprint,
    PeerDiscovery,
};

pub const DEFAULT_AGENT_PORT: u16 = 47_653;
pub const DISPLAYMUX_SERVICE_TYPE: &str = "_displaymux._tcp.local.";
/// Bumped whenever the agent wire protocol gains a field that changes how a
/// request must be interpreted (not just an additive/ignorable one). A peer
/// reporting a version below this may not understand per-monitor requests.
pub const AGENT_PROTOCOL_VERSION: u32 = 2;
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(30);
const MAX_PACKET_BYTES: usize = 8 * 1024;

type HmacSha256 = Hmac<Sha256>;

pub struct MdnsPeerDiscovery {
    _daemon: ServiceDaemon,
    local_id: String,
    peers: Arc<StdRwLock<HashMap<String, DiscoveredPeer>>>,
}

impl MdnsPeerDiscovery {
    pub fn start(local_platform: DestinationHost, port: u16) -> Result<Self, DisplayMuxError> {
        let host_name = hostname::get()
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?
            .to_string_lossy()
            .trim()
            .to_owned();
        let friendly_name = if host_name.is_empty() {
            "DisplayMux".to_owned()
        } else {
            host_name
        };
        let dns_label = dns_label(&friendly_name);
        let dns_host_name = format!("{dns_label}.local.");
        let mac_address = match mac_address::get_mac_address() {
            Ok(address) => address.map(|address| address.to_string()),
            Err(error) => {
                tracing::warn!(error = %error, "unable to advertise wake-on-lan address");
                None
            }
        };
        let platform = platform_name(local_platform);
        let local_id = peer_id(&friendly_name, platform, mac_address.as_deref());

        let mut properties = HashMap::from([
            ("id".to_owned(), local_id.clone()),
            ("name".to_owned(), friendly_name.clone()),
            ("platform".to_owned(), platform.to_owned()),
            ("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        ]);
        if let Some(address) = &mac_address {
            properties.insert("mac".to_owned(), address.clone());
        }

        let service = ServiceInfo::new(
            DISPLAYMUX_SERVICE_TYPE,
            &friendly_name,
            &dns_host_name,
            "",
            port,
            properties,
        )
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))?
        .enable_addr_auto();
        let daemon =
            ServiceDaemon::new().map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        daemon
            .register(service)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let receiver = daemon
            .browse(DISPLAYMUX_SERVICE_TYPE)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let peers = Arc::new(StdRwLock::new(HashMap::new()));
        let observed_peers = Arc::clone(&peers);
        let observed_local_id = local_id.clone();

        thread::Builder::new()
            .name("displaymux-mdns".to_owned())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(service) => {
                            let Some(peer) = discovered_peer(&service) else {
                                continue;
                            };
                            if peer.id == observed_local_id {
                                continue;
                            }
                            if let Ok(mut current) = observed_peers.write() {
                                current.insert(service.get_fullname().to_owned(), peer);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            if let Ok(mut current) = observed_peers.write() {
                                current.remove(&fullname);
                            }
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;

        tracing::info!(host = %friendly_name, "DisplayMux mDNS discovery started");
        Ok(Self {
            _daemon: daemon,
            local_id,
            peers,
        })
    }

    pub fn local_id(&self) -> &str {
        &self.local_id
    }
}

impl PeerDiscovery for MdnsPeerDiscovery {
    fn peers(&self) -> Result<Vec<DiscoveredPeer>, DisplayMuxError> {
        let current = self
            .peers
            .read()
            .map_err(|_| DisplayMuxError::Backend("無法讀取區域網路搜尋結果".to_owned()))?;
        let mut peers = current.values().cloned().collect::<Vec<_>>();
        peers.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
        Ok(peers)
    }
}

fn discovered_peer(service: &mdns_sd::ResolvedService) -> Option<DiscoveredPeer> {
    let platform = match service.get_property_val_str("platform")? {
        "windows" => DestinationHost::Windows,
        "mac" => DestinationHost::Mac,
        _ => return None,
    };
    let address = preferred_address(
        service
            .get_addresses()
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr),
    )?;
    let name = service
        .get_property_val_str("name")
        .map(str::to_owned)
        .unwrap_or_else(|| service.get_hostname().trim_end_matches('.').to_owned());
    let id = service
        .get_property_val_str("id")
        .map(str::to_owned)
        .unwrap_or_else(|| service.get_fullname().to_owned());
    let mac_address = service
        .get_property_val_str("mac")
        .and_then(|value| value.parse::<MacAddress>().ok())
        .map(|value| value.to_string());
    Some(DiscoveredPeer {
        id,
        name,
        platform,
        address,
        port: service.get_port(),
        mac_address,
    })
}

fn preferred_address(addresses: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    addresses
        .filter(|address| !address.is_loopback())
        .min_by_key(|address| match address {
            IpAddr::V4(address) if address.is_private() => 0,
            IpAddr::V4(_) => 1,
            IpAddr::V6(_) => 2,
        })
}

fn platform_name(platform: DestinationHost) -> &'static str {
    match platform {
        DestinationHost::Windows => "windows",
        DestinationHost::Mac => "mac",
    }
}

fn peer_id(host_name: &str, platform: &str, mac_address: Option<&str>) -> String {
    let identity = mac_address.unwrap_or(host_name);
    format!(
        "{}-{platform}",
        identity
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    )
}

fn dns_label(host_name: &str) -> String {
    let label = host_name
        .split('.')
        .next()
        .unwrap_or(host_name)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if label.is_empty() {
        "displaymux".to_owned()
    } else {
        label
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl FromStr for MacAddress {
    type Err = DisplayMuxError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value
            .split([':', '-'])
            .map(|part| u8::from_str_radix(part, 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| DisplayMuxError::InvalidMacAddress(value.to_owned()))?;

        let octets: [u8; 6] = parts
            .try_into()
            .map_err(|_| DisplayMuxError::InvalidMacAddress(value.to_owned()))?;
        Ok(Self(octets))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeTarget {
    pub mac_address: MacAddress,
    pub broadcast_address: Ipv4Addr,
    pub port: u16,
}

impl WakeTarget {
    pub async fn wake(&self) -> Result<(), DisplayMuxError> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;
        socket
            .set_broadcast(true)
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;

        let mut packet = [0_u8; 102];
        packet[..6].fill(0xff);
        for chunk in packet[6..].chunks_exact_mut(6) {
            chunk.copy_from_slice(&self.mac_address.octets());
        }

        socket
            .send_to(&packet, (self.broadcast_address, self.port))
            .await
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;
        tracing::info!(
            broadcast = %self.broadcast_address,
            port = self.port,
            "wake-on-lan packet sent"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEndpoint {
    pub address: IpAddr,
    pub port: u16,
}

impl PeerEndpoint {
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAction {
    Ping,
    SwitchInput {
        /// Which shared monitor to switch. `None` is the pre-v2 shape: the
        /// receiving agent must have exactly one shared monitor selected to
        /// accept it unambiguously (see `AGENT_PROTOCOL_VERSION`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        monitor: Option<MonitorFingerprint>,
        input: DisplayInput,
    },
    /// Best-effort notice that a paired host just switched `monitor` to
    /// `input`, so the receiver can update which host it shows as active.
    /// Agents older than this variant reject the request; senders ignore that.
    ActiveInputChanged {
        monitor: MonitorFingerprint,
        input: DisplayInput,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub timestamp_seconds: u64,
    pub nonce: String,
    pub action: AgentAction,
    pub signature: String,
}

impl AgentRequest {
    pub fn signed(
        action: AgentAction,
        nonce: impl Into<String>,
        shared_key: &[u8],
    ) -> Result<Self, DisplayMuxError> {
        let timestamp_seconds = unix_time()?;
        let nonce = nonce.into();
        let signature = sign(timestamp_seconds, &nonce, &action, shared_key)?;
        Ok(Self {
            timestamp_seconds,
            nonce,
            action,
            signature,
        })
    }

    fn verify(&self, shared_key: &[u8]) -> Result<(), DisplayMuxError> {
        let now = unix_time()?;
        if now.abs_diff(self.timestamp_seconds) > MAX_CLOCK_SKEW.as_secs() {
            return Err(DisplayMuxError::StaleRequest);
        }

        let supplied =
            hex::decode(&self.signature).map_err(|_| DisplayMuxError::AuthenticationFailed)?;
        let payload = signing_payload(self.timestamp_seconds, &self.nonce, &self.action)?;
        let mut mac = HmacSha256::new_from_slice(shared_key)
            .map_err(|_| DisplayMuxError::AuthenticationFailed)?;
        mac.update(&payload);
        mac.verify_slice(&supplied)
            .map_err(|_| DisplayMuxError::AuthenticationFailed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResponse {
    pub ready: bool,
    pub message: String,
    /// Kept for compatibility with pre-v2 clients that only read this field
    /// for their one implicit shared monitor; populated as
    /// `display_routes.first().cloned()` by v2+ responders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_route: Option<AgentDisplayRoute>,
    /// One entry per shared monitor the responder currently knows an input
    /// for. Absent on a pre-v2 peer's response, which deserializes to an
    /// empty vec via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub display_routes: Vec<AgentDisplayRoute>,
    /// The responder's `AGENT_PROTOCOL_VERSION`. Absent on a pre-v2 peer's
    /// response, which deserializes to `0` via `#[serde(default)]` — treat
    /// any value less than `AGENT_PROTOCOL_VERSION` as "does not understand
    /// per-monitor switch requests".
    #[serde(default)]
    pub protocol_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDisplayRoute {
    pub monitor: MonitorFingerprint,
    pub input: DisplayInput,
}

#[derive(Clone)]
pub struct AgentClient {
    endpoint: PeerEndpoint,
    shared_key: Arc<[u8]>,
    connect_timeout: Duration,
}

impl AgentClient {
    pub fn new(endpoint: PeerEndpoint, shared_key: impl Into<Arc<[u8]>>) -> Self {
        Self {
            endpoint,
            shared_key: shared_key.into(),
            connect_timeout: Duration::from_secs(2),
        }
    }

    pub async fn request(
        &self,
        action: AgentAction,
        nonce: impl Into<String>,
    ) -> Result<AgentResponse, DisplayMuxError> {
        let request = AgentRequest::signed(action, nonce, &self.shared_key)?;
        let stream = timeout(
            self.connect_timeout,
            TcpStream::connect(self.endpoint.socket_addr()),
        )
        .await
        .map_err(|_| DisplayMuxError::PeerUnavailable("連線逾時".to_owned()))?
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
        let (reader, mut writer) = stream.into_split();
        let mut payload = serde_json::to_vec(&request)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        payload.push(b'\n');
        writer
            .write_all(&payload)
            .await
            .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;

        let mut response = String::new();
        timeout(
            self.connect_timeout,
            BufReader::new(reader).read_line(&mut response),
        )
        .await
        .map_err(|_| DisplayMuxError::PeerUnavailable("回應逾時".to_owned()))?
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
        parse_agent_response(&response)
    }
}

pub struct AgentServer {
    bind_address: SocketAddr,
    shared_key: Arc<[u8]>,
    seen_nonces: Arc<Mutex<HashSet<String>>>,
}

impl AgentServer {
    pub fn new(bind_address: SocketAddr, shared_key: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bind_address,
            shared_key: shared_key.into(),
            seen_nonces: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn run<H, Fut>(self, handler: H) -> Result<(), DisplayMuxError>
    where
        H: Fn(AgentAction) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AgentResponse> + Send + 'static,
    {
        let listener = TcpListener::bind(self.bind_address)
            .await
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let handler = Arc::new(handler);

        loop {
            let (stream, peer) = listener
                .accept()
                .await
                .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
            let shared_key = Arc::clone(&self.shared_key);
            let seen_nonces = Arc::clone(&self.seen_nonces);
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                if let Err(error) =
                    handle_connection(stream, shared_key, seen_nonces, handler).await
                {
                    tracing::warn!(peer = %peer, error = %error, "agent request rejected");
                }
            });
        }
    }
}

async fn handle_connection<H, Fut>(
    stream: TcpStream,
    shared_key: Arc<[u8]>,
    seen_nonces: Arc<Mutex<HashSet<String>>>,
    handler: Arc<H>,
) -> Result<(), DisplayMuxError>
where
    H: Fn(AgentAction) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AgentResponse> + Send + 'static,
{
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader)
        .take(MAX_PACKET_BYTES as u64)
        .read_line(&mut line)
        .await
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
    let request: AgentRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(_) => {
            let error = DisplayMuxError::AuthenticationFailed;
            tracing::warn!(error = %error, "agent request rejected");
            return write_agent_response(&mut writer, &rejection_response(&error)).await;
        }
    };
    if let Err(error) = request.verify(&shared_key) {
        tracing::warn!(error = %error, "agent request rejected");
        return write_agent_response(&mut writer, &rejection_response(&error)).await;
    }

    let mut nonces = seen_nonces.lock().await;
    if !nonces.insert(request.nonce.clone()) {
        let error = DisplayMuxError::StaleRequest;
        drop(nonces);
        tracing::warn!(error = %error, "agent request rejected");
        return write_agent_response(&mut writer, &rejection_response(&error)).await;
    }
    if nonces.len() > 2_048 {
        nonces.clear();
    }
    drop(nonces);

    let response = handler(request.action).await;
    write_agent_response(&mut writer, &response).await
}

async fn write_agent_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &AgentResponse,
) -> Result<(), DisplayMuxError> {
    let mut payload = serde_json::to_vec(&response)
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    payload.push(b'\n');
    writer
        .write_all(&payload)
        .await
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))
}

fn rejection_response(error: &DisplayMuxError) -> AgentResponse {
    let message = match error {
        DisplayMuxError::StaleRequest => "連線驗證失敗，請確認兩台主機的系統時間已同步後再試一次",
        _ => "配對密碼不一致，請在兩台主機輸入完全相同的配對密碼並重新儲存",
    };
    AgentResponse {
        ready: false,
        message: message.to_owned(),
        display_route: None,
        display_routes: Vec::new(),
        protocol_version: AGENT_PROTOCOL_VERSION,
    }
}

fn parse_agent_response(response: &str) -> Result<AgentResponse, DisplayMuxError> {
    if response.trim().is_empty() {
        return Err(DisplayMuxError::PeerUnavailable(
            "另一台主機未回傳結果；請確認兩台主機皆已更新至最新版，並重新檢查配對密碼".to_owned(),
        ));
    }
    serde_json::from_str(response)
        .map_err(|error| DisplayMuxError::PeerUnavailable(format!("回應格式無效：{error}")))
}

fn sign(
    timestamp_seconds: u64,
    nonce: &str,
    action: &AgentAction,
    shared_key: &[u8],
) -> Result<String, DisplayMuxError> {
    let payload = signing_payload(timestamp_seconds, nonce, action)?;
    let mut mac = HmacSha256::new_from_slice(shared_key)
        .map_err(|_| DisplayMuxError::AuthenticationFailed)?;
    mac.update(&payload);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn signing_payload(
    timestamp_seconds: u64,
    nonce: &str,
    action: &AgentAction,
) -> Result<Vec<u8>, DisplayMuxError> {
    serde_json::to_vec(&(timestamp_seconds, nonce, action))
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

fn unix_time() -> Result<u64, DisplayMuxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_common_mac_address_formats() {
        let colon = "AA:BB:CC:DD:EE:FF".parse::<MacAddress>().unwrap();
        let dash = "aa-bb-cc-dd-ee-ff".parse::<MacAddress>().unwrap();
        assert_eq!(colon, dash);
        assert_eq!(colon.to_string(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn rejects_incomplete_mac_address() {
        assert!(matches!(
            "AA:BB:CC".parse::<MacAddress>(),
            Err(DisplayMuxError::InvalidMacAddress(_))
        ));
    }

    #[test]
    fn creates_stable_dns_safe_peer_identity() {
        assert_eq!(dns_label("Henry's MacBook.local"), "henry-s-macbook");
        assert_eq!(
            peer_id("Henry-PC", "windows", Some("AA:BB:CC:DD:EE:FF")),
            "aabbccddeeff-windows"
        );
    }

    #[test]
    fn prefers_private_ipv4_for_lan_connections() {
        let addresses = HashSet::from([
            "fe80::1234".parse().unwrap(),
            "192.168.1.25".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ]);
        assert_eq!(
            preferred_address(addresses.iter().copied()),
            Some("192.168.1.25".parse().unwrap())
        );
    }

    #[test]
    fn signed_request_rejects_tampering() {
        let key = b"a test key that is never persisted";
        let mut request = AgentRequest::signed(AgentAction::Ping, "nonce-1", key).unwrap();
        request.action = AgentAction::SwitchInput {
            monitor: None,
            input: DisplayInput::new(0x11).unwrap(),
        };
        assert_eq!(
            request.verify(key),
            Err(DisplayMuxError::AuthenticationFailed)
        );
    }

    #[test]
    fn authentication_rejection_explains_pairing_password_mismatch() {
        let response = rejection_response(&DisplayMuxError::AuthenticationFailed);
        assert!(!response.ready);
        assert!(response.message.contains("配對密碼不一致"));
    }

    #[test]
    fn empty_legacy_response_is_not_reported_as_json_eof() {
        let error = parse_agent_response("").unwrap_err();
        assert!(matches!(error, DisplayMuxError::PeerUnavailable(_)));
        assert!(!error.to_string().contains("EOF"));
        assert!(error.to_string().contains("配對密碼"));
    }

    #[test]
    fn older_agent_response_without_display_route_remains_compatible() {
        let response: AgentResponse =
            serde_json::from_str(r#"{"ready":true,"message":"ready"}"#).unwrap();
        assert!(response.ready);
        assert!(response.display_route.is_none());
        assert!(response.display_routes.is_empty());
        assert_eq!(response.protocol_version, 0);
    }

    #[test]
    fn pre_v2_switch_input_request_without_monitor_field_still_deserializes() {
        let action: AgentAction =
            serde_json::from_str(r#"{"type":"switch_input","input":17}"#).unwrap();
        assert_eq!(
            action,
            AgentAction::SwitchInput {
                monitor: None,
                input: DisplayInput::new(0x11).unwrap(),
            }
        );
    }

    #[test]
    fn switch_input_with_monitor_serializes_the_monitor_field() {
        let action = AgentAction::SwitchInput {
            monitor: Some(MonitorFingerprint::new("ACM", "1234", Some("SERIAL-1"))),
            input: DisplayInput::new(0x11).unwrap(),
        };
        let serialized = serde_json::to_value(&action).unwrap();
        assert!(serialized.get("monitor").is_some());
    }

    #[test]
    fn active_input_changed_notice_round_trips_with_its_monitor_and_input() {
        let action = AgentAction::ActiveInputChanged {
            monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
            input: DisplayInput::new(0x08).unwrap(),
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"active_input_changed""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[tokio::test]
    async fn wake_packet_has_the_expected_shape() {
        let mac = "01:23:45:67:89:AB".parse::<MacAddress>().unwrap();
        let mut packet = [0_u8; 102];
        packet[..6].fill(0xff);
        for chunk in packet[6..].chunks_exact_mut(6) {
            chunk.copy_from_slice(&mac.octets());
        }
        assert!(packet[..6].iter().all(|byte| *byte == 0xff));
        assert!(packet[6..]
            .chunks_exact(6)
            .all(|chunk| chunk == mac.octets()));
    }
}
