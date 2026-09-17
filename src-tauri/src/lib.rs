mod host_alias;
mod host_order;
mod input_label;
mod monitor_identity;

use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use displaymux_core::{
    AgentAction, AgentClient, AgentDisplayRoute, AgentResponse, AgentServer, DestinationHost,
    DiscoveredPeer, DisplayInput, DisplayMuxError, DisplayMuxProfile, DisplayMuxService, HostAlias,
    InputLabel, LocalHostIdentity, MacAddress, MdnsPeerDiscovery, MonitorControl,
    MonitorDescriptor, MonitorFingerprint, MonitorIdentityLink, PeerDiscovery, PeerEndpoint,
    ResolutionSource, SwitchMode, SwitchOutcome, WakeTarget, AGENT_PROTOCOL_VERSION,
    DEFAULT_AGENT_PORT,
};
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;
use tokio::{sync::Mutex, time::sleep};

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static UI_LOCALE: AtomicU64 = AtomicU64::new(0);
const MIN_SHARED_KEY_LENGTH: usize = 8;
const DEFAULT_HOST_SWITCHER_SHORTCUT: &str = "CommandOrControl+Alt+Space";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiLocale {
    English,
    TraditionalChinese,
}

impl UiLocale {
    fn current() -> Self {
        if UI_LOCALE.load(Ordering::Relaxed) == 1 {
            Self::TraditionalChinese
        } else {
            Self::English
        }
    }
}

fn locale_from_tag(locale: &str) -> UiLocale {
    let normalized = locale.to_ascii_lowercase();
    if normalized.starts_with("zh-tw")
        || normalized.starts_with("zh-hant")
        || normalized.starts_with("zh-hk")
        || normalized.starts_with("zh-mo")
    {
        UiLocale::TraditionalChinese
    } else {
        UiLocale::English
    }
}

fn ui_text(zh_tw: &'static str, en: &'static str) -> &'static str {
    match UiLocale::current() {
        UiLocale::TraditionalChinese => zh_tw,
        UiLocale::English => en,
    }
}

fn input_label(vendor_indexed: bool, input: DisplayInput) -> String {
    if !vendor_indexed {
        return localized_input_name(input);
    }
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!("輸入 {}", input.value()),
        UiLocale::English => format!("Input {}", input.value()),
    }
}

/// An input's name with the user's note in front, e.g. "USB-C（輸入 8）".
fn noted_input_label(
    settings: &AppSettings,
    selected: &SelectedMonitor,
    input: DisplayInput,
) -> String {
    let base = input_label(selected.vendor_indexed_inputs, input);
    match input_label::label_for(&settings.input_labels, &selected.fingerprint, input) {
        Some(note) => match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{note}（{base}）"),
            UiLocale::English => format!("{note} ({base})"),
        },
        None => base,
    }
}

fn localized_input_name(input: DisplayInput) -> String {
    let standard_name = match (UiLocale::current(), input.value()) {
        (_, 0x01) => Some("VGA"),
        (_, 0x03) => Some("DVI"),
        (_, 0x0f) => Some("DP"),
        (_, 0x10) => Some("DP 2"),
        (_, 0x1b) => Some("Type-C"),
        (UiLocale::TraditionalChinese, 0x05) => Some("複合視訊 1"),
        (UiLocale::TraditionalChinese, 0x06) => Some("複合視訊 2"),
        (UiLocale::TraditionalChinese, 0x09) => Some("電視調諧器 1"),
        (UiLocale::TraditionalChinese, 0x0a) => Some("電視調諧器 2"),
        (UiLocale::TraditionalChinese, 0x0b) => Some("電視調諧器 3"),
        (UiLocale::TraditionalChinese, 0x0c) => Some("色差視訊 1"),
        (UiLocale::TraditionalChinese, 0x0d) => Some("色差視訊 2"),
        (UiLocale::TraditionalChinese, 0x0e) => Some("色差視訊 3"),
        _ => input.standard_name(),
    };
    match standard_name {
        Some(name) => name.to_owned(),
        None => match UiLocale::current() {
            UiLocale::TraditionalChinese => "其他輸入".to_owned(),
            UiLocale::English => "Other input".to_owned(),
        },
    }
}

