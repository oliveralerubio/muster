// Regression tests for the panel's JavaScript, run by `cargo test` through
// tests/test_web_assets.rs (and usable directly: `node tests/js/app.test.mjs`).
//
// It loads src/web/assets/app.js in a `vm` sandbox with a minimal DOM stub
// seeded from index.html, then drives the profile dialog and the MCP form:
//   - Save is enabled only after a successful preview of the exact current
//     payload, and any edit disables it again (QA round-1 finding 4).
//   - List entries are one per line, so values containing commas survive the
//     edit → preview → save round trip losslessly (QA round-1 finding 5).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const appJsPath = process.env.APP_JS;
assert.ok(appJsPath, "APP_JS must point at src/web/assets/app.js");
const appSource = readFileSync(appJsPath, "utf8");

// --- minimal DOM -------------------------------------------------------------

const allElements = [];

class FakeElement {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.attributes = {};
    this.listeners = {};
    this.dataset = {};
    this.value = "";
    this.textContent = "";
    this.hidden = false;
    this.disabled = false;
    this.readOnly = false;
    this.checked = false;
    this.tabIndex = 0;
    this.open = false;
    allElements.push(this);
  }
  setAttribute(name, v) {
    this.attributes[name] = String(v);
    if (name === "id") this.id = String(v);
  }
  getAttribute(name) {
    return this.attributes[name] ?? null;
  }
  append(...kids) {
    for (const kid of kids) if (kid !== null && kid !== undefined) this.children.push(kid);
  }
  replaceChildren(...kids) {
    this.children = kids.filter((k) => k !== null && k !== undefined);
  }
  addEventListener(type, fn) {
    (this.listeners[type] ??= []).push(fn);
  }
  fire(type, event = {}) {
    for (const fn of this.listeners[type] ?? []) fn(event);
  }
  focus() {}
  showModal() {
    this.open = true;
  }
  close() {
    this.open = false;
    this.fire("close");
  }
  reset() {
    const clear = (node) => {
      for (const kid of node.children ?? []) {
        if (kid instanceof FakeElement) {
          if ("value" in kid) kid.value = "";
          if ("checked" in kid) kid.checked = false;
          clear(kid);
        }
      }
    };
    clear(this);
  }
  querySelector(selector) {
    const wanted = selector.split(",").map((s) => s.trim().toLowerCase());
    const walk = (node) => {
      for (const kid of node.children ?? []) {
        if (!(kid instanceof FakeElement)) continue;
        const tag = kid.tagName.toLowerCase();
        if (wanted.some((w) => w === tag || w === `#${kid.id}`)) return kid;
        const found = walk(kid);
        if (found) return found;
      }
      return null;
    };
    return walk(this);
  }
}

const byId = (id) => {
  // Views re-render and rebuild their forms: the newest element with an id is
  // the one in the live DOM.
  for (let i = allElements.length - 1; i >= 0; i--) {
    if (allElements[i].id === id) return allElements[i];
  }
  return undefined;
};
const need = (id) => {
  const el = byId(id);
  assert.ok(el, `seed is missing #${id} (index.html changed?)`);
  return el;
};

function seed(tag, id, parent) {
  const el = new FakeElement(tag);
  if (id) el.setAttribute("id", id);
  if (parent) parent.children.push(el);
  return el;
}

// Static structure of index.html (the parts app.js touches).
const view = seed("div", "view");
const actions = seed("div", "actions");
seed("code", "cli");
seed("h1", "title");
const nav = seed("nav", "nav");
const tabs = [];
for (const name of ["dashboard", "machines", "team", "secrets", "skills", "profiles", "history", "doctor"]) {
  const tab = seed("button", null, nav);
  tab.setAttribute("role", "tab");
  tab.dataset.view = name;
  tabs.push(tab);
}
seed("span", "skills-dot");
const theme = seed("button", "theme");
const revealDialog = seed("dialog", "reveal");
seed("code", "reveal-name", revealDialog);
seed("output", "reveal-value", revealDialog);
seed("button", "reveal-confirm", revealDialog);
const secretDialog = seed("dialog", "secret-edit");
const secretForm = seed("form", "secret-form", secretDialog);
seed("h2", "secret-edit-title", secretDialog);
seed("input", "secret-path", secretForm);
seed("input", "secret-input", secretForm);
seed("input", "secret-confirm", secretForm);
seed("button", "secret-save", secretForm);
seed("button", "secret-cancel", secretForm);
seed("p", "secret-error", secretForm);
const profileDialog = seed("dialog", "profile-edit");
const profileForm = seed("form", "profile-form", profileDialog);
seed("h2", "profile-edit-title", profileDialog);
seed("input", "profile-name", profileForm);
seed("textarea", "profile-toml", profileForm);
seed("button", "profile-preview", profileForm);
const profileSave = seed("button", "profile-save", profileForm);
profileSave.disabled = true; // index.html ships the Save button disabled
seed("button", "profile-cancel", profileForm);
seed("pre", "profile-diff", profileDialog);
seed("p", "profile-error", profileDialog);

