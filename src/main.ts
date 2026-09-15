import "@fontsource-variable/manrope";
import {
  Activity, ArrowLeftRight, CircleHelp, Computer, createIcons, Download, KeyRound, Laptop,
  ChevronDown, ExternalLink, Github, Languages, Monitor, MonitorOff, MoonStar, Network, Plus, RefreshCw, Save, Search, Settings,
  ShieldCheck, SunMoon, Trash2, UserRound, Zap,
} from "lucide";
import { getVersion } from "@tauri-apps/api/app";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import packageMetadata from "../package.json";
import { locale, localePreference, setLocalePreference, t, type MessageKey } from "./i18n";
import { initializeTheme, setThemePreference } from "./theme";
import "./styles.css";

type Platform = "windows" | "mac";
type ResolutionSource = "edid" | "coreGraphicsDisplayMode" | "windowsDisplayMode";

interface MonitorResolution {
  width: number;
  height: number;
}

interface Fingerprint {
  manufacturer_id: string;
  product_code: string;
  serial_number: string | null;
}

type HostOutput = "hdmi" | "displayPort" | "usbC" | "thunderbolt" | "dvi" | "vga" | "indirect";
type SinkInterface = "hdmi" | "displayPort" | "dvi" | "vga" | "unknownDigital";
type DdcRisk = "low" | "elevated" | "unsupported";

interface MonitorConnection {
  hostOutput: HostOutput | null;
  hostPort?: string | null;
  sinkInterface: SinkInterface | null;
  sharesUsbData: boolean;
  signalConversion: boolean;
  ddcRisk: DdcRisk | null;
}

interface MonitorDescriptor {
  id: string;
  name: string;
  active: boolean;
  builtIn: boolean;
  fingerprint: Fingerprint;
  maxResolution?: MonitorResolution | null;
  resolutionSource?: ResolutionSource | null;
  connection?: MonitorConnection | null;
}

interface SelectedMonitor {
  name: string;
  fingerprint: Fingerprint;
  maxResolution?: MonitorResolution | null;
  resolutionSource?: ResolutionSource | null;
  localInput: number | null;
  supportedInputs: number[] | null;
  activeRoute?: string | null;
}

interface MonitorInputAssignment {
  monitor: Fingerprint;
  input: number;
}

interface HostRoute {
  id: string;
  name: string;
  platform: Platform;
  address: string;
  port: number;
  macAddress: string;
  inputs: MonitorInputAssignment[];
}

interface AppSettings {
  localHost: Platform;
  sharedMonitors: SelectedMonitor[];
  peers: HostRoute[];
  broadcastIp: string;
  wakePort: number;
  sharedKey: string;
  waitSeconds: number;
  autostart: boolean;
  checkUpdates: boolean;
  onboardingCompleted: boolean;
  hostSwitcherEnabled: boolean;
  hostSwitcherShortcut: string;
}

interface SharedMonitorStatus {
  monitorKey: string;
  fingerprint: Fingerprint;
  name: string;
  ddcAvailable: boolean;
  statusText: string;
  connection: MonitorConnection | null;
  connectionInputConflict: boolean;
}

interface DashboardState {
  platform: string;
  localHost: Platform;
  agentConfigured: boolean;
  monitors: MonitorDescriptor[];
  uncontrollableMonitors: MonitorDescriptor[];
  shared: SharedMonitorStatus[];
  selectionNotices: string[];
}

interface DiscoveredPeer {
  id: string;
  name: string;
  platform: Platform;
  address: string;
  port: number;
  macAddress: string | null;
}

interface InputOption { value: number; name: string; }
interface OperationResult { title: string; detail: string; peerWoken: boolean; warning: boolean; }
interface ShortcutCheckResult { available: boolean; message: string; }
interface UpdateInfo { available: boolean; currentVersion: string; version: string | null; notes: string | null; }
interface ReleaseHistoryItem { date: string; version: string; url: string; }
interface GitHubRelease {
  tag_name?: unknown;
  published_at?: unknown;
  draft?: unknown;
  prerelease?: unknown;
}
type SwitchProgressEvent =
  | { event: "waking"; peerName: string }
  | { event: "checking"; peerName: string }
  | { event: "waiting"; peerName: string; seconds: number }
  | { event: "switching" }
  | { event: "remoteFallback"; peerName: string };
type UpdateDownloadEvent =
  | { event: "started"; contentLength: number | null }
  | { event: "progress"; downloaded: number; contentLength: number | null }
  | { event: "finished" };

const standardInputs: InputOption[] = [
  [0x01, "VGA"], [0x03, "DVI"], [0x0f, "DP"],
  [0x11, "HDMI 1"], [0x12, "HDMI 2"], [0x1b, "Type-C"],
].map(([value, name]) => ({ value: value as number, name: name as string }));

const previewSettings: AppSettings = {
  localHost: "windows", sharedMonitors: [], peers: [],
  broadcastIp: "255.255.255.255", wakePort: 9, sharedKey: "", waitSeconds: 45, autostart: true, checkUpdates: true,
  onboardingCompleted: false, hostSwitcherEnabled: false, hostSwitcherShortcut: "CommandOrControl+Alt+Space",
};
const previewDashboard: DashboardState = {
  platform: "windows", localHost: "windows", agentConfigured: false,
  monitors: [], uncontrollableMonitors: [], shared: [], selectionNotices: [],
};

let settings = previewSettings;
let dashboard = previewDashboard;
let discoveredPeers: DiscoveredPeer[] = [];
let inputOptionsByMonitor: Record<string, InputOption[]> = {};
let activeMonitorKey: string | null = null;
let isPreview = false;
let pendingUpdate: UpdateInfo | null = null;
let isRecordingShortcut = false;
let shortcutStatus: { kind: "checking" | "available" | "conflict"; text: string } | null = null;