#[tauri::command]
fn set_locale(locale: String, app: AppHandle) -> Result<(), String> {
    let selected = locale_from_tag(&locale);
    UI_LOCALE.store(
        u64::from(selected == UiLocale::TraditionalChinese),
        Ordering::Relaxed,
    );
    #[cfg(target_os = "windows")]
    if let Some(tray) = app.tray_by_id("displaymux") {
        use tauri::menu::MenuBuilder;
        let menu = MenuBuilder::new(&app)
            .text("tray-open", ui_text("開啟 DisplayMux", "Open DisplayMux"))
            .separator()
            .text("tray-quit", ui_text("結束 DisplayMux", "Quit DisplayMux"))
            .build()
            .map_err(user_error)?;
        tray.set_menu(Some(menu)).map_err(user_error)?;
    }
    #[cfg(not(target_os = "windows"))]
    let _ = app;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectedMonitor {
    name: String,
    fingerprint: MonitorFingerprint,
    #[serde(default)]
    max_resolution: Option<displaymux_core::MonitorResolution>,
    #[serde(default)]
    resolution_source: Option<ResolutionSource>,
    // Formerly top-level fields on `AppSettings`; each selected monitor now
    // carries its own live input state so N monitors can be tracked at once.
    #[serde(default)]
    local_input: Option<DisplayInput>,
    #[serde(default)]
    supported_inputs: Option<Vec<DisplayInput>>,
    // True when `supported_inputs` is the display's private 1..=max index
    // list rather than MCCS codes, because the advertised capabilities did
    // not even contain the input it was actually showing. Values are then
    // labelled "Input N" instead of by MCCS name.
    #[serde(default)]
    vendor_indexed_inputs: bool,
    // "local" or a peer id: whichever route was last confirmed as the
    // monitor's active input by a successful switch. Not re-derived from a
    // live DDC read, since some displays cannot be read back reliably once
    // switched away from (see macOS DDC/CI limitations in product-facts.md).
    #[serde(default)]
    active_route: Option<String>,
    /// When a switch or a paired host's notice last confirmed `active_route`
    /// (Unix milliseconds). Live reads cannot override it until
    /// `ACTIVE_ROUTE_SETTLE_MS` later. In memory only.
    #[serde(default, skip_serializing)]
    active_route_confirmed_at_ms: u64,
}

impl From<&MonitorDescriptor> for SelectedMonitor {
    fn from(monitor: &MonitorDescriptor) -> Self {
        Self {
            name: monitor.name.clone(),
            fingerprint: monitor.fingerprint.clone(),
            max_resolution: monitor.max_resolution,
            resolution_source: monitor.resolution_source,
            local_input: None,
            supported_inputs: None,
            vendor_indexed_inputs: false,
            active_route: None,
            active_route_confirmed_at_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonitorInputAssignment {
    monitor: MonitorFingerprint,
    input: DisplayInput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostRoute {
    id: String,
    name: String,
    platform: DestinationHost,
    address: String,
    port: u16,
    mac_address: String,
    #[serde(default)]
    inputs: Vec<MonitorInputAssignment>,
}

impl HostRoute {
    fn input_for(&self, fingerprint: &MonitorFingerprint) -> Option<DisplayInput> {
        self.inputs
            .iter()
            .find(|assignment| assignment.monitor.matches_exactly(fingerprint))
            .map(|assignment| assignment.input)
    }

    fn set_input_for(&mut self, fingerprint: &MonitorFingerprint, input: Option<DisplayInput>) {
        self.inputs
            .retain(|assignment| !assignment.monitor.matches_exactly(fingerprint));
        if let Some(input) = input {
            self.inputs.push(MonitorInputAssignment {
                monitor: fingerprint.clone(),
                input,
            });
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AppSettings {
    local_host: DestinationHost,
    shared_monitors: Vec<SelectedMonitor>,
    peers: Vec<HostRoute>,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
    onboarding_completed: bool,
    host_switcher_enabled: bool,
    host_switcher_shortcut: String,
    /// Host card order shared with paired hosts, as discovery ids (see
    /// `host_order`). Backend-owned: changed only through `set_host_order` or a
    /// paired host's notice, never by `save_settings`.
    host_order: Vec<String>,
    /// When `host_order` last changed (Unix milliseconds), so the newest order
    /// wins when several hosts reorder.
    host_order_updated_at_ms: u64,
    /// Custom host names shared with paired hosts (see `host_alias`).
    /// Backend-owned like `host_order`.
    host_aliases: Vec<HostAlias>,
    /// Notes for shared display inputs, shared with paired hosts (see
    /// `input_label`). Backend-owned like `host_order`.
    input_labels: Vec<InputLabel>,
    /// Which EDID identities the user declared to be the same physical display,
    /// shared with paired hosts (see `monitor_identity`). Backend-owned like
    /// `host_order`.
    monitor_identity_links: Vec<MonitorIdentityLink>,
    /// Whether the shared display list has ever been decided — by the user or
    /// by the one-time auto-select. An empty list means "none chosen" only
    /// until then; afterwards it means the user emptied it on purpose, and
    /// auto-select must not undo that.
    shared_monitors_chosen: bool,
    /// This computer's `LocalHostIdentity::id`, fixed the first time it is
    /// worked out. Peers store it to name this host in their pairings, host
    /// order and custom names, so it must never be re-derived: see
    /// `identifies_the_machine`. Backend-owned like `host_order`.
    local_host_id: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            shared_monitors: Vec::new(),
            peers: Vec::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
            onboarding_completed: false,
            host_switcher_enabled: false,
            host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
            host_order: Vec::new(),
            host_order_updated_at_ms: 0,
            host_aliases: Vec::new(),
            input_labels: Vec::new(),
            monitor_identity_links: Vec::new(),
            shared_monitors_chosen: false,
            local_host_id: String::new(),
        }
    }
}

/// A stable, opaque, frontend-facing id for a monitor identity. Not
/// persisted — recomputed from the fingerprint on every call. Kept local to
/// this crate because `MonitorFingerprint::stable_key()` in `displaymux-core`
/// is `pub(crate)` there and not visible here.
fn monitor_key(fingerprint: &MonitorFingerprint) -> String {
    format!(
        "{}:{}:{}",
        fingerprint.manufacturer_id,
        fingerprint.product_code,
        fingerprint.serial_number.as_deref().unwrap_or("")
    )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherOption {
    id: String,
    name: String,
    platform: DestinationHost,
    input_name: Option<String>,
    is_local: bool,
    available: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherMonitor {
    monitor_key: String,
    name: String,
    hosts: Vec<HostSwitcherOption>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherState {
    monitors: Vec<HostSwitcherMonitor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutCheckResult {
    available: bool,
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LegacySettings {
    local_host: DestinationHost,
    peer_id: String,
    peer_name: String,
    peer_ip: String,
    peer_port: u16,
    peer_mac: String,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
}

impl Default for LegacySettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            peer_id: String::new(),
            peer_name: String::new(),
            peer_ip: String::new(),
            peer_port: DEFAULT_AGENT_PORT,
            peer_mac: String::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
        }
    }
}

struct AppRuntime {
    settings: Arc<RwLock<AppSettings>>,
    settings_path: PathBuf,
    agent_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    discovery: Option<MdnsPeerDiscovery>,
    /// This computer's discovery id, used to name it in the shared host order.
    local_host_id: String,
    /// The machine name paired hosts discover this computer by. Used as the
    /// default for its host card, so this computer reads the same on both
    /// sides until the user renames it.
    local_host_name: String,
    /// The input last announced to paired hosts per shared display, keyed by
    /// `monitor_key`, so a repeating scan announces a value only once.
    announced_inputs: std::sync::Mutex<HashMap<String, DisplayInput>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedMonitorStatus {
    monitor_key: String,
    fingerprint: MonitorFingerprint,
    name: String,
    ddc_available: bool,
    display_state: SharedDisplayState,
    status_text: String,
    connection: Option<displaymux_core::MonitorConnection>,
    connection_input_conflict: bool,
}

/// Whether this computer can use a shared display right now, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum SharedDisplayState {
    /// This computer reads the display's input over DDC/CI.
    Ready,
    /// The display is showing a paired host. Some displays (the MSI MPG 274U
    /// over USB-C, for one) stop answering DDC/CI on inputs they are not
    /// showing, so this is expected rather than a fault; switching back falls
    /// back to asking the paired host.
    OnOtherHost,
    /// Missing, or unreadable with no known reason.
    Unavailable,
}

/// Whether `monitor`, present right now, is the display `selected` names — its
/// own identity or one the user merged into it. Deciding *which* display is
/// meant is separate from deciding what may be written to: every switch still
/// demands an exact fingerprint match (see `run_switch`).
fn is_selected_display(
    links: &[MonitorIdentityLink],
    selected: &SelectedMonitor,
    monitor: &MonitorDescriptor,
) -> bool {
    monitor_identity::is_same_display(links, &selected.fingerprint, &monitor.fingerprint)
}

fn shared_display_state(ddc_readable: bool, active_route: Option<&str>) -> SharedDisplayState {
    if ddc_readable {
        SharedDisplayState::Ready
    } else if active_route.is_some_and(|route| route != host_order::LOCAL_ROUTE_ID) {
        SharedDisplayState::OnOtherHost
    } else {
        SharedDisplayState::Unavailable
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardState {
    platform: &'static str,
    local_host: DestinationHost,
    agent_configured: bool,
    monitors: Vec<MonitorDescriptor>,
    uncontrollable_monitors: Vec<MonitorDescriptor>,
    shared: Vec<SharedMonitorStatus>,
    selection_notices: Vec<String>,
    monitor_identity_claims: Vec<MonitorIdentityClaim>,
    /// The name paired hosts discover this computer by, so its own card can
    /// default to it rather than to a generic "this Mac".
    local_host_name: String,
}

/// One "these two identities are the same display" claim, as the settings page
/// shows it. The keys go back to `set_monitor_identity_link`, so a claim can be
/// withdrawn even when neither identity is present or shared any more.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorIdentityClaim {
    alias_key: String,
    alias_label: String,
    primary_key: String,
    primary_label: String,
}

/// `MANUFACTURER / PRODUCT`, enough to tell two identities of one display apart
/// where the name is identical because it is the same panel.
fn identity_label(fingerprint: &MonitorFingerprint) -> String {
    format!(
        "{} / {}",
        fingerprint.manufacturer_id, fingerprint.product_code
    )
}

fn monitor_identity_claims(settings: &AppSettings) -> Vec<MonitorIdentityClaim> {
    settings
        .monitor_identity_links
        .iter()
        .filter_map(|link| {
            let primary = link.primary.as_ref()?;
            Some(MonitorIdentityClaim {
                alias_key: monitor_key(&link.alias),
                alias_label: identity_label(&link.alias),
                primary_key: monitor_key(primary),
                primary_label: identity_label(primary),
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MonitorSelectionChange {
    SelectedOnlyMonitor { name: String },
    RefreshedMetadata { name: String },
}

struct MonitorInventory {
    detected: Vec<MonitorDescriptor>,
    controllable: Vec<MonitorDescriptor>,
    /// The input each controllable display reported while it was probed.
    current_inputs: HashMap<displaymux_core::MonitorId, DisplayInput>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputOption {
    value: u32,
    /// The name to show, including the user's note when there is one.
    name: String,
    /// The name the display's input data gives, without the note.
    base_name: String,
    /// The user's note, or empty.
    label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationResult {
    title: String,
    detail: String,
    peer_woken: bool,
    warning: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum SwitchProgress {
    Waking { peer_name: String },
    Checking { peer_name: String },
    Waiting { peer_name: String, seconds: u64 },
    Switching,
    RemoteFallback { peer_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NetworkPreparation {
    NotRequired,
    Ready { wake_sent: bool },
    Unavailable { wake_sent: bool, reason: String },
}

impl NetworkPreparation {
    fn peer_woken(&self) -> bool {
        matches!(
            self,
            Self::Ready { wake_sent: true }
                | Self::Unavailable {
                    wake_sent: true,
                    ..
                }
        )
    }

    fn warning(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    available: bool,
    current_version: String,
    version: Option<String>,
    notes: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum UpdateDownloadEvent {
    Started {
        content_length: Option<u64>,
    },
    Progress {
        downloaded: u64,
        content_length: Option<u64>,
    },
    Finished,
}

#[tauri::command]
async fn discover_peers(state: State<'_, AppRuntime>) -> Result<Vec<DiscoveredPeer>, String> {
    sleep(Duration::from_millis(700)).await;
    let discovery = state.discovery.as_ref().ok_or_else(|| {
        ui_text(
            "無法啟動區域網路搜尋；請確認防火牆允許 DisplayMux 使用私人網路",
            "Unable to start local network discovery. Allow DisplayMux through the firewall on private networks.",
        )
        .to_owned()
    })?;
    let peers = discovery.peers().map_err(core_user_error)?;
    refresh_paired_endpoints(&state, &peers)?;
    Ok(peers)
}

#[tauri::command]
async fn select_peer(
    peer_id: String,
    shared_key: Option<String>,
    state: State<'_, AppRuntime>,
) -> Result<AppSettings, String> {
    let discovery = state.discovery.as_ref().ok_or_else(|| {
        ui_text(
            "區域網路搜尋目前不可用",
            "Local network discovery is unavailable",
        )
        .to_owned()
    })?;
    let peer = discovery
        .peers()
        .map_err(core_user_error)?
        .into_iter()
        .find(|peer| peer.id == peer_id)
        .ok_or_else(|| {
            ui_text(
                "這台主機已離線，請重新搜尋後再試一次",
                "This host is offline. Search again and retry.",
            )
            .to_owned()
        })?;
    let mut settings = read_settings(&state)?;
    upsert_discovered_peer(&mut settings, &peer);
    let route = settings
        .peers
        .iter()
        .find(|route| route.id == peer.id)
        .cloned()
        .expect("peer was just inserted");
    let query_key = shared_key
        .filter(|value| has_valid_shared_key(value))
        .unwrap_or_else(|| settings.shared_key.clone());
    if has_valid_shared_key(&query_key) {
        let mut query_settings = settings.clone();
        query_settings.shared_key = query_key;
        if let Ok(response) = request_peer(&query_settings, &route, AgentAction::Ping).await {
            for display_route in agent_display_routes(&response) {
                let _ = apply_verified_peer_route(&mut settings, &route.id, display_route);
            }
        }
    }
    store_settings(&state, settings)
}

#[tauri::command]
fn remove_peer(peer_id: String, state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    settings.peers.retain(|peer| peer.id != peer_id);
    store_settings(&state, settings)
}

/// Runs blocking display work (enumeration, DDC/CI) off the main thread,
/// serialized with dashboard scans so they never talk to a display at once.
async fn run_display_task<T: Send + 'static>(
    app: AppHandle,
    task: impl FnOnce(&AppRuntime) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        task(&app.state::<AppRuntime>())
    })
    .await
    .map_err(user_error)?
}

fn display_not_found() -> String {
    ui_text(
        "找不到這台螢幕，請重新整理後再選擇",
        "This display was not found. Refresh and select it again.",
    )
    .to_owned()
}

#[tauri::command]
async fn add_shared_monitor(monitor_id: String, app: AppHandle) -> Result<AppSettings, String> {
    run_display_task(app, move |state| add_shared_monitor_now(state, &monitor_id)).await
}

fn add_shared_monitor_now(state: &AppRuntime, monitor_id: &str) -> Result<AppSettings, String> {
    let controller = platform_controller().map_err(core_user_error)?;
    let inventory = monitor_inventory(&controller).map_err(core_user_error)?;
    // Any detected display may be shared, not only one answering DDC/CI right
    // now. A display refuses DDC/CI while it is showing another host or asleep,
    // and that is exactly the display the user is trying to share — refusing it
    // left the display they came to add with no way to add it.
    let monitor = inventory
        .detected
        .iter()
        .find(|monitor| monitor.id.as_str() == monitor_id)
        .ok_or_else(display_not_found)?
        .clone();
    let readable = inventory
        .controllable
        .iter()
        .any(|candidate| candidate.id == monitor.id);
    let mut settings = read_settings(state)?;
    let links = settings.monitor_identity_links.clone();
    let already_selected = settings
        .shared_monitors
        .iter()
        .any(|selected| is_selected_display(&links, selected, &monitor));
    settings.shared_monitors_chosen = true;
    if !already_selected {
        settings
            .shared_monitors
            .push(SelectedMonitor::from(&monitor));
    }
    if let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| is_selected_display(&links, selected, &monitor))
    {
        // Its inputs are read when it answers again; until then the display is
        // reported as unavailable rather than dropped.
        if readable {
            refresh_selected_input_data(&controller, &monitor, selected)
                .map_err(core_user_error)?;
        }
    }
    store_settings(state, settings)
}

/// Removes a shared display, named either by its own key or by a display
/// present right now. Never enumerates: a display is removed precisely when
/// this computer cannot see it — asleep, showing another host, or reporting an
/// identity this host no longer recognises — and requiring it to be present
/// made those the only ones that could not be removed.
#[tauri::command]
async fn remove_shared_monitor(monitor_id: String, app: AppHandle) -> Result<AppSettings, String> {
    run_display_task(app, move |state| {
        remove_shared_monitor_now(state, &monitor_id)
    })
    .await
}

fn remove_shared_monitor_now(state: &AppRuntime, monitor_id: &str) -> Result<AppSettings, String> {
    let mut settings = read_settings(state)?;
    let links = settings.monitor_identity_links.clone();
    let target = find_shared_monitor(&settings, monitor_id)?
        .fingerprint
        .clone();
    settings.shared_monitors_chosen = true;
    let removed = settings
        .shared_monitors
        .iter()
        .filter(|selected| {
            monitor_identity::is_same_display(&links, &selected.fingerprint, &target)
        })
        .map(|selected| selected.fingerprint.clone())
        .collect::<Vec<_>>();
    settings.shared_monitors.retain(|selected| {
        !monitor_identity::is_same_display(&links, &selected.fingerprint, &target)
    });
    for peer in &mut settings.peers {
        peer.set_input_for(&target, None);
        for fingerprint in &removed {
            peer.set_input_for(fingerprint, None);
        }
    }
    store_settings(state, settings)
}

#[tauri::command]
fn get_settings(state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    read_settings(&state)
}

#[tauri::command]
fn get_host_switcher_state(state: State<'_, AppRuntime>) -> Result<HostSwitcherState, String> {
    let settings = read_settings(&state)?;
    let route_order = ordered_routes(&state, &settings);
    let monitors = settings
        .shared_monitors
        .iter()
        .map(|selected| {
            let mut hosts = Vec::with_capacity(settings.peers.len() + 1);
            hosts.push(HostSwitcherOption {
                id: "local".to_owned(),
                // Falls back to the name paired hosts discover this computer
                // by, so every surface calls it the same thing.
                name: host_alias::alias_for(&settings.host_aliases, &state.local_host_id)
                    .unwrap_or(if state.local_host_name.is_empty() {
                        ui_text("這台電腦", "This computer")
                    } else {
                        state.local_host_name.as_str()
                    })
                    .to_owned(),
                platform: settings.local_host,
                input_name: selected
                    .local_input
                    .map(|input| noted_input_label(&settings, selected, input)),
                is_local: true,
                available: selected.local_input.is_some(),
            });
            hosts.extend(settings.peers.iter().map(|peer| {
                let input = peer.input_for(&selected.fingerprint);
                HostSwitcherOption {
                    id: peer.id.clone(),
                    name: host_alias::alias_for(&settings.host_aliases, &peer.id)
                        .unwrap_or(&peer.name)
                        .to_owned(),
                    platform: peer.platform,
                    input_name: input.map(|input| noted_input_label(&settings, selected, input)),
                    is_local: false,
                    available: input.is_some(),
                }
            }));
            hosts.sort_by_key(|host| route_order.iter().position(|route| *route == host.id));
            HostSwitcherMonitor {
                monitor_key: monitor_key(&selected.fingerprint),
                name: selected.name.clone(),
                hosts,
            }
        })
        .collect();
    Ok(HostSwitcherState { monitors })
}

#[tauri::command]
fn hide_host_switcher(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("host-switcher") {
        window.hide().map_err(user_error)?;
    }
    Ok(())
}

#[tauri::command]
fn check_host_switcher_shortcut(
    shortcut: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<ShortcutCheckResult, String> {
    let candidate = validate_host_switcher_shortcut(&shortcut).map_err(core_user_error)?;
    let settings = read_settings(&state)?;
    let current = settings
        .host_switcher_enabled
        .then(|| Shortcut::from_str(&settings.host_switcher_shortcut).ok())
        .flatten();
    if current.is_some_and(|registered| {
        registered.id() == candidate.id() && app.global_shortcut().is_registered(registered)
    }) {
        return Ok(ShortcutCheckResult {
            available: true,
            message: ui_text("快捷鍵可使用", "Shortcut is available").to_owned(),
        });
    }
    if app.global_shortcut().is_registered(candidate) {
        return Ok(ShortcutCheckResult {
            available: false,
            message: ui_text(
                "此快捷鍵已由 DisplayMux 的其他功能使用",
                "This shortcut is already used by another DisplayMux feature.",
            )
            .to_owned(),
        });
    }
    match app.global_shortcut().register(candidate) {
        Ok(()) => {
            app.global_shortcut()
                .unregister(candidate)
                .map_err(user_error)?;
            Ok(ShortcutCheckResult {
                available: true,
                message: ui_text("快捷鍵可使用", "Shortcut is available").to_owned(),
            })
        }
        Err(_) => Ok(ShortcutCheckResult {
            available: false,
            message: ui_text(
                "快捷鍵發生衝突，可能已被其他程式使用",
                "Shortcut conflict detected. Another application may already be using it.",
            )
            .to_owned(),
        }),
    }
}

#[tauri::command]
fn complete_onboarding(state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    settings.onboarding_completed = true;
    store_settings(&state, settings)
}

#[tauri::command]
fn get_input_options(
    monitor_id: String,
    state: State<'_, AppRuntime>,
) -> Result<Vec<InputOption>, String> {
    let settings = read_settings(&state)?;
    input_options(&settings, &monitor_id)
}

fn input_options(settings: &AppSettings, monitor_id: &str) -> Result<Vec<InputOption>, String> {
    let selected = find_shared_monitor(settings, monitor_id)?;
    let inputs = selected
        .supported_inputs
        .clone()
        .filter(|inputs| !inputs.is_empty())
        .unwrap_or_else(common_input_sources);
    Ok(inputs
        .into_iter()
        .map(|input| InputOption {
            value: input.value(),
            name: noted_input_label(settings, selected, input),
            base_name: input_label(selected.vendor_indexed_inputs, input),
            label: input_label::label_for(&settings.input_labels, &selected.fingerprint, input)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect())
}

fn find_shared_monitor<'a>(
    settings: &'a AppSettings,
    monitor_id: &str,
) -> Result<&'a SelectedMonitor, String> {
    settings
        .shared_monitors
        .iter()
        .find(|selected| monitor_key(&selected.fingerprint) == monitor_id)
        .ok_or_else(|| {
            ui_text(
                "找不到這台共用螢幕，請重新整理後再試一次",
                "This shared display was not found. Refresh and try again.",
            )
            .to_owned()
        })
}

#[tauri::command]
async fn save_settings(
    settings: AppSettings,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<OperationResult, String> {
    let protected = read_settings(&state)?;
    let mut settings = settings_for_current_build(settings);
    // Monitor identity and discovered input data are backend-owned. The webview may only
    // assign a filtered input to remote hosts; it cannot forge DDC discovery results.
    settings.local_host = protected.local_host;
    settings.shared_monitors = protected.shared_monitors.clone();
    settings.onboarding_completed = protected.onboarding_completed;
    // A settings form opened before a paired host reordered must not revert it.
    settings.host_order = protected.host_order.clone();
    settings.host_order_updated_at_ms = protected.host_order_updated_at_ms;
    settings.host_aliases = protected.host_aliases.clone();
    settings.input_labels = protected.input_labels.clone();
    settings.monitor_identity_links = protected.monitor_identity_links.clone();
    settings.shared_monitors_chosen = protected.shared_monitors_chosen;
    settings.local_host_id = protected.local_host_id.clone();
    validate_settings(&settings).map_err(core_user_error)?;
    let enable_autostart = settings.autostart;
    update_host_switcher_shortcut(&app, &protected, &settings)?;
    if let Err(error) = store_settings(&state, settings.clone()) {
        if let Err(rollback_error) = update_host_switcher_shortcut(&app, &settings, &protected) {
            tracing::warn!(error = %rollback_error, "unable to restore the previous host switcher shortcut");
        }
        return Err(error);
    }
    let autostart = app.autolaunch();
    let autostart_enabled = autostart.is_enabled().map_err(user_error)?;
    if enable_autostart != autostart_enabled {
        if enable_autostart {
            autostart.enable().map_err(user_error)?;
        } else {
            autostart.disable().map_err(user_error)?;
        }
    }
    restart_agent(&state, &app).await?;
    Ok(OperationResult {
        title: ui_text("設定已儲存", "Settings saved").to_owned(),
        detail: ui_text(
            "共用螢幕、各主機輸入與配對設定已更新。",
            "The shared display, host inputs, and pairing settings were updated.",
        )
        .to_owned(),
        peer_woken: false,
        warning: false,
    })
}

/// How much of the saved setup a reset clears.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ResetScope {
    /// The shared displays and everything keyed to them. Pairing survives, so
    /// this undoes a display setup without costing the user a re-pair on both
    /// computers.
    Displays,
    /// Everything a fresh install would not have, except this computer's host
    /// id: peers name this computer by it, so changing it here would strand
    /// them on the *other* computer, which a local reset has no business doing.
    Everything,
}

/// `settings` with `scope` cleared. Split out so the decision about what each
/// scope keeps is testable without a running app.
fn settings_after_reset(settings: &AppSettings, scope: ResetScope) -> AppSettings {
    match scope {
        ResetScope::Displays => AppSettings {
            shared_monitors: Vec::new(),
            // Emptied on purpose, by someone who is here. Auto-select exists
            // for a computer that has never chosen; treating a reset as "never
            // chosen" refills the list on the next refresh and makes the reset
            // look like it did nothing.
            shared_monitors_chosen: true,
            monitor_identity_links: Vec::new(),
            input_labels: Vec::new(),
            // Assignments name displays that no longer exist here.
            peers: settings
                .peers
                .iter()
                .map(|peer| HostRoute {
                    inputs: Vec::new(),
                    ..peer.clone()
                })
                .collect(),
            ..settings.clone()
        },
        ResetScope::Everything => AppSettings {
            local_host: settings.local_host,
            local_host_id: settings.local_host_id.clone(),
            // As above: a fresh install auto-selects because nobody is there to
            // choose, which is not the case after a reset.
            shared_monitors_chosen: true,
            ..AppSettings::default()
        },
    }
}

/// Clears the saved setup. Destructive and not undoable, so the webview asks
/// before calling it.
#[tauri::command]
async fn reset_settings(
    scope: ResetScope,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let previous = read_settings(&state)?;
    let settings = settings_for_current_build(settings_after_reset(&previous, scope));
    update_host_switcher_shortcut(&app, &previous, &settings)?;
    let settings = match store_settings(&state, settings.clone()) {
        Ok(settings) => settings,
        Err(error) => {
            if let Err(rollback) = update_host_switcher_shortcut(&app, &settings, &previous) {
                tracing::warn!(error = %rollback, "unable to restore the previous host switcher shortcut");
            }
            return Err(error);
        }
    };
    let autostart = app.autolaunch();
    if let Ok(enabled) = autostart.is_enabled() {
        if settings.autostart != enabled {
            let result = if settings.autostart {
                autostart.enable()
            } else {
                autostart.disable()
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "unable to apply the autostart setting after a reset");
            }
        }
    }
    // The agent is keyed to the pairing password, which a full reset clears.
    restart_agent(&state, &app).await?;
    for event in [
        HOST_ORDER_CHANGED_EVENT,
        HOST_NAMES_CHANGED_EVENT,
        INPUT_LABELS_CHANGED_EVENT,
        MONITOR_IDENTITIES_CHANGED_EVENT,
    ] {
        if let Err(error) = app.emit(event, ()) {
            tracing::warn!(error = %error, event, "unable to notify windows of a reset");
        }
    }
    Ok(settings)
}

#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current_version = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?;
    Ok(match update {
        Some(update) => UpdateInfo {
            available: true,
            current_version,
            version: Some(update.version),
            notes: update.body,
        },
        None => UpdateInfo {
            available: false,
            current_version,
            version: None,
            notes: None,
        },
    })
}

#[tauri::command]
async fn install_update(
    app: AppHandle,
    on_event: Channel<UpdateDownloadEvent>,
) -> Result<(), String> {
    let Some(update) = app
        .updater()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?
    else {
        return Err(ui_text(
            "目前沒有可安裝的更新",
            "No update is currently available to install",
        )
        .to_owned());
    };

    let progress_events = on_event.clone();
    let finished_events = on_event;
    let mut downloaded = 0_u64;
    let mut started = false;
    update
        .download_and_install(
            move |chunk_length, content_length| {
                if !started {
                    let _ = progress_events.send(UpdateDownloadEvent::Started { content_length });
                    started = true;
                }
                downloaded = downloaded.saturating_add(chunk_length as u64);
                let _ = progress_events.send(UpdateDownloadEvent::Progress {
                    downloaded,
                    content_length,
                });
            },
            move || {
                let _ = finished_events.send(UpdateDownloadEvent::Finished);
            },
        )
        .await
        .map_err(update_install_error)?;

    tracing::info!(version = %update.version, "signed application update installed");
    app.restart()
}

/// Serializes dashboard scans. As a synchronous command the scan was implicitly
/// serialized on the main thread; overlapping refreshes would otherwise issue
/// concurrent DDC/CI requests and race on writing reconciled settings.
static DASHBOARD_SCAN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tauri::command]
async fn get_dashboard_state(app: AppHandle) -> Result<DashboardState, String> {
    // Monitor enumeration and DDC/CI reads block for hundreds of milliseconds up
    // to seconds (capabilities strings, retries). Keep them off the main thread so
    // the window stays responsive while displays are scanned.
    run_display_task(app, build_dashboard_state).await
}

fn build_dashboard_state(state: &AppRuntime) -> Result<DashboardState, String> {
    let mut settings = read_settings(state)?;
    let (monitors, uncontrollable_monitors, shared, selection_notices) =
        match enumerate_monitor_inventory() {
            Ok(inventory) => {
                let changes = reconcile_monitor_selection(&mut settings, &inventory.controllable);
                if !changes.is_empty() {
                    if let Ok(controller) = platform_controller() {
                        let links = settings.monitor_identity_links.clone();
                        for selected in &mut settings.shared_monitors {
                            if let Some(current) = inventory
                                .controllable
                                .iter()
                                .find(|monitor| is_selected_display(&links, selected, monitor))
                            {
                                if let Err(error) =
                                    refresh_selected_input_data(&controller, current, selected)
                                {
                                    tracing::warn!(
                                        monitor_id = current.id.as_str(),
                                        error = %error,
                                        "unable to record input data for automatically selected display"
                                    );
                                }
                            }
                        }
                    }
                }
                let routes_changed =
                    sync_active_routes_with_live_inputs(&mut settings, &inventory, unix_time_ms());
                if !changes.is_empty() || routes_changed {
                    store_settings(state, settings.clone())?;
                }
                announce_confirmed_local_inputs(state, &settings);
                let selection_notices = changes
                    .into_iter()
                    .filter_map(selection_notice_text)
                    .collect();
                let shared = settings
                    .shared_monitors
                    .iter()
                    .map(|selected| {
                        let target_found = inventory.controllable.iter().any(|monitor| {
                            is_selected_display(&settings.monitor_identity_links, selected, monitor)
                        });
                        let detected = inventory.detected.iter().find(|monitor| {
                            is_selected_display(&settings.monitor_identity_links, selected, monitor)
                        });
                        let target_detected = detected.is_some();
                        let connection = detected.and_then(|monitor| monitor.connection.clone());
                        let display_state =
                            shared_display_state(target_found, selected.active_route.as_deref());
                        let showing_host = selected
                            .active_route
                            .as_deref()
                            .and_then(|route| settings.peers.iter().find(|peer| peer.id == route))
                            .map(|peer| {
                                host_alias::alias_for(&settings.host_aliases, &peer.id)
                                    .unwrap_or(&peer.name)
                            });
                        SharedMonitorStatus {
                            monitor_key: monitor_key(&selected.fingerprint),
                            fingerprint: selected.fingerprint.clone(),
                            name: selected.name.clone(),
                            ddc_available: target_found,
                            display_state,
                            status_text: shared_monitor_status_text(
                                &selected.name,
                                display_state,
                                target_detected,
                                showing_host,
                            ),
                            connection_input_conflict: connection_input_conflict(
                                selected,
                                connection.as_ref(),
                            ),
                            connection,
                        }
                    })
                    .collect();
                let uncontrollable = uncontrollable_monitors(&inventory);
                (
                    inventory.controllable,
                    uncontrollable,
                    shared,
                    selection_notices,
                )
            }
            Err(error) => {
                let message = core_user_error(error);
                let shared = settings
                    .shared_monitors
                    .iter()
                    .map(|selected| SharedMonitorStatus {
                        monitor_key: monitor_key(&selected.fingerprint),
                        fingerprint: selected.fingerprint.clone(),
                        name: selected.name.clone(),
                        ddc_available: false,
                        display_state: SharedDisplayState::Unavailable,
                        status_text: message.clone(),
                        connection: None,
                        connection_input_conflict: false,
                    })
                    .collect();
                (Vec::new(), Vec::new(), shared, Vec::new())
            }
        };
    Ok(DashboardState {
        platform: std::env::consts::OS,
        local_host: settings.local_host,
        agent_configured: has_valid_shared_key(&settings.shared_key),
        monitors,
        uncontrollable_monitors,
        shared,
        selection_notices,
        monitor_identity_claims: monitor_identity_claims(&settings),
        local_host_name: state.local_host_name.clone(),
    })
}

fn selection_notice_text(change: MonitorSelectionChange) -> Option<String> {
    match change {
        MonitorSelectionChange::SelectedOnlyMonitor { name } => Some(match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("已自動選取唯一可控制的 DDC/CI 螢幕：{name}")
            }
            UiLocale::English => {
                format!("Automatically selected the only controllable DDC/CI display: {name}")
            }
        }),
        MonitorSelectionChange::RefreshedMetadata { .. } => None,
    }
}

fn shared_monitor_status_text(
    name: &str,
    state: SharedDisplayState,
    target_detected: bool,
    showing_host: Option<&str>,
) -> String {
    if state == SharedDisplayState::OnOtherHost {
        let host = showing_host.unwrap_or(ui_text("另一台主機", "another host"));
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "{name} 目前顯示 {host}；顯示其他主機時，這台螢幕不回應這台電腦的 DDC/CI"
            ),
            UiLocale::English => format!(
                "{name} is showing {host}. While it shows another host, it does not answer this computer's DDC/CI"
            ),
        };
    }
    if state == SharedDisplayState::Ready {
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("已鎖定共用螢幕：{name}"),
            UiLocale::English => format!("Shared display locked: {name}"),
        };
    }
    if target_detected {
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("已偵測到 {name}，但目前無法讀取 DDC/CI 輸入")
            }
            UiLocale::English => {
                format!("{name} was detected, but its DDC/CI input cannot be read")
            }
        };
    }
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!("找不到先前選擇的共用螢幕：{name}"),
        UiLocale::English => format!("Previously selected shared display was not found: {name}"),
    }
}

#[tauri::command]
async fn probe_peer(
    peer_id: String,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let peer = find_peer(&settings, &peer_id)?;
    let peer_name = peer.name.clone();
    let response = request_peer(&settings, peer, AgentAction::Ping).await?;
    let adoption = adopt_peer_routes(&state, &peer_id, &response)?;
    Ok(OperationResult {
        title: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{peer_name} 已連線"),
            UiLocale::English => format!("{peer_name} connected"),
        },
        detail: adoption.detail,
        peer_woken: false,
        warning: adoption.warning,
    })
}

/// The outcome of adopting a paired host's reported inputs, phrased for a toast.
struct RouteAdoption {
    detail: String,
    warning: bool,
}

/// Applies every input a paired host reported for the displays shared here,
/// and describes what happened. A port that cannot be filled in says why,
/// rather than leaving the user with a field that silently stays empty.
fn adopt_peer_routes(
    state: &AppRuntime,
    peer_id: &str,
    response: &AgentResponse,
) -> Result<RouteAdoption, String> {
    let routes = agent_display_routes(response);
    if routes.is_empty() {
        return Ok(RouteAdoption {
            detail: ui_text(
                "Agent 已就緒，但這台主機沒有回報任何輸入值；請確認它也把同一台螢幕設為共用。",
                "The agent is ready, but this host reported no input. Check that it shares the same display.",
            )
            .to_owned(),
            warning: true,
        });
    }
    let mut settings = read_settings(state)?;
    let mut notes: Vec<String> = Vec::new();
    let mut applied = false;
    let mut warning = false;
    let chinese = matches!(UiLocale::current(), UiLocale::TraditionalChinese);
    for route in routes {
        let matched = shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            &route.monitor,
        )
        .map(|index| settings.shared_monitors[index].clone());
        let label = matched.as_ref().map_or_else(
            || input_label(false, route.input),
            |selected| noted_input_label(&settings, selected, route.input),
        );
        let monitor = matched.map_or_else(String::new, |selected| selected.name);
        let outcome = apply_verified_peer_route(&mut settings, peer_id, route);
        applied |= outcome == PeerRouteOutcome::Applied;
        warning |= !matches!(
            outcome,
            PeerRouteOutcome::Applied | PeerRouteOutcome::Unchanged
        );
        notes.push(match (outcome, chinese) {
            (PeerRouteOutcome::Applied, true) => format!("{monitor} 已帶入 {label}。"),
            (PeerRouteOutcome::Applied, false) => format!("{monitor} set to {label}."),
            (PeerRouteOutcome::Unchanged, true) => format!("{monitor} 已經是 {label}。"),
            (PeerRouteOutcome::Unchanged, false) => format!("{monitor} was already {label}."),
            (PeerRouteOutcome::Kept, true) => format!(
                "{monitor} 回報 {label}，但這台主機目前不在螢幕上，讀到的可能是別台，因此保留原本的設定。"
            ),
            (PeerRouteOutcome::Kept, false) => format!(
                "{monitor} reported {label}, but that host is not on screen so the reading may be another host's; the current setting was kept."
            ),
            (PeerRouteOutcome::UnknownMonitor, true) => {
                "這台主機回報了一台這裡沒有共用的螢幕。".to_owned()
            }
            (PeerRouteOutcome::UnknownMonitor, false) => {
                "This host reported a display that is not shared here.".to_owned()
            }
            (PeerRouteOutcome::Unsupported, true) => {
                format!("{monitor} 沒有 {label} 這個輸入。")
            }
            (PeerRouteOutcome::Unsupported, false) => {
                format!("{monitor} has no {label} input.")
            }
            (PeerRouteOutcome::Taken, true) => format!(
                "{monitor} 的 {label} 已指派給其他主機；請確認兩台主機接在不同的 Port，或先切換到這台主機再試一次。"
            ),
            (PeerRouteOutcome::Taken, false) => format!(
                "{label} on {monitor} is already assigned to another host. Check that the hosts use different ports, or switch to this host and retry."
            ),
            (PeerRouteOutcome::UnknownPeer, true) => {
                "這台主機已不在已加入的清單中。".to_owned()
            }
            (PeerRouteOutcome::UnknownPeer, false) => {
                "This host is no longer in the added list.".to_owned()
            }
        });
    }
    if applied {
        store_settings(state, settings)?;
    }
    Ok(RouteAdoption {
        detail: notes.join(" "),
        warning,
    })
}

#[tauri::command]
async fn wake_peer(
    peer_id: String,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let peer = find_peer(&settings, &peer_id)?;
    wake_route(&settings, peer).await?;
    Ok(OperationResult {
        title: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("已送出喚醒訊號給 {}", peer.name),
            UiLocale::English => format!("Wake signal sent to {}", peer.name),
        },
        detail: ui_text(
            "主機是否能喚醒仍取決於電源與網路設定。",
            "Whether the host wakes still depends on its power and network settings.",
        )
        .to_owned(),
        peer_woken: true,
        warning: false,
    })
}

#[tauri::command]
async fn switch_host(
    monitor_id: String,
    target_id: String,
    on_event: Channel<SwitchProgress>,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let selected = find_shared_monitor(&settings, &monitor_id)?.clone();
    let target = if target_id == "local" {
        None
    } else {
        Some(find_peer(&settings, &target_id)?)
    };
    let input = if let Some(peer) = target {
        peer.input_for(&selected.fingerprint).ok_or_else(|| {
            ui_text(
                "尚未設定這台主機使用的螢幕輸入",
                "The display input for this host is not configured",
            )
            .to_owned()
        })?
    } else {
        selected.local_input.ok_or_else(|| {
            ui_text(
                "尚未設定這台主機使用的螢幕輸入",
                "The display input for this host is not configured",
            )
            .to_owned()
        })?
    };
    let preparation = match target {
        Some(peer) => prepare_automatic_switch(&settings, peer, &on_event).await,
        None => NetworkPreparation::NotRequired,
    };
    let _ = on_event.send(SwitchProgress::Switching);
    let identities =
        monitor_identity::identities_for(&settings.monitor_identity_links, &selected.fingerprint);
    match run_switch(identities, input) {
        Ok(outcome) => {
            record_active_route(&state, &selected.fingerprint, &target_id)?;
            announce_active_input(&state, &selected.fingerprint, input);
            Ok(outcome_result(outcome, &preparation, |input| {
                noted_input_label(&settings, &selected, input)
            }))
        }
        Err(local_error) => {
            let local_error = core_user_error(local_error);
            let executor = if target_id == "local" {
                (settings.peers.len() == 1).then(|| &settings.peers[0])
            } else {
                settings.peers.iter().find(|peer| peer.id == target_id)
            }
            .ok_or_else(|| match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("本機無法切換，而且沒有其他已配對主機可代為執行：{local_error}")
                }
                UiLocale::English => format!(
                    "Local switching failed and no other paired host can perform it: {local_error}"
                ),
            })?;
            let _ = on_event.send(SwitchProgress::RemoteFallback {
                peer_name: executor.name.clone(),
            });
            let remote_result: Result<AgentResponse, String> = async {
                let monitor_field =
                    resolve_switch_monitor_field(&settings, executor, &selected.fingerprint)
                        .await?;
                request_peer(
                    &settings,
                    executor,
                    AgentAction::SwitchInput {
                        monitor: monitor_field,
                        input,
                    },
                )
                .await
            }
            .await;
            remote_result.map_err(|remote_error| match UiLocale::current() {
                UiLocale::TraditionalChinese => format!(
                    "本機與 {} 都無法切換。本機：{}；遠端：{}",
                    executor.name, local_error, remote_error
                ),
                UiLocale::English => format!(
                    "Neither this computer nor {} could switch. Local: {}; remote: {}",
                    executor.name, local_error, remote_error
                ),
            })?;
            record_active_route(&state, &selected.fingerprint, &target_id)?;
            announce_active_input(&state, &selected.fingerprint, input);
            Ok(OperationResult {
                title: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!("已由 {} 執行切換", executor.name),
                    UiLocale::English => format!("Switch performed by {}", executor.name),
                },
                detail: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!(
                        "遠端主機已切換至 {}。",
                        noted_input_label(&settings, &selected, input)
                    ),
                    UiLocale::English => format!(
                        "The remote host switched to {}.",
                        noted_input_label(&settings, &selected, input)
                    ),
                },
                peer_woken: preparation.peer_woken(),
                warning: false,
            })
        }
    }
}

