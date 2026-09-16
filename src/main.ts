import "@fontsource-variable/manrope";
import {
  Activity, ArrowLeftRight, CircleHelp, Computer, createIcons, Download, KeyRound, Laptop,
  ChevronDown, ChevronLeft, ChevronRight, ExternalLink, Pencil, Github, Languages, Monitor, MonitorOff, MoonStar, Network, Plus, RefreshCw, Save, Search, Settings,
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
  monitorIdentityLinks?: MonitorIdentityLink[];
}

interface MonitorIdentityLink {
  alias: Fingerprint;
  primary: Fingerprint | null;
  updatedAtMs: number;
}

interface SharedMonitorStatus {
  monitorKey: string;
  fingerprint: Fingerprint;
  name: string;
  ddcAvailable: boolean;
  /** "onOtherHost": unreadable because the display is showing a paired host, which is expected. */
  displayState: "ready" | "onOtherHost" | "unavailable";
  statusText: string;
  connection: MonitorConnection | null;
  connectionInputConflict: boolean;
}

interface MonitorIdentityClaim {
  aliasKey: string;
  aliasLabel: string;
  primaryKey: string;
  primaryLabel: string;
}

interface DashboardState {
  platform: string;
  localHost: Platform;
  agentConfigured: boolean;
  monitors: MonitorDescriptor[];
  uncontrollableMonitors: MonitorDescriptor[];
  shared: SharedMonitorStatus[];
  selectionNotices: string[];
  monitorIdentityClaims?: MonitorIdentityClaim[];
  localHostName?: string;
}

interface DiscoveredPeer {
  id: string;
  name: string;
  platform: Platform;
  address: string;
  port: number;
  macAddress: string | null;
}

/** `name` includes the user's note; `baseName` and `label` are its parts. */
interface InputOption { value: number; name: string; baseName?: string; label?: string; }
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
  { date: "2026-09-16", version: "v0.1.10", url: "https://github.com/OmarHung/DisplayMuxAuto/releases/tag/v0.1.10" },
  { date: "2026-09-16", version: "v0.1.9", url: "https://github.com/OmarHung/DisplayMuxAuto/releases/tag/v0.1.9" },
  { date: "2026-09-16", version: "v0.1.8", url: "https://github.com/OmarHung/DisplayMuxAuto/releases/tag/v0.1.8" },
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
        <div class="switch-all-bar" id="switch-all-bar" hidden></div>
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
                <div class="monitor-merge" id="monitor-merge"></div>
              </div>

              <div class="form-section two-columns">
                <label class="field"><span>${t("settings.localComputer")}</span><input id="local-host-name" maxlength="24" /><small>${t("settings.localComputerHint")}</small></label>
              </div>

              <div class="form-section">
                <div class="pairing-heading">
                  <strong>${t("settings.localInput")}</strong>
                </div>
                <div class="local-input-summary" id="local-input-summary"></div>
              </div>

              <div class="form-section">
                <div class="pairing-heading">
                  <strong>${t("settings.inputLabels")}</strong>
                </div>
                <small class="section-hint">${t("settings.inputLabelsHint")}</small>
                <div class="input-labels" id="input-labels"></div>
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

            <div class="form-section reset-section">
              <div class="pairing-heading"><strong>${t("settings.resetTitle")}</strong></div>
              <p class="monitor-merge-note">${t("settings.resetIntro")}</p>
              <div class="reset-actions">
                <button type="button" class="reset-button" data-reset-scope="displays">
                  <strong>${t("settings.resetDisplays")}</strong>
                  <small>${t("settings.resetDisplaysHint")}</small>
                </button>
                <button type="button" class="reset-button is-danger" data-reset-scope="everything">
                  <strong>${t("settings.resetEverything")}</strong>
                  <small>${t("settings.resetEverythingHint")}</small>
                </button>
              </div>
            </div>
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