const releaseHistoryFallback = [
  { date: "2026-09-15", version: "v0.1.7", url: "https://github.com/OmarHung/DisplayMuxAuto/releases/tag/v0.1.7" },
  { date: "2026-09-14", version: "v0.1.6", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.6" },
  { date: "2026-09-14", version: "v0.1.5", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.5" },
  { date: "2026-09-14", version: "v0.1.4", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.4" },
  { date: "2026-09-13", version: "v0.1.3", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.3" },
  { date: "2026-09-12", version: "v0.1.2", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.2" },
  { date: "2026-09-11", version: "v0.1.1", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.1" },
  { date: "2026-09-11", version: "v0.1.0", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.0" },
] satisfies ReleaseHistoryItem[];

const releaseUrl = (version: string) => `https://github.com/OmarHung/DisplayMuxAuto/releases/tag/${version}`;

const onboardingSteps = [
  {
    label: t("onboarding.stepWelcome"),
    title: t("onboarding.dashboardTitle"),
    body: t("onboarding.dashboardBody"),
    page: "dashboard",
    target: ".showcase-monitor-card",
    placement: "right",
  },
  {
    label: t("onboarding.stepSettings"),
    title: t("onboarding.settingsTitle"),
    body: t("onboarding.settingsBody"),
    page: "dashboard",
    target: '[data-page="settings"]',
    placement: "right",
  },
  {
    label: t("onboarding.stepDisplay"),
    title: t("onboarding.displayTitle"),
    body: t("onboarding.displayBody"),
    page: "settings",
    target: ".form-section.first",
    placement: "bottom",
  },
  {
    label: t("onboarding.stepPairing"),
    title: t("onboarding.pairingTitle"),
    body: t("onboarding.pairingBody"),
    page: "settings",
    target: ".pairing-section",
    placement: "top",
  },
  {
    label: t("onboarding.stepFinish"),
    title: t("onboarding.finishTitle"),
    body: t("onboarding.finishBody"),
    page: "settings",
    target: ".form-actions",
    placement: "top",
  },
] as const;

let onboardingStep = 0;

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) throw new Error(t("app.rootMissing"));
document.documentElement.lang = locale;
const themePreference = initializeTheme();

app.innerHTML = `
  <div class="app-shell">
    <aside class="sidebar" aria-label="${t("nav.aria")}">
      <div class="brand-block">
        <div class="brand-header">
          <div class="brand-icon"><i data-lucide="monitor"></i></div>
          <span class="brand-title">DisplayMux</span>
        </div>
        <label class="language-picker">
          <i data-lucide="languages"></i>
          <span class="sr-only">${t("language.label")}</span>
          <select id="language-select" aria-label="${t("language.label")}">
            <option value="system">${t("language.system")}</option>
            <option value="en">${t("language.english")}</option>
            <option value="zh-TW">${t("language.traditionalChinese")}</option>
          </select>
          <i class="language-chevron" data-lucide="chevron-down"></i>
        </label>
        <label class="language-picker">
          <i data-lucide="sun-moon"></i>
          <span class="sr-only">${t("theme.label")}</span>
          <select id="theme-select" aria-label="${t("theme.label")}">
            <option value="system">${t("theme.system")}</option>
            <option value="light">${t("theme.light")}</option>
            <option value="dark">${t("theme.dark")}</option>
          </select>
          <i class="language-chevron" data-lucide="chevron-down"></i>
        </label>
      </div>
      <nav class="sidebar-nav">
        <button class="nav-button is-active" data-page="dashboard"><i data-lucide="arrow-left-right"></i><span>${t("nav.dashboard")}</span></button>
        <button class="nav-button" data-page="settings"><i data-lucide="settings"></i><span>${t("nav.settings")}</span></button>
      </nav>
      <button class="nav-button nav-bottom" data-page="help"><i data-lucide="circle-help"></i><span>${t("nav.help")}</span></button>
    </aside>
    <main class="workspace">
      <header class="topbar">
        <h1 id="page-title">${t("page.dashboard")}</h1>
        <div class="topbar-actions">
          <div class="agent-pill" id="agent-pill"><span class="status-dot"></span><span>${t("dashboard.agentMissing")}</span></div>
          <button class="icon-button" id="update-button" title="${t("action.checkUpdates")}"><i data-lucide="download"></i></button>
          <button class="icon-button" id="refresh-button" title="${t("action.refresh")}"><i data-lucide="refresh-cw"></i></button>
        </div>
      </header>

      <section class="page is-active" id="dashboard-page">
        <div class="monitor-strip" id="monitor-strip"></div>
        <div class="switch-panel" id="switch-panel"></div>

        <section class="status-summary-bar">
          <div class="summary-item"><span>${t("dashboard.sharedLabel")}</span><strong id="monitor-health" class="text-accent">${t("dashboard.detecting")}</strong></div>
          <span class="summary-pipe"></span>
          <div class="summary-item"><span>${t("dashboard.hostsLabel")}</span><strong id="peer-health">${t("dashboard.hostCount", { count: 0 })}</strong></div>
          <span class="summary-pipe"></span>
          <div class="summary-item"><span>${t("dashboard.wakeLabel")}</span><strong id="wake-health" class="text-accent">${t("dashboard.noHosts")}</strong></div>
        </section>
      </section>

      <section class="page" id="settings-page">
        <div class="settings-layout">
          <section class="settings-main">
            <form id="settings-form">
              <div class="form-section first">
                <div class="pairing-heading">
                  <strong>${t("settings.stepMonitor")}</strong>
                </div>
                <div class="monitor-picker" id="monitor-picker"></div>
              </div>

              <div class="form-section two-columns">
                <label class="field"><span>${t("settings.localComputer")}</span><input id="local-host-name" disabled /></label>
              </div>

              <div class="form-section">
                <div class="pairing-heading">
                  <strong>${t("settings.localInput")}</strong>
                </div>
                <div class="local-input-summary" id="local-input-summary"></div>
              </div>

              <div class="form-section pairing-section">
                <div class="pairing-heading">
                  <strong>${t("settings.stepHosts")}</strong>
                  <button class="scan-button" id="scan-button" type="button"><i data-lucide="search"></i>${t("action.searchAgain")}</button>
                </div>
                <div class="peer-list" id="peer-list"></div>
                <div class="paired-routes" id="paired-routes"></div>
              </div>

              <div class="form-section two-columns">
                <label class="field"><span>${t("settings.password")}</span><div class="input-wrap"><i data-lucide="key-round"></i><input id="shared-key" type="password" minlength="8" placeholder="${t("settings.passwordPlaceholder")}" /></div><small>${t("settings.passwordHint")}</small></label>
                <label class="field compact"><span>${t("settings.wait")}</span><input id="wait-seconds" type="number" min="5" max="120" /><small>${t("settings.waitHint")}</small></label>
              </div>

              <div class="form-section shortcut-section">
                <label class="switch-row shortcut-toggle-row">
                  <span class="switch-label">
                    <strong>${t("settings.hostSwitcher")}</strong>
                    <small>${t("settings.hostSwitcherHint")}</small>
                  </span>
                  <input id="host-switcher-enabled" type="checkbox" class="toggle-checkbox" />
                  <span class="switch-slider"></span>
                </label>
                <div class="shortcut-editor">
                  <div>
                    <strong>${t("settings.shortcut")}</strong>
                    <small>${t("settings.shortcutHint")}</small>
                  </div>
                  <button id="shortcut-recorder" class="shortcut-recorder" type="button">
                    <span id="shortcut-value"></span>
                    <em>${t("settings.recordShortcut")}</em>
                  </button>
                </div>
                <small id="shortcut-status" class="shortcut-status" aria-live="polite"></small>
              </div>

              <div class="toggles-section">
                <label class="switch-row">
                  <span class="switch-label">
                    <strong>${t("settings.autostart")}</strong>
                    <small>${t("settings.autostartHint")}</small>
                  </span>
                  <input id="autostart" type="checkbox" class="toggle-checkbox" />
                  <span class="switch-slider"></span>
                </label>
                <label class="switch-row">
                  <span class="switch-label">
                    <strong>${t("settings.autoUpdates")}</strong>
                    <small>${t("settings.autoUpdatesHint")}</small>
                  </span>
                  <input id="check-updates" type="checkbox" class="toggle-checkbox" />
                  <span class="switch-slider"></span>
                </label>
              </div>

              <div class="form-actions">
                <button class="save-button full-width" type="submit"><i data-lucide="save"></i>${t("action.save")}</button>
              </div>
            </form>
          </section>

          <aside class="compatibility-panel">
            <h3>${t("settings.inputGuide")}</h3>
            <div class="path-item"><span class="path-badge">01</span><div><strong>${t("settings.mccsTitle")}</strong><p>${t("settings.mccsBody")}</p></div></div>
            <div class="path-item"><span class="path-badge">02</span><div><strong>${t("settings.ddcTitle")}</strong><p>${t("settings.ddcBody")}</p></div></div>
            <div class="path-item"><span class="path-badge">03</span><div><strong>${t("settings.routingTitle")}</strong><p>${t("settings.routingBody")}</p></div></div>
            <div class="compat-note"><i data-lucide="shield-check"></i><p>${t("settings.safetyBody")}</p></div>
          </aside>
        </div>
      </section>

      <section class="page" id="help-page">
        <div class="help-content">
          <p class="section-kicker">OPERATING NOTES</p><h2>${t("help.heading")}</h2>
          <div class="note-list">
            <article><span>01</span><div><h3>${t("help.replaceTitle")}</h3><p>${t("help.replaceBody")}</p></div></article>
            <article><span>02</span><div><h3>${t("help.inputTitle")}</h3><p>${t("help.inputBody")}</p></div></article>
            <article><span>03</span><div><h3>${t("help.autoTitle")}</h3><p>${t("help.autoBody")}</p></div></article>
            <article><span>04</span><div><h3>${t("help.adapterTitle")}</h3><p>${t("help.adapterBody")}</p></div></article>
          </div>

          <section class="release-history" aria-labelledby="release-history-title">
            <p class="section-kicker">RELEASE HISTORY</p><h2 id="release-history-title">${t("help.releaseHistoryTitle")}</h2>
            <div class="release-table-wrap">
              <table class="release-table">
                <thead><tr><th scope="col">${t("help.releaseDate")}</th><th scope="col">${t("help.releaseVersion")}</th><th scope="col">${t("help.releaseLink")}</th></tr></thead>
                <tbody id="release-history-body">
                  ${releaseHistoryRows(releaseHistoryFallback)}
                </tbody>
              </table>
            </div>
          </section>

          <section class="onboarding-replay" aria-labelledby="onboarding-replay-title">
            <div>
              <p class="section-kicker">GETTING STARTED</p>
              <h2 id="onboarding-replay-title">${t("help.onboardingTitle")}</h2>
              <p>${t("help.onboardingBody")}</p>
            </div>
            <button class="scan-button" id="onboarding-restart" type="button">${t("help.onboardingAction")}</button>
          </section>

          <section class="about-section" aria-labelledby="about-title">
            <p class="section-kicker">ABOUT</p><h2 id="about-title">${t("about.title")}</h2>
            <dl class="about-grid">
              <div class="about-item">
                <dt><i data-lucide="user-round"></i>${t("about.developer")}</dt>
                <dd>Henry Hsu</dd>
              </div>
              <div class="about-item">
                <dt><i data-lucide="github"></i>GitHub</dt>
                <dd><a href="https://github.com/OmarHung/DisplayMuxAuto" data-external-url>OmarHung/DisplayMuxAuto<i data-lucide="external-link"></i></a></dd>
              </div>
              <div class="about-item">
                <dt><i data-lucide="activity"></i>${t("about.version")}</dt>
                <dd id="app-version" aria-live="polite">${t("about.loading")}</dd>
              </div>
            </dl>
          </section>
        </div>
      </section>
    </main>
  </div>
  <div class="operation-overlay" id="operation-overlay" aria-live="polite" aria-hidden="true"><div class="operation-dialog"><div class="spinner"></div><p class="section-kicker">SMART SWITCH</p><h2 id="operation-title">${t("operation.running")}</h2><p id="operation-detail">${t("operation.preparingBody")}</p></div></div>
  <div class="update-overlay" id="update-overlay" aria-hidden="true">
    <div class="update-dialog" role="dialog" aria-modal="true" aria-labelledby="update-title" aria-describedby="update-version">
      <p class="section-kicker">SIGNED UPDATE</p>
      <h2 id="update-title">${t("update.available")}</h2>
      <p id="update-version"></p>
      <div class="update-notes" id="update-notes"></div>
      <div class="update-progress" id="update-progress" hidden><div id="update-progress-bar"></div></div>
      <p class="update-progress-label" id="update-progress-label"></p>
      <div class="update-actions"><button class="scan-button" id="update-cancel" type="button">${t("action.later")}</button><button class="save-button" id="update-install" type="button"><i data-lucide="download"></i>${t("action.downloadInstall")}</button></div>
    </div>
  </div>
  <div class="onboarding-overlay" id="onboarding-overlay" aria-hidden="true"></div>
  <section class="onboarding-tooltip" id="onboarding-tooltip" role="dialog" aria-modal="false" aria-labelledby="onboarding-title" aria-describedby="onboarding-body" aria-hidden="true">
    <div class="onboarding-tooltip-header">
      <div><p class="section-kicker">PRODUCT TOUR</p><p class="onboarding-counter" id="onboarding-counter"></p></div>
      <button class="onboarding-skip" id="onboarding-skip" type="button">${t("onboarding.skip")}</button>
    </div>
    <span class="onboarding-step-label" id="onboarding-step-label"></span>
    <h2 id="onboarding-title"></h2>
    <p class="onboarding-body" id="onboarding-body"></p>
    <div class="onboarding-status" id="onboarding-status" hidden><strong id="onboarding-status-title"></strong><span id="onboarding-status-detail"></span></div>
    <div class="onboarding-actions">
      <button class="scan-button" id="onboarding-previous" type="button">${t("onboarding.previous")}</button>
      <button class="save-button" id="onboarding-next" type="button"></button>
    </div>
  </section>
  <div class="toast" id="toast" role="status" aria-live="polite"><i data-lucide="zap"></i><div><strong id="toast-title"></strong><span id="toast-detail"></span></div></div>
`;

const iconSet = { Activity, ArrowLeftRight, ChevronDown, CircleHelp, Computer, Download, ExternalLink, Github, KeyRound, Languages, Laptop, Monitor, MonitorOff, MoonStar, Network, Plus, RefreshCw, Save, Search, Settings, ShieldCheck, SunMoon, Trash2, UserRound, Zap };
const refreshIcons = () => createIcons({ icons: iconSet });
refreshIcons();

const pageTitles: Record<string, string> = { dashboard: t("page.dashboard"), settings: t("page.settings"), help: t("page.help") };
document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach((button) => button.addEventListener("click", () => showPage(button.dataset.page ?? "dashboard")));
document.querySelector<HTMLButtonElement>("#refresh-button")?.addEventListener("click", () => void refresh());
document.querySelector<HTMLButtonElement>("#update-button")?.addEventListener("click", () => pendingUpdate ? showUpdateDialog(pendingUpdate) : void checkForUpdates(true));
document.querySelector<HTMLButtonElement>("#update-cancel")?.addEventListener("click", hideUpdateDialog);
document.querySelector<HTMLButtonElement>("#update-install")?.addEventListener("click", () => void installUpdate());
document.querySelector<HTMLButtonElement>("#onboarding-restart")?.addEventListener("click", () => showOnboarding(0));
document.querySelector<HTMLButtonElement>("#onboarding-previous")?.addEventListener("click", () => {
  if (onboardingStep > 0) showOnboarding(onboardingStep - 1);
});
document.querySelector<HTMLButtonElement>("#onboarding-next")?.addEventListener("click", () => {
  if (onboardingStep < onboardingSteps.length - 1) showOnboarding(onboardingStep + 1);
  else void completeOnboarding(true);
});
document.querySelector<HTMLButtonElement>("#onboarding-skip")?.addEventListener("click", () => void completeOnboarding(false));
document.querySelector<HTMLButtonElement>('[data-page="settings"]')?.addEventListener("click", () => {
  if (document.querySelector("#onboarding-overlay")?.classList.contains("is-visible") && onboardingStep === 1) {
    showOnboarding(2);
  }
});
window.addEventListener("resize", () => positionOnboardingTooltip());
document.querySelector(".workspace")?.addEventListener("scroll", () => positionOnboardingTooltip());
document.querySelector<HTMLButtonElement>("#scan-button")?.addEventListener("click", () => void scanPeers());
document.addEventListener("click", (event) => {
  const link = (event.target as HTMLElement).closest<HTMLAnchorElement>("a[data-external-url]");
  if (!link) return;
  event.preventDefault();
  void openExternalUrl(link.href);
});
const languageSelect = document.querySelector<HTMLSelectElement>("#language-select");
if (languageSelect) {
  languageSelect.value = localePreference;
  languageSelect.addEventListener("change", () => {
    if (setLocalePreference(languageSelect.value)) window.location.reload();
  });
}

const themeSelect = document.querySelector<HTMLSelectElement>("#theme-select");
if (themeSelect) {
  themeSelect.value = themePreference;
  themeSelect.addEventListener("change", () => {
    if (!setThemePreference(themeSelect.value)) themeSelect.value = themePreference;
  });
}
document.querySelector<HTMLFormElement>("#settings-form")?.addEventListener("submit", (event) => void saveSettings(event));
document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.addEventListener("change", () => {
  renderShortcutSetting();
  if (document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked) {
    void checkShortcutConflict(settings.hostSwitcherShortcut);
  }
});
document.querySelector<HTMLButtonElement>("#shortcut-recorder")?.addEventListener("click", beginShortcutRecording);
document.addEventListener("keydown", captureShortcut, true);
document.querySelector("#monitor-picker")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-monitor-id]");
  if (!button?.dataset.monitorId) return;
  if (button.dataset.monitorSelected === "true") void removeSharedMonitor(button.dataset.monitorId);
  else void addSharedMonitor(button.dataset.monitorId);
});
document.querySelector("#peer-list")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-add-peer]");
  if (button?.dataset.addPeer) void addPeer(button.dataset.addPeer);
});
document.querySelector("#paired-routes")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-remove-peer], [data-probe-id], [data-wake-id]");
  if (button?.dataset.removePeer) void removePeer(button.dataset.removePeer);
  if (button?.dataset.probeId) void peerCommand("probe_peer", button.dataset.probeId);
  if (button?.dataset.wakeId) void peerCommand("wake_peer", button.dataset.wakeId);
});
document.querySelector("#paired-routes")?.addEventListener("input", renderInputHints);
document.querySelector("#monitor-strip")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-monitor-key]");
  if (!button?.dataset.monitorKey || button.dataset.monitorKey === activeMonitorKey) return;
  activeMonitorKey = button.dataset.monitorKey;
  renderMonitorStrip();
  renderSwitchPanel();
});
document.querySelector("#switch-panel")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-switch-id]");
  if (!button?.dataset.switchId) return;
  const card = button.closest<HTMLElement>("[data-monitor-key]");
  if (card?.dataset.monitorKey) void switchHost(card.dataset.monitorKey, button.dataset.switchId);
});