/// Decides whether an outbound `AgentAction::SwitchInput` may name a target
/// monitor, based on the receiving peer's advertised protocol version. Never
/// guesses: a pre-v2 peer is only used when there is exactly one shared
/// monitor selected locally (unambiguous under the old, monitor-less wire
/// shape); otherwise the peer must be updated first.
async fn resolve_switch_monitor_field(
    settings: &AppSettings,
    peer: &HostRoute,
    target_fingerprint: &MonitorFingerprint,
) -> Result<Option<MonitorFingerprint>, String> {
    let ping = request_peer(settings, peer, AgentAction::Ping).await?;
    plan_switch_input(
        ping.protocol_version,
        settings.shared_monitors.len(),
        target_fingerprint,
    )
}

fn plan_switch_input(
    protocol_version: u32,
    shared_monitor_count: usize,
    target: &MonitorFingerprint,
) -> Result<Option<MonitorFingerprint>, String> {
    if protocol_version >= AGENT_PROTOCOL_VERSION {
        return Ok(Some(target.clone()));
    }
    if shared_monitor_count <= 1 {
        return Ok(None);
    }
    Err(ui_text(
        "此配對主機使用舊版 DisplayMux，僅支援單一共用螢幕；請將該主機更新到最新版本以切換多台螢幕",
        "This paired host is running an older DisplayMux version that only supports a single shared display; update it to switch multiple displays.",
    )
    .to_owned())
}

fn outcome_result(
    outcome: SwitchOutcome,
    preparation: &NetworkPreparation,
    label: impl Fn(DisplayInput) -> String,
) -> OperationResult {
    let mut result = match outcome {
        SwitchOutcome::DryRun { .. } => OperationResult {
            title: ui_text("檢查完成", "Check complete").to_owned(),
            detail: ui_text("未變更螢幕輸入。", "The display input was not changed.").to_owned(),
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
        SwitchOutcome::AlreadySelected { target, input } => OperationResult {
            title: ui_text("已在指定輸入", "Already on the assigned input").to_owned(),
            detail: match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("{} 已使用 {}。", target.name, label(input))
                }
                UiLocale::English => {
                    format!("{} is already using {}.", target.name, label(input))
                }
            },
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
        SwitchOutcome::Switched {
            target,
            previous,
            selected,
        } => OperationResult {
            title: ui_text("共用螢幕已切換", "Shared display switched").to_owned(),
            detail: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!(
                    "{} 已由 {} 切換至 {}。",
                    target.name,
                    label(previous),
                    label(selected)
                ),
                UiLocale::English => format!(
                    "{} switched from {} to {}.",
                    target.name,
                    label(previous),
                    label(selected)
                ),
            },
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
    };
    if let NetworkPreparation::Unavailable {
        wake_sent, reason, ..
    } = preparation
    {
        let wake_detail = if *wake_sent {
            ui_text("已先送出喚醒訊號，但", "A wake signal was sent, but ")
        } else {
            ui_text(
                "無法送出喚醒訊號，且",
                "A wake signal could not be sent, and ",
            )
        };
        result.detail.push_str(&match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(" {wake_detail}無法透過區域網路確認目標主機（{reason}）；已自動改用本機 DDC/CI。若目標主機尚未就緒，螢幕可能暫時黑畫面。"),
            UiLocale::English => format!(" {wake_detail}the target host could not be confirmed over the local network ({reason}); local DDC/CI was selected automatically. The display may be temporarily blank if the target host is not ready."),
        });
    }
    result
}

async fn prepare_automatic_switch(
    settings: &AppSettings,
    peer: &HostRoute,
    on_event: &Channel<SwitchProgress>,
) -> NetworkPreparation {
    let _ = on_event.send(SwitchProgress::Waking {
        peer_name: peer.name.clone(),
    });
    let wake_result = wake_route(settings, peer).await;
    let wake_sent = wake_result.is_ok();

    let _ = on_event.send(SwitchProgress::Checking {
        peer_name: peer.name.clone(),
    });
    if !has_valid_shared_key(&settings.shared_key) {
        return NetworkPreparation::Unavailable {
            wake_sent,
            reason: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!("網路 Agent 尚未設定至少 {MIN_SHARED_KEY_LENGTH} 個字元的配對密碼"),
                UiLocale::English => format!("The network Agent does not have a pairing password of at least {MIN_SHARED_KEY_LENGTH} characters"),
            },
        };
    }
    if request_peer(settings, peer, AgentAction::Ping)
        .await
        .is_ok()
    {
        return NetworkPreparation::Ready { wake_sent };
    }

    if let Err(wake_error) = wake_result {
        return NetworkPreparation::Unavailable {
            wake_sent: false,
            reason: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!("{}，且 Agent 目前沒有回應", wake_error),
                UiLocale::English => format!("{wake_error}, and the Agent is not responding"),
            },
        };
    }

    let _ = on_event.send(SwitchProgress::Waiting {
        peer_name: peer.name.clone(),
        seconds: settings.wait_seconds.clamp(5, 120),
    });
    match wait_until_peer_ready(settings, peer).await {
        Ok(()) => NetworkPreparation::Ready { wake_sent: true },
        Err(reason) => NetworkPreparation::Unavailable {
            wake_sent: true,
            reason,
        },
    }
}

async fn wait_until_peer_ready(settings: &AppSettings, peer: &HostRoute) -> Result<(), String> {
    let attempts = settings.wait_seconds.clamp(5, 120);
    for _ in 0..attempts {
        sleep(Duration::from_secs(1)).await;
        if request_peer(settings, peer, AgentAction::Ping)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(match UiLocale::current() {
        UiLocale::TraditionalChinese => {
            format!("{} 在送出喚醒訊號後 {} 秒內仍沒有回應", peer.name, attempts)
        }
        UiLocale::English => format!(
            "{} did not respond within {} seconds after the wake signal",
            peer.name, attempts
        ),
    })
}

async fn request_peer(
    settings: &AppSettings,
    peer: &HostRoute,
    action: AgentAction,
) -> Result<AgentResponse, String> {
    let endpoint = route_endpoint(peer).map_err(core_user_error)?;
    if !has_valid_shared_key(&settings.shared_key) {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("請先設定至少 {MIN_SHARED_KEY_LENGTH} 個字元的配對密碼")
            }
            UiLocale::English => format!(
                "Configure a pairing password of at least {MIN_SHARED_KEY_LENGTH} characters first"
            ),
        });
    }
    let response = AgentClient::new(endpoint, Arc::<[u8]>::from(settings.shared_key.as_bytes()))
        .request(action, next_nonce())
        .await
        .map_err(core_user_error)?;
    if response.ready {
        Ok(response)
    } else {
        Err(response.message)
    }
}

async fn wake_route(settings: &AppSettings, peer: &HostRoute) -> Result<(), String> {
    if peer.mac_address.trim().is_empty() {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{} 沒有提供可用的 MAC 位址，因此無法使用 Wake-on-LAN；主機醒著時仍可切換", peer.name),
            UiLocale::English => format!("{} has no usable MAC address, so Wake-on-LAN is unavailable; switching still works while the host is awake", peer.name),
        });
    }
    let mac_address = MacAddress::from_str(&peer.mac_address).map_err(core_user_error)?;
    let broadcast_address = Ipv4Addr::from_str(&settings.broadcast_ip)
        .map_err(|_| ui_text("廣播位址格式無效", "Invalid broadcast address").to_owned())?;
    WakeTarget {
        mac_address,
        broadcast_address,
        port: settings.wake_port,
    }
    .wake()
    .await
    .map_err(core_user_error)
}

fn route_endpoint(peer: &HostRoute) -> Result<PeerEndpoint, DisplayMuxError> {
    let address = IpAddr::from_str(&peer.address).map_err(|_| {
        DisplayMuxError::PeerUnavailable(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{} 的 IP 位址無效", peer.name),
            UiLocale::English => format!("{} has an invalid IP address", peer.name),
        })
    })?;
    Ok(PeerEndpoint {
        address,
        port: peer.port,
    })
}

fn find_peer<'a>(settings: &'a AppSettings, peer_id: &str) -> Result<&'a HostRoute, String> {
    settings
        .peers
        .iter()
        .find(|peer| peer.id == peer_id)
        .ok_or_else(|| {
            ui_text(
                "找不到這台已配對主機，請重新搜尋並加入",
                "This paired host was not found. Search for it and add it again.",
            )
            .to_owned()
        })
}

fn validate_settings(settings: &AppSettings) -> Result<(), DisplayMuxError> {
    if settings.local_host != local_host() {
        return Err(DisplayMuxError::Backend(
            ui_text(
                "這台電腦的主機類型必須由作業系統自動判定",
                "This computer's host type must be determined by the operating system",
            )
            .to_owned(),
        ));
    }
    if settings.host_switcher_enabled {
        validate_host_switcher_shortcut(&settings.host_switcher_shortcut)?;
    }
    let invalid_input = |input: DisplayInput| DisplayInput::new(input.value()).is_err();
    let has_invalid_input = settings
        .shared_monitors
        .iter()
        .any(|selected| selected.local_input.is_some_and(invalid_input))
        || settings.peers.iter().any(|peer| {
            peer.inputs
                .iter()
                .any(|assignment| invalid_input(assignment.input))
        });
    if has_invalid_input {
        return Err(DisplayMuxError::Backend(
            ui_text(
                "請選擇有效的螢幕輸入 Port",
                "Select a valid display input port",
            )
            .to_owned(),
        ));
    }
    for peer in &settings.peers {
        for assignment in &peer.inputs {
            let known_monitor = settings
                .shared_monitors
                .iter()
                .any(|selected| selected.fingerprint.matches_exactly(&assignment.monitor));
            if !known_monitor {
                return Err(DisplayMuxError::Backend(
                    ui_text(
                        "輸入設定對應到不存在的共用螢幕",
                        "The input assignment refers to a shared display that is not selected",
                    )
                    .to_owned(),
                ));
            }
        }
    }
    for selected in &settings.shared_monitors {
        let assigned_inputs = selected
            .local_input
            .into_iter()
            .chain(
                settings
                    .peers
                    .iter()
                    .filter_map(|peer| peer.input_for(&selected.fingerprint)),
            )
            .collect::<Vec<_>>();
        let unique_inputs = assigned_inputs
            .iter()
            .map(|input| input.value())
            .collect::<std::collections::HashSet<_>>();
        if unique_inputs.len() != assigned_inputs.len() {
            return Err(DisplayMuxError::Backend(
                ui_text(
                    "每個主機必須使用不同的螢幕輸入 Port",
                    "Each host must use a different display input port",
                )
                .to_owned(),
            ));
        }
        if let Some(supported) = &selected.supported_inputs {
            if settings
                .peers
                .iter()
                .filter_map(|peer| peer.input_for(&selected.fingerprint))
                .any(|assigned| !supported.contains(&assigned))
            {
                return Err(DisplayMuxError::Backend(
                    ui_text(
                        "輸入值不在這台螢幕的 MCCS capabilities 清單中",
                        "The input is not listed in this display's MCCS capabilities",
                    )
                    .to_owned(),
                ));
            }
        }
    }
    for peer in &settings.peers {
        route_endpoint(peer)?;
        if !peer.mac_address.trim().is_empty() {
            MacAddress::from_str(&peer.mac_address)?;
        }
    }
    Ipv4Addr::from_str(&settings.broadcast_ip).map_err(|_| {
        DisplayMuxError::WakeFailed(
            ui_text("廣播位址格式無效", "Invalid broadcast address").to_owned(),
        )
    })?;
    if !settings.shared_key.is_empty() && !has_valid_shared_key(&settings.shared_key) {
        return Err(DisplayMuxError::Backend(match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("配對密碼至少需要 {MIN_SHARED_KEY_LENGTH} 個字元")
            }
            UiLocale::English => format!(
                "The pairing password must contain at least {MIN_SHARED_KEY_LENGTH} characters"
            ),
        }));
    }
    Ok(())
}

fn validate_host_switcher_shortcut(value: &str) -> Result<Shortcut, DisplayMuxError> {
    let shortcut = Shortcut::from_str(value).map_err(|_| {
        DisplayMuxError::Backend(
            ui_text(
                "無法辨識快捷鍵，請同時按下修飾鍵與一個一般按鍵",
                "The shortcut was not recognized. Press a modifier and one regular key.",
            )
            .to_owned(),
        )
    })?;
    let required_modifier = platform_primary_shortcut_modifier();
    if !shortcut.mods.intersects(required_modifier) {
        return Err(DisplayMuxError::Backend(
            ui_text(
                "Windows 快捷鍵必須包含 Ctrl；macOS 快捷鍵必須包含 Command",
                "The shortcut must include Ctrl on Windows or Command on macOS.",
            )
            .to_owned(),
        ));
    }
    if shortcut.mods.bits().count_ones() > 2 {
        return Err(DisplayMuxError::Backend(
            ui_text(
                "Ctrl 或 Command 之外最多只能再搭配一個修飾鍵",
                "Use at most one additional modifier with Ctrl or Command.",
            )
            .to_owned(),
        ));
    }
    if is_common_application_shortcut(&shortcut) {
        return Err(DisplayMuxError::Backend(
            ui_text(
                "這是瀏覽器或常用應用程式的快捷鍵，請改用其他組合",
                "This shortcut is commonly used by browsers or other applications. Choose another combination.",
            )
            .to_owned(),
        ));
    }
    Ok(shortcut)
}

fn platform_primary_shortcut_modifier() -> Modifiers {
    #[cfg(target_os = "macos")]
    {
        Modifiers::SUPER
    }
    #[cfg(not(target_os = "macos"))]
    {
        Modifiers::CONTROL
    }
}

fn is_common_application_shortcut(shortcut: &Shortcut) -> bool {
    let primary = platform_primary_shortcut_modifier();
    let primary_only = shortcut.mods == primary;
    let primary_with_shift = shortcut.mods == (primary | Modifiers::SHIFT);
    (primary_only
        && matches!(
            shortcut.key,
            Code::KeyA
                | Code::KeyC
                | Code::KeyF
                | Code::KeyH
                | Code::KeyL
                | Code::KeyM
                | Code::KeyN
                | Code::KeyO
                | Code::KeyP
                | Code::KeyQ
                | Code::KeyR
                | Code::KeyS
                | Code::KeyT
                | Code::KeyV
                | Code::KeyW
                | Code::KeyX
                | Code::KeyY
                | Code::KeyZ
                | Code::Tab
                | Code::F4
        ))
        || (primary_with_shift
            && matches!(
                shortcut.key,
                Code::KeyN | Code::KeyP | Code::KeyR | Code::KeyS | Code::KeyT | Code::KeyW
            ))
}

fn update_host_switcher_shortcut(
    app: &AppHandle,
    previous: &AppSettings,
    next: &AppSettings,
) -> Result<(), String> {
    if previous.host_switcher_enabled == next.host_switcher_enabled
        && previous.host_switcher_shortcut == next.host_switcher_shortcut
    {
        return Ok(());
    }
    let next_shortcut = next
        .host_switcher_enabled
        .then(|| validate_host_switcher_shortcut(&next.host_switcher_shortcut))
        .transpose()
        .map_err(core_user_error)?;
    let previous_shortcut = previous
        .host_switcher_enabled
        .then(|| Shortcut::from_str(&previous.host_switcher_shortcut).ok())
        .flatten();
    let previous_was_registered =
        previous_shortcut.is_some_and(|shortcut| app.global_shortcut().is_registered(shortcut));
    if let Some(shortcut) = previous_shortcut.filter(|_| previous_was_registered) {
        app.global_shortcut()
            .unregister(shortcut)
            .map_err(user_error)?;
    }
    if let Some(shortcut) = next_shortcut {
        if let Err(error) = app.global_shortcut().register(shortcut) {
            if let Some(previous_shortcut) = previous_shortcut.filter(|_| previous_was_registered) {
                if let Err(restore_error) = app.global_shortcut().register(previous_shortcut) {
                    tracing::warn!(error = %restore_error, "unable to restore the previous global shortcut");
                }
            }
            return Err(match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("無法註冊快捷鍵，可能已被其他程式使用：{error}")
                }
                UiLocale::English => {
                    format!("Unable to register the shortcut. Another app may be using it: {error}")
                }
            });
        }
    }
    Ok(())
}

