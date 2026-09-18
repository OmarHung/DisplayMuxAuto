import { invoke } from "@tauri-apps/api/core";
import { t } from "./i18n";

/** The two settings this module owns; the backend changes them only through
 *  `set_diagnostics_consent`, never through the settings form. */
export interface DiagnosticsSettings {
  onboardingCompleted: boolean;
  diagnosticsEnabled?: boolean;
  diagnosticsAsked?: boolean;
}

interface DiagnosticsStatus { uploadAvailable: boolean; }
interface DiagnosticPreview { reportId: string; preview: string; }
interface OperationResult { title: string; detail: string; }

/** What the diagnostics UI needs from the page that hosts it. */
export interface DiagnosticsHost<S extends DiagnosticsSettings> {
  settings(): S;
  adoptSettings(settings: S): void;
  isPreview(): boolean;
  notify(title: string, detail: string, warning?: boolean): void;
  withBusyButton(button: HTMLButtonElement, run: () => Promise<void>): Promise<void>;
}

export function diagnosticsSectionHtml(): string {
  return `
    <div class="form-section diagnostics-section">
      <div class="pairing-heading"><strong>${t("diagnostics.title")}</strong></div>
      <div class="toggles-section">
        <label class="switch-row">
          <span class="switch-label">
            <strong>${t("diagnostics.allow")}</strong>
            <small id="diagnostics-hint"></small>
          </span>
          <input id="diagnostics-enabled" type="checkbox" class="toggle-checkbox" />
          <span class="switch-slider"></span>
        </label>
      </div>
      <div class="diagnostics-actions">
        <p>${t("diagnostics.reportIntro")}</p>
        <button type="button" class="scan-button" id="diagnostics-prepare">${t("diagnostics.prepare")}</button>
      </div>
    </div>`;
}

export function diagnosticsDialogsHtml(): string {
  return `
  <div class="update-overlay" id="diagnostics-consent-overlay" aria-hidden="true">
    <div class="update-dialog diagnostics-dialog" role="dialog" aria-modal="true" aria-labelledby="diagnostics-consent-title">
      <p class="section-kicker">DIAGNOSTICS</p>
      <h2 id="diagnostics-consent-title">${t("diagnostics.consentTitle")}</h2>
      <p>${t("diagnostics.consentBody")}</p>
      <div class="diagnostics-scope">
        <div><strong>${t("diagnostics.collectedTitle")}</strong><p>${t("diagnostics.collectedBody")}</p></div>
        <div><strong>${t("diagnostics.excludedTitle")}</strong><p>${t("diagnostics.excludedBody")}</p></div>
      </div>
      <p>${t("diagnostics.consentLater")}</p>
      <div class="update-actions">
        <button class="scan-button" id="diagnostics-decline" type="button">${t("diagnostics.decline")}</button>
        <button class="save-button" id="diagnostics-accept" type="button">${t("diagnostics.accept")}</button>
      </div>
    </div>
  </div>
  <div class="update-overlay" id="diagnostics-preview-overlay" aria-hidden="true">
    <div class="update-dialog diagnostics-dialog" role="dialog" aria-modal="true" aria-labelledby="diagnostics-preview-title">
      <p class="section-kicker">DIAGNOSTIC REPORT</p>
      <h2 id="diagnostics-preview-title">${t("diagnostics.previewTitle")}</h2>
      <p id="diagnostics-preview-intro"></p>
      <pre class="update-notes diagnostics-preview" id="diagnostics-preview"></pre>
      <div class="update-actions">
        <button class="scan-button" id="diagnostics-cancel" type="button">${t("diagnostics.cancel")}</button>
        <button class="scan-button" id="diagnostics-save" type="button">${t("diagnostics.save")}</button>
        <button class="save-button" id="diagnostics-send" type="button">${t("diagnostics.send")}</button>
      </div>
    </div>
  </div>`;
}

function setOverlay(selector: string, visible: boolean): void {
  const overlay = document.querySelector(selector);
  overlay?.classList.toggle("is-visible", visible);
  overlay?.setAttribute("aria-hidden", String(!visible));
}

/** Wires the consent question, the settings toggle and the report preview.
 *  Call once, after the page's markup is in place. */
