import { readFileSync, writeFileSync } from "node:fs";

function argument(name) {
  const index = process.argv.indexOf(name);
  if (index < 0 || !process.argv[index + 1]) throw new Error(`Missing ${name}`);
  return process.argv[index + 1];
}

const manifestPath = argument("--manifest");
const notesPath = argument("--notes");
const assetsPath = argument("--assets");
const tag = argument("--tag");
const repository = argument("--repository");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const notes = readFileSync(notesPath, "utf8").trim();
const assets = JSON.parse(readFileSync(assetsPath, "utf8"));

if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
  throw new Error("Updater manifest must be a JSON object");
}
if (typeof manifest.version !== "string" || !manifest.version.trim()) {
  throw new Error("Updater manifest version is missing");
}
if (!manifest.platforms || typeof manifest.platforms !== "object" || Array.isArray(manifest.platforms)) {
  throw new Error("Updater manifest platforms are missing");
}
if (Object.keys(manifest.platforms).length === 0) {
  throw new Error("Updater manifest does not contain any platform packages");
}
if (!notes) {
  throw new Error("Release notes are empty");
}

if (!Array.isArray(assets)) {
  throw new Error("Release assets must be a JSON array");
}

// tauri-action points each package at its api.github.com asset URL. Those
// count against GitHub's unauthenticated API limit of 60 requests an hour
// per IP, so on a shared office connection the download fails with
// 403 Forbidden. The release's own download URL has no such limit.
const assetNameByApiUrl = new Map(assets.map((asset) => [asset.apiUrl, asset.name]));
const downloadBase = `https://github.com/${repository}/releases/download/${encodeURIComponent(tag)}`;
for (const [platform, entry] of Object.entries(manifest.platforms)) {
  const name = assetNameByApiUrl.get(entry?.url);
  if (!name) throw new Error(`Updater package for ${platform} is not a release asset: ${entry?.url}`);
  entry.url = `${downloadBase}/${encodeURIComponent(name)}`;
}

manifest.notes = notes;
writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