fn show_host_switcher(app: &AppHandle) {
    let Some(window) = app.get_webview_window("host-switcher") else {
        tracing::warn!("host switcher window is unavailable");
        return;
    };
    if let Err(error) = window.center() {
        tracing::warn!(error = %error, "unable to center the host switcher window");
    }
    if let Err(error) = window.show() {
        tracing::warn!(error = %error, "unable to show the host switcher window");
        return;
    }
    if let Err(error) = window.set_focus() {
        tracing::warn!(error = %error, "unable to focus the host switcher window");
    }
}

fn upsert_discovered_peer(settings: &mut AppSettings, peer: &DiscoveredPeer) {
    if let Some(existing) = settings.peers.iter_mut().find(|item| item.id == peer.id) {
        existing.name.clone_from(&peer.name);
        existing.platform = peer.platform;
        existing.address = peer.address.to_string();
        existing.port = peer.port;
        existing.mac_address = peer.mac_address.clone().unwrap_or_default();
        return;
    }
    settings.peers.push(HostRoute {
        id: peer.id.clone(),
        name: peer.name.clone(),
        platform: peer.platform,
        address: peer.address.to_string(),
        port: peer.port,
        mac_address: peer.mac_address.clone().unwrap_or_default(),
        inputs: Vec::new(),
    });
}

/// A peer's `Ping` response may report one route (pre-v2) or several
/// (v2+); this always yields the full set to apply.
fn agent_display_routes(response: &AgentResponse) -> Vec<AgentDisplayRoute> {
    if !response.display_routes.is_empty() {
        response.display_routes.clone()
    } else {
        response.display_route.clone().into_iter().collect()
    }
}