function showPage(page: string): void {
  document.querySelectorAll(".page").forEach((item) => item.classList.remove("is-active"));
  document.querySelector(`#${page}-page`)?.classList.add("is-active");
  document.querySelectorAll(".nav-button").forEach((item) => item.classList.toggle("is-active", (item as HTMLElement).dataset.page === page));
  setText("#page-title", pageTitles[page] ?? pageTitles.dashboard);
}

function releaseHistoryRows(releases: ReleaseHistoryItem[]): string {
  return releases.map((release) => `
    <tr>
      <td>${escapeHtml(release.date)}</td>
      <td><code>${escapeHtml(release.version)}</code></td>
      <td><a href="${escapeHtml(release.url)}" data-external-url>${t("help.viewRelease")}<i data-lucide="external-link"></i></a></td>
    </tr>
  `).join("");
}

async function refreshReleaseHistory(): Promise<void> {
  try {
    const response = await fetch("https://api.github.com/repos/OmarHung/DisplayMuxAuto/releases?per_page=30", {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!response.ok) throw new Error(`GitHub Releases API returned ${response.status}`);
    const payload: unknown = await response.json();
    if (!Array.isArray(payload)) throw new Error("GitHub Releases API returned an invalid response");

    const seen = new Set<string>();
    const releases = (payload as GitHubRelease[]).flatMap((release): ReleaseHistoryItem[] => {
      const version = typeof release.tag_name === "string" ? release.tag_name : "";
      const publishedAt = typeof release.published_at === "string" ? release.published_at : "";
      if (release.draft === true || release.prerelease === true || !/^v\d+\.\d+\.\d+$/.test(version) || !/^\d{4}-\d{2}-\d{2}T/.test(publishedAt) || seen.has(version)) return [];
      seen.add(version);
      return [{ date: publishedAt.slice(0, 10), version, url: releaseUrl(version) }];
    });
    releases.sort((left, right) => compareReleaseVersions(right.version, left.version));
    if (!releases.length) return;

    const body = document.querySelector<HTMLTableSectionElement>("#release-history-body");
    if (body) {
      body.innerHTML = releaseHistoryRows(releases);
      refreshIcons();
    }
  } catch {
    // Keep the bundled history available when GitHub is unreachable or rate-limited.
  }
}

function compareReleaseVersions(left: string, right: string): number {
  const leftParts = left.slice(1).split(".").map(Number);
  const rightParts = right.slice(1).split(".").map(Number);
  for (let index = 0; index < 3; index += 1) {
    const difference = (leftParts[index] ?? 0) - (rightParts[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

async function openExternalUrl(value: string): Promise<void> {
  try {
    const url = new URL(value);
    const repositoryPaths = ["/OmarHung/DisplayMuxAuto", "/HenryHsu/DisplayMux"];
    const isAllowedPath = repositoryPaths.some((path) => url.pathname === path || url.pathname.startsWith(`${path}/`));
    if (url.protocol !== "https:" || url.hostname !== "github.com" || !isAllowedPath) {
      throw new Error("unsupported external URL");
    }
    if (isPreview) {
      window.open(url.href, "_blank", "noopener,noreferrer");
    } else {
      await openUrl(url.href);
    }
  } catch (error) {
    showToast(t("toast.openLinkFailed"), String(error), true);
  }
}

async function loadInputOptionsByMonitor(monitorKeys: string[]): Promise<Record<string, InputOption[]>> {
  const entries = await Promise.all(monitorKeys.map(async (monitorKey) =>
    [monitorKey, await invoke<InputOption[]>("get_input_options", { monitorId: monitorKey })] as const,
  ));
  return Object.fromEntries(entries);
}

/** Minimum gap between automatic rescans; each one issues DDC/CI reads. */
const FOCUS_REFRESH_INTERVAL_MS = 10_000;
/** Emitted by the backend when a paired host reports a switch. */
const ACTIVE_ROUTE_CHANGED_EVENT = "active-route-changed";
let isRefreshing = false;
let lastRefreshAt = 0;

async function refresh(): Promise<void> {
  isRefreshing = true;
  lastRefreshAt = Date.now();
  document.querySelector("#refresh-button svg")?.classList.add("is-spinning");
  try {
    dashboard = await invoke<DashboardState>("get_dashboard_state");
    [settings, inputOptionsByMonitor] = await Promise.all([
      invoke<AppSettings>("get_settings"),
      loadInputOptionsByMonitor(dashboard.shared.map((shared) => shared.monitorKey)),
    ]);
    try { discoveredPeers = await invoke<DiscoveredPeer[]>("discover_peers"); } catch { discoveredPeers = []; }
    isPreview = false;
  } catch {
    dashboard = previewDashboard; settings = previewSettings; inputOptionsByMonitor = {}; discoveredPeers = []; isPreview = true;
  } finally {
    isRefreshing = false;
    document.querySelector("#refresh-button svg")?.classList.remove("is-spinning");
  }
  renderState();
  if (!isPreview && dashboard.selectionNotices.length) {
    showToast(t("toast.selectionUpdated"), dashboard.selectionNotices.join(" "));
  }
}

/**
 * Rescans when the window comes back into view, so a switch made from the
 * display's own buttons shows up without pressing refresh. Only the dashboard
 * rescans: a full render would discard unsaved edits on the settings page.
 */
function refreshOnReturn(): void {
  if (isPreview || isRefreshing || document.visibilityState !== "visible") return;
  if (!document.querySelector("#dashboard-page")?.classList.contains("is-active")) return;
  if (Date.now() - lastRefreshAt < FOCUS_REFRESH_INTERVAL_MS) return;
  void refresh();
}

/** Re-reads only which host is active; no DDC scan and no settings form re-render. */
async function reloadActiveRoutes(): Promise<void> {
  try {
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, sharedMonitors: latest.sharedMonitors };
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.activeHostSyncFailed"), String(error), true);
  }
}

function getFlatMonitorSvg(isUltrawide: boolean): string {
  if (isUltrawide) {
    return `<svg class="flat-monitor-svg" viewBox="0 0 380 190" fill="none" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <linearGradient id="screen21" x1="190" y1="18" x2="190" y2="144" gradientUnits="userSpaceOnUse">
          <stop offset="0%" stop-color="#142c22"/>
          <stop offset="100%" stop-color="#0b1713"/>
        </linearGradient>
        <linearGradient id="glare21" x1="360" y1="20" x2="160" y2="140" gradientUnits="userSpaceOnUse">
          <stop offset="0%" stop-color="#ffffff" stop-opacity="0.16"/>
          <stop offset="45%" stop-color="#ffffff" stop-opacity="0.03"/>
          <stop offset="100%" stop-color="#ffffff" stop-opacity="0"/>
        </linearGradient>
        <linearGradient id="standNeck" x1="182" y1="144" x2="198" y2="144" gradientUnits="userSpaceOnUse">
          <stop offset="0%" stop-color="#2a3a32"/>
          <stop offset="50%" stop-color="#42574c"/>
          <stop offset="100%" stop-color="#1e2a24"/>
        </linearGradient>
        <linearGradient id="standBase" x1="190" y1="172" x2="190" y2="180" gradientUnits="userSpaceOnUse">
          <stop offset="0%" stop-color="#3c5045"/>
          <stop offset="100%" stop-color="#1a2520"/>
        </linearGradient>
      </defs>
      <rect x="183" y="142" width="14" height="32" rx="2" fill="url(#standNeck)"/>
      <rect x="177" y="132" width="26" height="18" rx="3" fill="#1b2520"/>
      <rect x="125" y="172" width="130" height="7" rx="3.5" fill="url(#standBase)"/>
      <rect x="126" y="172" width="128" height="1.5" rx="0.75" fill="#587363" opacity="0.6"/>
      <rect x="16" y="16" width="348" height="130" rx="6" fill="#15211b" stroke="#2c3f34" stroke-width="2"/>
      <rect x="20" y="20" width="340" height="122" rx="3" fill="url(#screen21)"/>
      <polygon points="20,20 220,20 120,142 20,142" fill="url(#glare21)"/>
      <circle cx="190" cy="142" r="1.5" fill="#4ade80" opacity="0.8"/>
    </svg>`;
  }
  return `<svg class="flat-monitor-svg" viewBox="0 0 380 190" fill="none" xmlns="http://www.w3.org/2000/svg">
    <defs>
      <linearGradient id="screen16" x1="190" y1="14" x2="190" y2="152" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#142c22"/>
        <stop offset="100%" stop-color="#0b1713"/>
      </linearGradient>
      <linearGradient id="glare16" x1="320" y1="16" x2="160" y2="150" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#ffffff" stop-opacity="0.16"/>
        <stop offset="45%" stop-color="#ffffff" stop-opacity="0.03"/>
        <stop offset="100%" stop-color="#ffffff" stop-opacity="0"/>
      </linearGradient>
      <linearGradient id="standNeck" x1="182" y1="144" x2="198" y2="144" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#2a3a32"/>
        <stop offset="50%" stop-color="#42574c"/>
        <stop offset="100%" stop-color="#1e2a24"/>
      </linearGradient>
      <linearGradient id="standBase" x1="190" y1="172" x2="190" y2="180" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#3c5045"/>
        <stop offset="100%" stop-color="#1a2520"/>
      </linearGradient>
    </defs>
    <rect x="183" y="148" width="14" height="26" rx="2" fill="url(#standNeck)"/>
    <rect x="177" y="138" width="26" height="18" rx="3" fill="#1b2520"/>
    <rect x="135" y="172" width="110" height="7" rx="3.5" fill="url(#standBase)"/>
    <rect x="136" y="172" width="108" height="1.5" rx="0.75" fill="#587363" opacity="0.6"/>
    <rect x="65" y="12" width="250" height="144" rx="6" fill="#15211b" stroke="#2c3f34" stroke-width="2"/>
    <rect x="69" y="16" width="242" height="136" rx="3" fill="url(#screen16)"/>
    <polygon points="69,16 220,16 140,152 69,152" fill="url(#glare16)"/>
    <circle cx="190" cy="152.5" r="1.5" fill="#4ade80" opacity="0.8"/>
  </svg>`;
}

function selectedMonitorFor(shared: SharedMonitorStatus): SelectedMonitor | undefined {
  return settings.sharedMonitors.find((sm) => sameFingerprint(sm.fingerprint, shared.fingerprint));
}

function resolutionFor(shared: SharedMonitorStatus): { resolution: MonitorResolution | null; source: ResolutionSource | null } {
  const selectedMonitor = selectedMonitorFor(shared);
  let resolution = selectedMonitor?.maxResolution ?? null;
  let source = selectedMonitor?.resolutionSource ?? null;
  if (!resolution) {
    const match = dashboard.monitors.find((m) => sameFingerprint(m.fingerprint, shared.fingerprint));
    if (match?.maxResolution) {
      resolution = match.maxResolution;
      source = match.resolutionSource ?? null;
    }
  }
  return { resolution, source };
}

function ratioText(resolution: MonitorResolution | null, source: ResolutionSource | null, isUltrawide: boolean): string {
  const ratio = isUltrawide ? "21:9" : "16:9";
  return resolution ? `${ratio} · ${resolution.width}×${resolution.height} · ${resolutionSourceName(source)}` : ratio;
}

function renderMonitorStrip(): void {
  const container = document.querySelector("#monitor-strip");
  if (!container) return;
  if (!dashboard.shared.length) {
    container.innerHTML = "";
    return;
  }
  container.innerHTML = dashboard.shared.map((shared) => {
    const { resolution, source } = resolutionFor(shared);
    const isUltrawide = Boolean(resolution && isUltrawideResolution(resolution));
    const isActive = shared.monitorKey === activeMonitorKey;
    return `<button type="button" class="monitor-strip-card ${isActive ? "is-active" : ""}" data-monitor-key="${escapeHtml(shared.monitorKey)}">
      <span class="monitor-strip-name">${escapeHtml(shared.name)}</span>
      <span class="monitor-strip-meta">${escapeHtml(ratioText(resolution, source, isUltrawide))}</span>
      <span class="status-badge ${shared.ddcAvailable ? "" : "subtle"}">${shared.ddcAvailable ? t("dashboard.ddcReady") : t("dashboard.notReady")}</span>
    </button>`;
  }).join("");
}

function renderSwitchPanel(): void {
  const container = document.querySelector("#switch-panel");
  if (!container) return;
  const shared = dashboard.shared.find((item) => item.monitorKey === activeMonitorKey);
  if (!shared) {
    const isUltrawide = Boolean(dashboard.monitors[0]?.maxResolution && isUltrawideResolution(dashboard.monitors[0].maxResolution));
    container.innerHTML = `
      <div class="showcase-monitor-card">
        <div class="showcase-header">
          <span class="showcase-title">${t("dashboard.sharedDisplay")}</span>
          <div class="showcase-badges"><span class="status-badge subtle">${ratioText(dashboard.monitors[0]?.maxResolution ?? null, dashboard.monitors[0]?.resolutionSource ?? null, isUltrawide)}</span></div>
        </div>
        <div class="flat-monitor-wrap">${getFlatMonitorSvg(isUltrawide)}</div>
        <div class="showcase-info">
          <strong class="showcase-monitor-name">${t("dashboard.notSelected")}</strong>
          <p class="showcase-monitor-desc">${isPreview ? t("preview.monitorStatus") : t("dashboard.identityHint")}</p>
        </div>
      </div>`;
    return;
  }
  const { resolution, source } = resolutionFor(shared);
  const isUltrawide = Boolean(resolution && isUltrawideResolution(resolution));
  container.innerHTML = `
    <div class="showcase-monitor-card" data-monitor-key="${escapeHtml(shared.monitorKey)}">
      <div class="showcase-header">
        <span class="showcase-title">${escapeHtml(shared.name)}</span>
        <div class="showcase-badges">
          <span class="status-badge subtle">${ratioText(resolution, source, isUltrawide)}</span>
          <span class="status-badge">${shared.ddcAvailable ? t("dashboard.ddcReady") : t("dashboard.notReady")}</span>
        </div>
      </div>
      <div class="flat-monitor-wrap">${getFlatMonitorSvg(isUltrawide)}</div>
      <div class="showcase-info">
        <strong class="showcase-monitor-name">${escapeHtml(shared.name)}</strong>
        <p class="showcase-monitor-desc">${escapeHtml(shared.statusText)}</p>
      </div>
      <div class="host-route-grid" data-host-route-grid="${escapeHtml(shared.monitorKey)}"></div>
    </div>`;
  renderHostRoutes(shared);
}

function renderState(): void {
  const ddcAvailable = dashboard.shared.some((shared) => shared.ddcAvailable);
  setText("#monitor-health", dashboard.shared.length === 0 ? t("dashboard.notSelected") : ddcAvailable ? t("dashboard.locked") : t("dashboard.notReady"));
  setText("#peer-health", t("dashboard.hostCount", { count: settings.peers.length }));
  setText("#wake-health", settings.peers.some((peer) => peer.macAddress) ? t("dashboard.wakeNormal") : (settings.peers.length ? t("dashboard.noMac") : t("dashboard.noHosts")));
  const pill = document.querySelector("#agent-pill");
  pill?.classList.toggle("is-ready", dashboard.agentConfigured);
  if (pill) pill.querySelector("span:last-child")!.textContent = isPreview ? t("dashboard.preview") : dashboard.agentConfigured ? t("dashboard.agentReady") : t("dashboard.agentMissing");
  setInput("#local-host-name", dashboard.localHost === "windows" ? t("dashboard.localWindowsPc") : t("dashboard.localMac"));
  setInput("#shared-key", settings.sharedKey);
  setInput("#wait-seconds", String(settings.waitSeconds));
  const autostart = document.querySelector<HTMLInputElement>("#autostart");
  if (autostart) autostart.checked = settings.autostart;
  const checkUpdates = document.querySelector<HTMLInputElement>("#check-updates");
  if (checkUpdates) checkUpdates.checked = settings.checkUpdates;
  const hostSwitcherEnabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled");
  if (hostSwitcherEnabled) hostSwitcherEnabled.checked = settings.hostSwitcherEnabled;
  renderShortcutSetting();
  if (!activeMonitorKey || !dashboard.shared.some((shared) => shared.monitorKey === activeMonitorKey)) {
    activeMonitorKey = dashboard.shared[0]?.monitorKey ?? null;
  }
  renderMonitorStrip(); renderSwitchPanel();
  renderMonitors(); renderPeerList(); renderPairedRoutes(); renderLocalInputSummary(); renderInputHints(); refreshIcons();
}

function shortcutDisplay(value: string): string {
  const isMac = dashboard.localHost === "mac";
  return value.split("+").map((part) => {
    const key = part.toLowerCase();
    if (key === "commandorcontrol") return isMac ? "Command" : "Ctrl";
    if (key === "super") return isMac ? "Command" : "Win";
    if (key === "alt") return isMac ? "Option" : "Alt";
    if (key.startsWith("key")) return part.slice(3).toUpperCase();
    if (key.startsWith("digit")) return part.slice(5);
    return part;
  }).join(" + ");
}

function renderShortcutSetting(): void {
  const enabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked ?? false;
  const button = document.querySelector<HTMLButtonElement>("#shortcut-recorder");
  const value = document.querySelector<HTMLElement>("#shortcut-value");
  const status = document.querySelector<HTMLElement>("#shortcut-status");
  if (button) button.disabled = !enabled;
  if (value) value.textContent = isRecordingShortcut ? t("settings.recordingShortcut") : shortcutDisplay(settings.hostSwitcherShortcut);
  if (status) {
    status.textContent = enabled ? shortcutStatus?.text ?? "" : "";
    status.className = `shortcut-status${shortcutStatus ? ` is-${shortcutStatus.kind}` : ""}`;
  }
}

function beginShortcutRecording(): void {
  if (document.querySelector<HTMLButtonElement>("#shortcut-recorder")?.disabled) return;
  isRecordingShortcut = true;
  shortcutStatus = null;
  renderShortcutSetting();
}

function shortcutFromEvent(event: KeyboardEvent): string | null {
  if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return null;
  const parts: string[] = [];
  const isMac = dashboard.localHost === "mac";
  if (event.ctrlKey) parts.push(isMac ? "Control" : "CommandOrControl");
  if (event.altKey) parts.push("Alt");
  if (event.shiftKey) parts.push("Shift");
  if (event.metaKey) parts.push(isMac ? "CommandOrControl" : "Super");
  if (!parts.some((part) => part !== "Shift")) return null;
  parts.push(event.code);
  return parts.join("+");
}

function captureShortcut(event: KeyboardEvent): void {
  if (!isRecordingShortcut) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  if (event.key === "Escape") {
    isRecordingShortcut = false;
    renderShortcutSetting();
    return;
  }
  const shortcut = shortcutFromEvent(event);
  if (!shortcut) return;
  settings.hostSwitcherShortcut = shortcut;
  isRecordingShortcut = false;
  renderShortcutSetting();
  void checkShortcutConflict(shortcut);
}

async function checkShortcutConflict(shortcut: string): Promise<void> {
  if (isCommonApplicationShortcut(shortcut)) {
    shortcutStatus = { kind: "conflict", text: t("settings.shortcutCommonConflict") };
    renderShortcutSetting();
    return;
  }
  shortcutStatus = { kind: "checking", text: t("settings.shortcutChecking") };
  renderShortcutSetting();
  if (isPreview) {
    shortcutStatus = { kind: "available", text: t("settings.shortcutAvailable") };
    renderShortcutSetting();
    return;
  }
  try {
    const result = await invoke<ShortcutCheckResult>("check_host_switcher_shortcut", { shortcut });
    shortcutStatus = {
      kind: result.available ? "available" : "conflict",
      text: result.message,
    };
  } catch (error) {
    shortcutStatus = { kind: "conflict", text: String(error) };
  }
  renderShortcutSetting();
}

function isCommonApplicationShortcut(shortcut: string): boolean {
  const parts = shortcut.toLowerCase().split("+");
  const key = (parts.pop() ?? "").replace(/^key/, "");
  const modifiers = new Set(parts);
  const primary = modifiers.has("commandorcontrol") ||
    (dashboard.localHost === "mac" ? modifiers.has("super") : modifiers.has("control"));
  if (!primary) return false;
  const additionalModifiers = [...modifiers].filter((modifier) =>
    !["commandorcontrol", dashboard.localHost === "mac" ? "super" : "control"].includes(modifier)
  );
  const primaryOnly = additionalModifiers.length === 0;
  const primaryWithShift = additionalModifiers.length === 1 && additionalModifiers[0] === "shift";
  return (primaryOnly && new Set([
    "a", "c", "f", "h", "l", "m", "n", "o", "p", "q", "r", "s", "t", "v", "w", "x", "y", "z", "tab", "f4",
  ]).has(key)) || (primaryWithShift && new Set(["n", "p", "r", "s", "t", "w"]).has(key));
}

function renderMonitors(): void {
  const container = document.querySelector("#monitor-picker");
  if (!container) return;
  const uncontrollable = dashboard.uncontrollableMonitors ?? [];
  if (!dashboard.monitors.length && !uncontrollable.length) {
    container.innerHTML = `<p class="peer-empty">${t("settings.noMonitors")}</p>`; return;
  }
  container.innerHTML = dashboard.monitors.map((monitor) => {
    const isSelected = settings.sharedMonitors.some((sm) => sameFingerprint(sm.fingerprint, monitor.fingerprint));
    const fp = monitor.fingerprint;
    const res = monitor.maxResolution;
    const resText = res
      ? `(${res.width}×${res.height} ${isUltrawideResolution(res) ? "21:9" : "16:9"} · ${resolutionSourceName(monitor.resolutionSource ?? null)})`
      : "";
    return `<article class="monitor-card-item ${isSelected ? "is-selected" : ""}">
      <div class="monitor-item-left">
        <div class="monitor-item-icon"><i data-lucide="monitor"></i></div>
        <div class="monitor-identity">
          <strong>${escapeHtml(monitor.name)}</strong>
          <span>${escapeHtml(fp.manufacturer_id)} / ${escapeHtml(fp.product_code)} / ${escapeHtml(fp.serial_number ?? t("settings.noSerial"))} ${resText} (${t("settings.ddcControllable")})</span>
          ${renderConnection(monitor.connection ?? null)}
        </div>
      </div>
      <button type="button" class="monitor-select-btn ${isSelected ? "is-selected" : ""}" data-monitor-id="${escapeHtml(monitor.id)}" data-monitor-selected="${isSelected}">
        ${isSelected ? t("action.removeShared") : t("action.selectShared")}
      </button>
    </article>`;
  }).join("") + uncontrollable.map((monitor) => {
    const fp = monitor.fingerprint;
    return `<article class="monitor-card-item is-unreachable">
      <div class="monitor-item-left">
        <div class="monitor-item-icon"><i data-lucide="monitor-off"></i></div>
        <div class="monitor-identity">
          <strong>${escapeHtml(monitor.name)}</strong>
          <span>${escapeHtml(fp.manufacturer_id)} / ${escapeHtml(fp.product_code)} / ${escapeHtml(fp.serial_number ?? t("settings.noSerial"))} (${t("settings.ddcUnreachable")})</span>
          ${renderConnection(monitor.connection ?? null)}
        </div>
      </div>
    </article>`;
  }).join("");
}

const hostOutputKeys = {
  hdmi: "connection.host.hdmi", displayPort: "connection.host.displayPort", usbC: "connection.host.usbC",
  thunderbolt: "connection.host.thunderbolt", dvi: "connection.host.dvi", vga: "connection.host.vga",
  indirect: "connection.host.indirect",
} as const satisfies Record<HostOutput, MessageKey>;

const sinkInterfaceKeys = {
  hdmi: "connection.sink.hdmi", displayPort: "connection.sink.displayPort", dvi: "connection.sink.dvi",
  vga: "connection.sink.vga", unknownDigital: "connection.sink.unknownDigital",
} as const satisfies Record<SinkInterface, MessageKey>;

function renderConnection(connection: MonitorConnection | null): string {
  if (!connection || (!connection.hostOutput && !connection.sinkInterface)) return "";
  const host = connection.hostOutput ? t(hostOutputKeys[connection.hostOutput]) : t("connection.unknown");
  const sink = connection.sinkInterface ? t(sinkInterfaceKeys[connection.sinkInterface]) : t("connection.unknown");
  const traits = [
    connection.signalConversion ? t("connection.conversion") : null,
    connection.sharesUsbData ? t("connection.sharesUsb") : null,
  ].filter((value): value is string => value !== null);
  const risk = connection.ddcRisk === "elevated" ? t("connection.riskElevated")
    : connection.ddcRisk === "unsupported" ? t("connection.riskUnsupported") : null;
  const title = connection.hostPort ? ` title="${escapeHtml(connection.hostPort)}"` : "";
  return `<span class="monitor-connection"${title}>${escapeHtml(t("connection.summary", { host, sink }))}${traits.length ? ` · ${escapeHtml(traits.join(" · "))}` : ""}</span>
    ${risk ? `<span class="monitor-connection-risk">${escapeHtml(risk)}</span>` : ""}`;
}

function renderLocalInputSummary(): void {
  const container = document.querySelector("#local-input-summary");
  if (!container) return;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="peer-empty">${t("settings.noMonitors")}</p>`;
    return;
  }
  container.innerHTML = dashboard.shared.map((shared) => {
    const value = selectedMonitorFor(shared)?.localInput ?? null;
    const conflict = shared.connectionInputConflict ? `<small class="local-input-conflict">${t("settings.inputConflict")}</small>` : "";
    return `<div class="local-input-row"><span>${escapeHtml(shared.name)}</span><strong>${value == null ? t("input.unset") : escapeHtml(inputName(value, shared.monitorKey))}</strong>${conflict}</div>`;
  }).join("");
}

function renderPeerList(): void {
  const list = document.querySelector("#peer-list");
  if (!list) return;
  const available = discoveredPeers.filter((peer) => !settings.peers.some((item) => item.id === peer.id));
  list.innerHTML = available.length ? available.map((peer) => `<article class="peer-row">
    <div class="peer-identity">
      <strong>${escapeHtml(peer.name)}</strong>
      <span>${platformName(peer.platform)} · ${t("settings.networkAuto")}</span>
    </div>
    <button type="button" class="peer-add-btn" data-add-peer="${escapeHtml(peer.id)}"><i data-lucide="plus"></i>${t("action.add")}</button>
  </article>`).join("") : `<p class="peer-empty">${t("settings.noAvailableHosts")}</p>`;
}

function peerInputValue(peerId: string, monitorKey: string): number | null {
  return inputValueFromElement(`[data-route-input="${cssEscape(peerId)}"][data-route-monitor="${cssEscape(monitorKey)}"]`, null);
}

function renderPairedRoutes(): void {
  const container = document.querySelector("#paired-routes");
  if (!container) return;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="peer-empty">${t("settings.noMonitors")}</p>`;
    return;
  }
  const discoveryNote = dashboard.shared.map((shared) => {
    const monitorOptions = inputOptionsByMonitor[shared.monitorKey] ?? standardInputs;
    const selectedMonitor = selectedMonitorFor(shared);
    return `<p class="input-discovery-note">${escapeHtml(shared.name)}: ${selectedMonitor?.supportedInputs?.length ? t("settings.capabilitiesDetected", { count: monitorOptions.length }) : t("settings.capabilitiesFallback")}</p>`;
  }).join("");
  container.innerHTML = discoveryNote + (settings.peers.length ? `<p class="field-title">${t("settings.addedHosts")}</p>` + settings.peers.map((peer) => `<article class="paired-route-card">
    <div class="peer-identity">
      <strong>${escapeHtml(peer.name)}</strong>
      <span>${platformName(peer.platform)} · ${escapeHtml(peer.address)}</span>
      <div class="peer-diagnostic-actions" aria-label="${escapeHtml(t("settings.diagnosticAria", { name: peer.name }))}">
        <button class="text-button" type="button" data-probe-id="${escapeHtml(peer.id)}">${t("action.testConnection")}</button>
        <span class="tool-sep">·</span>
        <button class="text-button" type="button" data-wake-id="${escapeHtml(peer.id)}" ${peer.macAddress.trim() ? "" : "disabled"}>${t("action.sendWake")}</button>
      </div>
    </div>
    <div class="paired-route-right">
      ${dashboard.shared.map((shared) => `<label class="paired-input-wrap">
        <span>${escapeHtml(shared.name)} ${t("settings.inputValue")}</span>
        <select class="paired-input-field" data-route-input="${escapeHtml(peer.id)}" data-route-monitor="${escapeHtml(shared.monitorKey)}">${renderInputOptions(peer.id, shared.monitorKey, peer.inputs.find((assignment) => sameFingerprint(assignment.monitor, shared.fingerprint))?.input ?? null)}</select>
      </label>`).join("")}
      <button class="delete-button" type="button" data-remove-peer="${escapeHtml(peer.id)}" title="${t("action.remove")}"><i data-lucide="trash-2"></i></button>
    </div>
  </article>`).join("") : `<p class="peer-empty">${t("settings.noAddedHosts")}</p>`);
}

function renderInputOptions(routeId: string, monitorKey: string, current: number | null): string {
  const assignedElsewhere = new Set<number>();
  const shared = dashboard.shared.find((item) => item.monitorKey === monitorKey);
  const selectedMonitor = shared ? selectedMonitorFor(shared) : undefined;
  if (routeId !== "local" && selectedMonitor?.localInput != null) assignedElsewhere.add(selectedMonitor.localInput);
  for (const peer of settings.peers) {
    if (peer.id === routeId) continue;
    const peerValue = peerInputValue(peer.id, monitorKey);
    if (peerValue != null) assignedElsewhere.add(peerValue);
  }
  const monitorOptions = inputOptionsByMonitor[monitorKey] ?? standardInputs;
  const options = monitorOptions
    .filter((item) => !assignedElsewhere.has(item.value) || item.value === current)
    .map((item) => `<option value="${item.value}" ${item.value === current ? "selected" : ""}>${escapeHtml(item.name)}</option>`)
    .join("");
  return `<option value="" ${current == null ? "selected" : ""}>${t("settings.selectInput")}</option>${options}`;
}

function inputValueFromElement(selector: string, fallback: number | null): number | null {
  const value = document.querySelector<HTMLInputElement>(selector)?.value;
  if (value == null) return fallback;
  try { return parseInput(value); } catch { return null; }
}

function renderHostRoutes(shared: SharedMonitorStatus): void {
  const container = document.querySelector(`[data-host-route-grid="${cssEscape(shared.monitorKey)}"]`);
  if (!container) return;
  const selectedMonitor = selectedMonitorFor(shared);
  const activeRouteId = selectedMonitor?.activeRoute ?? "local";
  const routes = [
    { id: "local", name: dashboard.localHost === "windows" ? t("dashboard.localWindows") : t("dashboard.localMac"), platform: dashboard.localHost, input: selectedMonitor?.localInput ?? null, local: true },
    ...settings.peers.map((peer) => ({
      id: peer.id, name: peer.name, platform: peer.platform,
      input: peer.inputs.find((assignment) => sameFingerprint(assignment.monitor, shared.fingerprint))?.input ?? null,
      local: false,
    })),
  ];
  container.innerHTML = routes.map((route) => {
    const isActive = route.id === activeRouteId;
    const badgeText = route.local
      ? (route.platform === "mac" ? t("dashboard.localMacOs") : t("dashboard.localWindowsBadge"))
      : (route.platform === "mac" ? t("dashboard.connectedMacOs") : t("dashboard.connectedWindows"));
    const inputDesc = route.input == null
      ? t("dashboard.inputUnset")
      : (route.local ? t("dashboard.currentInput", { input: escapeHtml(inputName(route.input, shared.monitorKey)) }) : t("dashboard.assignedInput", { input: escapeHtml(inputName(route.input, shared.monitorKey)) }));
    const iconName = route.platform === "mac" ? "laptop" : "computer";

    return `
      <article class="host-route-card ${route.local ? "is-local" : ""}">
        <div class="host-card-header">
          <div class="host-icon ${route.platform}">
            <i data-lucide="${iconName}"></i>
          </div>
          <div class="host-copy">
            <span class="host-label ${route.local ? "is-local" : ""}">${badgeText}</span>
            <h2 class="host-title">${escapeHtml(route.name)}</h2>
            <p class="host-input-desc">${inputDesc}</p>
          </div>
        </div>
        ${isActive ? `
          <div class="active-route-state">
            <span>${t("dashboard.currentlyDisplayed")}</span>
            <span class="active-toggle-indicator"></span>
          </div>
        ` : `
          <button class="switch-button primary" data-switch-id="${escapeHtml(route.id)}" ${route.input == null || (!shared.ddcAvailable && !dashboard.agentConfigured) ? "disabled" : ""}>
            <i data-lucide="arrow-left-right"></i>
            <span>${t("action.switchHost")}</span>
          </button>
        `}
      </article>
    `;
  }).join("");
}

function renderInputHints(): void {
  document.querySelectorAll<HTMLSelectElement>("[data-route-input]").forEach((input) => {
    const routeId = input.dataset.routeInput ?? "";
    const monitorKey = input.dataset.routeMonitor ?? "";
    const current = peerInputValue(routeId, monitorKey);
    input.innerHTML = renderInputOptions(routeId, monitorKey, current);
    input.value = current == null ? "" : String(current);
  });
}

async function scanPeers(): Promise<void> {
  const button = document.querySelector<HTMLButtonElement>("#scan-button");
  if (button) { button.disabled = true; button.textContent = t("action.searching"); }
  try {
    discoveredPeers = isPreview ? [] : await invoke<DiscoveredPeer[]>("discover_peers");
    renderPeerList(); refreshIcons();
    if (!discoveredPeers.length) showToast(t("toast.noPeersTitle"), t("toast.noPeersBody"), true);
  } catch (error) { showToast(t("toast.scanFailed"), String(error), true); }
  finally { if (button) { button.disabled = false; button.textContent = t("action.searchAgain"); } }
}

async function addSharedMonitor(monitorId: string): Promise<void> {
  const monitor = dashboard.monitors.find((item) => item.id === monitorId);
  try {
    settings = await invoke<AppSettings>("add_shared_monitor", { monitorId });
    await refresh();
    showToast(t("toast.monitorSelected"), t("toast.monitorSelectedBody", { name: monitor?.name ?? t("dashboard.sharedDisplay") }));
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function removeSharedMonitor(monitorId: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("remove_shared_monitor", { monitorId });
    await refresh();
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function addPeer(peerId: string): Promise<void> {
  try {
    const sharedKey = document.querySelector<HTMLInputElement>("#shared-key")?.value ?? "";
    settings = await invoke<AppSettings>("select_peer", { peerId, sharedKey });
    const added = settings.peers.find((peer) => peer.id === peerId);
    renderState();
    const detectedPorts = (added?.inputs ?? []).map((assignment) => {
      const shared = dashboard.shared.find((item) => sameFingerprint(item.fingerprint, assignment.monitor));
      return shared ? `${shared.name}: ${inputName(assignment.input, shared.monitorKey)}` : String(assignment.input);
    });
    showToast(t("toast.peerAdded"), detectedPorts.length ? t("toast.peerPortDetected", { port: detectedPorts.join(", ") }) : t("toast.peerAddedBody"));
  }
  catch (error) { showToast(t("toast.peerAddFailed"), String(error), true); }
}

async function removePeer(peerId: string): Promise<void> {
  try { settings = await invoke<AppSettings>("remove_peer", { peerId }); renderState(); }
  catch (error) { showToast(t("toast.peerRemoveFailed"), String(error), true); }
}

async function saveSettings(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  try {
    settings = {
      ...settings,
      peers: settings.peers.map((peer) => ({
        ...peer,
        inputs: dashboard.shared.flatMap((shared) => {
          const value = document.querySelector<HTMLSelectElement>(`[data-route-input="${cssEscape(peer.id)}"][data-route-monitor="${cssEscape(shared.monitorKey)}"]`)?.value ?? "";
          const input = parseInput(value);
          return input == null ? [] : [{ monitor: shared.fingerprint, input }];
        }),
      })),
      sharedKey: document.querySelector<HTMLInputElement>("#shared-key")?.value ?? "",
      waitSeconds: Number(document.querySelector<HTMLInputElement>("#wait-seconds")?.value ?? 45),
      autostart: document.querySelector<HTMLInputElement>("#autostart")?.checked ?? true,
      checkUpdates: document.querySelector<HTMLInputElement>("#check-updates")?.checked ?? true,
      hostSwitcherEnabled: document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked ?? false,
      hostSwitcherShortcut: settings.hostSwitcherShortcut,
    };
    const result = await invoke<OperationResult>("save_settings", { settings });
    showToast(result.title, result.detail); await refresh();
  } catch (error) { showToast(t("toast.settingsFailed"), String(error), true); }
}

async function switchHost(monitorKey: string, targetId: string): Promise<void> {
  showOperation(t("operation.preparingTitle"), t("operation.preparingBody"));
  const onEvent = new Channel<SwitchProgressEvent>();
  onEvent.onmessage = (event) => {
    if (event.event === "waking") {
      showOperation(t("operation.wakingTitle", { name: event.peerName }), t("operation.wakingBody"));
    } else if (event.event === "checking") {
      showOperation(t("operation.checkingTitle", { name: event.peerName }), t("operation.checkingBody"));
    } else if (event.event === "waiting") {
      showOperation(t("operation.waitingTitle", { name: event.peerName }), t("operation.waitingBody", { seconds: event.seconds }));
    } else if (event.event === "remoteFallback") {
      showOperation(t("operation.remoteTitle", { name: event.peerName }), t("operation.remoteBody"));
    } else {
      showOperation(t("operation.switchingTitle"), t("operation.switchingBody"));
    }
  };
  try {
    const result = await invoke<OperationResult>("switch_host", { monitorId: monitorKey, targetId, onEvent });
    showToast(result.title, result.detail, result.warning);
    await refresh();
  } catch (error) {
    showToast(t("toast.switchFailed"), String(error), true);
  } finally {
    hideOperation();
  }
}

async function peerCommand(command: "probe_peer" | "wake_peer", peerId: string): Promise<void> {
  try { const result = await invoke<OperationResult>(command, { peerId }); showToast(result.title, result.detail); }
  catch (error) { showToast(command === "probe_peer" ? t("toast.probeFailed") : t("toast.wakeFailed"), String(error), true); }
}

async function checkForUpdates(manual: boolean): Promise<void> {
  if (isPreview) {
    if (manual) showToast(t("toast.updateUnavailable"), t("toast.updateUnavailableBody"), true);
    return;
  }
  const button = document.querySelector<HTMLButtonElement>("#update-button");
  button?.classList.add("is-checking");
  try {
    const update = await invoke<UpdateInfo>("check_for_update");
    pendingUpdate = update.available ? update : null;
    button?.classList.toggle("has-update", update.available);
    button?.setAttribute("title", update.available ? t("update.availableTooltip", { version: update.version ?? "" }) : t("action.checkUpdates"));
    if (update.available) {
      if (manual) showUpdateDialog(update);
      else showToast(t("update.availableTitle"), t("update.availableBody", { version: update.version ?? "" }));
    } else if (manual) {
      showToast(t("update.latestTitle"), t("update.latestBody", { version: update.currentVersion }));
    }
  } catch (error) {
    if (manual) showToast(t("toast.updateFailed"), String(error), true);
  } finally {
    button?.classList.remove("is-checking");
  }
}

function showUpdateDialog(update: UpdateInfo): void {
  setText("#update-title", `DisplayMux ${update.version ?? ""}`);
  setText("#update-version", t("update.currentVersion", { version: update.currentVersion }));
  const notes = document.querySelector<HTMLElement>("#update-notes");
  if (notes) renderMarkdown(notes, update.notes?.trim() || t("update.noneNotes"));
  const overlay = document.querySelector("#update-overlay");
  overlay?.classList.add("is-visible");
  overlay?.setAttribute("aria-hidden", "false");
}

function showOnboarding(step: number): void {
  clearOnboardingTarget();
  onboardingStep = Math.max(0, Math.min(step, onboardingSteps.length - 1));
  const current = onboardingSteps[onboardingStep];
  showPage(current.page);
  setText("#onboarding-counter", t("onboarding.progress", { current: onboardingStep + 1, total: onboardingSteps.length }));
  setText("#onboarding-step-label", current.label);
  setText("#onboarding-title", current.title);
  setText("#onboarding-body", current.body);

  const status = document.querySelector<HTMLElement>("#onboarding-status");
  const statusCopy = onboardingStatus(onboardingStep);
  if (status) {
    status.hidden = statusCopy == null;
    status.classList.toggle("is-ready", statusCopy?.ready ?? false);
  }
  if (statusCopy) {
    setText("#onboarding-status-title", statusCopy.title);
    setText("#onboarding-status-detail", statusCopy.detail);
  }

  const previous = document.querySelector<HTMLButtonElement>("#onboarding-previous");
  if (previous) previous.hidden = onboardingStep === 0;
  setText("#onboarding-next", onboardingStep === onboardingSteps.length - 1 ? t("onboarding.finishTour") : t("onboarding.next"));

  const overlay = document.querySelector("#onboarding-overlay");
  const tooltip = document.querySelector("#onboarding-tooltip");
  overlay?.classList.add("is-visible");
  overlay?.setAttribute("aria-hidden", "false");
  tooltip?.classList.add("is-visible");
  tooltip?.setAttribute("aria-hidden", "false");

  const workspace = document.querySelector<HTMLElement>(".workspace");
  if (onboardingStep <= 2) workspace?.scrollTo({ top: 0, behavior: "auto" });
  window.requestAnimationFrame(() => {
    const target = document.querySelector<HTMLElement>(current.target);
    if (!target) return;
    target.scrollIntoView({ block: "center", inline: "nearest", behavior: "auto" });
    target.classList.add("onboarding-target");
    target.setAttribute("aria-describedby", "onboarding-title onboarding-body");
    positionOnboardingTooltip(target);
    document.querySelector<HTMLButtonElement>("#onboarding-next")?.focus();
  });
}

function onboardingStatus(step: number): { ready: boolean; title: string; detail: string } | null {
  if (step === 2) {
    if (settings.sharedMonitors.length > 0) {
      return { ready: true, title: t("onboarding.displaySelected"), detail: settings.sharedMonitors.map((sm) => sm.name).join(", ") };
    }
    if (dashboard.monitors.length > 0) {
      return { ready: true, title: t("onboarding.displaysDetected", { count: dashboard.monitors.length }), detail: t("onboarding.displaysDetectedDetail") };
    }
    return { ready: false, title: t("onboarding.noDisplayDetected"), detail: t("onboarding.noDisplayDetectedDetail") };
  }
  if (step === 3) {
    const ready = settings.sharedKey.trim().length >= 8;
    return {
      ready,
      title: ready ? t("onboarding.pairingReady") : t("onboarding.pairingNotReady"),
      detail: ready ? t("onboarding.pairingReadyDetail") : t("onboarding.pairingNotReadyDetail"),
    };
  }
  return null;
}

function clearOnboardingTarget(): void {
  document.querySelectorAll<HTMLElement>(".onboarding-target").forEach((target) => {
    target.classList.remove("onboarding-target");
    target.removeAttribute("aria-describedby");
  });
}

function positionOnboardingTooltip(explicitTarget?: HTMLElement): void {
  const tooltip = document.querySelector<HTMLElement>("#onboarding-tooltip");
  if (!tooltip?.classList.contains("is-visible")) return;
  const target = explicitTarget ?? document.querySelector<HTMLElement>(".onboarding-target");
  if (!target) return;

  const targetRect = target.getBoundingClientRect();
  const tooltipRect = tooltip.getBoundingClientRect();
  const gap = 18;
  const edge = 16;
  const preferred = onboardingSteps[onboardingStep].placement;
  const placements = [preferred, "right", "left", "bottom", "top"]
    .filter((placement, index, all) => all.indexOf(placement) === index);

  const coordinates = (placement: string): { left: number; top: number } => {
    if (placement === "left") return { left: targetRect.left - tooltipRect.width - gap, top: targetRect.top + (targetRect.height - tooltipRect.height) / 2 };
    if (placement === "bottom") return { left: targetRect.left + (targetRect.width - tooltipRect.width) / 2, top: targetRect.bottom + gap };
    if (placement === "top") return { left: targetRect.left + (targetRect.width - tooltipRect.width) / 2, top: targetRect.top - tooltipRect.height - gap };
    return { left: targetRect.right + gap, top: targetRect.top + (targetRect.height - tooltipRect.height) / 2 };
  };

  let placement = placements[0];
  let position = coordinates(placement);
  for (const candidate of placements) {
    const next = coordinates(candidate);
    if (next.left >= edge && next.top >= edge && next.left + tooltipRect.width <= window.innerWidth - edge && next.top + tooltipRect.height <= window.innerHeight - edge) {
      placement = candidate;
      position = next;
      break;
    }
  }

  tooltip.dataset.placement = placement;
  tooltip.style.left = `${Math.min(Math.max(position.left, edge), window.innerWidth - tooltipRect.width - edge)}px`;
  tooltip.style.top = `${Math.min(Math.max(position.top, edge), window.innerHeight - tooltipRect.height - edge)}px`;
}

async function completeOnboarding(openSettings: boolean): Promise<void> {
  try {
    settings = isPreview
      ? { ...settings, onboardingCompleted: true }
      : await invoke<AppSettings>("complete_onboarding");
    const overlay = document.querySelector("#onboarding-overlay");
    const tooltip = document.querySelector("#onboarding-tooltip");
    clearOnboardingTarget();
    overlay?.classList.remove("is-visible");
    overlay?.setAttribute("aria-hidden", "true");
    tooltip?.classList.remove("is-visible");
    tooltip?.setAttribute("aria-hidden", "true");
    if (openSettings) {
      showPage("settings");
      document.querySelector(".workspace")?.scrollTo({ top: 0, behavior: "smooth" });
      window.requestAnimationFrame(() => document.querySelector<HTMLButtonElement>("[data-monitor-id]:not(:disabled)")?.focus());
    }
  } catch (error) {
    showToast(t("toast.onboardingFailed"), String(error), true);
  }
}

function appendInlineMarkdown(parent: HTMLElement, source: string): void {
  const pattern = /(\[([^\]]+)\]\(([^)\s]+)\)|\*\*([^*]+)\*\*|`([^`]+)`|\*([^*]+)\*)/g;
  let cursor = 0;

  for (const match of source.matchAll(pattern)) {
    const index = match.index ?? 0;
    parent.append(document.createTextNode(source.slice(cursor, index)));
    if (match[2] && match[3]) {
      try {
        const url = new URL(match[3]);
        if (url.protocol !== "https:") throw new Error("unsupported Markdown link protocol");
        const link = document.createElement("a");
        link.href = url.href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.textContent = match[2];
        parent.append(link);
      } catch {
        parent.append(document.createTextNode(match[0]));
      }
    } else if (match[4]) {
      const strong = document.createElement("strong");
      strong.textContent = match[4];
      parent.append(strong);
    } else if (match[5]) {
      const code = document.createElement("code");
      code.textContent = match[5];
      parent.append(code);
    } else if (match[6]) {
      const emphasis = document.createElement("em");
      emphasis.textContent = match[6];
      parent.append(emphasis);
    }
    cursor = index + match[0].length;
  }
  parent.append(document.createTextNode(source.slice(cursor)));
}

function renderMarkdown(container: HTMLElement, source: string): void {
  container.replaceChildren();
  let list: HTMLUListElement | HTMLOListElement | null = null;

  for (const rawLine of source.replace(/\r\n?/g, "\n").split("\n")) {
    const line = rawLine.trim();
    if (!line) {
      list = null;
      continue;
    }

    const heading = /^(#{1,6})\s+(.+)$/.exec(line);
    if (heading) {
      list = null;
      const element = document.createElement(`h${heading[1].length}`) as HTMLHeadingElement;
      appendInlineMarkdown(element, heading[2]);
      container.append(element);
      continue;
    }

    if (/^(?:-{3,}|\*{3,}|_{3,})$/.test(line)) {
      list = null;
      container.append(document.createElement("hr"));
      continue;
    }

    const listItem = /^(?:([-*+])|(\d+)\.)\s+(.+)$/.exec(line);
    if (listItem) {
      const tagName = listItem[2] ? "OL" : "UL";
      if (!list || list.tagName !== tagName) {
        list = document.createElement(tagName.toLowerCase()) as HTMLUListElement | HTMLOListElement;
        container.append(list);
      }
      const item = document.createElement("li");
      appendInlineMarkdown(item, listItem[3]);
      list.append(item);
      continue;
    }

    list = null;
    const quote = /^>\s?(.*)$/.exec(line);
    const element = document.createElement(quote ? "blockquote" : "p");
    appendInlineMarkdown(element, quote?.[1] ?? line);
    container.append(element);
  }
}

function hideUpdateDialog(): void {
  const overlay = document.querySelector("#update-overlay");
  overlay?.classList.remove("is-visible");
  overlay?.setAttribute("aria-hidden", "true");
}

async function installUpdate(): Promise<void> {
  const installButton = document.querySelector<HTMLButtonElement>("#update-install");
  const cancelButton = document.querySelector<HTMLButtonElement>("#update-cancel");
  const progress = document.querySelector<HTMLElement>("#update-progress");
  if (installButton) { installButton.disabled = true; installButton.textContent = t("update.preparing"); }
  if (cancelButton) cancelButton.disabled = true;
  if (progress) progress.hidden = false;
  const onEvent = new Channel<UpdateDownloadEvent>();
  onEvent.onmessage = (event) => {
    if (event.event === "started") {
      setText("#update-progress-label", t("update.downloadingSigned"));
    } else if (event.event === "progress") {
      const percent = event.contentLength ? Math.min(100, Math.round(event.downloaded / event.contentLength * 100)) : 0;
      const bar = document.querySelector<HTMLElement>("#update-progress-bar");
      if (bar) bar.style.width = event.contentLength ? `${percent}%` : "35%";
      setText("#update-progress-label", event.contentLength ? t("update.downloaded", { percent }) : t("update.downloading"));
    } else {
      setText("#update-progress-label", t("update.installing"));
    }
  };
  try {
    await invoke("install_update", { onEvent });
  } catch (error) {
    showToast(t("toast.installFailed"), String(error), true);
    if (installButton) { installButton.disabled = false; installButton.textContent = t("update.retry"); }
    if (cancelButton) cancelButton.disabled = false;
  }
}

function parseInput(value: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const parsed = /^0x/i.test(trimmed) ? Number.parseInt(trimmed.slice(2), 16) : Number.parseInt(trimmed, 10);
  if (!Number.isInteger(parsed) || parsed < 1 || parsed > 255) throw new Error(t("input.parseError", { value: trimmed }));
  return parsed;
}

function inputName(value: number, monitorKey: string): string {
  const known = (inputOptionsByMonitor[monitorKey] ?? standardInputs).find((item) => item.value === value);
  return known ? known.name : t("input.other");
}
function platformName(value: Platform): string { return value === "mac" ? "macOS" : "Windows"; }
function isUltrawideResolution(value: MonitorResolution): boolean { return value.height > 0 && value.width >= value.height * 2; }
function resolutionSourceName(value: ResolutionSource | null): string {
  if (value === "edid") return "EDID";
  if (value === "coreGraphicsDisplayMode") return t("resolution.coreGraphics");
  if (value === "windowsDisplayMode") return t("resolution.windows");
  return t("resolution.unknown");
}
function sameFingerprint(left: Fingerprint, right: Fingerprint): boolean { return left.manufacturer_id.toUpperCase() === right.manufacturer_id.toUpperCase() && left.product_code.toUpperCase() === right.product_code.toUpperCase() && left.serial_number === right.serial_number; }
function escapeHtml(value: string): string { return value.replace(/[&<>'"]/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[char] ?? char); }
function cssEscape(value: string): string { return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : value.replace(/["\\]/g, "\\$&"); }
function setText(selector: string, value: string): void { const element = document.querySelector(selector); if (element) element.textContent = value; }
function setInput(selector: string, value: string): void { const element = document.querySelector<HTMLInputElement>(selector); if (element) element.value = value; }
function showOperation(title: string, detail?: string): void {
  setText("#operation-title", title);
  if (detail) setText("#operation-detail", detail);
  document.querySelector("#operation-overlay")?.classList.add("is-visible");
}
function hideOperation(): void { document.querySelector("#operation-overlay")?.classList.remove("is-visible"); }
let toastTimer = 0;
function showToast(title: string, detail: string, warning = false): void {
  const toast = document.querySelector("#toast"); if (!toast) return;
  window.clearTimeout(toastTimer); setText("#toast-title", title); setText("#toast-detail", detail);
  toast.classList.toggle("is-warning", warning); toast.classList.add("is-visible");
  toastTimer = window.setTimeout(() => toast.classList.remove("is-visible"), 5200);
}

async function bootstrap(): Promise<void> {
  if (!isPreview) {
    try { await invoke("set_locale", { locale }); } catch { /* Preview mode has no Tauri backend. */ }
  }
  await Promise.all([refresh(), renderAppVersion()]);
  if (!isPreview) {
    try {
      await listen(ACTIVE_ROUTE_CHANGED_EVENT, () => void reloadActiveRoutes());
    } catch (error) {
      showToast(t("toast.activeHostSyncFailed"), String(error), true);
    }
    window.addEventListener("focus", refreshOnReturn);
    document.addEventListener("visibilitychange", refreshOnReturn);
  }
  void refreshReleaseHistory();
  if (!settings.onboardingCompleted) showOnboarding(0);
  if (settings.checkUpdates && !isPreview) window.setTimeout(() => void checkForUpdates(false), 1800);
}

async function renderAppVersion(): Promise<void> {
  try {
    setText("#app-version", `v${await getVersion()}`);
  } catch {
    setText("#app-version", `v${packageMetadata.version}`);
  }
}

void bootstrap();
