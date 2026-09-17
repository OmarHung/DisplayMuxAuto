import { getCurrentWindow } from "@tauri-apps/api/window";

export type ThemePreference = "system" | "light" | "dark";
type ResolvedTheme = Exclude<ThemePreference, "system">;

const themeStorageKey = "displaymux.theme";
const darkSchemeQuery = "(prefers-color-scheme: dark)";

function isThemePreference(value: string | null): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

export function readThemePreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(themeStorageKey);
    return isThemePreference(stored) ? stored : "system";
  } catch {
    return "system";
  }
}

function resolveTheme(preference: ThemePreference): ResolvedTheme {
  if (preference !== "system") return preference;
  return window.matchMedia(darkSchemeQuery).matches ? "dark" : "light";
}

function applyResolvedTheme(preference: ThemePreference): void {
  document.documentElement.dataset.theme = resolveTheme(preference);
}

function syncNativeWindowTheme(preference: ThemePreference): void {
  // The page's own colours are already set by the time this runs, and the
  // title bar is the only thing left. getCurrentWindow() throws outside a Tauri
  // window rather than rejecting, and this is called while the module is still
  // evaluating — unguarded, it takes the whole interface down over a title bar.
  try {
    // null lets the native title bar follow the OS again.
    getCurrentWindow()
      .setTheme(preference === "system" ? null : preference)
      .catch((error: unknown) => console.warn("Unable to sync the window theme", error));
  } catch (error) {
    console.warn("Unable to reach the window to sync its theme", error);
  }
}

let currentPreference: ThemePreference = "system";

/** Applies the stored preference and keeps "system" in step with the OS. */
export function initializeTheme(): ThemePreference {
  currentPreference = readThemePreference();
  applyResolvedTheme(currentPreference);
  syncNativeWindowTheme(currentPreference);
  window.matchMedia(darkSchemeQuery).addEventListener("change", () => {
    if (currentPreference === "system") applyResolvedTheme(currentPreference);
  });
  return currentPreference;
}

export function setThemePreference(preference: string): boolean {
  if (!isThemePreference(preference)) return false;
  try {
    if (preference === "system") localStorage.removeItem(themeStorageKey);
    else localStorage.setItem(themeStorageKey, preference);
  } catch {
    return false;
  }
  currentPreference = preference;
  applyResolvedTheme(preference);
  syncNativeWindowTheme(preference);
  return true;
}