/// Which shared display a paired host means by `remote`. Hosts read serial
/// numbers differently, so after an exact match fails the display is matched
/// by model, but only when a single shared display has that model.
fn shared_monitor_index_for_peer(
    monitors: &[SelectedMonitor],
    links: &[MonitorIdentityLink],
    remote: &MonitorFingerprint,
) -> Option<usize> {
    // An identity the user merged into a shared display names that display, so
    // a paired host reading the same panel in another display mode still lands
    // on it rather than looking like a display we do not share.
    if let Some(exact) = monitors.iter().position(|selected| {
        monitor_identity::is_same_display(links, &selected.fingerprint, remote)
    }) {
        return Some(exact);
    }
    let mut same_model = monitors.iter().enumerate().filter(|(_, selected)| {
        monitor_identity::identities_for(links, &selected.fingerprint)
            .iter()
            .any(|identity| identity.is_same_model(remote))
    });
    match (same_model.next(), same_model.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Whether this host believes `selected` is currently showing it. An unset
/// `active_route` renders as this host everywhere else, so it counts as this
/// host here too — which is how a host that was never switched away sees
/// itself.
fn shows_this_host(selected: &SelectedMonitor) -> bool {
    selected.active_route.as_deref().unwrap_or("local") == "local"
}

/// What `apply_verified_peer_route` did with a route a paired host reported.
/// Every rejection carries its reason so the caller can say why a port stayed
/// empty instead of leaving the user with a blank field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerRouteOutcome {
    /// The reported input became the peer's assignment for that display.
    Applied,
    /// The peer was already assigned that input.
    Unchanged,
    /// The peer kept the input it already had, because it did not claim to be
    /// on screen and so cannot vouch for what it reported.
    Kept,
    /// The peer named a display this host does not share.
    UnknownMonitor,
    /// The display does not offer that input.
    Unsupported,
    /// This host or another paired host already uses that input.
    Taken,
    /// The peer is no longer in the saved list.
    UnknownPeer,
}

/// Adopts the input a paired host reported for one shared display.
///
/// A reading the peer cannot vouch for never overwrites an input that is
/// already set: DDC reports the input a display is showing, not the port the
/// reader occupies, so a host that is off screen reads whoever is on screen.
fn apply_verified_peer_route(
    settings: &mut AppSettings,
    peer_id: &str,
    route: AgentDisplayRoute,
) -> PeerRouteOutcome {
    let Some((fingerprint, local_input, supported_inputs)) = shared_monitor_index_for_peer(
        &settings.shared_monitors,
        &settings.monitor_identity_links,
        &route.monitor,
    )
    .map(|index| &settings.shared_monitors[index])
    .map(|selected| {
        (
            selected.fingerprint.clone(),
            selected.local_input,
            selected.supported_inputs.clone(),
        )
    }) else {
        return PeerRouteOutcome::UnknownMonitor;
    };
    let supported = supported_inputs.as_ref().map_or_else(
        || common_input_sources().contains(&route.input),
        |inputs| inputs.contains(&route.input),
    );
    if !supported {
        return PeerRouteOutcome::Unsupported;
    }
    let Some(existing) = settings
        .peers
        .iter()
        .find(|peer| peer.id == peer_id)
        .map(|peer| peer.input_for(&fingerprint))
    else {
        return PeerRouteOutcome::UnknownPeer;
    };
    if existing == Some(route.input) {
        return PeerRouteOutcome::Unchanged;
    }
    if existing.is_some() && !route.confirmed {
        return PeerRouteOutcome::Kept;
    }
    let taken = local_input == Some(route.input)
        || settings
            .peers
            .iter()
            .any(|peer| peer.id != peer_id && peer.input_for(&fingerprint) == Some(route.input));
    if taken {
        return PeerRouteOutcome::Taken;
    }
    let Some(peer) = settings.peers.iter_mut().find(|peer| peer.id == peer_id) else {
        return PeerRouteOutcome::UnknownPeer;
    };
    peer.set_input_for(&fingerprint, Some(route.input));
    PeerRouteOutcome::Applied
}

fn refresh_paired_endpoints(state: &AppRuntime, peers: &[DiscoveredPeer]) -> Result<(), String> {
    let current = read_settings_inner(state)?;
    let mut updated = current.clone();
    for peer in peers {
        if updated.peers.iter().any(|item| item.id == peer.id) {
            upsert_discovered_peer(&mut updated, peer);
        }
    }
    if updated != current {
        store_settings(state, updated)?;
    }
    Ok(())
}

async fn restart_agent(state: &AppRuntime, app: &AppHandle) -> Result<(), String> {
    let settings = read_settings_inner(state)?;
    let mut current_task = state.agent_task.lock().await;
    if let Some(task) = current_task.take() {
        task.abort();
    }
    if !has_valid_shared_key(&settings.shared_key) {
        return Ok(());
    }
    let server = AgentServer::new(
        SocketAddr::from(([0, 0, 0, 0], DEFAULT_AGENT_PORT)),
        Arc::<[u8]>::from(settings.shared_key.as_bytes()),
    );
    let live_settings = Arc::clone(&state.settings);
    let app = app.clone();
    *current_task = Some(tauri::async_runtime::spawn(async move {
        let result = server
            .run(move |action| {
                let live_settings = Arc::clone(&live_settings);
                let app = app.clone();
                async move {
                    match action {
                        AgentAction::ActiveInputChanged { monitor, input } => {
                            receive_active_input_notice(app, monitor, input).await
                        }
                        AgentAction::HostOrderChanged {
                            order,
                            updated_at_ms,
                        } => receive_host_order_notice(app, order, updated_at_ms).await,
                        AgentAction::HostAliasesChanged { aliases } => {
                            receive_host_aliases_notice(app, aliases).await
                        }
                        AgentAction::InputLabelsChanged { labels } => {
                            receive_input_labels_notice(app, labels).await
                        }
                        AgentAction::MonitorIdentitiesChanged { links } => {
                            receive_monitor_identities_notice(app, links).await
                        }
                        AgentAction::LocalInputConfirmed {
                            host_id,
                            monitor,
                            input,
                        } => receive_local_input_confirmed(app, host_id, monitor, input).await,
                        AgentAction::Ping => {
                            let snapshot = live_settings.read().ok().map(|settings| settings.clone());
                            let display_routes = snapshot
                                .as_ref()
                                .map(|settings| {
                                    settings
                                        .shared_monitors
                                        .iter()
                                        .filter_map(|selected| {
                                            Some(AgentDisplayRoute {
                                                monitor: selected.fingerprint.clone(),
                                                input: selected.local_input?,
                                                confirmed: shows_this_host(selected),
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            let (
                                host_order,
                                host_order_updated_at_ms,
                                host_aliases,
                                input_labels,
                                monitor_identity_links,
                            ) = snapshot
                                .map(|settings| {
                                    (
                                        settings.host_order,
                                        settings.host_order_updated_at_ms,
                                        host_alias::shareable_aliases(&settings.host_aliases),
                                        input_label::shareable_labels(&settings.input_labels),
                                        settings.monitor_identity_links,
                                    )
                                })
                                .unwrap_or_default();
                            AgentResponse {
                                ready: true,
                                message: ui_text(
                                    "DisplayMux Agent 已就緒",
                                    "DisplayMux Agent is ready",
                                )
                                .to_owned(),
                                display_route: display_routes.first().cloned(),
                                display_routes,
                                protocol_version: AGENT_PROTOCOL_VERSION,
                                host_order,
                                host_order_updated_at_ms,
                                host_aliases,
                                input_labels,
                                monitor_identity_links,
                            }
                        }
                        AgentAction::SwitchInput { monitor, input } => {
                            let resolved = live_settings.read().ok().map(|settings| {
                                match &monitor {
                                    Some(requested) => shared_monitor_index_for_peer(
                                        &settings.shared_monitors,
                                        &settings.monitor_identity_links,
                                        requested,
                                    )
                                        .map(|index| &settings.shared_monitors[index])
                                        .map(|selected| {
                                            (
                                                monitor_identity::identities_for(
                                                    &settings.monitor_identity_links,
                                                    &selected.fingerprint,
                                                ),
                                                selected.vendor_indexed_inputs,
                                            )
                                        })
                                        .ok_or_else(|| ui_text(
                                            "找不到指定的共用螢幕，請確認雙方設定一致",
                                            "The requested shared display was not found; confirm both hosts' selections match",
                                        ).to_owned()),
                                    None => match settings.shared_monitors.as_slice() {
                                        [] => Err(ui_text(
                                            "這台主機尚未選擇共用螢幕",
                                            "No shared display is selected on this host",
                                        )
                                        .to_owned()),
                                        [only] => Ok((
                                            monitor_identity::identities_for(
                                                &settings.monitor_identity_links,
                                                &only.fingerprint,
                                            ),
                                            only.vendor_indexed_inputs,
                                        )),
                                        _ => Err(ui_text(
                                            "配對主機切換到了多台共用螢幕，請將這台電腦更新到最新版本",
                                            "The paired host is now managing multiple shared displays; update this computer to the latest version.",
                                        )
                                        .to_owned()),
                                    },
                                }
                            });
                            let (identities, vendor_indexed) = match resolved {
                                Some(Ok(resolved)) => resolved,
                                Some(Err(message)) => {
                                    return AgentResponse {
                                        ready: false,
                                        message,
                                        display_route: None,
                                        display_routes: Vec::new(),
                                        protocol_version: AGENT_PROTOCOL_VERSION,
                                        ..AgentResponse::default()
                                    };
                                }
                                None => {
                                    return AgentResponse {
                                        ready: false,
                                        message: ui_text(
                                            "無法讀取這台主機的設定",
                                            "Unable to read this host's settings",
                                        )
                                        .to_owned(),
                                        display_route: None,
                                        display_routes: Vec::new(),
                                        protocol_version: AGENT_PROTOCOL_VERSION,
                                        ..AgentResponse::default()
                                    };
                                }
                            };
                            match tauri::async_runtime::spawn_blocking(move || {
                                run_switch(identities, input)
                            })
                            .await
                            {
                                Ok(Ok(_)) => AgentResponse {
                                    ready: true,
                                    message: match UiLocale::current() {
                                        UiLocale::TraditionalChinese => format!(
                                            "遠端主機已切換至 {}",
                                            input_label(vendor_indexed, input)
                                        ),
                                        UiLocale::English => format!(
                                            "The remote host switched to {}",
                                            input_label(vendor_indexed, input)
                                        ),
                                    },
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                                Ok(Err(error)) => AgentResponse {
                                    ready: false,
                                    message: core_user_error(error),
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                                Err(error) => AgentResponse {
                                    ready: false,
                                    message: match UiLocale::current() {
                                        UiLocale::TraditionalChinese => {
                                            format!("切換工作無法執行：{error}")
                                        }
                                        UiLocale::English => {
                                            format!("The switching task could not run: {error}")
                                        }
                                    },
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                            }
                        }
                    }
                }
            })
            .await;
        if let Err(error) = result {
            tracing::error!(error = %error, "DisplayMux agent stopped");
        }
    }));
    Ok(())
}

/// Marks which route ("local" or a peer id) a shared monitor's input was
/// just confirmed switched to, so the dashboard can show the true active
/// host instead of always assuming local. Returns `false` (no-op) if the
/// monitor was since unselected, e.g. removed while the switch was in flight.
fn set_active_route(
    settings: &mut AppSettings,
    fingerprint: &MonitorFingerprint,
    route_id: &str,
    now_ms: u64,
) -> bool {
    let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| selected.fingerprint.matches_exactly(fingerprint))
    else {
        return false;
    };
    selected.active_route = Some(route_id.to_owned());
    selected.active_route_confirmed_at_ms = now_ms;
    true
}

fn record_active_route(
    state: &AppRuntime,
    fingerprint: &MonitorFingerprint,
    route_id: &str,
) -> Result<(), String> {
    let mut settings = read_settings(state)?;
    if set_active_route(&mut settings, fingerprint, route_id, unix_time_ms()) {
        store_settings(state, settings)?;
    }
    Ok(())
}

/// Frontend event telling the dashboard to re-read which host is active.
const ACTIVE_ROUTE_CHANGED_EVENT: &str = "active-route-changed";

/// How long a confirmed switch outranks live reads of the display's input.
/// Displays can keep reporting the previous input for several seconds while
/// they change over, and Windows returns from a switch without waiting.
const ACTIVE_ROUTE_SETTLE_MS: u64 = 15_000;

/// Tells every paired host that `fingerprint` now shows `input`, so their
/// dashboards update without a manual refresh. Fire-and-forget: hosts that are
/// offline or run an agent predating the notice are only logged.
fn announce_active_input(
    state: &AppRuntime,
    fingerprint: &MonitorFingerprint,
    input: DisplayInput,
) {
    broadcast_to_peers(
        state,
        &AgentAction::ActiveInputChanged {
            monitor: fingerprint.clone(),
            input,
        },
    );
}

/// Sends `action` to every paired host without waiting. Hosts that are
/// offline or run an agent predating the action are only logged.
fn broadcast_to_peers(state: &AppRuntime, action: &AgentAction) {
    let Ok(settings) = read_settings(state) else {
        return;
    };
    if !has_valid_shared_key(&settings.shared_key) {
        return;
    }
    let settings = Arc::new(settings);
    for index in 0..settings.peers.len() {
        let settings = Arc::clone(&settings);
        let action = action.clone();
        tauri::async_runtime::spawn(async move {
            let peer = &settings.peers[index];
            if let Err(error) = request_peer(&settings, peer, action).await {
                tracing::info!(
                    peer = peer.name.as_str(),
                    error = %error,
                    "paired host did not accept the notice"
                );
            }
        });
    }
}

/// Frontend event telling the dashboard and host switcher to re-read the order.
const HOST_ORDER_CHANGED_EVENT: &str = "host-order-changed";

/// Adopts a host order from a paired host when it is well formed and newer
/// than the saved one. Returns whether the saved order changed.
fn apply_host_order_notice(
    settings: &mut AppSettings,
    order: Vec<String>,
    updated_at_ms: u64,
) -> bool {
    if updated_at_ms <= settings.host_order_updated_at_ms
        || !host_order::is_valid_shared_host_order(&order)
    {
        return false;
    }
    settings.host_order = order;
    settings.host_order_updated_at_ms = updated_at_ms;
    true
}

/// Whether a paired host whose order changed at `theirs_updated_at_ms` is
/// behind ours.
fn host_order_is_newer_than(settings: &AppSettings, theirs_updated_at_ms: u64) -> bool {
    !settings.host_order.is_empty() && settings.host_order_updated_at_ms > theirs_updated_at_ms
}

/// Minimum gap between exchanges of host names and order with paired hosts.
const HOST_LAYOUT_EXCHANGE_INTERVAL_MS: u64 = 30_000;
static LAST_HOST_LAYOUT_EXCHANGE_MS: AtomicU64 = AtomicU64::new(0);

/// Catches up with changes a host missed while it was offline. Called when the
/// app starts and when the dashboard refreshes, at most every 30 seconds.
#[tauri::command]
fn exchange_host_layout(state: State<'_, AppRuntime>, app: AppHandle) {
    exchange_host_layout_with_peers(&state, &app);
}

/// Asks every paired host for its host names and order, adopts whatever is
/// newer, and sends back whatever that host is missing. Runs in the background.
fn exchange_host_layout_with_peers(state: &AppRuntime, app: &AppHandle) {
    let now_ms = unix_time_ms();
    let last_ms = LAST_HOST_LAYOUT_EXCHANGE_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_ms) < HOST_LAYOUT_EXCHANGE_INTERVAL_MS
        || LAST_HOST_LAYOUT_EXCHANGE_MS
            .compare_exchange(last_ms, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let Ok(settings) = read_settings(state) else {
        return;
    };
    if !has_valid_shared_key(&settings.shared_key) {
        return;
    }
    let settings = Arc::new(settings);
    for index in 0..settings.peers.len() {
        let settings = Arc::clone(&settings);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let peer = &settings.peers[index];
            let theirs = match request_peer(&settings, peer, AgentAction::Ping).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(peer = peer.name.as_str(), error = %error, "paired host unavailable for host layout exchange");
                    return;
                }
            };
            let their_order_updated_at_ms = theirs.host_order_updated_at_ms;
            let their_aliases = theirs.host_aliases.clone();
            let their_labels = theirs.input_labels.clone();
            let their_identity_links = theirs.monitor_identity_links.clone();
            let app_for_adopt = app.clone();
            let adopted = run_display_task(app_for_adopt.clone(), move |state| {
                let mut latest = read_settings(state)?;
                let order_changed = apply_host_order_notice(
                    &mut latest,
                    theirs.host_order,
                    theirs.host_order_updated_at_ms,
                );
                let merged = host_alias::merged_aliases(&latest.host_aliases, &theirs.host_aliases);
                let names_changed = merged.is_some();
                if let Some(merged) = merged {
                    latest.host_aliases = merged;
                }
                let labels_changed = adopt_input_labels(&mut latest, &theirs.input_labels);
                let identities_changed =
                    adopt_monitor_identities(&mut latest, &theirs.monitor_identity_links);
                let latest = if order_changed || names_changed || labels_changed || identities_changed
                {
                    store_settings(state, latest)?
                } else {
                    latest
                };
                for (changed, event) in [
                    (order_changed, HOST_ORDER_CHANGED_EVENT),
                    (names_changed, HOST_NAMES_CHANGED_EVENT),
                    (labels_changed, INPUT_LABELS_CHANGED_EVENT),
                    (identities_changed, MONITOR_IDENTITIES_CHANGED_EVENT),
                ] {
                    if changed {
                        if let Err(error) = app_for_adopt.emit(event, ()) {
                            tracing::warn!(error = %error, event, "unable to notify windows of a host layout change");
                        }
                    }
                }
                Ok(latest)
            })
            .await;
            let latest = match adopted {
                Ok(latest) => latest,
                Err(error) => {
                    tracing::warn!(peer = peer.name.as_str(), error = %error, "unable to adopt a paired host's host layout");
                    return;
                }
            };
            if host_order_is_newer_than(&latest, their_order_updated_at_ms) {
                let action = AgentAction::HostOrderChanged {
                    order: latest.host_order.clone(),
                    updated_at_ms: latest.host_order_updated_at_ms,
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host order");
                }
            }
            if host_alias::has_newer_entries(&latest.host_aliases, &their_aliases) {
                let action = AgentAction::HostAliasesChanged {
                    aliases: host_alias::shareable_aliases(&latest.host_aliases),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host names");
                }
            }
            if monitor_identity::needs_push(&latest.monitor_identity_links, &their_identity_links) {
                let action = AgentAction::MonitorIdentitiesChanged {
                    links: latest.monitor_identity_links.clone(),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the display identities");
                }
            }
            let their_labels = labels_on_local_monitors(&latest, &their_labels);
            if input_label::has_newer_entries(&latest.input_labels, &their_labels) {
                let action = AgentAction::InputLabelsChanged {
                    labels: input_label::shareable_labels(&latest.input_labels),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the input notes");
                }
            }
        });
    }
}

fn peer_ids(settings: &AppSettings) -> Vec<&str> {
    settings.peers.iter().map(|peer| peer.id.as_str()).collect()
}

fn ordered_routes(state: &AppRuntime, settings: &AppSettings) -> Vec<String> {
    host_order::ordered_route_ids(
        &settings.host_order,
        &state.local_host_id,
        &peer_ids(settings),
    )
}

/// Route ids ("local" and peer ids) in the saved host card order.
#[tauri::command]
fn get_host_order(state: State<'_, AppRuntime>) -> Result<Vec<String>, String> {
    let settings = read_settings(&state)?;
    Ok(ordered_routes(&state, &settings))
}

/// Saves a new host card order from dashboard route ids and shares it with
/// every paired host. Returns the order as saved.
#[tauri::command]
fn set_host_order(
    route_ids: Vec<String>,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<Vec<String>, String> {
    let mut settings = read_settings(&state)?;
    let order = host_order::host_order_from_route_ids(
        &route_ids,
        &state.local_host_id,
        &peer_ids(&settings),
        &settings.host_order,
    )
    .ok_or_else(|| {
        ui_text(
            "主機清單已變更，請重新整理後再調整順序",
            "The host list changed. Refresh and reorder again.",
        )
        .to_owned()
    })?;
    // Never move backwards in time, or peers holding a newer order ignore this one.
    let updated_at_ms = unix_time_ms().max(settings.host_order_updated_at_ms + 1);
    settings.host_order = order.clone();
    settings.host_order_updated_at_ms = updated_at_ms;
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(HOST_ORDER_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify the host switcher of a host order change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::HostOrderChanged {
            order,
            updated_at_ms,
        },
    );
    Ok(ordered_routes(&state, &settings))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

async fn receive_host_order_notice(
    app: AppHandle,
    order: Vec<String>,
    updated_at_ms: u64,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if apply_host_order_notice(&mut settings, order, updated_at_ms) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(HOST_ORDER_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a host order change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling windows to re-read custom host names.
const HOST_NAMES_CHANGED_EVENT: &str = "host-names-changed";

/// Custom names by route id ("local" and peer ids); hosts without one are omitted.
fn route_host_names(state: &AppRuntime, settings: &AppSettings) -> HashMap<String, String> {
    std::iter::once((host_order::LOCAL_ROUTE_ID, state.local_host_id.as_str()))
        .chain(
            settings
                .peers
                .iter()
                .map(|peer| (peer.id.as_str(), peer.id.as_str())),
        )
        .filter_map(|(route, host_id)| {
            host_alias::alias_for(&settings.host_aliases, host_id)
                .map(|name| (route.to_owned(), name.to_owned()))
        })
        .collect()
}

#[tauri::command]
fn get_host_names(state: State<'_, AppRuntime>) -> Result<HashMap<String, String>, String> {
    let settings = read_settings(&state)?;
    Ok(route_host_names(&state, &settings))
}

/// Gives the host behind `route_id` a custom name (empty restores the default)
/// and shares every custom name with paired hosts. Returns names by route id.
#[tauri::command]
fn set_host_name(
    route_id: String,
    name: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<HashMap<String, String>, String> {
    let mut settings = read_settings(&state)?;
    let host_id = if route_id == host_order::LOCAL_ROUTE_ID {
        state.local_host_id.clone()
    } else {
        settings
            .peers
            .iter()
            .find(|peer| peer.id == route_id)
            .map(|peer| peer.id.clone())
            .ok_or_else(|| {
                ui_text(
                    "找不到這台主機，請重新整理後再試一次",
                    "This host was not found. Refresh and try again.",
                )
                .to_owned()
            })?
    };
    let name = host_alias::normalize_alias(&name).map_err(alias_error_text)?;
    settings.host_aliases =
        host_alias::with_alias(&settings.host_aliases, &host_id, name, unix_time_ms());
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(HOST_NAMES_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify the host switcher of a host name change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::HostAliasesChanged {
            aliases: host_alias::shareable_aliases(&settings.host_aliases),
        },
    );
    Ok(route_host_names(&state, &settings))
}

fn alias_error_text(error: host_alias::AliasError) -> String {
    match error {
        host_alias::AliasError::TooLong => match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("主機名稱最多 {} 個字", host_alias::MAX_ALIAS_CHARS)
            }
            UiLocale::English => format!(
                "Host names can be at most {} characters",
                host_alias::MAX_ALIAS_CHARS
            ),
        },
        host_alias::AliasError::ControlCharacter => ui_text(
            "主機名稱不能包含換行或控制字元",
            "Host names cannot contain line breaks or control characters",
        )
        .to_owned(),
    }
}

async fn receive_host_aliases_notice(app: AppHandle, aliases: Vec<HostAlias>) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if let Some(merged) = host_alias::merged_aliases(&settings.host_aliases, &aliases) {
            settings.host_aliases = merged;
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(HOST_NAMES_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a host name change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling windows to re-read input names and notes.
const INPUT_LABELS_CHANGED_EVENT: &str = "input-labels-changed";
const MONITOR_IDENTITIES_CHANGED_EVENT: &str = "monitor-identities-changed";

/// Merges a paired host's display-identity claims into ours, keeping the newer
/// entry for each alias. Returns whether anything changed.
fn adopt_monitor_identities(settings: &mut AppSettings, incoming: &[MonitorIdentityLink]) -> bool {
    match monitor_identity::merged_links(&settings.monitor_identity_links, incoming) {
        Some(merged) => {
            settings.monitor_identity_links = merged;
            true
        }
        None => false,
    }
}

/// `labels` from a paired host with each display mapped to this host's shared
/// display, since hosts can read the same display's serial number differently.
fn labels_on_local_monitors(settings: &AppSettings, labels: &[InputLabel]) -> Vec<InputLabel> {
    input_label::with_local_monitors(labels, |monitor| {
        shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            monitor,
        )
        .map(|index| settings.shared_monitors[index].fingerprint.clone())
    })
}

/// Merges a paired host's input notes into `settings`. Returns whether any changed.
fn adopt_input_labels(settings: &mut AppSettings, incoming: &[InputLabel]) -> bool {
    let incoming = labels_on_local_monitors(settings, incoming);
    match input_label::merged_labels(&settings.input_labels, &incoming) {
        Some(merged) => {
            settings.input_labels = merged;
            true
        }
        None => false,
    }
}

/// Moves everything keyed by `alias` onto `primary`, so a merge keeps the
/// inputs and notes the user already set against the identity being absorbed.
/// Entries already held for `primary` win; nothing is overwritten.
fn adopt_alias_settings(
    settings: &mut AppSettings,
    alias: &MonitorFingerprint,
    primary: &MonitorFingerprint,
) {
    for peer in &mut settings.peers {
        if let Some(input) = peer.input_for(alias) {
            if peer.input_for(primary).is_none() {
                peer.set_input_for(primary, Some(input));
            }
            peer.set_input_for(alias, None);
        }
    }
    let now = unix_time_ms();
    let moved = settings
        .input_labels
        .iter()
        .filter(|entry| entry.monitor.matches_exactly(alias))
        .cloned()
        .collect::<Vec<_>>();
    for entry in moved {
        if input_label::label_for(&settings.input_labels, primary, entry.input).is_none() {
            settings.input_labels = input_label::with_label(
                &settings.input_labels,
                primary,
                entry.input,
                entry.label.clone(),
                now,
            );
        }
        settings.input_labels = input_label::with_label(
            &settings.input_labels,
            alias,
            entry.input,
            String::new(),
            now,
        );
    }
}

/// The fingerprint behind an id the UI holds. A display the user wants to merge
/// is usually not a shared display at all — it is the unfamiliar one that
/// appeared when the display mode changed — so a display present right now is
/// accepted by its platform id as well.
fn fingerprint_for_ui_id(
    settings: &AppSettings,
    monitor_id: &str,
) -> Result<MonitorFingerprint, String> {
    if let Ok(selected) = find_shared_monitor(settings, monitor_id) {
        return Ok(selected.fingerprint.clone());
    }
    // Withdrawing a claim must work when neither identity is shared or present
    // any more, which is the state a stale claim leaves behind.
    if let Some(link) = settings
        .monitor_identity_links
        .iter()
        .find(|link| monitor_key(&link.alias) == monitor_id)
    {
        return Ok(link.alias.clone());
    }
    platform_controller()
        .and_then(|controller| controller.enumerate())
        .map_err(core_user_error)?
        .into_iter()
        .find(|monitor| monitor.id.as_str() == monitor_id)
        .map(|monitor| monitor.fingerprint)
        .ok_or_else(display_not_found)
}

/// Declares that the display `alias_id` is the same physical display as the
/// shared display `primary_id`, or withdraws that claim when `primary_id` is
/// `None`.
///
/// Some displays publish a different EDID product code per display mode, which
/// reads as a different display on every host at once. Only the user can say
/// the two are one panel, so this records that claim, folds the absorbed
/// display's own settings into the one it joins, and shares the claim with
/// paired hosts. Switching still demands an exact fingerprint match against a
/// display present right now, so a claim never widens what may be written to.
#[tauri::command]
fn set_monitor_identity_link(
    alias_id: String,
    primary_id: Option<String>,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    let alias = fingerprint_for_ui_id(&settings, &alias_id)?;
    let primary = match primary_id {
        Some(primary_id) => Some(
            find_shared_monitor(&settings, &primary_id)?
                .fingerprint
                .clone(),
        ),
        None => None,
    };
    if primary
        .as_ref()
        .is_some_and(|primary| primary.matches_exactly(&alias))
    {
        return Err(ui_text(
            "無法把螢幕合併到自己",
            "A display cannot be merged into itself",
        )
        .to_owned());
    }

    settings.monitor_identity_links = monitor_identity::with_link(
        &settings.monitor_identity_links,
        &alias,
        primary.as_ref(),
        unix_time_ms(),
    );
    if let Some(primary) = primary.as_ref() {
        adopt_alias_settings(&mut settings, &alias, primary);
        // The absorbed identity is no longer its own shared display; it is
        // reached through the display it joined.
        settings
            .shared_monitors
            .retain(|selected| !selected.fingerprint.matches_exactly(&alias));
    }
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(MONITOR_IDENTITIES_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a display identity change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::MonitorIdentitiesChanged {
            links: settings.monitor_identity_links.clone(),
        },
    );
    Ok(settings)
}

/// Sets the note for one input of a shared display (empty clears it) and
/// shares every note with paired hosts. Returns that display's input options.
#[tauri::command]
fn set_input_label(
    monitor_id: String,
    input: u32,
    label: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<Vec<InputOption>, String> {
    let mut settings = read_settings(&state)?;
    let fingerprint = find_shared_monitor(&settings, &monitor_id)?
        .fingerprint
        .clone();
    let input = DisplayInput::new(input).map_err(core_user_error)?;
    let label = input_label::normalize_label(&label).map_err(label_error_text)?;
    settings.input_labels = input_label::with_label(
        &settings.input_labels,
        &fingerprint,
        input,
        label,
        unix_time_ms(),
    );
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of an input note change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::InputLabelsChanged {
            labels: input_label::shareable_labels(&settings.input_labels),
        },
    );
    input_options(&settings, &monitor_id)
}

fn label_error_text(error: input_label::LabelError) -> String {
    match error {
        input_label::LabelError::TooLong => match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("輸入備註最多 {} 個字", input_label::MAX_LABEL_CHARS)
            }
            UiLocale::English => format!(
                "Input notes can be at most {} characters",
                input_label::MAX_LABEL_CHARS
            ),
        },
        input_label::LabelError::ControlCharacter => ui_text(
            "輸入備註不能包含換行或控制字元",
            "Input notes cannot contain line breaks or control characters",
        )
        .to_owned(),
    }
}

async fn receive_input_labels_notice(app: AppHandle, labels: Vec<InputLabel>) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_input_labels(&mut settings, &labels) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of an input note change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

async fn receive_monitor_identities_notice(
    app: AppHandle,
    links: Vec<MonitorIdentityLink>,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_monitor_identities(&mut settings, &links) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(MONITOR_IDENTITIES_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a display identity change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

fn agent_notice_response(applied: Result<Result<(), String>, tauri::Error>) -> AgentResponse {
    let (ready, message) = match applied {
        Ok(Ok(())) => (true, ui_text("已同步設定", "Settings synced").to_owned()),
        Ok(Err(message)) => (false, message),
        Err(error) => (false, user_error(error)),
    };
    AgentResponse {
        ready,
        message,
        display_route: None,
        display_routes: Vec::new(),
        protocol_version: AGENT_PROTOCOL_VERSION,
        ..AgentResponse::default()
    }
}

async fn receive_active_input_notice(
    app: AppHandle,
    monitor: MonitorFingerprint,
    input: DisplayInput,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if apply_active_input_notice(&mut settings, &monitor, input, unix_time_ms()) {
            tracing::info!(
                input = input.value(),
                "paired host notice moved the active host"
            );
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify the dashboard of an active host change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling the settings page to re-read the saved peer inputs.
const PEER_INPUTS_CHANGED_EVENT: &str = "peer-inputs-changed";

/// Adopts a paired host's confirmed port. The sender vouches for the value
/// because the display was showing *it* when the input was read, which is the
/// only moment a DDC read identifies the reader's own port rather than
/// whichever host happens to be on screen.
async fn receive_local_input_confirmed(
    app: AppHandle,
    host_id: String,
    monitor: MonitorFingerprint,
    input: DisplayInput,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        let route = AgentDisplayRoute {
            monitor,
            input,
            confirmed: true,
        };
        if apply_verified_peer_route(&mut settings, &host_id, route) == PeerRouteOutcome::Applied {
            tracing::info!(
                host_id = host_id.as_str(),
                input = input.value(),
                "adopted a paired host's confirmed display input"
            );
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a paired host's input");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Asks every paired host once, at startup, which port it occupies. A host
/// announces its port when the value changes, so a computer that was off at
/// that moment would otherwise never hear it.
fn adopt_peer_routes_at_startup(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(runtime) = app.try_state::<AppRuntime>() else {
            return;
        };
        let Ok(settings) = read_settings_inner(&runtime) else {
            return;
        };
        if !has_valid_shared_key(&settings.shared_key) {
            return;
        }
        let settings = Arc::new(settings);
        let mut adopted = false;
        for index in 0..settings.peers.len() {
            let peer = &settings.peers[index];
            let response = match request_peer(&settings, peer, AgentAction::Ping).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::info!(
                        peer = peer.name.as_str(),
                        error = %error,
                        "paired host did not answer the startup input query"
                    );
                    continue;
                }
            };
            let mut saved = match read_settings_inner(&runtime) {
                Ok(saved) => saved,
                Err(_) => return,
            };
            let mut changed = false;
            for route in agent_display_routes(&response) {
                if apply_verified_peer_route(&mut saved, &peer.id, route)
                    == PeerRouteOutcome::Applied
                {
                    changed = true;
                    adopted = true;
                }
            }
            if changed {
                if let Err(error) = store_settings(&runtime, saved) {
                    tracing::warn!(error = %error, "unable to save a paired host's reported input");
                    return;
                }
            }
        }
        if adopted {
            if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a paired host's input");
            }
        }
    });
}

/// Tells every paired host which port this computer occupies on the displays
/// it is currently showing on, so their settings fill themselves in. Only
/// displays this host is on screen for are announced, and only when the value
/// changed since the last announcement, so an idle scan loop stays quiet.
fn announce_confirmed_local_inputs(state: &AppRuntime, settings: &AppSettings) {
    let host_id = state.local_host_id.clone();
    if host_id.is_empty() {
        return;
    }
    let confirmed: Vec<(MonitorFingerprint, DisplayInput)> = settings
        .shared_monitors
        .iter()
        .filter(|selected| shows_this_host(selected))
        .filter_map(|selected| Some((selected.fingerprint.clone(), selected.local_input?)))
        .collect();
    let Ok(mut announced) = state.announced_inputs.lock() else {
        return;
    };
    for (fingerprint, input) in confirmed {
        let key = monitor_key(&fingerprint);
        if announced.get(&key) == Some(&input) {
            continue;
        }
        announced.insert(key, input);
        broadcast_to_peers(
            state,
            &AgentAction::LocalInputConfirmed {
                host_id: host_id.clone(),
                monitor: fingerprint,
                input,
            },
        );
    }
}

fn store_settings(state: &AppRuntime, settings: AppSettings) -> Result<AppSettings, String> {
    persist_settings(&state.settings_path, &settings).map_err(core_user_error)?;
    let mut current = state.settings.write().map_err(|_| {
        ui_text(
            "無法更新設定，請重新啟動 DisplayMux",
            "Unable to update settings. Restart DisplayMux.",
        )
        .to_owned()
    })?;
    *current = settings.clone();
    Ok(settings)
}

fn read_settings(state: &AppRuntime) -> Result<AppSettings, String> {
    read_settings_inner(state)
}

fn read_settings_inner(state: &AppRuntime) -> Result<AppSettings, String> {
    state
        .settings
        .read()
        .map(|settings| settings.clone())
        .map_err(|_| {
            ui_text(
                "無法讀取設定，請重新啟動 DisplayMux",
                "Unable to read settings. Restart DisplayMux.",
            )
            .to_owned()
        })
}

fn load_settings(path: &Path) -> AppSettings {
    let Ok(contents) = fs::read_to_string(path) else {
        return AppSettings::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return AppSettings::default();
    };
    settings_from_value(value)
}

/// The `AppSettings`/`HostRoute` shape shipped before multi-monitor support:
/// a single optional `sharedMonitor` plus top-level `localInput`/
/// `supportedInputs`, and one `input` value per peer. Frozen here purely to
/// migrate existing users' `settings.json` without data loss — the live
/// `AppSettings`/`HostRoute` types have since moved to `sharedMonitors`/
/// `inputs`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SingleMonitorHostRoute {
    id: String,
    name: String,
    platform: DestinationHost,
    address: String,
    port: u16,
    mac_address: String,
    input: Option<DisplayInput>,
}

impl Default for SingleMonitorHostRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            platform: local_host(),
            address: String::new(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            input: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SingleMonitorSettings {
    local_host: DestinationHost,
    shared_monitor: Option<SelectedMonitor>,
    local_input: Option<DisplayInput>,
    supported_inputs: Option<Vec<DisplayInput>>,
    peers: Vec<SingleMonitorHostRoute>,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
    onboarding_completed: bool,
    host_switcher_enabled: bool,
    host_switcher_shortcut: String,
}

impl Default for SingleMonitorSettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            shared_monitor: None,
            local_input: None,
            supported_inputs: None,
            peers: Vec::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
            onboarding_completed: false,
            host_switcher_enabled: false,
            host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
        }
    }
}

fn settings_from_value(value: serde_json::Value) -> AppSettings {
    if value.get("sharedMonitors").is_some() {
        let is_existing_install = value.get("onboardingCompleted").is_none();
        let mut settings = serde_json::from_value::<AppSettings>(value).unwrap_or_default();
        if is_existing_install {
            settings.onboarding_completed = true;
        }
        settings
    } else if value.get("sharedMonitor").is_some() || value.get("peers").is_some() {
        migrate_single_monitor_settings(value)
    } else {
        serde_json::from_value::<LegacySettings>(value)
            .map(migrate_legacy_settings)
            .unwrap_or_default()
    }
}

fn migrate_single_monitor_settings(value: serde_json::Value) -> AppSettings {
    let is_existing_install = value.get("onboardingCompleted").is_none();
    let old = serde_json::from_value::<SingleMonitorSettings>(value).unwrap_or_default();
    let fingerprint = old
        .shared_monitor
        .as_ref()
        .map(|monitor| monitor.fingerprint.clone());
    let shared_monitors = old
        .shared_monitor
        .into_iter()
        .map(|mut monitor| {
            monitor.local_input = old.local_input;
            monitor.supported_inputs = old.supported_inputs.clone();
            monitor
        })
        .collect::<Vec<_>>();
    let peers = old
        .peers
        .into_iter()
        .map(|peer| {
            let inputs = match (&fingerprint, peer.input) {
                (Some(monitor), Some(input)) => vec![MonitorInputAssignment {
                    monitor: monitor.clone(),
                    input,
                }],
                _ => Vec::new(),
            };
            HostRoute {
                id: peer.id,
                name: peer.name,
                platform: peer.platform,
                address: peer.address,
                port: peer.port,
                mac_address: peer.mac_address,
                inputs,
            }
        })
        .collect();
    AppSettings {
        local_host: old.local_host,
        shared_monitors,
        peers,
        broadcast_ip: old.broadcast_ip,
        wake_port: old.wake_port,
        shared_key: old.shared_key,
        wait_seconds: old.wait_seconds,
        autostart: old.autostart,
        check_updates: old.check_updates,
        onboarding_completed: old.onboarding_completed || is_existing_install,
        host_switcher_enabled: old.host_switcher_enabled,
        host_switcher_shortcut: old.host_switcher_shortcut,
        host_order: Vec::new(),
        host_order_updated_at_ms: 0,
        host_aliases: Vec::new(),
        input_labels: Vec::new(),
        monitor_identity_links: Vec::new(),
        shared_monitors_chosen: false,
        local_host_id: String::new(),
    }
}

fn migrate_legacy_settings(legacy: LegacySettings) -> AppSettings {
    let peers = if legacy.peer_id.is_empty() || legacy.peer_ip.is_empty() {
        Vec::new()
    } else {
        vec![HostRoute {
            id: legacy.peer_id,
            name: legacy.peer_name,
            platform: match legacy.local_host {
                DestinationHost::Windows => DestinationHost::Mac,
                DestinationHost::Mac => DestinationHost::Windows,
            },
            address: legacy.peer_ip,
            port: legacy.peer_port,
            mac_address: legacy.peer_mac,
            inputs: Vec::new(),
        }]
    };
    AppSettings {
        local_host: legacy.local_host,
        // 舊版沒有保存使用者選擇，也沒有螢幕身分可供輸入值附掛；升級後要求
        // 重新選取，避免沿用硬體假設。
        shared_monitors: Vec::new(),
        peers,
        broadcast_ip: legacy.broadcast_ip,
        wake_port: legacy.wake_port,
        shared_key: legacy.shared_key,
        wait_seconds: legacy.wait_seconds,
        autostart: legacy.autostart,
        check_updates: legacy.check_updates,
        onboarding_completed: true,
        host_switcher_enabled: false,
        host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
        host_order: Vec::new(),
        host_order_updated_at_ms: 0,
        host_aliases: Vec::new(),
        input_labels: Vec::new(),
        monitor_identity_links: Vec::new(),
        shared_monitors_chosen: false,
        local_host_id: String::new(),
    }
}

fn persist_settings(path: &Path, settings: &AppSettings) -> Result<(), DisplayMuxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    }
    let serialized = serde_json::to_vec_pretty(settings)
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    fs::write(path, serialized).map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

fn next_nonce() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{now}-{counter}", std::process::id())
}

/// Switches the display named by `identities` — its own identity and any the
/// user merged into it. Exactly one of them is handed to the service, so a
/// merge decides *which* display is meant and never relaxes the exact
/// fingerprint match the write itself still demands.
fn run_switch(
    identities: Vec<MonitorFingerprint>,
    input: DisplayInput,
) -> Result<SwitchOutcome, DisplayMuxError> {
    let controller = platform_controller()?;
    let present = controller.enumerate()?;
    let shared_monitor = identities
        .iter()
        .find(|identity| {
            present
                .iter()
                .any(|monitor| identity.matches_exactly(&monitor.fingerprint))
        })
        .or_else(|| identities.first())
        .ok_or(DisplayMuxError::TargetNotFound)?
        .clone();
    let service = DisplayMuxService::new(controller, DisplayMuxProfile { shared_monitor });
    service.switch_to_input(input, SwitchMode::Apply)
}

fn enumerate_monitor_inventory() -> Result<MonitorInventory, DisplayMuxError> {
    let controller = platform_controller()?;
    monitor_inventory(&controller)
}

fn monitor_inventory<C: MonitorControl>(
    controller: &C,
) -> Result<MonitorInventory, DisplayMuxError> {
    let detected = controller.enumerate()?;
    let current_inputs: HashMap<_, _> = detected
        .iter()
        .filter_map(|monitor| match controller.read_input(&monitor.id) {
            Ok(input) => Some((monitor.id.clone(), input)),
            Err(error) => {
                tracing::debug!(
                    monitor_id = monitor.id.as_str(),
                    error = %error,
                    "display does not expose a controllable DDC/CI input"
                );
                None
            }
        })
        .collect();
    let controllable = detected
        .iter()
        .filter(|monitor| current_inputs.contains_key(&monitor.id))
        .cloned()
        .collect();
    Ok(MonitorInventory {
        detected,
        controllable,
        current_inputs,
    })
}

/// Re-derives each shared display's active route from the input it just
/// reported, so a switch made outside this app (from another host, the
/// display's own buttons, or a cable swap) shows up on the next refresh.
/// Only an input owned by exactly one route is trusted; an unreadable display
/// or an unrecognized or ambiguous input keeps the last confirmed route.
/// A display confirmed switched within `ACTIVE_ROUTE_SETTLE_MS` is skipped: it
/// can still report its previous input while it changes over.
/// Returns whether any route changed.
fn sync_active_routes_with_live_inputs(
    settings: &mut AppSettings,
    inventory: &MonitorInventory,
    now_ms: u64,
) -> bool {
    let peers = settings.peers.clone();
    let links = settings.monitor_identity_links.clone();
    let mut changed = false;
    for selected in &mut settings.shared_monitors {
        if now_ms.saturating_sub(selected.active_route_confirmed_at_ms) < ACTIVE_ROUTE_SETTLE_MS {
            continue;
        }
        let Some(current) = inventory
            .controllable
            .iter()
            .find(|monitor| is_selected_display(&links, selected, monitor))
            .and_then(|monitor| inventory.current_inputs.get(&monitor.id))
        else {
            continue;
        };
        let previous = selected.active_route.clone();
        if adopt_route_for_input(selected, &peers, *current) == Some(true) {
            tracing::info!(
                monitor = selected.name.as_str(),
                input = current.value(),
                previous = previous.as_deref().unwrap_or(host_order::LOCAL_ROUTE_ID),
                active = selected.active_route.as_deref().unwrap_or_default(),
                "live input moved the active host"
            );
            changed = true;
        }
    }
    changed
}

/// Applies a paired host's notice that it switched `fingerprint` to `input`.
/// The receiver resolves the owning route from its own settings rather than
/// trusting a route id from the sender. A recognized notice starts the settle
/// period (see `sync_active_routes_with_live_inputs`) even when the route
/// already matches. Returns whether the route changed.
fn apply_active_input_notice(
    settings: &mut AppSettings,
    fingerprint: &MonitorFingerprint,
    input: DisplayInput,
    now_ms: u64,
) -> bool {
    let Some(index) = shared_monitor_index_for_peer(
        &settings.shared_monitors,
        &settings.monitor_identity_links,
        fingerprint,
    ) else {
        return false;
    };
    let peers = &settings.peers;
    let selected = &mut settings.shared_monitors[index];
    let Some(changed) = adopt_route_for_input(selected, peers, input) else {
        return false;
    };
    selected.active_route_confirmed_at_ms = now_ms;
    changed
}

/// Marks the single route ("local" or a peer id) configured for `input` as the
/// display's active route. An input owned by no route or by several keeps the
/// last confirmed route. Returns whether the route changed, or `None` when no
/// single route owns `input`.
fn adopt_route_for_input(
    selected: &mut SelectedMonitor,
    peers: &[HostRoute],
    input: DisplayInput,
) -> Option<bool> {
    let local = (selected.local_input == Some(input)).then_some("local");
    let owners: Vec<&str> = local
        .into_iter()
        .chain(
            peers
                .iter()
                .filter(|peer| peer.input_for(&selected.fingerprint) == Some(input))
                .map(|peer| peer.id.as_str()),
        )
        .collect();
    let [owner] = owners.as_slice() else {
        return None;
    };
    // An unset route is rendered as this host, so it already agrees.
    if selected.active_route.as_deref().unwrap_or("local") == *owner {
        return Some(false);
    }
    selected.active_route = Some((*owner).to_owned());
    Some(true)
}

/// External monitors the OS reports but whose DDC/CI input cannot be read.
fn uncontrollable_monitors(inventory: &MonitorInventory) -> Vec<MonitorDescriptor> {
    inventory
        .detected
        .iter()
        .filter(|monitor| {
            !monitor.built_in
                && !inventory
                    .controllable
                    .iter()
                    .any(|controllable| controllable.id == monitor.id)
        })
        .cloned()
        .collect()
}

/// True when the input recorded for this host is of a different kind than
/// the physical connection (e.g. DP recorded on an HDMI link), which usually
/// means the monitor was showing another host when the input was read.
fn connection_input_conflict(
    selected: &SelectedMonitor,
    connection: Option<&displaymux_core::MonitorConnection>,
) -> bool {
    if selected.vendor_indexed_inputs {
        return false;
    }
    let (Some(input), Some(sink)) = (
        selected.local_input,
        connection.and_then(|connection| connection.sink_interface),
    ) else {
        return false;
    };
    displaymux_core::input_matches_sink(sink, input) == Some(false)
}

fn common_input_sources() -> Vec<DisplayInput> {
    [0x01, 0x03, 0x0f, 0x11, 0x12, 0x1b]
        .into_iter()
        .filter_map(|value| DisplayInput::new(value).ok())
        .collect()
}

fn refresh_selected_input_data<C: MonitorControl>(
    controller: &C,
    monitor: &MonitorDescriptor,
    selected: &mut SelectedMonitor,
) -> Result<(), DisplayMuxError> {
    // Reading VCP 0x60 is non-disruptive. Never write or cycle ports for discovery.
    selected.local_input = None;
    selected.supported_inputs = None;
    selected.vendor_indexed_inputs = false;
    let current = controller.read_input(&monitor.id)?;
    selected.local_input = Some(current);
    let advertised = match controller.supported_inputs(&monitor.id) {
        Ok(inputs) if !inputs.is_empty() => Some(inputs),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                error = %error,
                "monitor capabilities unavailable; using common MCCS input list"
            );
            None
        }
    };
    let Some(advertised) = advertised else {
        return Ok(());
    };
    if advertised.contains(&current) {
        selected.supported_inputs = Some(advertised);
        return Ok(());
    }

    // The display is showing an input its own capabilities string does not
    // list, so that list cannot be trusted for writes either. Fall back to
    // the private index range the display reports for VCP 0x60, if any.
    match controller.input_value_maximum(&monitor.id) {
        Ok(Some(maximum)) => {
            let inputs = vendor_index_inputs(maximum, current);
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                current = current.value(),
                maximum,
                "capabilities omit the active input; using the display's private 1..=max index list"
            );
            selected.supported_inputs = Some(inputs);
            selected.vendor_indexed_inputs = true;
        }
        Ok(None) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                current = current.value(),
                "capabilities omit the active input and no value range is available; using common MCCS input list"
            );
        }
        Err(error) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                error = %error,
                "capabilities omit the active input and the value range could not be read; using common MCCS input list"
            );
        }
    }
    Ok(())
}

