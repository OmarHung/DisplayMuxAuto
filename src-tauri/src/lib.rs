use std::{
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
    DiscoveredPeer, DisplayInput, DisplayMuxError, DisplayMuxProfile, DisplayMuxService,
    MacAddress, MdnsPeerDiscovery, MonitorControl, MonitorDescriptor, MonitorFingerprint,
    PeerDiscovery, PeerEndpoint, ResolutionSource, SwitchMode, SwitchOutcome, WakeTarget,
    AGENT_PROTOCOL_VERSION, DEFAULT_AGENT_PORT,
};
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Manager, State};
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
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedMonitorStatus {
    monitor_key: String,
    fingerprint: MonitorFingerprint,
    name: String,
    ddc_available: bool,
    status_text: String,
    connection: Option<displaymux_core::MonitorConnection>,
    connection_input_conflict: bool,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MonitorSelectionChange {
    SelectedOnlyMonitor {
        name: String,
    },
    RemovedMissingMonitor {
        name: String,
        fingerprint: MonitorFingerprint,
    },
    RefreshedMetadata {
        name: String,
    },
}

struct MonitorInventory {
    detected: Vec<MonitorDescriptor>,
    controllable: Vec<MonitorDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputOption {
    value: u32,
    name: String,
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
                apply_verified_peer_route(&mut settings, &route.id, display_route);
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

fn resolve_controllable_monitor(
    monitor_id: &str,
) -> Result<(impl MonitorControl, MonitorDescriptor), String> {
    let controller = platform_controller().map_err(core_user_error)?;
    let monitor = monitor_inventory(&controller)
        .map_err(core_user_error)?
        .controllable
        .into_iter()
        .find(|monitor| monitor.id.as_str() == monitor_id)
        .ok_or_else(|| {
            ui_text(
                "找不到這台螢幕，請重新整理後再選擇",
                "This display was not found. Refresh and select it again.",
            )
            .to_owned()
        })?;
    Ok((controller, monitor))
}

#[tauri::command]
fn add_shared_monitor(
    monitor_id: String,
    state: State<'_, AppRuntime>,
) -> Result<AppSettings, String> {
    let (controller, monitor) = resolve_controllable_monitor(&monitor_id)?;
    let mut settings = read_settings(&state)?;
    let already_selected = settings
        .shared_monitors
        .iter()
        .any(|selected| selected.fingerprint.matches_exactly(&monitor.fingerprint));
    if !already_selected {
        settings
            .shared_monitors
            .push(SelectedMonitor::from(&monitor));
    }
    if let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| selected.fingerprint.matches_exactly(&monitor.fingerprint))
    {
        refresh_selected_input_data(&controller, &monitor, selected).map_err(core_user_error)?;
    }
    store_settings(&state, settings)
}

#[tauri::command]
fn remove_shared_monitor(
    monitor_id: String,
    state: State<'_, AppRuntime>,
) -> Result<AppSettings, String> {
    let (_controller, monitor) = resolve_controllable_monitor(&monitor_id)?;
    let mut settings = read_settings(&state)?;
    settings
        .shared_monitors
        .retain(|selected| !selected.fingerprint.matches_exactly(&monitor.fingerprint));
    for peer in &mut settings.peers {
        peer.set_input_for(&monitor.fingerprint, None);
    }
    store_settings(&state, settings)
}

#[tauri::command]
fn get_settings(state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    read_settings(&state)
}

#[tauri::command]
fn get_host_switcher_state(state: State<'_, AppRuntime>) -> Result<HostSwitcherState, String> {
    let settings = read_settings(&state)?;
    let monitors = settings
        .shared_monitors
        .iter()
        .map(|selected| {
            let mut hosts = Vec::with_capacity(settings.peers.len() + 1);
            hosts.push(HostSwitcherOption {
                id: "local".to_owned(),
                name: ui_text("這台電腦", "This computer").to_owned(),
                platform: settings.local_host,
                input_name: selected.local_input.map(localized_input_name),
                is_local: true,
                available: selected.local_input.is_some(),
            });
            hosts.extend(settings.peers.iter().map(|peer| {
                let input = peer.input_for(&selected.fingerprint);
                HostSwitcherOption {
                    id: peer.id.clone(),
                    name: peer.name.clone(),
                    platform: peer.platform,
                    input_name: input.map(localized_input_name),
                    is_local: false,
                    available: input.is_some(),
                }
            }));
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
    let selected = find_shared_monitor(&settings, &monitor_id)?;
    let inputs = selected
        .supported_inputs
        .clone()
        .filter(|inputs| !inputs.is_empty())
        .unwrap_or_else(common_input_sources);
    Ok(inputs
        .into_iter()
        .map(|input| InputOption {
            value: input.value(),
            name: input_label(selected.vendor_indexed_inputs, input),
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
    restart_agent(&state).await?;
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
    tauri::async_runtime::spawn_blocking(move || {
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_dashboard_state(&app.state::<AppRuntime>())
    })
    .await
    .map_err(user_error)?
}

fn build_dashboard_state(state: &AppRuntime) -> Result<DashboardState, String> {
    let mut settings = read_settings(state)?;
    let (monitors, uncontrollable_monitors, shared, selection_notices) =
        match enumerate_monitor_inventory() {
            Ok(inventory) => {
                let changes = reconcile_monitor_selection(
                    &mut settings,
                    &inventory.detected,
                    &inventory.controllable,
                );
                if !changes.is_empty() {
                    for change in &changes {
                        if let MonitorSelectionChange::RemovedMissingMonitor {
                            fingerprint, ..
                        } = change
                        {
                            for peer in &mut settings.peers {
                                peer.set_input_for(fingerprint, None);
                            }
                        }
                    }
                    if let Ok(controller) = platform_controller() {
                        for selected in &mut settings.shared_monitors {
                            if let Some(current) = inventory.controllable.iter().find(|monitor| {
                                selected.fingerprint.matches_exactly(&monitor.fingerprint)
                            }) {
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
                    store_settings(state, settings.clone())?;
                }
                let selection_notices = changes
                    .into_iter()
                    .filter_map(selection_notice_text)
                    .collect();
                let shared = settings
                    .shared_monitors
                    .iter()
                    .map(|selected| {
                        let target_found = inventory.controllable.iter().any(|monitor| {
                            selected.fingerprint.matches_exactly(&monitor.fingerprint)
                        });
                        let detected = inventory.detected.iter().find(|monitor| {
                            selected.fingerprint.matches_exactly(&monitor.fingerprint)
                        });
                        let target_detected = detected.is_some();
                        let connection = detected.and_then(|monitor| monitor.connection.clone());
                        SharedMonitorStatus {
                            monitor_key: monitor_key(&selected.fingerprint),
                            fingerprint: selected.fingerprint.clone(),
                            name: selected.name.clone(),
                            ddc_available: target_found,
                            status_text: shared_monitor_status_text(
                                &selected.name,
                                target_found,
                                target_detected,
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
        MonitorSelectionChange::RemovedMissingMonitor { name, .. } => {
            Some(match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("先前選取的 {name} 已消失，已自動移出共用螢幕清單")
                }
                UiLocale::English => format!(
                    "Previously selected {name} disappeared and was automatically removed from the shared display list"
                ),
            })
        }
        MonitorSelectionChange::RefreshedMetadata { .. } => None,
    }
}

fn shared_monitor_status_text(name: &str, target_found: bool, target_detected: bool) -> String {
    if target_found {
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
    request_peer(&settings, peer, AgentAction::Ping).await?;
    Ok(OperationResult {
        title: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{} 已連線", peer.name),
            UiLocale::English => format!("{} connected", peer.name),
        },
        detail: ui_text(
            "DisplayMux Agent 已就緒。",
            "The DisplayMux Agent is ready.",
        )
        .to_owned(),
        peer_woken: false,
        warning: false,
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
    match run_switch(selected.fingerprint.clone(), input) {
        Ok(outcome) => {
            record_active_route(&state, &selected.fingerprint, &target_id)?;
            Ok(outcome_result(
                outcome,
                &preparation,
                selected.vendor_indexed_inputs,
            ))
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
            Ok(OperationResult {
                title: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!("已由 {} 執行切換", executor.name),
                    UiLocale::English => format!("Switch performed by {}", executor.name),
                },
                detail: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!(
                        "遠端主機已切換至 {}。",
                        input_label(selected.vendor_indexed_inputs, input)
                    ),
                    UiLocale::English => format!(
                        "The remote host switched to {}.",
                        input_label(selected.vendor_indexed_inputs, input)
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
    vendor_indexed: bool,
) -> OperationResult {
    let label = |input: DisplayInput| input_label(vendor_indexed, input);
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

fn apply_verified_peer_route(
    settings: &mut AppSettings,
    peer_id: &str,
    route: AgentDisplayRoute,
) -> bool {
    let Some((fingerprint, local_input, supported_inputs)) = settings
        .shared_monitors
        .iter()
        .find(|selected| selected.fingerprint.matches_exactly(&route.monitor))
        .map(|selected| {
            (
                selected.fingerprint.clone(),
                selected.local_input,
                selected.supported_inputs.clone(),
            )
        })
    else {
        return false;
    };
    let supported = supported_inputs.as_ref().map_or_else(
        || common_input_sources().contains(&route.input),
        |inputs| inputs.contains(&route.input),
    );
    let already_assigned = local_input == Some(route.input)
        || settings
            .peers
            .iter()
            .any(|peer| peer.id != peer_id && peer.input_for(&fingerprint) == Some(route.input));
    if !supported || already_assigned {
        return false;
    }
    let Some(peer) = settings.peers.iter_mut().find(|peer| peer.id == peer_id) else {
        return false;
    };
    peer.set_input_for(&fingerprint, Some(route.input));
    true
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

async fn restart_agent(state: &AppRuntime) -> Result<(), String> {
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
    *current_task = Some(tauri::async_runtime::spawn(async move {
        let result = server
            .run(move |action| {
                let live_settings = Arc::clone(&live_settings);
                async move {
                    match action {
                        AgentAction::Ping => {
                            let display_routes = live_settings
                                .read()
                                .ok()
                                .map(|settings| {
                                    settings
                                        .shared_monitors
                                        .iter()
                                        .filter_map(|selected| {
                                            Some(AgentDisplayRoute {
                                                monitor: selected.fingerprint.clone(),
                                                input: selected.local_input?,
                                            })
                                        })
                                        .collect::<Vec<_>>()
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
                            }
                        }
                        AgentAction::SwitchInput { monitor, input } => {
                            let resolved = live_settings.read().ok().map(|settings| {
                                match &monitor {
                                    Some(requested) => settings
                                        .shared_monitors
                                        .iter()
                                        .find(|selected| {
                                            selected.fingerprint.matches_exactly(requested)
                                        })
                                        .map(|selected| {
                                            (selected.fingerprint.clone(), selected.vendor_indexed_inputs)
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
                                        [only] => Ok((only.fingerprint.clone(), only.vendor_indexed_inputs)),
                                        _ => Err(ui_text(
                                            "配對主機切換到了多台共用螢幕，請將這台電腦更新到最新版本",
                                            "The paired host is now managing multiple shared displays; update this computer to the latest version.",
                                        )
                                        .to_owned()),
                                    },
                                }
                            });
                            let (fingerprint, vendor_indexed) = match resolved {
                                Some(Ok(resolved)) => resolved,
                                Some(Err(message)) => {
                                    return AgentResponse {
                                        ready: false,
                                        message,
                                        display_route: None,
                                        display_routes: Vec::new(),
                                        protocol_version: AGENT_PROTOCOL_VERSION,
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
                                    };
                                }
                            };
                            match tauri::async_runtime::spawn_blocking(move || {
                                run_switch(fingerprint, input)
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
                                },
                                Ok(Err(error)) => AgentResponse {
                                    ready: false,
                                    message: core_user_error(error),
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
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
) -> bool {
    let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| selected.fingerprint.matches_exactly(fingerprint))
    else {
        return false;
    };
    selected.active_route = Some(route_id.to_owned());
    true
}

fn record_active_route(
    state: &AppRuntime,
    fingerprint: &MonitorFingerprint,
    route_id: &str,
) -> Result<(), String> {
    let mut settings = read_settings(state)?;
    if set_active_route(&mut settings, fingerprint, route_id) {
        store_settings(state, settings)?;
    }
    Ok(())
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

fn run_switch(
    fingerprint: MonitorFingerprint,
    input: DisplayInput,
) -> Result<SwitchOutcome, DisplayMuxError> {
    let service = DisplayMuxService::new(
        platform_controller()?,
        DisplayMuxProfile {
            shared_monitor: fingerprint,
        },
    );
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
    let controllable = detected
        .iter()
        .filter(|monitor| match controller.read_input(&monitor.id) {
            Ok(_) => true,
            Err(error) => {
                tracing::debug!(
                    monitor_id = monitor.id.as_str(),
                    error = %error,
                    "display does not expose a controllable DDC/CI input"
                );
                false
            }
        })
        .cloned()
        .collect();
    Ok(MonitorInventory {
        detected,
        controllable,
    })
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
/// results. Never reassigns a missing selection to a different physical
/// monitor — a disappeared monitor is only ever removed, matching the
/// exact-fingerprint safety guarantee in product-facts.md. Auto-select only
/// fires from an empty selection; once at least one monitor is selected, a
/// newly appeared monitor is never added automatically.
fn reconcile_monitor_selection(
    settings: &mut AppSettings,
    detected: &[MonitorDescriptor],
    controllable: &[MonitorDescriptor],
) -> Vec<MonitorSelectionChange> {
    // Auto-select is only for "nothing has ever been selected" (onboarding).
    // Captured before the removal pass below so a monitor disappearing
    // during this same call never triggers an auto-pick of a replacement.
    let had_no_selection = settings.shared_monitors.is_empty();
    let mut changes = Vec::new();
    let mut index = 0;
    while index < settings.shared_monitors.len() {
        let selected = &settings.shared_monitors[index];
        if let Some(current) = controllable
            .iter()
            .find(|monitor| selected.fingerprint.matches_exactly(&monitor.fingerprint))
        {
            let mut refreshed = SelectedMonitor::from(current);
            refreshed.local_input = selected.local_input;
            refreshed.supported_inputs = selected.supported_inputs.clone();
            refreshed.vendor_indexed_inputs = selected.vendor_indexed_inputs;
            refreshed.active_route = selected.active_route.clone();
            let metadata_changed = selected.name != refreshed.name
                || selected.max_resolution != refreshed.max_resolution
                || selected.resolution_source != refreshed.resolution_source;
            if metadata_changed {
                let name = refreshed.name.clone();
                settings.shared_monitors[index] = refreshed;
                changes.push(MonitorSelectionChange::RefreshedMetadata { name });
            }
            index += 1;
            continue;
        }
        if detected
            .iter()
            .any(|monitor| selected.fingerprint.matches_exactly(&monitor.fingerprint))
        {
            // Transient DDC failure (e.g. the monitor is asleep); keep the
            // selection rather than dropping it.
            index += 1;
            continue;
        }
        // Physically gone; remove, never reassign to a different monitor.
        let removed = settings.shared_monitors.remove(index);
        changes.push(MonitorSelectionChange::RemovedMissingMonitor {
            name: removed.name,
            fingerprint: removed.fingerprint,
        });
    }

    if had_no_selection {
        let mut auto_candidates = controllable.iter().filter(|monitor| !monitor.built_in);
        if let Some(only) = auto_candidates.next() {
            if auto_candidates.next().is_none() {
                let name = only.name.clone();
                settings.shared_monitors.push(SelectedMonitor::from(only));
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
            let discovery = MdnsPeerDiscovery::start(local_host(), DEFAULT_AGENT_PORT)
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
                    if let Err(error) = restart_agent(&runtime).await {
                        tracing::warn!(error = %error, "unable to start DisplayMux agent");
                    }
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

        let changes = reconcile_monitor_selection(
            &mut settings,
            &inventory.detected,
            &inventory.controllable,
        );

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

        assert!(apply_verified_peer_route(
            &mut settings,
            "peer",
            AgentDisplayRoute {
                monitor: selected.fingerprint.clone(),
                input: DisplayInput::new(0x11).unwrap(),
            },
        ));
        assert_eq!(
            settings.peers[0]
                .input_for(&selected.fingerprint)
                .unwrap()
                .value(),
            0x11
        );

        settings.peers[0].set_input_for(&selected.fingerprint, None);
        assert!(!apply_verified_peer_route(
            &mut settings,
            "peer",
            AgentDisplayRoute {
                monitor: monitor("different").fingerprint,
                input: DisplayInput::new(0x12).unwrap(),
            },
        ));
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());

        assert!(!apply_verified_peer_route(
            &mut settings,
            "peer",
            AgentDisplayRoute {
                monitor: selected.fingerprint.clone(),
                input: DisplayInput::new(0x0f).unwrap(),
            },
        ));
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());
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

        assert!(apply_verified_peer_route(
            &mut settings,
            "peer",
            AgentDisplayRoute {
                monitor: monitor_b.fingerprint.clone(),
                input: DisplayInput::new(0x0f).unwrap(),
            },
        ));

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
            "peer"
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
            "peer"
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
    fn removes_a_missing_selection_without_reassigning_to_a_different_monitor() {
        let previous = monitor("disconnected");
        let replacement = monitor("replacement");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&previous)],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(
            &mut settings,
            std::slice::from_ref(&replacement),
            std::slice::from_ref(&replacement),
        );

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::RemovedMissingMonitor {
                name: "disconnected".to_owned(),
                fingerprint: previous.fingerprint,
            }]
        );
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn never_guesses_between_multiple_controllable_monitors() {
        let mut settings = AppSettings::default();
        let monitors = [monitor("first"), monitor("second")];

        assert!(reconcile_monitor_selection(&mut settings, &monitors, &monitors).is_empty());
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn missing_selection_is_removed_and_not_replaced_when_multiple_candidates_remain() {
        let disconnected = monitor("disconnected");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&disconnected)],
            ..AppSettings::default()
        };
        let monitors = [monitor("first"), monitor("second")];

        let changes = reconcile_monitor_selection(&mut settings, &monitors, &monitors);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::RemovedMissingMonitor {
                name: "disconnected".to_owned(),
                fingerprint: disconnected.fingerprint,
            }]
        );
        assert!(settings.shared_monitors.is_empty());
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

        let changes = reconcile_monitor_selection(&mut settings, &monitors, &monitors);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::RemovedMissingMonitor {
                name: "disconnected".to_owned(),
                fingerprint: disconnected.fingerprint,
            }]
        );
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&stays)]
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

        let changes = reconcile_monitor_selection(&mut settings, &monitors, &monitors);

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

        let changes = reconcile_monitor_selection(&mut settings, &monitors, &monitors);

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
            false,
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
        let detected = [selected.clone(), replacement.clone()];

        assert!(reconcile_monitor_selection(
            &mut settings,
            &detected,
            std::slice::from_ref(&replacement),
        )
        .is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&selected)]
        );
    }
}
