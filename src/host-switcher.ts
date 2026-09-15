import "@fontsource-variable/manrope";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { locale, t } from "./i18n";
import "./host-switcher.css";

type Platform = "windows" | "mac";
type SwitchProgressEvent =
  | { event: "waking"; peerName: string }
  | { event: "checking"; peerName: string }
  | { event: "waiting"; peerName: string; seconds: number }
  | { event: "switching" }
  | { event: "remoteFallback"; peerName: string };

interface HostOption {
  id: string;
  name: string;
  platform: Platform;
  inputName: string | null;
  isLocal: boolean;
  available: boolean;
}

interface HostSwitcherMonitor {
  monitorKey: string;
  name: string;
  hosts: HostOption[];
}

interface HostSwitcherState {
  monitors: HostSwitcherMonitor[];
}

interface OperationResult {
  title: string;
  detail: string;
}

type Row = { kind: "header"; monitorName: string } | { kind: "host"; monitorKey: string; host: HostOption };

const root = document.querySelector<HTMLElement>("#host-switcher-app")!;
if (!root) throw new Error("DisplayMux host switcher root was not found");

let state: HostSwitcherState = { monitors: [] };
let rows: Row[] = [];
let selectedIndex = 0;
let switching = false;
/** Emitted by the backend when this or a paired host saves a new host card order. */
const HOST_ORDER_CHANGED_EVENT = "host-order-changed";
/** Emitted by the backend when this or a paired host renames a host. */
const HOST_NAMES_CHANGED_EVENT = "host-names-changed";

function escapeHtml(value: string): string {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", "\"": "&quot;",
  })[character] ?? character);
}

function platformLabel(platform: Platform): string {
  return platform === "mac" ? "macOS" : "Windows";
}

function computeRows(): Row[] {
  const showHeaders = state.monitors.length > 1;
  return state.monitors.flatMap((monitor) => [
    ...(showHeaders ? [{ kind: "header" as const, monitorName: monitor.name }] : []),
    ...monitor.hosts.map((host) => ({ kind: "host" as const, monitorKey: monitor.monitorKey, host })),
  ]);
}

function isSelectableRow(row: Row | undefined): row is Extract<Row, { kind: "host" }> {
  return row?.kind === "host" && row.host.available;
}

function render(message?: { title: string; detail: string; error?: boolean }): void {
  const headerLine = state.monitors.length
    ? state.monitors.map((monitor) => monitor.name).join(" · ")
    : t("switcher.noDisplay");
  root.innerHTML = `
    <main class="switcher-shell" aria-labelledby="switcher-title">
      <header class="switcher-header">
        <div>
          <p class="eyebrow">DISPLAYMUX</p>
          <h1 id="switcher-title">${t("switcher.title")}</h1>
          <p>${escapeHtml(headerLine)}</p>
        </div>
      </header>
      <section class="host-list" role="listbox" aria-label="${t("switcher.hostListAria")}">
        ${rows.map((row, index) => row.kind === "header"
          ? `<div class="host-group-header">${escapeHtml(row.monitorName)}</div>`
          : `<button type="button" class="host-option ${index === selectedIndex ? "is-selected" : ""}"
            data-row-index="${index}" role="option" aria-selected="${index === selectedIndex}"
            ${row.host.available && !switching ? "" : "disabled"}>
            <span class="platform-mark ${row.host.platform}">${row.host.platform === "mac" ? "M" : "W"}</span>
            <span class="host-copy">
              <strong>${escapeHtml(row.host.name)}</strong>
              <small>${platformLabel(row.host.platform)} · ${escapeHtml(row.host.inputName ?? t("switcher.inputUnset"))}</small>
            </span>
            <span class="host-status">${row.host.isLocal ? t("switcher.local") : t("switcher.select")}</span>
          </button>`).join("") || `<p class="empty-state">${t("switcher.noHosts")}</p>`}
      </section>
      ${message ? `<div class="switch-message ${message.error ? "is-error" : ""}" role="status"><strong>${escapeHtml(message.title)}</strong><span>${escapeHtml(message.detail)}</span></div>` : ""}
      <footer>
        <span>${t("switcher.navigationHint")}</span>
        <span>${t("switcher.closeHint")}</span>
      </footer>
    </main>`;
}