/// `1..=maximum`, always including `current` even if the display under-reports
/// its range, capped at the one-byte VCP value space.
fn vendor_index_inputs(maximum: u32, current: DisplayInput) -> Vec<DisplayInput> {
    let upper = maximum.max(current.value()).min(u32::from(u8::MAX));
    (1..=upper)
        .filter_map(|value| DisplayInput::new(value).ok())
        .collect()
}

/// Reconciles every currently selected monitor against fresh enumeration
/// results. A selection is only ever refreshed from a monitor whose full EDID
/// fingerprint matches it exactly, so it is never reassigned to a different
/// physical monitor — the exact-fingerprint safety guarantee in
/// product-facts.md, which governs which display may be *switched*.
///
/// Failing to match is not evidence the display is gone. It is asleep, showing
/// a paired host that its other inputs cannot see past, or re-enumerated after
/// a mode switch reporting a serial this host can no longer read the same way
/// (an unreadable EDID falls back to CoreGraphics identity on macOS, and an
/// empty WMI `SerialNumberID` reads as no serial on Windows). Discarding the
/// selection on any of those also discarded every paired host's input for it,
/// so an unmatched selection is kept as it stands and reported as unavailable.
/// Only the user removes a shared display.
///
/// Auto-select only fires from an empty selection; once at least one monitor is
/// selected, a newly appeared monitor is never added automatically.
fn reconcile_monitor_selection(
    settings: &mut AppSettings,
    controllable: &[MonitorDescriptor],
) -> Vec<MonitorSelectionChange> {
    // Auto-select is a one-time onboarding step. An empty list stops meaning
    // "nothing chosen yet" the moment the user chooses, so emptying the list
    // on purpose must not be undone on the next refresh.
    let never_chosen = !settings.shared_monitors_chosen && settings.shared_monitors.is_empty();
    // Taken by value so the selections below can be borrowed mutably.
    let links = settings.monitor_identity_links.clone();
    let mut changes = Vec::new();
    for selected in &mut settings.shared_monitors {
        let Some(current) = controllable.iter().find(|monitor| {
            monitor_identity::is_same_display(&links, &selected.fingerprint, &monitor.fingerprint)
        }) else {
            // Unidentified this time round; keep the stored identity rather
            // than adopting an unproven reading of it.
            continue;
        };
        let mut refreshed = SelectedMonitor::from(current);
        refreshed.fingerprint = selected.fingerprint.clone();
        refreshed.local_input = selected.local_input;
        refreshed.supported_inputs = selected.supported_inputs.clone();
        refreshed.vendor_indexed_inputs = selected.vendor_indexed_inputs;
        refreshed.active_route = selected.active_route.clone();
        refreshed.active_route_confirmed_at_ms = selected.active_route_confirmed_at_ms;
        let metadata_changed = selected.name != refreshed.name
            || selected.max_resolution != refreshed.max_resolution
            || selected.resolution_source != refreshed.resolution_source;
        if metadata_changed {
            let name = refreshed.name.clone();
            *selected = refreshed;
            changes.push(MonitorSelectionChange::RefreshedMetadata { name });
        }
    }

    if never_chosen {
        let mut auto_candidates = controllable.iter().filter(|monitor| !monitor.built_in);
        if let Some(only) = auto_candidates.next() {
            if auto_candidates.next().is_none() {
                let name = only.name.clone();
                settings.shared_monitors.push(SelectedMonitor::from(only));
                settings.shared_monitors_chosen = true;
                changes.push(MonitorSelectionChange::SelectedOnlyMonitor { name });
            }
        }
    }

    changes
}

#[cfg(target_os = "windows")]
fn platform_controller() -> Result<impl MonitorControl, DisplayMuxError> {
    displaymux_core::windows::WindowsMonitorController::new()
}

#[cfg(target_os = "macos")]
fn platform_controller() -> Result<impl MonitorControl, DisplayMuxError> {
    Ok(displaymux_core::macos::MacOsMonitorController::new())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_controller() -> Result<UnsupportedController, DisplayMuxError> {
    Err(DisplayMuxError::UnsupportedPlatform)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
struct UnsupportedController;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl MonitorControl for UnsupportedController {
    fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn read_input(
        &self,
        _monitor: &displaymux_core::MonitorId,
    ) -> Result<DisplayInput, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn supported_inputs(
        &self,
        _monitor: &displaymux_core::MonitorId,
    ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn write_input(
        &self,
        _monitor: &displaymux_core::MonitorId,
        _input: DisplayInput,
    ) -> Result<(), DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "windows")]
const fn local_host() -> DestinationHost {
    DestinationHost::Windows
}
#[cfg(target_os = "macos")]
const fn local_host() -> DestinationHost {
    DestinationHost::Mac
}
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const fn local_host() -> DestinationHost {
    DestinationHost::Windows
}

fn user_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn core_user_error(error: DisplayMuxError) -> String {
    error.localized_message(UiLocale::current() == UiLocale::TraditionalChinese)
}

fn update_error(error: impl std::fmt::Display) -> String {
    tracing::warn!(error = %error, "application update check failed");
    ui_text(
        "無法檢查更新；請確認網路可連線至 GitHub Releases，稍後再試一次",
        "Unable to check for updates. Confirm that GitHub Releases is reachable and try again later.",
    ).to_owned()
}

fn update_install_error(error: impl std::fmt::Display) -> String {
    tracing::error!(error = %error, "signed application update installation failed");
    ui_text(
        "更新下載或簽章驗證失敗；目前版本未變更，請稍後再試一次",
        "The update download or signature verification failed. The current version was not changed; try again later.",
    ).to_owned()
}
fn has_valid_shared_key(shared_key: &str) -> bool {
    shared_key.chars().count() >= MIN_SHARED_KEY_LENGTH
}

fn settings_for_current_build(settings: AppSettings) -> AppSettings {
    // A development executable may point at a dev server and, on Windows, may
    // be a console process. Never persist it as a login item.
    #[cfg(debug_assertions)]
    let settings = AppSettings {
        autostart: false,
        ..settings
    };
    settings
}

fn autostart_args() -> Option<Vec<&'static str>> {
    #[cfg(target_os = "windows")]
    {
        Some(vec!["--autostart"])
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

fn launched_from_autostart(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|arg| arg == "--autostart")
}

fn hide_main_window(window: &tauri::Window) {
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_skip_taskbar(true) {
        tracing::warn!(error = %error, "unable to remove DisplayMux from the taskbar");
    }
    if let Err(error) = window.hide() {
        tracing::warn!(error = %error, "unable to hide DisplayMux in the system tray");
    }
}

#[cfg(target_os = "windows")]
fn hide_windows_main_webview(window: &tauri::WebviewWindow) {
    if let Err(error) = window.set_skip_taskbar(true) {
        tracing::warn!(error = %error, "unable to remove DisplayMux from the taskbar");
    }
    if let Err(error) = window.hide() {
        tracing::warn!(error = %error, "unable to hide DisplayMux in the system tray");
    }
}

fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("unable to find the DisplayMux main window");
        return;
    };
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_skip_taskbar(false) {
        tracing::warn!(error = %error, "unable to restore DisplayMux to the taskbar");
    }
    if let Err(error) = window.show() {
        tracing::warn!(error = %error, "unable to show DisplayMux from the system tray");
    }
    if let Err(error) = window.unminimize() {
        tracing::warn!(error = %error, "unable to unminimize DisplayMux");
    }
    if let Err(error) = window.set_focus() {
        tracing::warn!(error = %error, "unable to focus DisplayMux");
    }
}

