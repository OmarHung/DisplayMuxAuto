import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const scriptPath = join(dirname(fileURLToPath(import.meta.url)), "set-updater-notes.mjs");
const API_URL = "https://api.github.com/repos/owner/app/releases/assets/42";
const ASSETS = [
  { apiUrl: API_URL, name: "App 1.2.3-setup.exe" },
  { apiUrl: "https://api.github.com/repos/owner/app/releases/assets/43", name: "latest.json" },
];

function fixture(manifest, notes, assets = ASSETS) {
  const directory = mkdtempSync(join(tmpdir(), "displaymux-updater-notes-"));
  const manifestPath = join(directory, "latest.json");
  const notesPath = join(directory, "release-notes.md");
  const assetsPath = join(directory, "release-assets.json");
  writeFileSync(manifestPath, JSON.stringify(manifest));
  writeFileSync(notesPath, notes);
  writeFileSync(assetsPath, JSON.stringify(assets));
  return { manifestPath, notesPath, assetsPath };
}

function run({ manifestPath, notesPath, assetsPath }) {
  return spawnSync(
    process.execPath,
    [
      scriptPath,
      "--manifest", manifestPath,
      "--notes", notesPath,
      "--assets", assetsPath,
      "--tag", "v1.2.3",
      "--repository", "owner/app",
    ],
    { encoding: "utf8" },
  );
}

test("copies release notes and points packages at release download URLs", () => {
  const original = {
    version: "1.2.3",
    notes: "",
    pub_date: "2026-09-13T00:00:00.000Z",
    platforms: {
      "windows-x86_64": { url: API_URL, signature: "signature" },
      "windows-x86_64-nsis": { url: API_URL, signature: "signature" },
    },
  };
  const paths = fixture(original, "# Release\n\nFixed update notes.\n");

  const result = run(paths);

  assert.equal(result.status, 0, result.stderr);
  const url = "https://github.com/owner/app/releases/download/v1.2.3/App%201.2.3-setup.exe";
  assert.deepEqual(JSON.parse(readFileSync(paths.manifestPath, "utf8")), {
    ...original,
    notes: "# Release\n\nFixed update notes.",
    platforms: {
      "windows-x86_64": { url, signature: "signature" },
      "windows-x86_64-nsis": { url, signature: "signature" },
    },
  });
});

test("rejects a package that is not one of the release assets", () => {
  const paths = fixture(
    { version: "1.2.3", platforms: { "darwin-aarch64": { url: "https://example.test/app.tar.gz" } } },
    "Release notes",
  );

  const result = run(paths);

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /darwin-aarch64 is not a release asset/);
});

test("rejects a manifest without platform packages", () => {
  const paths = fixture({ version: "1.2.3", platforms: {} }, "Release notes");

  const result = run(paths);

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /does not contain any platform packages/);
});

test("rejects empty release notes", () => {
  const paths = fixture(
    { version: "1.2.3", platforms: { "darwin-aarch64": { url: API_URL } } },
    "  \n",
  );

  const result = run(paths);

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Release notes are empty/);
});
