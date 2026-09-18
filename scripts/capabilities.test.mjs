import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(join(root, path), "utf8");

/** Commands registered with `generate_handler!`. */
function registeredCommands() {
  const handler = read("src-tauri/src/lib.rs").match(/generate_handler!\[([^\]]*)\]/);
  assert.ok(handler, "generate_handler! not found in src-tauri/src/lib.rs");
  return new Set(handler[1].split(",").map((name) => name.trim()).filter(Boolean));
}

/** Commands build.rs generates `allow-*` permissions for. */
function manifestCommands() {
  const list = read("src-tauri/build.rs").match(/const COMMANDS: &\[&str\] = &\[([^\]]*)\]/);
  assert.ok(list, "COMMANDS not found in src-tauri/build.rs");
  return new Set([...list[1].matchAll(/"([a-z_]+)"/g)].map((match) => match[1]));
}

/** Every local script a page loads, following relative imports. */
function scriptsLoadedBy(page) {
  const entries = [...read(page).matchAll(/<script[^>]*src="\/([^"]+)"/g)].map((match) => match[1]);
  const seen = new Set();
  const visit = (path) => {
    if (seen.has(path)) return;
    seen.add(path);
    // Only extensionless specifiers are scripts; `../package.json` is data.
    for (const [, specifier] of read(path).matchAll(/from\s+"(\.{1,2}\/[^".]+)"/g)) {
      visit(join(dirname(path), `${specifier}.ts`));
    }
  };
  entries.forEach(visit);
  return [...seen];
}

/** Registered commands a page's scripts name, as string literals. */
function commandsInvokedBy(page, registered) {
  const invoked = new Set();
  for (const script of scriptsLoadedBy(page)) {
    for (const [, name] of read(script).matchAll(/"([a-z_]+)"/g)) {
      if (registered.has(name)) invoked.add(name);
    }
  }
  return invoked;
}

/** App commands a window's capabilities allow. */
function commandsGrantedTo(label) {
  const granted = new Set();
  for (const file of readdirSync(join(root, "src-tauri/capabilities"))) {
    const capability = JSON.parse(read(`src-tauri/capabilities/${file}`));
    if (!capability.windows.includes(label)) continue;
    for (const permission of capability.permissions) {
      const identifier = typeof permission === "string" ? permission : permission.identifier;
      if (identifier.startsWith("allow-")) granted.add(identifier.slice(6).replaceAll("-", "_"));
    }
  }
  return granted;
}

const sorted = (set) => [...set].sort();

test("every registered command has a permission to grant", () => {
  assert.deepEqual(sorted(manifestCommands()), sorted(registeredCommands()));
});

for (const window of JSON.parse(read("src-tauri/tauri.conf.json")).app.windows) {
  const page = window.url ?? "index.html";
  test(`the ${window.label} window is granted exactly the commands it calls`, () => {
    const invoked = commandsInvokedBy(page, registeredCommands());
    assert.ok(invoked.size > 0, `no commands found for ${page}`);
    assert.deepEqual(sorted(commandsGrantedTo(window.label)), sorted(invoked));
  });
}

/** Core permissions a window's capabilities grant, as written. */
function corePermissionsGrantedTo(label) {
  const granted = new Set();
  for (const file of readdirSync(join(root, "src-tauri/capabilities"))) {
    const capability = JSON.parse(read(`src-tauri/capabilities/${file}`));
    if (!capability.windows.includes(label)) continue;
    for (const permission of capability.permissions) {
      const identifier = typeof permission === "string" ? permission : permission.identifier;
      if (identifier.startsWith("core:")) granted.add(identifier);
    }
  }
  return granted;
}

// `core:default` would also let it emit events to the main window and drive the
// tray and menus. The switcher only listens for the backend's change events.
test("the host-switcher window is granted no core capability beyond listening", () => {
  assert.deepEqual(sorted(corePermissionsGrantedTo("host-switcher")), [
    "core:event:allow-listen",
    "core:event:allow-unlisten",
  ]);
});