#[cfg(target_os = "windows")]
fn setup_windows_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::{
        menu::MenuBuilder,
        tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    };

    let menu = MenuBuilder::new(app)
        .text("tray-open", ui_text("開啟 DisplayMux", "Open DisplayMux"))
        .separator()
        .text("tray-quit", ui_text("結束 DisplayMux", "Quit DisplayMux"))
        .build()?;
    let mut tray = TrayIconBuilder::with_id("displaymux")
        .menu(&menu)
        .tooltip("DisplayMux")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-open" => show_main_window(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    tray.build(app)?;
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .try_init();
    let builder = tauri::Builder::default()
        // This must remain the first plugin so a second launch exits before any
        // other plugin or application setup can create duplicate resources.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("second DisplayMux launch redirected to the existing instance");
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            autostart_args(),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        show_host_switcher(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build());
    let builder = builder.on_window_event(|window, event| match (window.label(), event) {
        ("host-switcher", tauri::WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            if let Err(error) = window.hide() {
                tracing::warn!(error = %error, "unable to hide the host switcher window");
            }
        }
        ("main", tauri::WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            hide_main_window(window);
        }
        #[cfg(target_os = "windows")]
        ("main", tauri::WindowEvent::Resized(_)) if window.is_minimized().unwrap_or(false) => {
            hide_main_window(window);
        }
        _ => {}
    });
    builder
        .setup(|app| {
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|error| anyhow::anyhow!(error))?;
            let settings_path = config_dir.join("settings.json");
            let settings = settings_for_current_build(load_settings(&settings_path));
            #[cfg(debug_assertions)]
            if let Err(error) = app.autolaunch().disable() {
                tracing::warn!(error = %error, "unable to remove development autostart entry");
            }
            #[cfg(all(target_os = "windows", not(debug_assertions)))]
            if settings.autostart {
                if let Err(error) = app.autolaunch().enable() {
                    tracing::warn!(error = %error, "unable to refresh the login autostart entry");
                }
            }
            let detected = LocalHostIdentity::detect(local_host()).unwrap_or_else(|error| {
                tracing::warn!(error = %error, "unable to read this computer's host name");
                LocalHostIdentity::from_parts("DisplayMux".to_owned(), local_host(), None)
            });
            // Fixed once and then kept: every paired host stores this id, so
            // re-deriving it would silently strand this computer's pairings,
            // its place in the shared host order and its custom name.
            let mut settings = settings;
            let identity = if settings.local_host_id.is_empty() {
                settings.local_host_id = detected.id.clone();
                if let Err(error) = persist_settings(&settings_path, &settings) {
                    tracing::warn!(error = %error, "unable to save this computer's host id");
                }
                detected
            } else {
                LocalHostIdentity::with_id(
                    settings.local_host_id.clone(),
                    detected.name,
                    detected.platform,
                    detected.mac_address,
                )
            };
            let discovery = MdnsPeerDiscovery::start(&identity, DEFAULT_AGENT_PORT)
                .map(Some)
                .unwrap_or_else(|error| {
                    tracing::warn!(error = %error, "unable to start DisplayMux mDNS discovery");
                    None
                });
            app.manage(AppRuntime {
                settings: Arc::new(RwLock::new(settings)),
                settings_path,
                agent_task: Mutex::new(None),
                discovery,
                local_host_id: identity.id,
                local_host_name: identity.name,
                announced_inputs: std::sync::Mutex::new(HashMap::new()),
            });
            if let Some(runtime) = app.try_state::<AppRuntime>() {
                let settings = read_settings_inner(&runtime).map_err(anyhow::Error::msg)?;
                if settings.host_switcher_enabled {
                    match validate_host_switcher_shortcut(&settings.host_switcher_shortcut) {
                        Ok(shortcut) => {
                            if let Err(error) = app.global_shortcut().register(shortcut) {
                                tracing::warn!(error = %error, "unable to register the saved host switcher shortcut");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "saved host switcher shortcut is invalid");
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            {
                setup_windows_tray(app)?;
                if launched_from_autostart(std::env::args()) {
                    if let Some(window) = app.get_webview_window("main") {
                        hide_windows_main_webview(&window);
                    }
                }
            }
            let handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Some(runtime) = handle.try_state::<AppRuntime>() {
                    if let Err(error) = restart_agent(&runtime, &handle).await {
                        tracing::warn!(error = %error, "unable to start DisplayMux agent");
                    }
                    exchange_host_layout_with_peers(&runtime, &handle);
                    adopt_peer_routes_at_startup(&handle);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_locale,
            discover_peers,
            select_peer,
            remove_peer,
            add_shared_monitor,
            remove_shared_monitor,
            get_settings,
            get_host_switcher_state,
            get_host_order,
            set_host_order,
            get_host_names,
            set_host_name,
            set_input_label,
            set_monitor_identity_link,
            reset_settings,
            exchange_host_layout,
            hide_host_switcher,
            check_host_switcher_shortcut,
            complete_onboarding,
            get_input_options,
            save_settings,
            check_for_update,
            install_update,
            get_dashboard_state,
            probe_peer,
            wake_peer,
            switch_host
        ])
        .run(tauri::generate_context!())
        .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    struct SelectionController {
        monitors: Vec<MonitorDescriptor>,
        controllable: HashSet<String>,
    }

    impl MonitorControl for SelectionController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(self.monitors.clone())
        }

        fn read_input(
            &self,
            monitor: &displaymux_core::MonitorId,
        ) -> Result<DisplayInput, DisplayMuxError> {
            if self.controllable.contains(monitor.as_str()) {
                DisplayInput::new(0x0f)
            } else {
                Err(DisplayMuxError::Backend("DDC/CI unavailable".to_owned()))
            }
        }

        fn supported_inputs(
            &self,
            monitor: &displaymux_core::MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            if self.controllable.contains(monitor.as_str()) {
                Ok(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                    DisplayInput::new(0x1b).unwrap(),
                ])
            } else {
                Err(DisplayMuxError::Backend(
                    "capabilities unavailable".to_owned(),
                ))
            }
        }

        fn write_input(
            &self,
            _monitor: &displaymux_core::MonitorId,
            _input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            unreachable!("selection tests never write an input")
        }
    }

    fn monitor(id: &str) -> MonitorDescriptor {
        MonitorDescriptor {
            id: displaymux_core::MonitorId::new(id),
            name: id.to_owned(),
            fingerprint: MonitorFingerprint::new("ACM", id, Some(format!("serial-{id}"))),
            active: true,
            built_in: false,
            max_resolution: Some(displaymux_core::MonitorResolution::new(2560, 1440)),
            resolution_source: Some(ResolutionSource::WindowsDisplayMode),
            connection: None,
        }
    }

    #[test]
    fn uncontrollable_monitors_list_detected_externals_that_ddc_cannot_reach() {
        let external = monitor("external");
        let mut internal = monitor("internal");
        internal.built_in = true;
        let unreachable = monitor("unreachable");
        let controller = SelectionController {
            monitors: vec![internal, unreachable.clone(), external.clone()],
            controllable: HashSet::from(["internal".to_owned(), external.id.as_str().to_owned()]),
        };
        let inventory = monitor_inventory(&controller).unwrap();

        assert_eq!(uncontrollable_monitors(&inventory), vec![unreachable]);
    }

    fn hdmi_connection() -> displaymux_core::MonitorConnection {
        displaymux_core::MonitorConnection::classify(
            Some(displaymux_core::HostOutput::UsbC),
            None,
            Some(displaymux_core::SinkInterface::Hdmi),
            false,
        )
    }

    fn selected_with_input(value: u32, vendor_indexed: bool) -> SelectedMonitor {
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.local_input = DisplayInput::new(value).ok();
        selected.vendor_indexed_inputs = vendor_indexed;
        selected
    }

    #[test]
    fn recorded_displayport_input_on_an_hdmi_connection_is_flagged() {
        let connection = hdmi_connection();
        assert!(connection_input_conflict(
            &selected_with_input(0x0f, false),
            Some(&connection)
        ));
        assert!(!connection_input_conflict(
            &selected_with_input(0x11, false),
            Some(&connection)
        ));
    }

    #[test]
    fn input_conflict_is_never_guessed_from_vague_data() {
        let connection = hdmi_connection();
        // Private index values, missing connection data, or no recorded input.
        assert!(!connection_input_conflict(
            &selected_with_input(0x0f, true),
            Some(&connection)
        ));
        assert!(!connection_input_conflict(
            &selected_with_input(0x0f, false),
            None
        ));
        let unset = SelectedMonitor::from(&monitor("shared"));
        assert!(!connection_input_conflict(&unset, Some(&connection)));
    }

    #[test]
    fn shared_key_requires_at_least_eight_characters() {
        assert!(!has_valid_shared_key("1234567"));
        assert!(has_valid_shared_key("12345678"));
        assert!(has_valid_shared_key("配對密碼八個字元"));
    }

    #[test]
    fn new_install_does_not_assume_a_monitor_or_input() {
        let settings = AppSettings::default();
        assert!(settings.shared_monitors.is_empty());
        assert!(settings.peers.is_empty());
        assert!(settings.check_updates);
        assert!(!settings.onboarding_completed);
        assert!(!settings.host_switcher_enabled);
        assert_eq!(
            settings.host_switcher_shortcut,
            DEFAULT_HOST_SWITCHER_SHORTCUT
        );
    }

    #[test]
    fn host_switcher_shortcut_requires_a_non_shift_modifier() {
        assert!(validate_host_switcher_shortcut("CommandOrControl+Alt+Space").is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyK").is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Shift+KeyA").is_ok());
        assert!(validate_host_switcher_shortcut("Control+Super+KeyC").is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyW").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyS").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Shift+KeyW").is_err());
        assert!(validate_host_switcher_shortcut("Shift+KeyK").is_err());
        assert!(validate_host_switcher_shortcut("KeyK").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Alt+Shift+KeyA").is_err());
        assert!(validate_host_switcher_shortcut("not-a-shortcut").is_err());
    }

    #[test]
    fn existing_settings_receive_disabled_host_switcher_defaults() {
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("hostSwitcherEnabled");
        object.remove("hostSwitcherShortcut");

        let settings = settings_from_value(value);

        assert!(!settings.host_switcher_enabled);
        assert_eq!(
            settings.host_switcher_shortcut,
            DEFAULT_HOST_SWITCHER_SHORTCUT
        );
    }

    #[test]
    fn existing_install_without_onboarding_marker_does_not_show_first_run_flow() {
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        value.as_object_mut().unwrap().remove("onboardingCompleted");

        let settings = settings_from_value(value);

        assert!(settings.onboarding_completed);
    }

    #[test]
    fn development_builds_do_not_register_autostart() {
        let settings = settings_for_current_build(AppSettings::default());
        assert_eq!(settings.autostart, !cfg!(debug_assertions));
    }

    #[test]
    fn only_the_explicit_login_argument_starts_windows_hidden() {
        assert!(launched_from_autostart([
            "DisplayMux.exe".to_owned(),
            "--autostart".to_owned(),
        ]));
        assert!(!launched_from_autostart(["DisplayMux.exe".to_owned()]));
    }

    #[test]
    fn automatic_switch_tracks_wake_and_network_fallback_state() {
        assert!(!NetworkPreparation::NotRequired.peer_woken());
        assert!(!NetworkPreparation::Ready { wake_sent: true }.warning());
        assert!(NetworkPreparation::Ready { wake_sent: true }.peer_woken());
        assert!(NetworkPreparation::Unavailable {
            wake_sent: false,
            reason: "offline".to_owned(),
        }
        .warning());
    }

    #[test]
    fn v011_selected_monitor_without_resolution_source_still_loads() {
        let value = serde_json::json!({
            "name": "Existing monitor",
            "fingerprint": {
                "manufacturer_id": "ACM",
                "product_code": "1234",
                "serial_number": "serial"
            },
            "maxResolution": { "width": 3440, "height": 1440 }
        });
        let selected: SelectedMonitor = serde_json::from_value(value).unwrap();
        assert_eq!(selected.resolution_source, None);
        assert_eq!(selected.max_resolution.unwrap().width, 3440);
        assert!(selected.local_input.is_none());
        assert!(selected.supported_inputs.is_none());
    }

    #[test]
    fn migration_preserves_the_previous_two_host_configuration() {
        let cases = [
            (DestinationHost::Windows, DestinationHost::Mac),
            (DestinationHost::Mac, DestinationHost::Windows),
        ];

        for (local_host, peer_platform) in cases {
            let legacy = LegacySettings {
                local_host,
                peer_id: "peer".to_owned(),
                peer_name: "Peer computer".to_owned(),
                peer_ip: "192.168.1.20".to_owned(),
                ..LegacySettings::default()
            };
            let migrated = migrate_legacy_settings(legacy);
            // The legacy shape has no monitor identity to attach an input
            // guess to; upgrading requires an explicit re-selection.
            assert!(migrated.shared_monitors.is_empty());
            assert!(migrated.onboarding_completed);
            assert_eq!(migrated.peers[0].platform, peer_platform);
            assert!(migrated.peers[0].inputs.is_empty());
        }
    }

    #[test]
    fn migrate_single_monitor_settings_carries_forward_selection_and_peer_input() {
        let selected = monitor("shared");
        let value = serde_json::json!({
            "localHost": "mac",
            "sharedMonitor": {
                "name": selected.name,
                "fingerprint": {
                    "manufacturer_id": selected.fingerprint.manufacturer_id,
                    "product_code": selected.fingerprint.product_code,
                    "serial_number": selected.fingerprint.serial_number,
                },
            },
            "localInput": 15,
            "supportedInputs": [15, 17],
            "peers": [{
                "id": "peer",
                "name": "Peer computer",
                "platform": "windows",
                "address": "192.168.1.20",
                "port": DEFAULT_AGENT_PORT,
                "macAddress": "",
                "input": 17,
            }],
        });

        let migrated = migrate_single_monitor_settings(value);

        assert_eq!(migrated.shared_monitors.len(), 1);
        let migrated_selection = &migrated.shared_monitors[0];
        assert!(migrated_selection
            .fingerprint
            .matches_exactly(&selected.fingerprint));
        assert_eq!(migrated_selection.local_input.unwrap().value(), 15);
        assert_eq!(
            migrated_selection
                .supported_inputs
                .as_ref()
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            vec![15, 17]
        );
        assert_eq!(
            migrated.peers[0]
                .input_for(&selected.fingerprint)
                .unwrap()
                .value(),
            17
        );
    }

    #[test]
    fn selects_and_persists_the_only_controllable_monitor() {
        let external = monitor("external");
        let mut internal = monitor("internal");
        internal.built_in = true;
        let uncontrollable = monitor("uncontrollable");
        let controller = SelectionController {
            monitors: vec![internal, uncontrollable, external.clone()],
            controllable: HashSet::from(["internal".to_owned(), external.id.as_str().to_owned()]),
        };
        let inventory = monitor_inventory(&controller).unwrap();
        assert_eq!(inventory.detected.len(), 3);
        assert_eq!(inventory.controllable.len(), 2);
        let mut settings = AppSettings::default();

        let changes = reconcile_monitor_selection(&mut settings, &inventory.controllable);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::SelectedOnlyMonitor {
                name: "external".to_owned()
            }]
        );
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&external)]
        );
    }

    #[test]
    fn selected_monitor_records_current_input_and_capability_values_without_writes() {
        let external = monitor("external");
        let controller = SelectionController {
            monitors: vec![external.clone()],
            controllable: HashSet::from([external.id.as_str().to_owned()]),
        };
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(&controller, &external, &mut selected).unwrap();

        assert_eq!(selected.local_input.unwrap().value(), 0x0f);
        assert_eq!(
            selected
                .supported_inputs
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            vec![0x0f, 0x11, 0x1b]
        );
        assert!(!selected.vendor_indexed_inputs);
    }

    /// Mimics an MStar-style display: capabilities advertise MCCS codes it
    /// never honours, while the live value and range use a private index.
    struct VendorIndexedController {
        maximum: Option<u32>,
    }

    impl MonitorControl for VendorIndexedController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(Vec::new())
        }

        fn read_input(
            &self,
            _monitor: &displaymux_core::MonitorId,
        ) -> Result<DisplayInput, DisplayMuxError> {
            DisplayInput::new(0x08)
        }

        fn supported_inputs(
            &self,
            _monitor: &displaymux_core::MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            Ok(vec![
                DisplayInput::new(0x0f).unwrap(),
                DisplayInput::new(0x11).unwrap(),
            ])
        }

        fn input_value_maximum(
            &self,
            _monitor: &displaymux_core::MonitorId,
        ) -> Result<Option<u32>, DisplayMuxError> {
            Ok(self.maximum)
        }

        fn write_input(
            &self,
            _monitor: &displaymux_core::MonitorId,
            _input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            unreachable!("selection tests never write an input")
        }
    }

    #[test]
    fn capabilities_that_omit_the_active_input_fall_back_to_the_private_index_range() {
        let external = monitor("external");
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(
            &VendorIndexedController {
                maximum: Some(0x0e),
            },
            &external,
            &mut selected,
        )
        .unwrap();

        assert_eq!(selected.local_input.unwrap().value(), 0x08);
        assert!(selected.vendor_indexed_inputs);
        assert_eq!(
            selected
                .supported_inputs
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            (1..=0x0e).collect::<Vec<_>>()
        );
    }

    #[test]
    fn capabilities_that_omit_the_active_input_without_a_range_use_the_common_list() {
        let external = monitor("external");
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(
            &VendorIndexedController { maximum: None },
            &external,
            &mut selected,
        )
        .unwrap();

        assert_eq!(selected.supported_inputs, None);
        assert!(!selected.vendor_indexed_inputs);
    }

    #[test]
    fn vendor_index_inputs_always_include_the_active_value() {
        let current = DisplayInput::new(0x08).unwrap();
        let values = vendor_index_inputs(0x05, current)
            .iter()
            .map(|input| input.value())
            .collect::<Vec<_>>();
        assert_eq!(values, (1..=0x08).collect::<Vec<_>>());
    }

    #[test]
    fn vendor_indexed_inputs_are_labelled_by_index_not_mccs_name() {
        let seven = DisplayInput::new(0x07).unwrap();
        assert!(input_label(true, seven).ends_with(" 7"));
        assert!(!input_label(false, seven).ends_with(" 7"));
    }

    #[test]
    fn common_input_names_include_vga_dvi_dp_hdmi_and_type_c_without_codes() {
        let inputs = common_input_sources();
        for value in [0x01, 0x03, 0x0f, 0x11, 0x1b] {
            assert!(inputs.iter().any(|input| input.value() == value));
        }
        assert_eq!(
            localized_input_name(DisplayInput::new(0x01).unwrap()),
            "VGA"
        );
        assert_eq!(
            localized_input_name(DisplayInput::new(0x03).unwrap()),
            "DVI"
        );
        assert_eq!(localized_input_name(DisplayInput::new(0x0f).unwrap()), "DP");
        assert_eq!(
            localized_input_name(DisplayInput::new(0x1b).unwrap()),
            "Type-C"
        );
        assert!(!localized_input_name(DisplayInput::new(0x11).unwrap()).contains("0x"));
    }

    #[test]
    fn verified_peer_route_is_applied_only_for_the_same_monitor_and_free_port() {
        let selected = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(0x0f).ok(),
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                    DisplayInput::new(0x12).unwrap(),
                ]),
                ..SelectedMonitor::from(&selected)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: selected.fingerprint.clone(),
                    input: DisplayInput::new(0x11).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&selected.fingerprint)
                .unwrap()
                .value(),
            0x11
        );

        settings.peers[0].set_input_for(&selected.fingerprint, None);
        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: monitor("different").fingerprint,
                    input: DisplayInput::new(0x12).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::UnknownMonitor
        );
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: selected.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Taken
        );
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());
    }

    /// A host that is off screen reads whichever host *is* on screen, so its
    /// report must not overwrite an input the user already has set.
    #[test]
    fn an_unconfirmed_report_keeps_the_input_a_peer_already_has() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x11).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Kept
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&display.fingerprint)
                .unwrap()
                .value(),
            0x11
        );
    }

    /// A host that is on screen when it reads the input can only be reading
    /// its own port, so it corrects a wrong value the user picked earlier.
    #[test]
    fn a_confirmed_report_corrects_the_input_a_peer_already_has() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x11).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&display.fingerprint)
                .unwrap()
                .value(),
            0x0f
        );
    }

    /// Re-reporting the value a peer already has is not a change to save.
    #[test]
    fn re_reporting_the_same_input_changes_nothing() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x0f).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Unchanged
        );
    }

    /// A display this host does not offer cannot be assigned, however sure
    /// the reporting host is.
    #[test]
    fn a_confirmed_report_of_an_unsupported_input_is_rejected() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x11).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Unsupported
        );
        assert!(settings.peers[0].input_for(&display.fingerprint).is_none());
    }

    #[test]
    fn a_display_never_switched_away_still_counts_as_showing_this_host() {
        let display = monitor("display");
        let mut selected = SelectedMonitor::from(&display);

        assert!(shows_this_host(&selected));

        selected.active_route = Some("local".to_owned());
        assert!(shows_this_host(&selected));

        selected.active_route = Some("peer".to_owned());
        assert!(!shows_this_host(&selected));
    }

    fn peer_route(id: &str) -> HostRoute {
        HostRoute {
            id: id.to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn verified_peer_route_for_one_monitor_never_leaks_into_another() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor {
                    supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                    ..SelectedMonitor::from(&monitor_a)
                },
                SelectedMonitor {
                    supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                    ..SelectedMonitor::from(&monitor_b)
                },
            ],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: monitor_b.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Applied
        );

        assert!(settings.peers[0]
            .input_for(&monitor_a.fingerprint)
            .is_none());
        assert_eq!(
            settings.peers[0]
                .input_for(&monitor_b.fingerprint)
                .unwrap()
                .value(),
            0x0f
        );
    }

    #[test]
    fn set_active_route_updates_the_matching_monitor_only() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&monitor_a),
                SelectedMonitor::from(&monitor_b),
            ],
            ..AppSettings::default()
        };

        assert!(set_active_route(
            &mut settings,
            &monitor_b.fingerprint,
            "peer",
            SETTLED_MS
        ));

        assert_eq!(settings.shared_monitors[0].active_route, None);
        assert_eq!(
            settings.shared_monitors[1].active_route,
            Some("peer".to_owned())
        );
    }

    #[test]
    fn set_active_route_is_a_noop_for_an_unselected_monitor() {
        let selected = monitor("shared");
        let missing = monitor("missing");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected)],
            ..AppSettings::default()
        };

        assert!(!set_active_route(
            &mut settings,
            &missing.fingerprint,
            "peer",
            SETTLED_MS
        ));
        assert_eq!(settings.shared_monitors[0].active_route, None);
    }

    fn peer_using_input(id: &str, monitor: &MonitorDescriptor, input: u32) -> HostRoute {
        HostRoute {
            id: id.to_owned(),
            name: id.to_owned(),
            platform: DestinationHost::Windows,
            address: "192.168.1.30".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: vec![MonitorInputAssignment {
                monitor: monitor.fingerprint.clone(),
                input: DisplayInput::new(input).unwrap(),
            }],
        }
    }

    fn routed_settings(
        shared: &MonitorDescriptor,
        local_input: u32,
        peer_input: u32,
        active_route: Option<&str>,
    ) -> AppSettings {
        AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(local_input).ok(),
                active_route: active_route.map(str::to_owned),
                ..SelectedMonitor::from(shared)
            }],
            peers: vec![peer_using_input("peer", shared, peer_input)],
            ..AppSettings::default()
        }
    }

    fn inventory_reading(monitor: &MonitorDescriptor, input: u32) -> MonitorInventory {
        MonitorInventory {
            detected: vec![monitor.clone()],
            controllable: vec![monitor.clone()],
            current_inputs: HashMap::from([(
                monitor.id.clone(),
                DisplayInput::new(input).unwrap(),
            )]),
        }
    }

    #[test]
    fn monitor_inventory_keeps_the_input_each_controllable_display_reported() {
        let external = monitor("external");
        let unreachable = monitor("unreachable");
        let controller = SelectionController {
            monitors: vec![unreachable.clone(), external.clone()],
            controllable: HashSet::from([external.id.as_str().to_owned()]),
        };

        let inventory = monitor_inventory(&controller).unwrap();

        assert_eq!(
            inventory.current_inputs.get(&external.id),
            Some(&DisplayInput::new(0x0f).unwrap())
        );
        assert!(!inventory.current_inputs.contains_key(&unreachable.id));
    }

    #[test]
    fn live_input_moves_active_route_to_the_peer_that_owns_it() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x07),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn live_input_moves_active_route_back_to_this_host_after_an_external_switch() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn live_input_matching_no_route_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x03),
            SETTLED_MS,
        );

        assert!(!changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn live_input_shared_by_several_routes_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x07, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x07),
            SETTLED_MS,
        );

        assert!(!changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn unreadable_display_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));
        let inventory = MonitorInventory {
            detected: vec![shared.clone()],
            controllable: Vec::new(),
            current_inputs: HashMap::new(),
        };

        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory,
            SETTLED_MS
        ));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn peer_notice_marks_this_host_active_when_it_names_the_local_input() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_notice_marks_the_peer_that_owns_the_input_active() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x07).unwrap(),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn peer_notice_for_an_unknown_display_or_input_changes_nothing() {
        let shared = monitor("shared");
        let other = monitor("other");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        assert!(!apply_active_input_notice(
            &mut settings,
            &other.fingerprint,
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS
        ));
        assert!(!apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x03).unwrap(),
            SETTLED_MS
        ));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    /// How a paired host can describe `display`: same model, but it read a
    /// different serial number (EDID text on Windows, a number or none on macOS).
    fn as_seen_by_peer(display: &MonitorDescriptor) -> MonitorFingerprint {
        MonitorFingerprint {
            serial_number: Some("EDID-TEXT-SERIAL".to_owned()),
            ..display.fingerprint.clone()
        }
    }

    #[test]
    fn peer_notice_matches_the_display_when_hosts_read_different_serials() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &as_seen_by_peer(&shared),
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_display_is_not_guessed_between_two_shared_displays_of_the_same_model() {
        let left = monitor("twin");
        let right = MonitorDescriptor {
            id: displaymux_core::MonitorId::new("twin-right"),
            fingerprint: MonitorFingerprint {
                serial_number: Some("serial-twin-right".to_owned()),
                ..left.fingerprint.clone()
            },
            ..left.clone()
        };
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&left), SelectedMonitor::from(&right)],
            ..AppSettings::default()
        };

        assert_eq!(
            shared_monitor_index_for_peer(
                &settings.shared_monitors,
                &settings.monitor_identity_links,
                &as_seen_by_peer(&left)
            ),
            None
        );
        assert_eq!(
            shared_monitor_index_for_peer(
                &settings.shared_monitors,
                &settings.monitor_identity_links,
                &right.fingerprint
            ),
            Some(1)
        );
    }

    #[test]
    fn display_state_explains_an_unreadable_display_that_another_host_is_showing() {
        let peer = Some("2cf05de0c029-windows");

        assert_eq!(shared_display_state(true, peer), SharedDisplayState::Ready);
        assert_eq!(
            shared_display_state(false, peer),
            SharedDisplayState::OnOtherHost
        );
        assert_eq!(
            shared_display_state(false, Some("local")),
            SharedDisplayState::Unavailable
        );
        assert_eq!(
            shared_display_state(false, None),
            SharedDisplayState::Unavailable
        );
    }

    #[test]
    fn newer_host_order_from_a_peer_replaces_the_saved_order() {
        let mut settings = AppSettings {
            host_order: vec!["mac-a".to_owned(), "pc-b".to_owned()],
            host_order_updated_at_ms: 100,
            ..AppSettings::default()
        };

        let changed = apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned(), "mac-a".to_owned()],
            200,
        );

        assert!(changed);
        assert_eq!(settings.host_order, vec!["pc-b", "mac-a"]);
        assert_eq!(settings.host_order_updated_at_ms, 200);
    }

    #[test]
    fn a_peer_needs_our_host_order_only_when_ours_is_newer() {
        let settings = AppSettings {
            host_order: vec!["mac-a".to_owned(), "pc-b".to_owned()],
            host_order_updated_at_ms: 200,
            ..AppSettings::default()
        };

        assert!(host_order_is_newer_than(&settings, 0));
        assert!(host_order_is_newer_than(&settings, 199));
        assert!(!host_order_is_newer_than(&settings, 200));
        assert!(!host_order_is_newer_than(&AppSettings::default(), 0));
    }

    #[test]
    fn stale_or_malformed_host_order_from_a_peer_is_ignored() {
        let saved = vec!["mac-a".to_owned(), "pc-b".to_owned()];
        let mut settings = AppSettings {
            host_order: saved.clone(),
            host_order_updated_at_ms: 100,
            ..AppSettings::default()
        };

        assert!(!apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned()],
            100
        ));
        assert!(!apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned(), "pc-b".to_owned()],
            300
        ));
        assert_eq!(settings.host_order, saved);
        assert_eq!(settings.host_order_updated_at_ms, 100);
    }

    /// A time well past any settle period, for tests about other behaviour.
    const SETTLED_MS: u64 = ACTIVE_ROUTE_SETTLE_MS * 100;

    #[test]
    fn live_input_right_after_a_confirmed_switch_does_not_revert_it() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        let switched_at = SETTLED_MS;
        assert!(set_active_route(
            &mut settings,
            &shared.fingerprint,
            "peer",
            switched_at
        ));
        // The display still reports the old input while it changes over.
        let stale = inventory_reading(&shared, 0x08);

        let changed_while_settling = sync_active_routes_with_live_inputs(
            &mut settings,
            &stale,
            switched_at + ACTIVE_ROUTE_SETTLE_MS - 1,
        );
        assert!(!changed_while_settling);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );

        let changed_after_settling = sync_active_routes_with_live_inputs(
            &mut settings,
            &stale,
            switched_at + ACTIVE_ROUTE_SETTLE_MS,
        );
        assert!(changed_after_settling);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_notice_starts_the_settle_period_even_when_the_route_already_matches() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x07).unwrap(),
            SETTLED_MS,
        );

        assert!(!changed);
        assert_eq!(
            settings.shared_monitors[0].active_route_confirmed_at_ms,
            SETTLED_MS
        );
        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS + 1
        ));
    }

    #[test]
    fn unset_active_route_already_means_this_host() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);

        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS
        ));
        assert_eq!(settings.shared_monitors[0].active_route, None);
    }

    #[test]
    fn settings_reject_duplicate_and_unadvertised_input_assignments() {
        let selected = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(0x0f).ok(),
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&selected)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });
        settings.peers[0].set_input_for(&selected.fingerprint, DisplayInput::new(0x0f).ok());
        assert!(validate_settings(&settings).is_err());

        settings.peers[0].set_input_for(&selected.fingerprint, DisplayInput::new(0x1b).ok());
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn the_same_input_value_may_be_assigned_to_different_monitors() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&monitor_a),
                SelectedMonitor::from(&monitor_b),
            ],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });
        settings.peers[0].set_input_for(&monitor_a.fingerprint, DisplayInput::new(0x0f).ok());
        settings.peers[0].set_input_for(&monitor_b.fingerprint, DisplayInput::new(0x0f).ok());

        assert!(validate_settings(&settings).is_ok());
    }

    #[test]
    fn emptying_the_shared_list_on_purpose_is_not_undone_by_auto_select() {
        // With one controllable display, removing it emptied the list and the
        // next refresh auto-selected it straight back, so it could never be
        // removed at all.
        let only = monitor("only");
        let mut settings = AppSettings {
            shared_monitors: Vec::new(),
            shared_monitors_chosen: true,
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

        assert!(changes.is_empty());
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn auto_select_still_runs_for_a_computer_that_has_never_chosen() {
        let only = monitor("only");
        let mut settings = AppSettings::default();

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::SelectedOnlyMonitor {
                name: "only".to_owned()
            }]
        );
        assert_eq!(settings.shared_monitors.len(), 1);
        assert!(
            settings.shared_monitors_chosen,
            "auto-select decides the list, so it must not run twice"
        );
    }

    fn configured_settings() -> AppSettings {
        let shared = monitor("shared");
        AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            shared_monitors_chosen: true,
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &MonitorFingerprint::new("MSI", "7CF0", None::<String>),
                Some(&MonitorFingerprint::new("MSI", "3CF0", None::<String>)),
                10,
            ),
            input_labels: vec![InputLabel {
                monitor: shared.fingerprint.clone(),
                input: DisplayInput::new(8).unwrap(),
                label: "USB-C".to_owned(),
                updated_at_ms: 10,
            }],
            peers: vec![peer_using_input("ITX-PC", &shared, 7)],
            shared_key: "pairing-password".to_owned(),
            host_switcher_shortcut: "Alt+Q".to_owned(),
            local_host_id: "kept-id".to_owned(),
            ..AppSettings::default()
        }
    }

    #[test]
    fn a_reset_list_is_not_refilled_by_auto_select() {
        // Auto-select runs for a computer that has never chosen. A reset is a
        // choice, made by someone who is present, so the list stays empty
        // instead of the display reappearing on the next refresh.
        for scope in [ResetScope::Displays, ResetScope::Everything] {
            let mut settings = settings_after_reset(&configured_settings(), scope);
            assert!(settings.shared_monitors.is_empty());

            let only = monitor("only");
            let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

            assert!(changes.is_empty(), "{scope:?} refilled the list");
            assert!(
                settings.shared_monitors.is_empty(),
                "{scope:?} refilled the list"
            );
        }
    }

    #[test]
    fn resetting_displays_keeps_the_pairing() {
        let settings = settings_after_reset(&configured_settings(), ResetScope::Displays);

        assert!(settings.shared_monitors.is_empty());
        assert!(
            settings.shared_monitors_chosen,
            "emptied on purpose, so auto-select must not refill it"
        );
        assert!(settings.monitor_identity_links.is_empty());
        assert!(settings.input_labels.is_empty());
        assert_eq!(settings.peers.len(), 1, "the paired host survives");
        assert!(
            settings.peers[0].inputs.is_empty(),
            "its inputs named displays that are gone"
        );
        assert_eq!(settings.shared_key, "pairing-password");
        assert_eq!(settings.host_switcher_shortcut, "Alt+Q");
    }

    #[test]
    fn resetting_everything_keeps_only_this_computer_identity() {
        let settings = settings_after_reset(&configured_settings(), ResetScope::Everything);

        assert_eq!(
            settings.local_host_id, "kept-id",
            "paired hosts name this computer by its id, so a local reset must not change it"
        );
        assert!(settings.shared_monitors.is_empty());
        assert!(settings.peers.is_empty());
        assert!(settings.shared_key.is_empty());
        assert!(settings.monitor_identity_links.is_empty());
        assert_eq!(
            settings.host_switcher_shortcut,
            AppSettings::default().host_switcher_shortcut
        );
    }

    #[test]
    fn a_withdrawn_claim_is_not_shown_as_one() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let links = monitor_identity::with_link(
            &monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            &at_1080,
            None,
            20,
        );
        let settings = AppSettings {
            monitor_identity_links: links,
            ..AppSettings::default()
        };

        assert!(monitor_identity_claims(&settings).is_empty());
    }

    #[test]
    fn a_claim_is_shown_with_keys_that_withdraw_it() {
        // Neither identity is shared or present here, which is exactly the
        // state a claim left behind by earlier testing sits in.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let settings = AppSettings {
            monitor_identity_links: monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            ..AppSettings::default()
        };

        let claims = monitor_identity_claims(&settings);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].alias_key, monitor_key(&at_1080));
        assert_eq!(claims[0].primary_label, "MSI / 3CF0");
        assert_eq!(
            fingerprint_for_ui_id(&settings, &claims[0].alias_key),
            Ok(at_1080)
        );
    }

    #[test]
    fn a_shared_display_is_named_by_its_own_key_so_it_can_be_removed_while_absent() {
        // Removal used to enumerate and fail when the display was not present,
        // which left exactly the displays a user wants to drop — asleep, on
        // another host, or reporting an identity this host no longer knows —
        // as the only ones that could not be dropped.
        let absent = monitor("disconnected");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&absent)],
            ..AppSettings::default()
        };

        let key = monitor_key(&absent.fingerprint);
        assert_eq!(
            find_shared_monitor(&settings, &key).map(|selected| selected.fingerprint.clone()),
            Ok(absent.fingerprint)
        );
    }

    #[test]
    fn a_selection_stored_under_an_alias_is_not_added_a_second_time() {
        // Guards the shape that produced three byte-identical entries: the
        // stored selection was the alias, so it failed to recognise itself.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_4k.clone();
        let links = monitor_identity::with_link(&[], &at_4k, Some(&at_1080), 10);

        assert!(
            is_selected_display(&links, &selected, &present),
            "a display stored under an alias must recognise itself"
        );
    }

    #[test]
    fn a_merged_identity_is_recognised_as_the_shared_display_everywhere() {
        // Reconciling kept the selection but the dashboard still reported it as
        // missing, so the display read as "not found" right after a merge.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_1080.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_4k.clone();
        let links = monitor_identity::with_link(&[], &at_4k, Some(&at_1080), 10);

        assert!(is_selected_display(&links, &selected, &present));
        assert!(!is_selected_display(
            &links,
            &selected,
            &monitor("somebody-else")
        ));
    }

    #[test]
    fn a_merged_identity_keeps_the_shared_display_available() {
        // The MSI MPG 274U publishes MSI:3CF0 at 4K and MSI:7CF0 at 1080. Once
        // the user says they are one panel, the display stays usable in both.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_1080.clone();
        let mut settings = AppSettings {
            shared_monitors: vec![selected],
            monitor_identity_links: monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&present));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors[0].fingerprint, at_4k,
            "the stored identity stays put so paired hosts keep naming the same display"
        );
    }

    #[test]
    fn an_unmerged_identity_of_the_same_model_is_still_a_different_display() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k;
        let mut present = monitor("shared");
        present.fingerprint = at_1080;
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&present));

        assert!(changes.is_empty());
        assert_eq!(settings.shared_monitors, vec![selected]);
    }

    #[test]
    fn a_merged_identity_names_the_same_display_to_a_paired_host() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let monitors = [selected];
        let links = monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10);

        assert_eq!(
            shared_monitor_index_for_peer(&monitors, &links, &at_1080),
            Some(0)
        );
        assert_eq!(
            shared_monitor_index_for_peer(
                &monitors,
                &links,
                &MonitorFingerprint::new("ACR", "0725", Some("576726074".to_owned()))
            ),
            None
        );
    }

    #[test]
    fn merging_moves_a_paired_host_input_onto_the_display_it_joins() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let input = DisplayInput::new(7).unwrap();
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 1);
        peer.inputs.clear();
        peer.set_input_for(&at_1080, Some(input));
        let mut settings = AppSettings {
            peers: vec![peer],
            ..AppSettings::default()
        };

        adopt_alias_settings(&mut settings, &at_1080, &at_4k);

        assert_eq!(settings.peers[0].input_for(&at_4k), Some(input));
        assert_eq!(settings.peers[0].input_for(&at_1080), None);
    }

    #[test]
    fn merging_never_overwrites_an_input_already_set_for_the_display_it_joins() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let kept = DisplayInput::new(7).unwrap();
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 1);
        peer.inputs.clear();
        peer.set_input_for(&at_4k, Some(kept));
        peer.set_input_for(&at_1080, Some(DisplayInput::new(8).unwrap()));
        let mut settings = AppSettings {
            peers: vec![peer],
            ..AppSettings::default()
        };

        adopt_alias_settings(&mut settings, &at_1080, &at_4k);

        assert_eq!(settings.peers[0].input_for(&at_4k), Some(kept));
    }

    #[test]
    fn a_selection_whose_serial_stops_being_readable_is_kept() {
        // A display re-enumerated after a mode switch can report the same model
        // with no serial at all (an unreadable EDID on macOS, an empty WMI
        // SerialNumberID on Windows). That is a failure to identify it, not
        // proof it is gone, so the user's selection has to survive it.
        let selected_monitor = monitor("shared");
        let mut reappeared = selected_monitor.clone();
        reappeared.fingerprint = MonitorFingerprint::new("ACM", "shared", None::<String>);
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected_monitor)],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&reappeared));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&selected_monitor)],
            "the stored identity must not be rewritten from an unproven reading"
        );
    }

    #[test]
    fn a_selection_that_cannot_be_identified_is_kept_rather_than_reassigned() {
        let previous = monitor("disconnected");
        let replacement = monitor("replacement");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&previous)],
            ..AppSettings::default()
        };

        let changes =
            reconcile_monitor_selection(&mut settings, std::slice::from_ref(&replacement));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&previous)],
            "an unidentified selection is never reassigned to a different monitor"
        );
    }

    #[test]
    fn a_paired_host_input_note_names_this_host_shared_display_input() {
        let ours = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let theirs = MonitorFingerprint::new("MSI", "3CF0", Some("PC-SERIAL".to_owned()));
        let mut selected = SelectedMonitor::from(&monitor("mpg"));
        selected.fingerprint = ours.clone();
        selected.vendor_indexed_inputs = true;
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };
        let input = DisplayInput::new(8).unwrap();
        let notice = [InputLabel {
            monitor: theirs,
            input,
            label: "USB-C".to_owned(),
            updated_at_ms: 10,
        }];

        assert!(adopt_input_labels(&mut settings, &notice));
        assert!(!adopt_input_labels(&mut settings, &notice));

        assert_eq!(settings.input_labels[0].monitor, ours);
        let name = noted_input_label(&settings, &selected, input);
        assert!(name.starts_with("USB-C"));
        assert!(name.contains(&input_label(true, input)));
        assert_eq!(
            noted_input_label(&settings, &selected, DisplayInput::new(7).unwrap()),
            input_label(true, DisplayInput::new(7).unwrap())
        );
    }

    #[test]
    fn keeps_a_missing_selection_that_a_paired_host_is_showing() {
        let switched_away = monitor("switched-away");
        let stays = monitor("stays");
        let mut selected = SelectedMonitor::from(&switched_away);
        selected.active_route = Some("peer-a".to_owned());
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone(), SelectedMonitor::from(&stays)],
            peers: vec![peer_using_input("peer-a", &switched_away, 0x0f)],
            ..AppSettings::default()
        };
        let monitors = [stays.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![selected, SelectedMonitor::from(&stays)]
        );
    }

    #[test]
    fn a_missing_selection_is_kept_even_when_its_active_host_is_no_longer_paired() {
        let disconnected = monitor("disconnected");
        let mut selected = SelectedMonitor::from(&disconnected);
        selected.active_route = Some("removed-peer".to_owned());
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, &[]);

        assert!(changes.is_empty());
        assert_eq!(settings.shared_monitors, vec![selected]);
    }

    #[test]
    fn never_guesses_between_multiple_controllable_monitors() {
        let mut settings = AppSettings::default();
        let monitors = [monitor("first"), monitor("second")];

        assert!(reconcile_monitor_selection(&mut settings, &monitors).is_empty());
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn missing_selection_is_kept_and_not_replaced_when_multiple_candidates_remain() {
        let disconnected = monitor("disconnected");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&disconnected)],
            ..AppSettings::default()
        };
        let monitors = [monitor("first"), monitor("second")];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&disconnected)]
        );
    }

    #[test]
    fn one_monitor_disappearing_does_not_affect_another_independently_selected_monitor() {
        let stays = monitor("stays");
        let disconnected = monitor("disconnected");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&stays),
                SelectedMonitor::from(&disconnected),
            ],
            ..AppSettings::default()
        };
        let monitors = [stays.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![
                SelectedMonitor::from(&stays),
                SelectedMonitor::from(&disconnected),
            ]
        );
    }

    #[test]
    fn auto_select_never_adds_a_new_monitor_once_one_is_already_selected() {
        let already_selected = monitor("already-selected");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&already_selected)],
            ..AppSettings::default()
        };
        let monitors = [already_selected.clone(), monitor("new-arrival")];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&already_selected)]
        );
    }

    #[test]
    fn metadata_refresh_for_one_selected_monitor_does_not_touch_another() {
        let mut refreshed = monitor("refreshed");
        let unaffected = monitor("unaffected");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&refreshed),
                SelectedMonitor::from(&unaffected),
            ],
            ..AppSettings::default()
        };
        refreshed.name = "renamed".to_owned();
        let monitors = [refreshed.clone(), unaffected.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::RefreshedMetadata {
                name: "renamed".to_owned()
            }]
        );
        assert_eq!(settings.shared_monitors[0].name, "renamed");
        assert_eq!(
            settings.shared_monitors[1],
            SelectedMonitor::from(&unaffected)
        );
    }

    #[test]
    fn plan_switch_input_targets_v2_peers_by_fingerprint() {
        let fingerprint = monitor("target").fingerprint;
        assert_eq!(
            plan_switch_input(AGENT_PROTOCOL_VERSION, 1, &fingerprint),
            Ok(Some(fingerprint.clone()))
        );
        assert_eq!(
            plan_switch_input(AGENT_PROTOCOL_VERSION, 3, &fingerprint),
            Ok(Some(fingerprint))
        );
    }

    #[test]
    fn plan_switch_input_falls_back_to_legacy_shape_for_a_single_monitor() {
        let fingerprint = monitor("target").fingerprint;
        assert_eq!(plan_switch_input(0, 1, &fingerprint), Ok(None));
        assert_eq!(plan_switch_input(0, 0, &fingerprint), Ok(None));
    }

    #[test]
    fn plan_switch_input_refuses_to_guess_for_an_old_peer_with_multiple_monitors() {
        let fingerprint = monitor("target").fingerprint;
        assert!(plan_switch_input(0, 2, &fingerprint).is_err());
        assert!(plan_switch_input(AGENT_PROTOCOL_VERSION - 1, 2, &fingerprint).is_err());
    }

    #[test]
    fn automatic_offline_fallback_explains_the_black_screen_risk() {
        let target = monitor("external");
        let input = DisplayInput::new(0x11).unwrap();
        let preparation = NetworkPreparation::Unavailable {
            wake_sent: true,
            reason: "Agent 沒有回應".to_owned(),
        };
        let result = outcome_result(
            SwitchOutcome::AlreadySelected { target, input },
            &preparation,
            |input| input_label(false, input),
        );

        assert!(result.warning);
        assert!(result.peer_woken);
        assert!(result
            .detail
            .contains("local DDC/CI was selected automatically"));
        assert!(result.detail.contains("temporarily blank"));
    }

    #[test]
    fn locale_detection_uses_traditional_chinese_and_falls_back_to_english() {
        assert_eq!(locale_from_tag("zh-TW"), UiLocale::TraditionalChinese);
        assert_eq!(locale_from_tag("zh-Hant-HK"), UiLocale::TraditionalChinese);
        assert_eq!(locale_from_tag("en-US"), UiLocale::English);
        assert_eq!(locale_from_tag("ja-JP"), UiLocale::English);
        assert_eq!(locale_from_tag("zh-CN"), UiLocale::English);
    }

    #[test]
    fn transient_ddc_failure_does_not_replace_a_still_detected_selection() {
        let selected = monitor("selected");
        let replacement = monitor("replacement");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected)],
            ..AppSettings::default()
        };

        // The selection is detected but unreadable, so it is absent from the
        // controllable list while another monitor is present in it.
        assert!(
            reconcile_monitor_selection(&mut settings, std::slice::from_ref(&replacement))
                .is_empty()
        );
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&selected)]
        );
    }
}
