import en from "./locales/en";
import zhTW from "./locales/zh-TW";

export type AppLocale = "en" | "zh-TW";
export type LocalePreference = "system" | AppLocale;
export type MessageKey = keyof typeof en;
type Parameters = Record<string, string | number>;

const localeStorageKey = "muxsu.locale";

function detectLocale(languageTags: readonly string[]): AppLocale {
  for (const tag of languageTags) {
    const normalized = tag.toLowerCase();
    if (normalized.startsWith("zh-tw") || normalized.startsWith("zh-hant") ||
      normalized.startsWith("zh-hk") || normalized.startsWith("zh-mo")) return "zh-TW";
    if (normalized.startsWith("en") || normalized.startsWith("zh")) return "en";
  }
  return "en";
}

export const systemLocale = detectLocale(
  typeof navigator === "undefined" ? [] : (navigator.languages.length ? navigator.languages : [navigator.language]),
);

function readLocalePreference(): LocalePreference {
  if (typeof localStorage === "undefined") return "system";
  try {
    const stored = localStorage.getItem(localeStorageKey);
    return stored === "en" || stored === "zh-TW" ? stored : "system";
  } catch {
    return "system";
  }
}

export const localePreference = readLocalePreference();
export const locale: AppLocale = localePreference === "system" ? systemLocale : localePreference;

export function setLocalePreference(preference: string): boolean {
  if (preference !== "system" && preference !== "en" && preference !== "zh-TW") return false;
  if (typeof localStorage !== "undefined") {
    try {
      if (preference === "system") localStorage.removeItem(localeStorageKey);
      else localStorage.setItem(localeStorageKey, preference);
    } catch {
      return false;
    }
  }
  return true;
}

const messages: Record<AppLocale, Record<MessageKey, string>> = { en, "zh-TW": zhTW };

export function t(key: MessageKey, parameters: Parameters = {}): string {
  const message = messages[locale][key] ?? en[key];
  return message.replace(/\{(\w+)\}/g, (placeholder, name: string) =>
    Object.prototype.hasOwnProperty.call(parameters, name) ? String(parameters[name]) : placeholder,
  );
}

export const __test__ = { detectLocale };