// --- browser globals ----------------------------------------------------------

const calls = [];
let respond = () => ({ ok: true, result: {}, diagnostics: [], cli: "muster" });
const setResponder = (fn) => {
  respond = fn;
};

const sandbox = {
  Node: FakeElement,
  console,
  setTimeout,
  location: { search: "?token=seed" },
  history: { replaceState() {} },
  localStorage: {
    store: new Map(),
    getItem(k) {
      return this.store.get(k) ?? null;
    },
    setItem(k, v) {
      this.store.set(k, v);
    },
    removeItem(k) {
      this.store.delete(k);
    },
  },
  confirm() {
    return true;
  },
  alert() {},
  async fetch(path, options) {
    const body = options && options.body ? JSON.parse(options.body) : undefined;
    const call = { path: path.replace(/^\/api\//, ""), method: (options && options.method) || "GET", body };
    calls.push(call);
    return { ok: true, json: async () => respond(call) };
  },
  document: {
    activeElement: null,
    documentElement: { dataset: {} },
    addEventListener() {},
    createElement: (tag) => new FakeElement(tag),
    querySelector(selector) {
      const id = selector.match(/^#([a-z-]+)$/);
      if (id) return byId(id[1]) ?? null;
      if (selector === "#nav [role=tab]") return nav.children[0] ?? null;
      return null;
    },
    querySelectorAll(selector) {
      if (selector === "#nav [role=tab]") return tabs;
      return [];
    },
  },
};
vm.createContext(sandbox);
vm.runInContext(appSource, sandbox, { filename: "app.js" });

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
// Let the load-time dashboard view finish before any test observes calls.
await flush();
const lastCall = () => calls[calls.length - 1];
const baseline = () => calls.length;
const callSince = (mark, path) => calls.slice(mark).find((c) => c.path === path);

// --- tests --------------------------------------------------------------------

async function testProfileSaveIsGatedOnPreview() {
  // Open the new-profile dialog the way the panel does (initially disabled),
  // preview, edit → Save must lock again until re-previewed.
  const save = need("profile-save");
  need("profile-name").value = "dev";
  need("profile-toml").value = '[agents.models]\nclaude = "sonnet"\n';
  assert.equal(save.disabled, true, "Save must start disabled before any preview");

  setResponder(() => ({ ok: true, result: { diff: "+[agents.models]\n" }, diagnostics: [] }));
  const mark = baseline();
  need("profile-preview").fire("click", { preventDefault() {} });
  await flush();
  assert.equal(save.disabled, false, "a successful preview enables Save");
  const previewCall = callSince(mark, "profile/set");
  assert.ok(previewCall, "the preview posts profile/set");
  assert.equal(previewCall.body.dry_run, true);
  assert.equal(previewCall.body.toml, '[agents.models]\nclaude = "sonnet"\n');

  // Any edit after the preview invalidates it.
  need("profile-toml").fire("input");
  assert.equal(save.disabled, true, "an edit after preview disables Save");
  need("profile-name").fire("input");
  assert.equal(save.disabled, true);

  // Re-previewing the (edited) payload enables Save again.
  need("profile-preview").fire("click", { preventDefault() {} });
  await flush();
  assert.equal(save.disabled, false, "re-preview enables Save again");

  // A failed preview must never enable Save.
  setResponder(() => ({ ok: false, error: "invalid profile", diagnostics: [] }));
  need("profile-preview").fire("click", { preventDefault() {} });
  await flush();
  assert.equal(save.disabled, true, "a failed preview keeps Save disabled");
  setResponder();
}

async function testMcpFormUsesOneEntryPerLine() {
  // Render the profiles view so the MCP form exists.
  tabs.find((t) => t.dataset.view === "profiles").fire("click");
  await flush();

  const save = need("mcp-save");
  assert.equal(save.disabled, true, "MCP Save must start disabled before any preview");

  // Comma-containing values, one entry per line (QA finding 5): the payload
  // must keep them verbatim instead of splitting on commas.
  need("mcp-name").value = "context";
  need("mcp-command").value = "npx";
  need("mcp-args").value = "-y\n--cache=/tmp/a,b\nserver-name";
  need("mcp-env").value = "MSG=hello, world\nAPI_KEY=native://app/api-key";
  need("mcp-header").value = "Authorization=native://app/token";

  setResponder(() => ({ ok: true, result: { diff: "+command = \"npx\"\n" }, diagnostics: [] }));
  const previewButtons = allElements.filter(
    (el) => el.tagName === "BUTTON" && el.children[0] === "Preview diff",
  );
  assert.equal(previewButtons.length, 1, "the MCP form has one Preview diff button");
  const mark = baseline();
  previewButtons[0].fire("click");
  await flush();

  const previewCall = callSince(mark, "mcp-server/add");
  assert.ok(previewCall, "the preview posts mcp-server/add");
  assert.equal(previewCall.body.dry_run, true);
  assert.deepEqual(previewCall.body.args, ["-y", "--cache=/tmp/a,b", "server-name"]);
  assert.deepEqual(previewCall.body.env, ["MSG=hello, world", "API_KEY=native://app/api-key"]);
  assert.deepEqual(previewCall.body.header, ["Authorization=native://app/token"]);
  assert.equal(save.disabled, false, "a successful preview enables Save");

  need("mcp-command").fire("input");
  assert.equal(save.disabled, true, "an edit after preview disables Save");

  // Saving sends confirm and the same lossless arrays.
  previewButtons[0].fire("click");
  await flush();
  const saved = baseline();
  save.fire("click", { preventDefault() {} });
  await flush();
  const saveCall = callSince(saved, "mcp-server/add");
  assert.ok(saveCall, "saving posts mcp-server/add");
  assert.equal(saveCall.body.confirm, true);
  assert.deepEqual(saveCall.body.args, ["-y", "--cache=/tmp/a,b", "server-name"]);
  assert.deepEqual(saveCall.body.env, ["MSG=hello, world", "API_KEY=native://app/api-key"]);
}

async function testEditFillsOneEntryPerLineAndPreviewGuardsSave() {
  // The profiles view re-rendered after the save above; the server list now
  // has one Edit button wired to openMcpEdit.
  const servers = [
    {
      name: "context",
      transport: "stdio",
      server: {
        command: "npx",
        args: ["-y", "--cache=/tmp/a,b"],
        env: { MSG: "hello, world" },
        headers: {},
      },
    },
  ];
  setResponder((call) => {
    if (call.path === "mcp-servers") return { ok: true, result: { servers }, diagnostics: [] };
    if (call.path === "profiles") return { ok: true, result: { profiles: [] }, diagnostics: [] };
    if (call.path === "plan") return { ok: true, result: { agents: [] }, diagnostics: [] };
    return { ok: true, result: {}, diagnostics: [], cli: "muster" };
  });
  tabs.find((t) => t.dataset.view === "profiles").fire("click");
  await flush();

  const edit = allElements.filter((el) => el.tagName === "BUTTON" && el.children[0] === "Edit");
  assert.ok(edit.length >= 1, "the MCP server row has an Edit button");
  edit[edit.length - 1].fire("click");

  // Filling the form is lossless: commas stay inside their entries.
  assert.equal(need("mcp-args").value, "-y\n--cache=/tmp/a,b");
  assert.equal(need("mcp-env").value, "MSG=hello, world");
  assert.equal(need("mcp-save").disabled, true, "opening an edit does not enable Save");
  setResponder();
}

const tests = {
  testProfileSaveIsGatedOnPreview,
  testMcpFormUsesOneEntryPerLine,
  testEditFillsOneEntryPerLineAndPreviewGuardsSave,
};

let failed = 0;
for (const [name, run] of Object.entries(tests)) {
  try {
    await run();
    console.log(`ok ${name}`);
  } catch (err) {
    failed += 1;
    console.error(`FAIL ${name}: ${err.message}`);
  }
}
if (failed) {
  console.error(`${failed} test(s) failed`);
  process.exitCode = 1;
}