export function initDiagnostics<S extends DiagnosticsSettings>(host: DiagnosticsHost<S>) {
  let uploadAvailable = false;
  let pendingReportId: string | null = null;

  const toggle = () => document.querySelector<HTMLInputElement>("#diagnostics-enabled");

  function render(): void {
    const input = toggle();
    if (input) input.checked = host.settings().diagnosticsEnabled === true;
    const hint = document.querySelector("#diagnostics-hint");
    if (hint) hint.textContent = uploadAvailable ? t("diagnostics.allowHint") : t("diagnostics.allowHintNoUpload");
    const send = document.querySelector<HTMLButtonElement>("#diagnostics-send");
    if (send) send.hidden = !uploadAvailable;
  }

  async function setConsent(enabled: boolean): Promise<void> {
    if (host.isPreview()) {
      host.adoptSettings({ ...host.settings(), diagnosticsEnabled: enabled, diagnosticsAsked: true });
    } else {
      host.adoptSettings(await invoke<S>("set_diagnostics_consent", { enabled }));
    }
    render();
  }

  const busy = (event: Event, run: () => Promise<void>) =>
    void host.withBusyButton(event.currentTarget as HTMLButtonElement, run);

  function answer(enabled: boolean): void {
    setOverlay("#diagnostics-consent-overlay", false);
    setConsent(enabled).catch((error) => host.notify(t("diagnostics.consentFailed"), String(error), true));
  }

  /** Puts the question once, to a user who has finished the tour. */
  function askIfUnasked(): void {
    const current = host.settings();
    if (host.isPreview() || !current.onboardingCompleted || current.diagnosticsAsked) return;
    setOverlay("#diagnostics-consent-overlay", true);
    document.querySelector<HTMLButtonElement>("#diagnostics-decline")?.focus();
  }

  async function prepare(): Promise<void> {
    if (host.isPreview()) { host.notify(t("diagnostics.prepare"), t("dashboard.preview")); return; }
    try {
      const report = await invoke<DiagnosticPreview>("prepare_diagnostic_report");
      pendingReportId = report.reportId;
      const preview = document.querySelector("#diagnostics-preview");
      if (preview) preview.textContent = report.preview;
      const intro = document.querySelector("#diagnostics-preview-intro");
      if (intro) intro.textContent = uploadAvailable ? t("diagnostics.previewIntro") : t("diagnostics.previewIntroNoUpload");
      render();
      setOverlay("#diagnostics-preview-overlay", true);
    } catch (error) {
      host.notify(t("diagnostics.prepareFailed"), String(error), true);
    }
  }

  function closePreview(): void {
    setOverlay("#diagnostics-preview-overlay", false);
    pendingReportId = null;
  }

  async function send(): Promise<void> {
    if (!pendingReportId) return;
    try {
      const result = await invoke<OperationResult>("send_diagnostic_report", { reportId: pendingReportId });
      closePreview();
      host.notify(result.title, result.detail);
    } catch (error) {
      host.notify(t("diagnostics.sendFailed"), String(error), true);
    }
  }

  async function save(): Promise<void> {
    if (!pendingReportId) return;
    try {
      const path = await invoke<string>("save_diagnostic_report", { reportId: pendingReportId });
      closePreview();
      host.notify(t("diagnostics.saved"), path);
    } catch (error) {
      host.notify(t("diagnostics.saveFailed"), String(error), true);
    }
  }

  document.querySelector("#diagnostics-accept")?.addEventListener("click", () => answer(true));
  document.querySelector("#diagnostics-decline")?.addEventListener("click", () => answer(false));
  toggle()?.addEventListener("change", (event) => {
    const input = event.target as HTMLInputElement;
    setConsent(input.checked).catch((error) => {
      input.checked = !input.checked;
      host.notify(t("diagnostics.consentFailed"), String(error), true);
    });
  });
  document.querySelector("#diagnostics-prepare")?.addEventListener("click", (event) => busy(event, prepare));
  document.querySelector("#diagnostics-cancel")?.addEventListener("click", closePreview);
  document.querySelector("#diagnostics-send")?.addEventListener("click", (event) => busy(event, send));
  document.querySelector("#diagnostics-save")?.addEventListener("click", (event) => busy(event, save));

  invoke<DiagnosticsStatus>("diagnostics_status")
    .then((status) => { uploadAvailable = status.uploadAvailable; render(); })
    .catch(() => { /* Preview mode has no Tauri backend. */ });

  return { render, askIfUnasked };
}