const iconSet = { Activity, ArrowLeftRight, ChevronDown, ChevronLeft, ChevronRight, CircleHelp, Pencil, Computer, Download, ExternalLink, Github, KeyRound, Languages, Laptop, Monitor, MonitorOff, MoonStar, Network, Plus, RefreshCw, Save, Search, Settings, ShieldCheck, SunMoon, Trash2, UserRound, Zap };
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
  const monitorId = button?.dataset.monitorId;
  if (!button || !monitorId) return;
  const selected = button.dataset.monitorSelected === "true";
  void withBusyButton(button, () => (selected ? removeSharedMonitor(monitorId) : addSharedMonitor(monitorId)));
});
document.querySelector("#local-host-name")?.addEventListener("change", (event) => {
  void renameLocalHost((event.target as HTMLInputElement).value);
});
document.querySelector(".settings-main")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-reset-scope]");
  if (button?.dataset.resetScope) requestReset(button.dataset.resetScope, button);
});
document.querySelector("#monitor-merge")?.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const undo = target.closest<HTMLButtonElement>("[data-unmerge-alias]");
  const undoAlias = undo?.dataset.unmergeAlias;
  if (undo && undoAlias) {
    void withBusyButton(undo, () => unmergeSharedMonitor(undoAlias));
    return;
  }
  const button = target.closest<HTMLButtonElement>("[data-merge-alias]");
  const aliasId = button?.dataset.mergeAlias;
  if (!button || !aliasId) return;
  const select = document.querySelector<HTMLSelectElement>(`[data-merge-target="${CSS.escape(aliasId)}"]`);
  const primaryId = select?.value;
  if (primaryId) void withBusyButton(button, () => mergeSharedMonitor(aliasId, primaryId));
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
const inputLabels = document.querySelector<HTMLElement>("#input-labels");
inputLabels?.addEventListener("change", (event) => {
  const field = (event.target as HTMLElement).closest<HTMLInputElement>("[data-label-input]");
  if (field) void commitInputLabel(field);
});
inputLabels?.addEventListener("keydown", (event) => {
  const field = (event.target as HTMLElement).closest<HTMLInputElement>("[data-label-input]");
  if (!field) return;
  if (event.key === "Enter") {
    // Inside the settings form, Enter would otherwise submit every setting.
    event.preventDefault();
    field.blur();
  } else if (event.key === "Escape") {
    field.value = inputOptionsByMonitor[field.dataset.labelMonitor ?? ""]?.find((option) => option.value === Number(field.dataset.labelInput))?.label ?? "";
    field.blur();
  }
});
inputLabels?.addEventListener("toggle", (event) => {
  const group = event.target as HTMLDetailsElement;
  const monitorKey = group.dataset.labelGroup;
  if (!monitorKey) return;
  const next = new Set(openInputLabelGroups);
  if (group.open) next.add(monitorKey); else next.delete(monitorKey);
  openInputLabelGroups = next;
}, true);
document.querySelector("#monitor-strip")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-monitor-key]");
  if (!button?.dataset.monitorKey || button.dataset.monitorKey === activeMonitorKey) return;
  activeMonitorKey = button.dataset.monitorKey;
  renderMonitorStrip();
  renderSwitchPanel();
  refreshIcons();
});
const switchPanel = document.querySelector<HTMLElement>("#switch-panel");
switchPanel?.addEventListener("click", (event) => {
  const moveButton = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-move-route]");
  if (moveButton?.dataset.moveRoute) {
    moveRouteBy(moveButton.dataset.moveRoute, Number(moveButton.dataset.moveOffset));
  }
  const renameButton = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-rename-route]");
  if (renameButton?.dataset.renameRoute) startRenaming(renameButton.dataset.renameRoute);
});
switchPanel?.addEventListener("input", (event) => {
  const field = event.target as HTMLInputElement;
  if (field.dataset.renameInput && renaming?.routeId === field.dataset.renameInput) {
    renaming = { ...renaming, draft: field.value };
  }
});
switchPanel?.addEventListener("keydown", (event) => {
  const field = event.target as HTMLInputElement;
  if (!field.dataset.renameInput) return;
  if (event.key === "Enter") {
    event.preventDefault();
    void commitRename(field.dataset.renameInput);
  } else if (event.key === "Escape") {
    event.preventDefault();
    stopRenaming(field.dataset.renameInput);
  }
});
switchPanel?.addEventListener("focusout", (event) => {
  const field = event.target as HTMLInputElement;
  if (field.dataset.renameInput) void commitRename(field.dataset.renameInput);
});
switchPanel?.addEventListener("dragstart", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (!card?.dataset.routeCard || !event.dataTransfer) return;
  draggedRouteId = card.dataset.routeCard;
  event.dataTransfer.effectAllowed = "move";
  event.dataTransfer.setData("text/plain", draggedRouteId);
  card.classList.add("is-dragging");
});
switchPanel?.addEventListener("dragover", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (!draggedRouteId || !card) return;
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
  switchPanel.querySelectorAll(".is-drop-target").forEach((item) => item.classList.remove("is-drop-target"));
  if (card.dataset.routeCard !== draggedRouteId) card.classList.add("is-drop-target");
});
switchPanel?.addEventListener("drop", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (!draggedRouteId || !card?.dataset.routeCard) return;
  event.preventDefault();
  const order = currentRouteIds();
  const next = movedRoute(order, draggedRouteId, order.indexOf(card.dataset.routeCard));
  if (next.join() !== order.join()) void saveRouteOrder(next);
});
switchPanel?.addEventListener("dragend", () => {
  draggedRouteId = null;
  switchPanel.querySelectorAll(".is-dragging, .is-drop-target").forEach((item) => item.classList.remove("is-dragging", "is-drop-target"));
});
document.querySelector("#switch-all-bar")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-switch-all-id]");
  if (button?.dataset.switchAllId) void switchAllToHost(button.dataset.switchAllId);
});
switchPanel?.addEventListener("click", (event) => {
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
  renderSwitchAllBar();
  refreshIcons();
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
/** Emitted by the backend when this or a paired host saves a new host card order. */
const HOST_ORDER_CHANGED_EVENT = "host-order-changed";
/** Route ids ("local" and peer ids) in the saved host card order. */
let routeOrder: string[] = [];
/** Emitted by the backend when this or a paired host renames a host. */
const HOST_NAMES_CHANGED_EVENT = "host-names-changed";
/** Emitted by the backend when this or a paired host changes an input note. */
const INPUT_LABELS_CHANGED_EVENT = "input-labels-changed";
/** Emitted by the backend when a paired host reports the port it occupies. */
const PEER_INPUTS_CHANGED_EVENT = "peer-inputs-changed";
/** Longest input note the backend accepts, in characters. */
const MAX_INPUT_LABEL_CHARS = 24;
/** Shared displays whose input note list is expanded, by monitor key. */
let openInputLabelGroups = new Set<string>();
/** Longest custom host name the backend accepts, in characters. */
const MAX_HOST_NAME_CHARS = 32;
/** Custom host names by route id; hosts using their default name are absent. */
let hostNames: Record<string, string> = {};
/** The host card whose name is being edited, and the unsaved text. */
let renaming: { routeId: string; draft: string } | null = null;
let isRefreshing = false;
let lastRefreshAt = 0;

async function refresh(): Promise<void> {
  isRefreshing = true;
  lastRefreshAt = Date.now();
  document.querySelector("#refresh-button svg")?.classList.add("is-spinning");
  try {
    dashboard = await invoke<DashboardState>("get_dashboard_state");
    [settings, inputOptionsByMonitor, routeOrder, hostNames] = await Promise.all([
      invoke<AppSettings>("get_settings"),
      loadInputOptionsByMonitor(dashboard.shared.map((shared) => shared.monitorKey)),
      invoke<string[]>("get_host_order"),
      invoke<Record<string, string>>("get_host_names"),
    ]);
    try { discoveredPeers = await invoke<DiscoveredPeer[]>("discover_peers"); } catch { discoveredPeers = []; }
    isPreview = false;
    // Catch up on host names and order changed while a paired host was offline.
    // Throttled and run in the background by the backend; results arrive as events.
    void invoke("exchange_host_layout").catch((error: unknown) => showToast(t("toast.hostNameFailed"), String(error), true));
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
  scheduleSettledRescans();
}

/**
 * Re-reads the saved peer inputs after a paired host reported the port it
 * occupies, leaving a select the user has open alone so the new value never
 * closes a list mid-pick.
 */
async function reloadPeerInputs(): Promise<void> {
  const routes = document.querySelector("#paired-routes");
  if (routes?.contains(document.activeElement)) return;
  try {
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, peers: latest.peers };
    renderPairedRoutes();
    renderInputNames();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.peerInputSyncFailed"), String(error), true);
  }
}

function renderMonitorHealth(): void {
  const usable = dashboard.shared.some((shared) => shared.displayState !== "unavailable");
  setText("#monitor-health", dashboard.shared.length === 0 ? t("dashboard.notSelected") : usable ? t("dashboard.locked") : t("dashboard.notReady"));
}

/**
 * When to rescan after a switch. Displays take a few seconds to change input
 * and keep answering (or not answering) DDC/CI as before until they do, so an
 * immediate scan shows the old readiness.
 */
const SETTLED_RESCAN_DELAYS_MS = [3_000, 8_000];
let settledRescanTimers: number[] = [];

function scheduleSettledRescans(): void {
  settledRescanTimers.forEach((timer) => window.clearTimeout(timer));
  settledRescanTimers = SETTLED_RESCAN_DELAYS_MS.map((delay) => window.setTimeout(() => void rescanDisplays(), delay));
}

/**
 * Rescans displays and redraws only the dashboard's display cards, so unsaved
 * edits on the settings page survive.
 */
async function rescanDisplays(): Promise<void> {
  if (isPreview || isRefreshing) return;
  isRefreshing = true;
  lastRefreshAt = Date.now();
  try {
    // The scan can move the active host, so read settings after it.
    dashboard = await invoke<DashboardState>("get_dashboard_state");
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, sharedMonitors: latest.sharedMonitors };
    renderMonitorHealth();
    keepActiveMonitorSelected();
    renderMonitorStrip();
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.activeHostSyncFailed"), String(error), true);
  } finally {
    isRefreshing = false;
  }
}

/** Falls back to the first shared display when the selected one disappeared. */
function keepActiveMonitorSelected(): void {
  if (!activeMonitorKey || !dashboard.shared.some((shared) => shared.monitorKey === activeMonitorKey)) {
    activeMonitorKey = dashboard.shared[0]?.monitorKey ?? null;
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

/** A display's resolution belongs to the display mode it is in right now, not
 *  to the snapshot taken when it was selected, so a live reading wins. The
 *  stored value is only a last-known fallback for a display nothing can see:
 *  it is refreshed solely while the display answers DDC/CI, so it outlives the
 *  mode — and, on displays that change identity with the mode, the reading it
 *  was taken for. Displays showing another host are included, since they are
 *  still enumerated even though they cannot be read. */
function resolutionFor(shared: SharedMonitorStatus): { resolution: MonitorResolution | null; source: ResolutionSource | null } {
  const live = [...dashboard.monitors, ...(dashboard.uncontrollableMonitors ?? [])]
    .find((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint));
  if (live?.maxResolution) {
    return { resolution: live.maxResolution, source: live.resolutionSource ?? null };
  }
  const selectedMonitor = selectedMonitorFor(shared);
  return {
    resolution: selectedMonitor?.maxResolution ?? null,
    source: selectedMonitor?.resolutionSource ?? null,
  };
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
      ${displayStateBadge(shared)}
    </button>`;
  }).join("");
}

function displayStateBadge(shared: SharedMonitorStatus): string {
  if (shared.displayState === "ready") return `<span class="status-badge">${t("dashboard.ddcReady")}</span>`;
  if (shared.displayState === "onOtherHost") return `<span class="status-badge is-elsewhere">${t("dashboard.onOtherHost")}</span>`;
  return `<span class="status-badge subtle">${t("dashboard.notReady")}</span>`;
}

function renderSwitchPanel(): void {
  renderSwitchAllBar();
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
          ${displayStateBadge(shared)}
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
  renderMonitorHealth();
  setText("#peer-health", t("dashboard.hostCount", { count: settings.peers.length }));
  setText("#wake-health", settings.peers.some((peer) => peer.macAddress) ? t("dashboard.wakeNormal") : (settings.peers.length ? t("dashboard.noMac") : t("dashboard.noHosts")));
  const pill = document.querySelector("#agent-pill");
  pill?.classList.toggle("is-ready", dashboard.agentConfigured);
  if (pill) pill.querySelector("span:last-child")!.textContent = isPreview ? t("dashboard.preview") : dashboard.agentConfigured ? t("dashboard.agentReady") : t("dashboard.agentMissing");
  setInput("#local-host-name", routeDisplayName("local"));
  setInput("#shared-key", settings.sharedKey);
  setInput("#wait-seconds", String(settings.waitSeconds));
  const autostart = document.querySelector<HTMLInputElement>("#autostart");
  if (autostart) autostart.checked = settings.autostart;
  const checkUpdates = document.querySelector<HTMLInputElement>("#check-updates");
  if (checkUpdates) checkUpdates.checked = settings.checkUpdates;
  const hostSwitcherEnabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled");
  if (hostSwitcherEnabled) hostSwitcherEnabled.checked = settings.hostSwitcherEnabled;
  renderShortcutSetting();
  keepActiveMonitorSelected();
  renderMonitorStrip(); renderSwitchPanel();
  renderMonitors(); renderMonitorMerge(); renderPeerList(); renderPairedRoutes(); renderLocalInputSummary(); renderInputLabels(); renderInputHints(); refreshIcons();
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
  // A shared display showing another host can't be read from here, but it is
  // still this computer's shared display, so list it with the controllable ones.
  const isOnOtherHost = (monitor: MonitorDescriptor) => dashboard.shared.some((shared) =>
    shared.displayState === "onOtherHost" && sameDisplay(shared.fingerprint, monitor.fingerprint));
  const uncontrollable = dashboard.uncontrollableMonitors ?? [];
  const elsewhere = uncontrollable.filter(isOnOtherHost);
  const unreachable = uncontrollable.filter((monitor) => !isOnOtherHost(monitor));
  if (!dashboard.monitors.length && !uncontrollable.length) {
    container.innerHTML = `<p class="peer-empty">${t("settings.noMonitors")}</p>`; return;
  }
  // A shared display this computer cannot see at all is listed from the saved
  // selection, because it is exactly the one the user may need to remove and
  // nothing enumerates a row for it.
  const absent = dashboard.shared.filter((shared) =>
    ![...dashboard.monitors, ...uncontrollable].some((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint)));
  container.innerHTML = dashboard.monitors.map((monitor) => selectableMonitorCard(monitor, t("settings.ddcControllable")))
    .concat(elsewhere.map((monitor) => selectableMonitorCard(monitor, t("dashboard.onOtherHost"))))
    .join("") + absent.map((shared) => `
    <article class="monitor-card-item is-selected">
      <div class="monitor-item-left">
        <div class="monitor-item-icon"><i data-lucide="monitor-off"></i></div>
        <div class="monitor-identity">
          <strong>${escapeHtml(shared.name)}</strong>
          <span>${escapeHtml(shared.fingerprint.manufacturer_id)} / ${escapeHtml(shared.fingerprint.product_code)} / ${escapeHtml(shared.fingerprint.serial_number ?? t("settings.noSerial"))} (${escapeHtml(t("settings.notDetected"))})</span>
        </div>
      </div>
      <button type="button" class="monitor-select-btn is-selected" data-monitor-id="${escapeHtml(shared.monitorKey)}" data-monitor-selected="true">
        ${t("action.removeShared")}
      </button>
    </article>
  `).join("") + unreachable.map((monitor) => selectableMonitorCard(monitor, t("settings.ddcUnreachable"))).join("");
}

/** Offers to merge a display that is present but belongs to no shared display
 *  into one that is. A display that reports a different identity per display
 *  mode shows up as an unfamiliar new display while the shared one it really
 *  is goes unreadable, and only the user can say they are one panel. */
function renderMonitorMerge(): void {
  const container = document.querySelector("#monitor-merge");
  if (!container) return;
  const present = [...dashboard.monitors, ...(dashboard.uncontrollableMonitors ?? [])];
  const strangers = present.filter((monitor) => !isSharedDisplay(monitor.fingerprint));
  // Only a shared display that has gone missing can be the other identity of a
  // display that just turned up. With every shared display accounted for there
  // is nothing to merge, and offering it anyway invites merging two displays
  // that are genuinely different — which is not something a user can undo by
  // looking at the screen.
  const targets = dashboard.shared.filter((shared) =>
    !present.some((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint)));
  const claims = dashboard.monitorIdentityClaims ?? [];
  const canMerge = strangers.length > 0 && targets.length > 0;
  if (!canMerge && !claims.length) { container.innerHTML = ""; return; }

  const describeMonitor = (monitor: MonitorDescriptor) =>
    `${monitor.name} (${monitor.fingerprint.manufacturer_id}/${monitor.fingerprint.product_code})`;
  const describeShared = (shared: SharedMonitorStatus) =>
    `${shared.name} (${shared.fingerprint.manufacturer_id}/${shared.fingerprint.product_code})`;
  container.innerHTML = `
    <div class="pairing-heading"><strong>${t("settings.mergeTitle")}</strong></div>
    <p class="monitor-merge-note">${t("settings.mergeIntro")}</p>
    ${claims.map((claim) => `
      <article class="monitor-card-item is-selected">
        <div class="monitor-item-left">
          <div class="monitor-item-icon"><i data-lucide="link"></i></div>
          <div class="monitor-identity">
            <strong>${escapeHtml(claim.aliasLabel)} → ${escapeHtml(claim.primaryLabel)}</strong>
            <span>${escapeHtml(t("settings.mergedInto", { name: claim.primaryLabel }))}</span>
          </div>
        </div>
        <button type="button" class="monitor-select-btn" data-unmerge-alias="${escapeHtml(claim.aliasKey)}">
          ${t("settings.mergeUndo")}
        </button>
      </article>
    `).join("")}
    ${(canMerge ? strangers : []).map((monitor) => `
      <article class="monitor-card-item">
        <div class="monitor-item-left">
          <div class="monitor-item-icon"><i data-lucide="monitor-dot"></i></div>
          <div class="monitor-identity">
            <strong>${escapeHtml(describeMonitor(monitor))}</strong>
            <span>${escapeHtml(t("settings.mergeUnidentified"))}</span>
          </div>
        </div>
        <label class="monitor-merge-choice">
          <span>${t("settings.mergeSelect")}</span>
          <select data-merge-target="${escapeHtml(monitor.id)}">
            ${targets.map((target) => `<option value="${escapeHtml(target.monitorKey)}">${escapeHtml(describeShared(target))}</option>`).join("")}
          </select>
        </label>
        <button type="button" class="monitor-select-btn" data-merge-alias="${escapeHtml(monitor.id)}">
          ${t("settings.mergeAction")}
        </button>
      </article>
    `).join("")}
  `;
}

function selectableMonitorCard(monitor: MonitorDescriptor, statusLabel: string): string {
  const shared = dashboard.shared.find((item) => sameDisplay(item.fingerprint, monitor.fingerprint));
  const isSelected = Boolean(shared);
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
        <span>${escapeHtml(fp.manufacturer_id)} / ${escapeHtml(fp.product_code)} / ${escapeHtml(fp.serial_number ?? t("settings.noSerial"))} ${resText} (${escapeHtml(statusLabel)})</span>
        ${renderConnection(monitor.connection ?? null)}
      </div>
    </div>
    <button type="button" class="monitor-select-btn ${isSelected ? "is-selected" : ""}" data-monitor-id="${escapeHtml(shared?.monitorKey ?? monitor.id)}" data-monitor-selected="${isSelected}">
      ${isSelected ? t("action.removeShared") : t("action.selectShared")}
    </button>
  </article>`;
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

/** Hosts whose saved input for `shared` is `value`, by display name. */
function inputUsers(shared: SharedMonitorStatus, value: number): string[] {
  const localUser = selectedMonitorFor(shared)?.localInput === value ? [routeDisplayName("local")] : [];
  const peerUsers = settings.peers
    .filter((peer) => peer.inputs.some((assignment) => assignment.input === value && sameDisplay(assignment.monitor, shared.fingerprint)))
    .map((peer) => routeDisplayName(peer.id));
  return [...localUser, ...peerUsers];
}

/**
 * One note field per input of each shared display. Left alone while a field
 * has focus, so a sync from a paired host never replaces what is being typed.
 */
function renderInputLabels(): void {
  const container = document.querySelector<HTMLElement>("#input-labels");
  if (!container || container.contains(document.activeElement)) return;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="peer-empty">${t("settings.noMonitors")}</p>`;
    return;
  }
  const listFormat = new Intl.ListFormat(locale, { type: "conjunction" });
  container.innerHTML = dashboard.shared.map((shared) => {
    const rows = (inputOptionsByMonitor[shared.monitorKey] ?? []).map((option) => {
      const baseName = option.baseName ?? option.name;
      const users = inputUsers(shared, option.value);
      return `<label class="input-label-row">
        <span class="input-label-base">
          <strong>${escapeHtml(baseName)}</strong>
          ${users.length ? `<small>${escapeHtml(t("settings.inputUsedBy", { hosts: listFormat.format(users) }))}</small>` : ""}
        </span>
        <input class="input-label-field" data-label-monitor="${escapeHtml(shared.monitorKey)}" data-label-input="${option.value}" value="${escapeHtml(option.label ?? "")}" placeholder="${escapeHtml(t("settings.inputLabelPlaceholder"))}" maxlength="${MAX_INPUT_LABEL_CHARS}" aria-label="${escapeHtml(t("settings.inputLabelAria", { monitor: shared.name, input: baseName }))}" />
      </label>`;
    }).join("");
    return `<details class="input-label-group" data-label-group="${escapeHtml(shared.monitorKey)}" ${openInputLabelGroups.has(shared.monitorKey) ? "open" : ""}>
      <summary>${escapeHtml(shared.name)}</summary>
      <div class="input-label-rows">${rows}</div>
    </details>`;
  }).join("");
}

/** Redraws every place that shows input names. */
function renderInputNames(): void {
  renderLocalInputSummary();
  renderInputHints();
  renderInputLabels();
  renderSwitchPanel();
  refreshIcons();
}

async function reloadInputOptions(): Promise<void> {
  try {
    inputOptionsByMonitor = await loadInputOptionsByMonitor(dashboard.shared.map((shared) => shared.monitorKey));
    renderInputNames();
  } catch (error) {
    showToast(t("toast.inputLabelFailed"), String(error), true);
  }
}

async function commitInputLabel(field: HTMLInputElement): Promise<void> {
  const monitorKey = field.dataset.labelMonitor ?? "";
  // The field commits on blur, which is also what clicking anything else does,
  // so it can fire for a display that stopped being shared while it was open —
  // after a reset, or after the display was removed. A note for a display that
  // is no longer shared means nothing; reporting it as a failure to save does.
  if (!dashboard.shared.some((shared) => shared.monitorKey === monitorKey)) return;
  const input = Number(field.dataset.labelInput);
  const saved = inputOptionsByMonitor[monitorKey]?.find((option) => option.value === input)?.label ?? "";
  const label = field.value.trim();
  if (label === saved) {
    field.value = saved;
    return;
  }
  try {
    const options = await invoke<InputOption[]>("set_input_label", { monitorId: monitorKey, input, label });
    inputOptionsByMonitor = { ...inputOptionsByMonitor, [monitorKey]: options };
    field.value = options.find((option) => option.value === input)?.label ?? "";
    renderInputNames();
  } catch (error) {
    field.value = saved;
    showToast(t("toast.inputLabelFailed"), String(error), true);
  }
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
      <strong>${escapeHtml(hostNames[peer.id] ?? peer.name)}</strong>
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
        <select class="paired-input-field" data-route-input="${escapeHtml(peer.id)}" data-route-monitor="${escapeHtml(shared.monitorKey)}">${renderInputOptions(peer.id, shared.monitorKey, peer.inputs.find((assignment) => sameDisplay(assignment.monitor, shared.fingerprint))?.input ?? null)}</select>
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

/** Position of a route in the saved order; routes not yet ordered sort last. */
function routeRank(routeId: string): number {
  const rank = routeOrder.indexOf(routeId);
  return rank === -1 ? Number.MAX_SAFE_INTEGER : rank;
}

/** Every current route id in display order, as the backend expects it. */
function currentRouteIds(): string[] {
  return ["local", ...settings.peers.map((peer) => peer.id)]
    .sort((left, right) => routeRank(left) - routeRank(right));
}

function movedRoute(order: string[], routeId: string, targetIndex: number): string[] {
  const without = order.filter((id) => id !== routeId);
  const clamped = Math.max(0, Math.min(targetIndex, without.length));
  return [...without.slice(0, clamped), routeId, ...without.slice(clamped)];
}

async function saveRouteOrder(next: string[]): Promise<void> {
  const previous = routeOrder;
  routeOrder = next;
  renderSwitchPanel();
  refreshIcons();
  try {
    routeOrder = await invoke<string[]>("set_host_order", { routeIds: next });
  } catch (error) {
    routeOrder = previous;
    showToast(t("toast.hostOrderFailed"), String(error), true);
  }
  renderSwitchPanel();
  refreshIcons();
}

async function reloadHostOrder(): Promise<void> {
  try {
    routeOrder = await invoke<string[]>("get_host_order");
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostOrderFailed"), String(error), true);
  }
}

function moveRouteBy(routeId: string, offset: number): void {
  const order = currentRouteIds();
  const index = order.indexOf(routeId);
  if (index === -1) return;
  const next = movedRoute(order, routeId, index + offset);
  if (next.join() === order.join()) return;
  void saveRouteOrder(next).then(() => {
    document.querySelector<HTMLButtonElement>(`[data-move-route="${cssEscape(routeId)}"][data-move-offset="${offset}"]:not(:disabled)`)?.focus();
  });
}

let draggedRouteId: string | null = null;

async function reloadHostNames(): Promise<void> {
  try {
    hostNames = await invoke<Record<string, string>>("get_host_names");
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostNameFailed"), String(error), true);
  }
}

function startRenaming(routeId: string): void {
  const shownTitle = document.querySelector<HTMLElement>(`[data-route-card="${cssEscape(routeId)}"] .host-title`);
  renaming = { routeId, draft: shownTitle?.textContent ?? hostNames[routeId] ?? "" };
  renderSwitchPanel();
  refreshIcons();
  const field = document.querySelector<HTMLInputElement>(`[data-rename-input="${cssEscape(routeId)}"]`);
  field?.focus();
  field?.select();
}

function stopRenaming(routeId: string): void {
  if (renaming?.routeId !== routeId) return;
  renaming = null;
  renderSwitchPanel();
  refreshIcons();
  document.querySelector<HTMLButtonElement>(`[data-rename-route="${cssEscape(routeId)}"]`)?.focus();
}

async function commitRename(routeId: string): Promise<void> {
  if (renaming?.routeId !== routeId) return;
  const { draft } = renaming;
  const defaultName = document.querySelector<HTMLInputElement>(`[data-rename-input="${cssEscape(routeId)}"]`)?.placeholder ?? "";
  const currentName = hostNames[routeId] ?? defaultName;
  stopRenaming(routeId);
  const name = draft.trim();
  if (name === currentName) return;
  try {
    // Typing the default name back is the same as clearing the custom one.
    hostNames = await invoke<Record<string, string>>("set_host_name", { routeId, name: name === defaultName ? "" : name });
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostNameFailed"), String(error), true);
  }
}

/**
 * Two overlapping displays, drawn on Lucide's 24px grid and stroke so it sits
 * with the other icons. Lucide has no multi-display icon.
 */
const ALL_DISPLAYS_ICON = `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
  <path d="M7 7V5a2 2 0 0 1 2-2h11a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-3"/>
  <rect width="15" height="10" x="2" y="7" rx="2"/>
  <path d="M9.5 17v4"/>
  <path d="M6 21h7"/>
</svg>`;

/** A host's name as its card shows it: the custom name, else the default. */
function routeDisplayName(routeId: string): string {
  const custom = hostNames[routeId];
  if (custom) return custom;
  if (routeId === "local") {
    return dashboard.localHostName
      || (dashboard.localHost === "windows" ? t("dashboard.localWindows") : t("dashboard.localMac"));
  }
  return settings.peers.find((peer) => peer.id === routeId)?.name ?? routeId;
}

/**
 * One button per host that switches every shared display at once. Sits beside
 * the dashboard title because it acts on all displays, not the selected one.
 */
function renderSwitchAllBar(): void {
  const container = document.querySelector<HTMLElement>("#switch-all-bar");
  if (!container) return;
  const hasMultipleDisplays = dashboard.shared.length > 1;
  container.hidden = !hasMultipleDisplays || !document.querySelector("#dashboard-page")?.classList.contains("is-active");
  if (!hasMultipleDisplays) {
    container.innerHTML = "";
    return;
  }
  const buttons = currentRouteIds().map((routeId) => {
    const name = routeDisplayName(routeId);
    const platform = routeId === "local" ? dashboard.localHost : settings.peers.find((peer) => peer.id === routeId)?.platform;
    const icon = platform === "mac" ? "laptop" : "computer";
    const isAllShowing = dashboard.shared.every((shared) => (selectedMonitorFor(shared)?.activeRoute ?? "local") === routeId);
    if (isAllShowing) {
      return `<div class="switch-all-host is-showing" title="${escapeHtml(`${name} · ${t("dashboard.allDisplayed")}`)}">
        <i data-lucide="${icon}"></i><span class="switch-all-name">${escapeHtml(name)}</span>
        <span class="switch-all-state"><span class="active-route-dot" aria-hidden="true"></span><span class="switch-all-state-text">${t("dashboard.allDisplayed")}</span></span>
      </div>`;
    }
    const label = escapeHtml(t("action.switchAllToHost", { name }));
    return `<button type="button" class="switch-all-host" data-switch-all-id="${escapeHtml(routeId)}" aria-label="${label}" title="${label}" ${switchAllTargets(routeId).length === 0 ? "disabled" : ""}>
      <i data-lucide="${icon}"></i><span class="switch-all-name">${escapeHtml(name)}</span>
    </button>`;
  }).join("");
  container.innerHTML = `
    <span class="switch-all-label" title="${escapeHtml(t("dashboard.switchAllLabel"))}">${ALL_DISPLAYS_ICON}<span>${t("dashboard.switchAllLabel")}</span></span>
    <div class="switch-all-hosts">${buttons}</div>`;
}

function renderHostRoutes(shared: SharedMonitorStatus): void {
  const container = document.querySelector(`[data-host-route-grid="${cssEscape(shared.monitorKey)}"]`);
  if (!container) return;
  const selectedMonitor = selectedMonitorFor(shared);
  const activeRouteId = selectedMonitor?.activeRoute ?? "local";
  const routes = [
    { id: "local", name: routeDisplayName("local"), platform: dashboard.localHost, input: selectedMonitor?.localInput ?? null, local: true },
    ...settings.peers.map((peer) => ({
      id: peer.id, name: peer.name, platform: peer.platform,
      input: peer.inputs.find((assignment) => sameDisplay(assignment.monitor, shared.fingerprint))?.input ?? null,
      local: false,
    })),
  ].sort((left, right) => routeRank(left.id) - routeRank(right.id));
  container.innerHTML = routes.map((route, index) => {
    const isActive = route.id === activeRouteId;
    const displayName = routeDisplayName(route.id);
    const isRenaming = renaming?.routeId === route.id;
    const title = isRenaming
      ? `<input class="host-title-input" data-rename-input="${escapeHtml(route.id)}" value="${escapeHtml(renaming?.draft ?? displayName)}" placeholder="${escapeHtml(route.name)}" maxlength="${MAX_HOST_NAME_CHARS}" aria-label="${escapeHtml(t("dashboard.hostNameLabel"))}" />`
      : `<h2 class="host-title">${escapeHtml(displayName)}</h2>`;
    const badgeText = route.local
      ? (route.platform === "mac" ? t("dashboard.localMacOs") : t("dashboard.localWindowsBadge"))
      : (route.platform === "mac" ? t("dashboard.connectedMacOs") : t("dashboard.connectedWindows"));
    const inputDesc = route.input == null
      ? t("dashboard.inputUnset")
      : (route.local ? t("dashboard.currentInput", { input: escapeHtml(inputName(route.input, shared.monitorKey)) }) : t("dashboard.assignedInput", { input: escapeHtml(inputName(route.input, shared.monitorKey)) }));
    const iconName = route.platform === "mac" ? "laptop" : "computer";

    return `
      <article class="host-route-card ${route.local ? "is-local" : ""}" draggable="${isRenaming ? "false" : "true"}" data-route-card="${escapeHtml(route.id)}">
        <div class="host-card-tools ${isRenaming ? "is-hidden" : ""}" title="${escapeHtml(t("dashboard.dragToReorder"))}">
          <button type="button" class="host-tool-button" data-rename-route="${escapeHtml(route.id)}" aria-label="${escapeHtml(t("action.renameHost", { name: displayName }))}" title="${escapeHtml(t("action.renameHost", { name: displayName }))}"><i data-lucide="pencil"></i></button>
          <button type="button" class="host-tool-button" data-move-route="${escapeHtml(route.id)}" data-move-offset="-1" aria-label="${escapeHtml(t("action.moveHostEarlier", { name: displayName }))}" title="${escapeHtml(t("action.moveHostEarlier", { name: displayName }))}" ${index === 0 ? "disabled" : ""}><i data-lucide="chevron-left"></i></button>
          <button type="button" class="host-tool-button" data-move-route="${escapeHtml(route.id)}" data-move-offset="1" aria-label="${escapeHtml(t("action.moveHostLater", { name: displayName }))}" title="${escapeHtml(t("action.moveHostLater", { name: displayName }))}" ${index === routes.length - 1 ? "disabled" : ""}><i data-lucide="chevron-right"></i></button>
        </div>
        <div class="host-card-header">
          <div class="host-icon ${route.platform}">
            <i data-lucide="${iconName}"></i>
          </div>
          <div class="host-copy">
            <span class="host-label ${route.local ? "is-local" : ""}">${badgeText}</span>
            ${title}
            <p class="host-input-desc">${inputDesc}</p>
          </div>
        </div>
        ${isActive ? `
          <div class="active-route-state">
            <span class="active-route-dot" aria-hidden="true"></span>
            <span>${t("dashboard.currentlyDisplayed")}</span>
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

/** Marks a button as working until its command settles. Re-rendering replaces
 *  the element, so restoring it afterwards is harmless when that happens. */
async function withBusyButton(button: HTMLButtonElement, run: () => Promise<void>): Promise<void> {
  if (button.disabled) return;
  button.disabled = true;
  button.classList.add("is-busy");
  try {
    await run();
  } finally {
    button.disabled = false;
    button.classList.remove("is-busy");
  }
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

async function mergeSharedMonitor(aliasId: string, primaryId: string): Promise<void> {
  const primary = dashboard.shared.find((shared) => shared.monitorKey === primaryId);
  try {
    settings = await invoke<AppSettings>("set_monitor_identity_link", { aliasId, primaryId });
    await refresh();
    showToast(
      t("settings.mergeAction"),
      t("settings.mergedInto", { name: primary?.name ?? t("dashboard.sharedDisplay") }),
    );
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

/** Renames this computer. An empty name, or the discovered one, clears the
 *  custom name rather than storing a copy of the default. */
async function renameLocalHost(value: string): Promise<void> {
  const name = value.trim();
  const fallback = dashboard.localHostName ?? "";
  try {
    hostNames = await invoke<Record<string, string>>("set_host_name", {
      routeId: "local",
      name: name === fallback ? "" : name,
    });
    renderState();
    setInput("#local-host-name", routeDisplayName("local"));
  } catch (error) {
    showToast(t("toast.monitorSelectFailed"), String(error), true);
    setInput("#local-host-name", routeDisplayName("local"));
  }
}

let pendingReset: { scope: string; timer: number } | null = null;

/** Asks once, then performs. The second press within ten seconds confirms; any
 *  other press, or the timeout, puts the button back. */
function requestReset(scope: string, button: HTMLButtonElement): void {
  if (pendingReset?.scope === scope) {
    window.clearTimeout(pendingReset.timer);
    pendingReset = null;
    button.classList.remove("is-confirming");
    // Resetting restarts the agent and rewrites the settings file, so it is
    // not instant; without this the confirmed press looked like no press.
    void withBusyButton(button, () => performReset(scope));
    return;
  }
  if (pendingReset) window.clearTimeout(pendingReset.timer);
  const label = button.querySelector("small");
  const original = label?.textContent ?? "";
  if (label) label.textContent = t("settings.resetConfirm");
  button.classList.add("is-confirming");
  pendingReset = {
    scope,
    timer: window.setTimeout(() => {
      pendingReset = null;
      if (label) label.textContent = original;
      button.classList.remove("is-confirming");
    }, 10_000),
  };
}

async function performReset(scope: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("reset_settings", { scope });
    await refresh();
    renderState();
    showToast(t("settings.resetTitle"), t("settings.resetDone"));
  } catch (error) { showToast(t("settings.resetTitle"), String(error), true); }
}

async function unmergeSharedMonitor(aliasId: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("set_monitor_identity_link", { aliasId, primaryId: null });
    await refresh();
    showToast(t("settings.mergeUndo"), t("settings.mergeUndone"));
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function addPeer(peerId: string): Promise<void> {
  try {
    const sharedKey = document.querySelector<HTMLInputElement>("#shared-key")?.value ?? "";
    settings = await invoke<AppSettings>("select_peer", { peerId, sharedKey });
    const added = settings.peers.find((peer) => peer.id === peerId);
    renderState();
    const detectedPorts = (added?.inputs ?? []).map((assignment) => {
      const shared = dashboard.shared.find((item) => sameDisplay(item.fingerprint, assignment.monitor));
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

function switchProgressText(event: SwitchProgressEvent): { title: string; detail: string } {
  if (event.event === "waking") return { title: t("operation.wakingTitle", { name: event.peerName }), detail: t("operation.wakingBody") };
  if (event.event === "checking") return { title: t("operation.checkingTitle", { name: event.peerName }), detail: t("operation.checkingBody") };
  if (event.event === "waiting") return { title: t("operation.waitingTitle", { name: event.peerName }), detail: t("operation.waitingBody", { seconds: event.seconds }) };
  if (event.event === "remoteFallback") return { title: t("operation.remoteTitle", { name: event.peerName }), detail: t("operation.remoteBody") };
  return { title: t("operation.switchingTitle"), detail: t("operation.switchingBody") };
}

/**
 * Runs one backend switch and mirrors its progress in the operation dialog.
 * With a `step` label, the dialog keeps that label as its title so a batch
 * shows which display it is on.
 */
async function requestSwitch(monitorKey: string, targetId: string, step?: string): Promise<OperationResult> {
  const onEvent = new Channel<SwitchProgressEvent>();
  onEvent.onmessage = (event) => {
    const { title, detail } = switchProgressText(event);
    if (step) showOperation(step, title);
    else showOperation(title, detail);
  };
  return invoke<OperationResult>("switch_host", { monitorId: monitorKey, targetId, onEvent });
}

async function switchHost(monitorKey: string, targetId: string): Promise<void> {
  showOperation(t("operation.preparingTitle"), t("operation.preparingBody"));
  try {
    const result = await requestSwitch(monitorKey, targetId);
    showToast(result.title, result.detail, result.warning);
    await refresh();
    scheduleSettledRescans();
  } catch (error) {
    showToast(t("toast.switchFailed"), String(error), true);
  } finally {
    hideOperation();
  }
}

/** The display input a host uses on a shared display, or null when it is not configured. */
function routeInputFor(shared: SharedMonitorStatus, routeId: string): number | null {
  if (routeId === "local") return selectedMonitorFor(shared)?.localInput ?? null;
  const peer = settings.peers.find((item) => item.id === routeId);
  return peer?.inputs.find((assignment) => sameDisplay(assignment.monitor, shared.fingerprint))?.input ?? null;
}

/** Shared displays that switching everything to this host would change. */
function switchAllTargets(routeId: string): SharedMonitorStatus[] {
  return dashboard.shared.filter((shared) =>
    (selectedMonitorFor(shared)?.activeRoute ?? "local") !== routeId
    && routeInputFor(shared, routeId) != null
    && (shared.ddcAvailable || dashboard.agentConfigured));
}

/**
 * Switches every shared display to one host, one display at a time: the first
 * switch wakes a sleeping host, so later ones find it ready. A failed display
 * does not stop the rest.
 */
async function switchAllToHost(targetId: string): Promise<void> {
  const hostName = routeDisplayName(targetId);
  const targets = switchAllTargets(targetId);
  if (!targets.length) return;
  const problems: string[] = [];
  let switched = 0;
  showOperation(t("operation.preparingTitle"), t("operation.preparingBody"));
  try {
    for (const [index, shared] of targets.entries()) {
      const step = t("operation.switchAllStep", { current: index + 1, total: targets.length, name: shared.name });
      showOperation(step, t("operation.preparingBody"));
      try {
        const result = await requestSwitch(shared.monitorKey, targetId, step);
        switched += 1;
        if (result.warning) problems.push(`${shared.name}: ${result.detail}`);
      } catch (error) {
        problems.push(`${shared.name}: ${String(error)}`);
      }
    }
  } finally {
    hideOperation();
  }
  const title = switched === targets.length
    ? t("toast.switchAllDone", { count: switched, name: hostName })
    : t("toast.switchAllPartial", { count: switched, total: targets.length, name: hostName });
  showToast(switched === 0 ? t("toast.switchFailed") : title, problems.join(" "), problems.length > 0);
  await refresh();
  scheduleSettledRescans();
}

async function peerCommand(command: "probe_peer" | "wake_peer", peerId: string): Promise<void> {
  try {
    const result = await invoke<OperationResult>(command, { peerId });
    showToast(result.title, result.detail, result.warning);
    // A connection test adopts whatever input the host reported for itself.
    if (command === "probe_peer") await reloadPeerInputs();
  } catch (error) {
    showToast(command === "probe_peer" ? t("toast.probeFailed") : t("toast.wakeFailed"), String(error), true);
  }
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
/** The identity a fingerprint resolves to, following the user's merges. Mirrors
 *  `monitor_identity::primary_for`; the bound stops a malformed chain looping. */
function primaryFingerprint(fingerprint: Fingerprint): Fingerprint {
  let current = fingerprint;
  for (let step = 0; step < 8; step += 1) {
    const link = (settings.monitorIdentityLinks ?? []).find((entry) => sameFingerprint(entry.alias, current));
    if (!link?.primary || sameFingerprint(link.primary, current)) return current;
    current = link.primary;
  }
  return current;
}

/** Whether two identities name one physical display, following the user's
 *  merges. Mirrors `monitor_identity::is_same_display`: being the same display
 *  is an equivalence, so both sides are resolved — a stored selection can
 *  itself be an alias. */
function sameDisplay(left: Fingerprint, right: Fingerprint): boolean {
  return sameFingerprint(primaryFingerprint(left), primaryFingerprint(right));
}

/** Whether a display present right now is one of this computer's shared displays. */
function isSharedDisplay(fingerprint: Fingerprint): boolean {
  return settings.sharedMonitors.some((sm) => sameDisplay(sm.fingerprint, fingerprint));
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
      await listen(HOST_ORDER_CHANGED_EVENT, () => void reloadHostOrder());
      await listen(HOST_NAMES_CHANGED_EVENT, () => void reloadHostNames());
      await listen(INPUT_LABELS_CHANGED_EVENT, () => void reloadInputOptions());
      await listen(PEER_INPUTS_CHANGED_EVENT, () => void reloadPeerInputs());
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