function nextAvailableIndex(direction: 1 | -1): number {
  if (!rows.some((row) => isSelectableRow(row))) return selectedIndex;
  let candidate = selectedIndex;
  do {
    candidate = (candidate + direction + rows.length) % rows.length;
  } while (!isSelectableRow(rows[candidate]));
  return candidate;
}

function selectIndex(index: number): void {
  if (!isSelectableRow(rows[index]) || switching) return;
  selectedIndex = index;
  render();
  document.querySelector<HTMLElement>(`[data-row-index="${index}"]`)?.focus();
}

async function hideSwitcher(): Promise<void> {
  try { await invoke("hide_host_switcher"); } catch { window.close(); }
}

async function switchToSelected(): Promise<void> {
  const row = rows[selectedIndex];
  if (!isSelectableRow(row) || switching) return;
  switching = true;
  render({ title: t("switcher.preparing"), detail: t("switcher.preparingDetail") });
  const onEvent = new Channel<SwitchProgressEvent>();
  onEvent.onmessage = (event) => {
    const detail = event.event === "waking"
      ? t("switcher.waking", { name: event.peerName })
      : event.event === "waiting"
        ? t("switcher.waiting", { name: event.peerName, seconds: event.seconds })
        : event.event === "remoteFallback"
          ? t("switcher.remoteFallback", { name: event.peerName })
          : t("switcher.switching");
    render({ title: t("switcher.preparing"), detail });
  };
  try {
    const result = await invoke<OperationResult>("switch_host", { monitorId: row.monitorKey, targetId: row.host.id, onEvent });
    render({ title: result.title, detail: result.detail });
    window.setTimeout(() => void hideSwitcher(), 450);
  } catch (error) {
    switching = false;
    render({ title: t("switcher.failed"), detail: String(error), error: true });
  }
}

root.addEventListener("click", (event) => {
  const option = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-row-index]");
  if (!option) return;
  const index = Number(option.dataset.rowIndex);
  if (Number.isInteger(index)) {
    selectedIndex = index;
    void switchToSelected();
  }
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    void hideSwitcher();
    return;
  }
  if (switching) return;
  if (event.key === "Tab" || event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const backwards = event.key === "ArrowUp" || (event.key === "Tab" && event.shiftKey);
    selectIndex(nextAvailableIndex(backwards ? -1 : 1));
  } else if (event.key === "Enter") {
    event.preventDefault();
    void switchToSelected();
  }
});

async function initialize(): Promise<void> {
  try {
    await invoke("set_locale", { locale });
    state = await invoke<HostSwitcherState>("get_host_switcher_state");
  } catch {
    state = {
      monitors: [{
        monitorKey: "preview",
        name: t("switcher.previewDisplay"),
        hosts: [
          { id: "local", name: t("switcher.previewWindows"), platform: "windows", inputName: "HDMI 1", isLocal: true, available: true },
          { id: "peer", name: t("switcher.previewMac"), platform: "mac", inputName: "DisplayPort", isLocal: false, available: true },
        ],
      }],
    };
  }
  rows = computeRows();
  selectedIndex = Math.max(0, rows.findIndex((row) => isSelectableRow(row)));
  render();
  try {
    await listen(HOST_ORDER_CHANGED_EVENT, () => void reloadState());
    await listen(HOST_NAMES_CHANGED_EVENT, () => void reloadState());
  } catch {
    // Preview mode has no Tauri backend; the static preview order never changes.
    return;
  }
  // The window is hidden rather than closed, so re-read hosts each time it opens.
  window.addEventListener("focus", () => void reloadState());
}

/** Re-reads hosts and their order, keeping the same host selected. */
async function reloadState(): Promise<void> {
  if (switching) return;
  const selected = rows[selectedIndex];
  let latest: HostSwitcherState;
  try {
    latest = await invoke<HostSwitcherState>("get_host_switcher_state");
  } catch {
    // Keep showing the previous hosts; the next time the switcher opens it retries.
    return;
  }
  state = latest;
  rows = computeRows();
  const kept = selected?.kind === "host"
    ? rows.findIndex((row) => row.kind === "host" && row.monitorKey === selected.monitorKey && row.host.id === selected.host.id)
    : -1;
  selectedIndex = kept !== -1 ? kept : Math.max(0, rows.findIndex((row) => isSelectableRow(row)));
  render();
}

void initialize();
