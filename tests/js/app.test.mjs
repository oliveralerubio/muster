// Regression tests for the panel's JavaScript, run by `cargo test` through
// tests/test_web_assets.rs (and usable directly: `node tests/js/app.test.mjs`).
//
// It loads src/web/assets/app.js in a `vm` sandbox with a minimal DOM stub
// seeded from index.html, then drives the profile dialog and the MCP form:
//   - Save is enabled only after a successful preview of the exact current
//     payload; any edit disables it again, stale preview responses are ignored,
//     and Save re-checks the payload before acting (QA round-2 finding 1).
//   - List fields hold JSON arrays, so " padded ", "" and commas survive the
//     edit → preview → save round trip losslessly (QA round-2 finding 3).

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
    // Reflect hidden like a real element: views render <pre hidden>.
    if (name === "hidden") this.hidden = String(v) !== "false";
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
for (const name of ["dashboard", "machines", "team", "secrets", "providers", "skills", "effective", "profiles", "history", "doctor"]) {
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
// Views re-render: the newest matching button is the one in the live DOM.
const liveButton = (label) => allElements
  .filter((el) => el.tagName === "BUTTON" && el.children[0] === label).at(-1);

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

  // A preview response that arrives after the form moved on is ignored: no
  // diff, Save stays disabled (QA round-2 finding 1).
  let releaseStale;
  setResponder(() => new Promise((resolve) => {
    releaseStale = () => resolve({ ok: true, result: { diff: "+stale\n" }, diagnostics: [] });
  }));
  need("profile-preview").fire("click", { preventDefault() {} });
  await flush();
  need("profile-toml").value = '[agents.models]\nclaude = "opus"\n'; // inflight edit
  releaseStale();
  await flush();
  assert.equal(save.disabled, true, "a stale preview response keeps Save disabled");
  assert.equal(need("profile-diff").hidden, true, "a stale response must not render a diff");

  // Save re-checks the payload even if the input events were bypassed by a
  // programmatic change: nothing may be acted on that was not previewed.
  setResponder(() => ({ ok: true, result: { diff: "+x\n" }, diagnostics: [] }));
  need("profile-preview").fire("click", { preventDefault() {} });
  await flush();
  assert.equal(save.disabled, false);
  need("profile-toml").value = "[agents.models]\n"; // no input event fired
  const guardMark = baseline();
  need("profile-form").fire("submit", { preventDefault() {} });
  await flush();
  assert.equal(save.disabled, true, "save re-checks the previewed payload");
  assert.ok(!callSince(guardMark, "profile/set"), "an unpreviewed payload must not be posted");
  assert.notEqual(need("profile-error").textContent, "", "the guard explains why save was blocked");
  setResponder();
}

async function testMcpFormUsesJsonArrayValues() {
  // Render the profiles view so the MCP form exists.
  tabs.find((t) => t.dataset.view === "profiles").fire("click");
  await flush();

  const save = need("mcp-save");
  assert.equal(save.disabled, true, "MCP Save must start disabled before any preview");

  // JSON array values (QA round-2 finding 3): whitespace, empty strings and
  // commas must survive verbatim instead of being trimmed or split.
  need("mcp-name").value = "context";
  need("mcp-command").value = "npx";
  need("mcp-args").value = '["-y", "--cache=/tmp/a,b", " padded ", ""]';
  need("mcp-env").value = '["MSG=hello, world", "API_KEY=native://app/api-key"]';
  need("mcp-header").value = '["Authorization=native://app/token"]';

  setResponder(() => ({ ok: true, result: { diff: "+command = \"npx\"\n" }, diagnostics: [] }));
  assert.ok(liveButton("Preview diff"), "the MCP form has a Preview diff button");
  const mark = baseline();
  liveButton("Preview diff").fire("click");
  await flush();

  const previewCall = callSince(mark, "mcp-server/add");
  assert.ok(previewCall, "the preview posts mcp-server/add");
  assert.equal(previewCall.body.dry_run, true);
  assert.deepEqual(previewCall.body.args, ["-y", "--cache=/tmp/a,b", " padded ", ""]);
  assert.deepEqual(previewCall.body.env, ["MSG=hello, world", "API_KEY=native://app/api-key"]);
  assert.deepEqual(previewCall.body.header, ["Authorization=native://app/token"]);
  assert.equal(save.disabled, false, "a successful preview enables Save");

  need("mcp-command").fire("input");
  assert.equal(save.disabled, true, "an edit after preview disables Save");

  // Unparsable list values block the preview, clear the old diff and keep
  // Save disabled.
  need("mcp-args").value = "-y";
  liveButton("Preview diff").fire("click");
  await flush();
  assert.equal(save.disabled, true, "unparsable JSON keeps Save disabled");
  assert.ok(need("mcp-error").textContent.includes("JSON array"), "the parse error is shown");
  assert.equal(need("mcp-diff").hidden, true, "a failed parse clears the old diff");
  setResponder();
}

async function testMcpPreviewIgnoresStaleResponse() {
  tabs.find((t) => t.dataset.view === "profiles").fire("click");
  await flush();
  const save = need("mcp-save");

  need("mcp-name").value = "context";
  need("mcp-args").value = '["-y"]';
  let releaseStale;
  setResponder(() => new Promise((resolve) => {
    releaseStale = () => resolve({ ok: true, result: { diff: "+stale\n" }, diagnostics: [] });
  }));
  liveButton("Preview diff").fire("click");
  await flush();
  need("mcp-args").value = '["-y", "other"]'; // inflight edit
  releaseStale();
  await flush();
  assert.equal(save.disabled, true, "a stale preview response keeps Save disabled");
  assert.equal(need("mcp-diff").hidden, true, "a stale response must not render a diff");
  setResponder();
}

async function testMcpEditClearsPreviewAndGuardsSave() {
  // The profiles view re-rendered; the server list has one Edit button wired
  // to openMcpEdit. Its values include the awkward ones, so the fill →
  // preview → save round trip must be lossless.
  const servers = [
    {
      name: "context",
      transport: "stdio",
      server: {
        command: "npx",
        args: ["-y", " padded ", ""],
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

  const save = need("mcp-save");

  // Preview first: Save enabled and the diff visible.
  need("mcp-name").value = "context";
  need("mcp-command").value = "npx";
  need("mcp-args").value = '["-y", " padded ", ""]';
  need("mcp-env").value = '["MSG=hello, world"]';
  liveButton("Preview diff").fire("click");
  await flush();
  assert.equal(save.disabled, false);
  assert.equal(need("mcp-diff").hidden, false);

  // Opening an edit clears the earlier preview state and locks Save (QA
  // round-2 finding 2), and the fill keeps every value verbatim.
  const edit = allElements.filter((el) => el.tagName === "BUTTON" && el.children[0] === "Edit").at(-1);
  edit.fire("click");
  assert.equal(need("mcp-args").value, JSON.stringify(["-y", " padded ", ""], null, 2));
  assert.equal(need("mcp-env").value, JSON.stringify(["MSG=hello, world"], null, 2));
  assert.equal(save.disabled, true, "opening an edit locks Save");
  assert.equal(need("mcp-diff").hidden, true, "opening an edit clears the diff");
  assert.equal(need("mcp-error").textContent, "", "opening an edit clears the error");

  // A locked Save must not post anything.
  const lockedMark = baseline();
  save.fire("click", { preventDefault() {} });
  await flush();
  assert.ok(!callSince(lockedMark, "mcp-server/add"), "a locked Save must not post");

  // Re-preview the filled values, then save round-trips them exactly.
  setResponder(() => ({ ok: true, result: { diff: "+args\n" }, diagnostics: [] }));
  liveButton("Preview diff").fire("click");
  await flush();
  assert.equal(save.disabled, false, "the filled values preview cleanly");
  const savedMark = baseline();
  save.fire("click", { preventDefault() {} });
  await flush();
  const saveCall = callSince(savedMark, "mcp-server/add");
  assert.ok(saveCall, "saving posts mcp-server/add");
  assert.equal(saveCall.body.confirm, true);
  assert.deepEqual(saveCall.body.args, ["-y", " padded ", ""]);
  assert.deepEqual(saveCall.body.env, ["MSG=hello, world"]);
  setResponder();
}

async function testProvidersViewListsReferencesAndAddsAndRemoves() {
  setResponder((call) => ({ ok: true, result: call.path === "providers" ? { providers: [{
    id: "gateway", kind: "anthropic", base_url: "https://example.invalid",
    default_model: "example-model", key: "env://EXAMPLE_KEY", agents: ["claude"]
  }], subscriptions: [
    { agent: "claude", installed: true, logged_in: true },
    { agent: "codex", installed: true, logged_in: false }
  ] } : {}, diagnostics: [] }));
  const mark = baseline();
  tabs.find((t) => t.dataset.view === "providers").fire("click");
  await flush();
  assert.ok(callSince(mark, "providers"));
  const text = (node) => node instanceof FakeElement
    ? [node.textContent, ...node.children.map(text)].join(" ") : String(node);
  for (const expected of ["gateway", "anthropic", "https://example.invalid", "example-model", "env://EXAMPLE_KEY", "claude"]) {
    assert.ok(text(view).includes(expected), `provider table includes ${expected}`);
  }
  const body = text(view);
  for (const expected of ["Subscriptions on this machine", "Signed in", "Not signed in", "muster provider login codex", "Custom endpoints (advanced)"]) {
    assert.ok(body.includes(expected), `providers view includes ${expected}`);
  }
  assert.ok(!body.includes("muster provider login claude"), "signed-in agent shows no login command");
  const details = (function find(node) {
    if (!(node instanceof FakeElement)) return null;
    if (node.tagName === "DETAILS") return node;
    for (const kid of node.children) { const f = find(kid); if (f) return f; }
    return null;
  })(view);
  assert.ok(details, "custom endpoints are in a details element");
  assert.equal(details.getAttribute("open"), "", "details open when a provider is configured");
  need("provider-id").value = "local";
  need("provider-kind").value = "ollama";
  need("provider-url").value = "http://localhost:11434/v1";
  need("provider-model").value = "local-model";
  need("provider-key").value = "env://LOCAL_KEY";
  need("provider-form").fire("submit", { preventDefault() {} });
  await flush();
  const added = callSince(mark, "provider/add");
  assert.ok(added);
  assert.equal(added.body.confirm, true);
  assert.equal(added.body.id, "local");
  assert.equal(added.body.kind, "ollama");
  assert.equal(added.body["base-url"], "http://localhost:11434/v1");
  assert.equal(added.body["default-model"], "local-model");
  assert.equal(added.body.key, "env://LOCAL_KEY");
  const rmMark = baseline();
  liveButton("Remove").fire("click");
  await flush();
  const removed = callSince(rmMark, "provider/rm");
  assert.ok(removed);
  assert.equal(removed.body.id, "gateway");
  assert.equal(removed.body.confirm, true);
  setResponder();
}

async function testSecretApprovalsRequireConfirmationAndRefresh() {
  setResponder((call) => ({ ok: true, diagnostics: [], result:
    call.path === "secret/requests" ? [{ id: "request-1", reference: "native://app/key", agent: "claude", requested_at: 100 }] :
    call.path === "secret/leases" ? [{ id: "lease-1", reference: "native://app/key", agent: "claude", expires_at: 200 }] : [] }));
  await sandbox.show("secrets");
  assert.ok(liveButton("Approve"));
  assert.ok(liveButton("Deny"));
  assert.ok(liveButton("Revoke"));
  const previousConfirm = sandbox.confirm;
  sandbox.confirm = () => false;
  const canceled = baseline();
  liveButton("Approve").fire("click");
  await flush();
  assert.equal(callSince(canceled, "secret/approve"), undefined);
  sandbox.confirm = previousConfirm;
  for (const [label, action, id] of [["Approve", "approve", "request-1"], ["Deny", "deny", "request-1"], ["Revoke", "revoke", "lease-1"]]) {
    const mark = baseline();
    liveButton(label).fire("click");
    await flush();
    const posted = callSince(mark, "secret/" + action);
    assert.ok(posted);
    assert.deepEqual(posted.body, { id, confirm: true });
    assert.ok(callSince(mark, "secret/requests"));
    assert.ok(callSince(mark, "secret/leases"));
  }
  setResponder();
}

async function testEffectiveViewShowsStatesAndEscapesNames() {
  // The real `effective` schema: `instructions` is a list of sources (user
  // scope plus the project scope) and per-adapter states live in `adapters`.
  setResponder(() => ({ ok: true, diagnostics: [], result: {
    layers: [{ kind: "team", name: "team" }, { kind: "project", name: "/srv/demo" }],
    instructions: [
      { source: "AGENTS.md", layer: "team", approx_tokens: 2, adapters: { claude: { state: "in_sync" } } },
      { source: "demo/AGENTS.md", layer: "project", approx_tokens: 4, adapters: { claude: { state: "missing" } } },
    ],
    skills: [{ name: "<script>alert(1)</script>", origin: "local", layer: "team", verification: "unreviewed",
      description_tokens: 3, body_tokens: 13, adapters: { claude: { state: "drifted" } } }],
    mcp: [{ name: "context", layer: "team", transport: "stdio", overridden_layers: [], adapters: { claude: { state: "missing" } } }],
    subagents: [{ name: "reviewer", layer: "team", adapters: { claude: { state: "installed" } } }],
    totals: { claude: 42 }, extra_on_disk: { claude: ["personal"] },
  } }));
  await sandbox.show("effective");
  const nodes = [];
  const walk = (node) => { if (node instanceof FakeElement) { nodes.push(node); node.children.forEach(walk); } };
  walk(view);
  const text = nodes.flatMap((node) => [node.textContent, ...node.children.filter((c) => typeof c === "string")]).join(" ");
  // Both instruction sources render with their supplying layer.
  for (const expected of ["AGENTS.md", "team", "demo/AGENTS.md", "project",
    "<script>alert(1)</script>", "context", "reviewer", "42", "personal"]) {
    assert.ok(text.includes(expected), `effective view includes ${expected}`);
  }
  assert.ok(nodes.some((node) => node.children.includes("drifted") && node.getAttribute("class").includes("danger")));
  assert.ok(nodes.some((node) => node.children.includes("missing") && node.getAttribute("class").includes("danger")));
  assert.ok(nodes.some((node) => node.children.includes("in_sync")));
  assert.ok(nodes.some((node) => node.children.includes("installed")));
  assert.ok(!nodes.some((node) => node.tagName === "SCRIPT"));
  setResponder();
}

async function testSkillToggleControls() {
  setResponder((call) => ({ ok: true, diagnostics: [], result: call.path === "skills" ? { skills: [{ name: "pdf", origin: "local", status: "local", enabled: true }] } : {} }));
  await sandbox.show("skills");
  const mark = baseline();
  liveButton("Disable").fire("click");
  await flush();
  assert.deepEqual(callSince(mark, "disable").body, { item: "skill:pdf", confirm: true });

  // Disabled skill renders Enable button, not Disable button
  setResponder((call) => ({ ok: true, diagnostics: [], result: call.path === "skills" ? { skills: [{ name: "pdf", origin: "local", status: "local", enabled: false }] } : {} }));
  await sandbox.show("skills");
  const mark2 = baseline();
  liveButton("Enable").fire("click");
  await flush();
  assert.deepEqual(callSince(mark2, "enable").body, { item: "skill:pdf", confirm: true });
  setResponder();
}

async function testMcpToggleShowsOnlyValidAction() {
  setResponder((call) => ({
    ok: true, diagnostics: [],
    result: call.path === "mcp-servers" ? { servers: [{ name: "github", enabled: false, server: {} }] } : {},
  }));
  await sandbox.show("profiles");
  const mark = baseline();
  liveButton("Enable").fire("click");
  await flush();
  assert.deepEqual(callSince(mark, "enable").body, { item: "mcp:github", confirm: true });
  setResponder();
}

async function testCliFooterNotOverwrittenByBackgroundFetch() {
  setResponder((call) => ({
    ok: true, diagnostics: [], cli: call.path === "skills" ? "muster skill list" : "muster usage",
    result: call.path === "skills" ? { skills: [] } : {},
  }));
  await sandbox.show("skills");
  await flush();
  assert.equal(need("cli").textContent, "muster skill list");
  setResponder();
}

async function testUsageShowsUnusedItemsAsText() {
  setResponder((call) => ({ ok: true, diagnostics: [], result: call.path === "usage" ? { unused: [{ kind: "skill", name: "<unsafe>", suggestion: "muster disable skill:<unsafe>" }] } : { skills: [] } }));
  await sandbox.show("skills");
  const text = (node) => typeof node === "string" ? node : (node.children || []).map(text).join(" ");
  assert.match(text(view), /Usage/);
  assert.match(text(view), /<unsafe>/);
  assert.match(text(view), /muster disable skill:<unsafe>/);
  setResponder();
}

const USAGE_FIXTURE = {
  items: [
    { kind: "skill", name: "review", uses: 3, sessions: 2, last_used: "2099-01-02T00:00:00Z", agents: { claude: { uses: 3, sessions: 2 } } },
    { kind: "mcp", name: "docs", uses: 0, sessions: 0, last_used: null, agents: {} },
  ],
  unused: [{ kind: "mcp", name: "docs", suggestion: "muster disable mcp:docs" }],
  coverage: "partial",
  sessions_scanned: { claude: 2, codex: 0, opencode: 0 },
  sources: [
    { agent: "claude", status: "supported", directory: "~/.claude/projects", malformed_lines: 0, skipped: {} },
    { agent: "codex", status: "partial", directory: "~/.codex/sessions" },
    { agent: "opencode", status: "unsupported", directory: "~/.local/share/opencode" },
  ],
};

async function testUsageShowsCoverageActiveAndUnusedItems() {
  setResponder((call) => ({ ok: true, diagnostics: [], result: call.path === "usage" ? USAGE_FIXTURE : { skills: [] } }));
  await sandbox.show("skills");
  const text = (node) => typeof node === "string" ? node : (node.children || []).map(text).join(" ");
  const rendered = text(view);
  assert.match(rendered, /claude: 2/);
  assert.match(rendered, /unsupported: opencode/);
  assert.doesNotMatch(rendered, /unsupported: 2/);
  assert.match(rendered, /Active items/);
  assert.match(rendered, /review/);
  assert.match(rendered, /2099/);
  assert.doesNotMatch(rendered, /mcp docs 0/);
  assert.match(rendered, /Unused items/);
  assert.match(rendered, /muster disable mcp:docs/);
  setResponder();
}

async function testUsageWithoutSessionsExplainsNoCoverage() {
  const empty = { items: [], unused: [], coverage: "none", sessions_scanned: { claude: 0 }, sources: [{ agent: "claude", status: "supported" }] };
  setResponder((call) => ({ ok: true, diagnostics: [], result: call.path === "usage" ? empty : { skills: [] } }));
  await sandbox.show("skills");
  const text = (node) => typeof node === "string" ? node : (node.children || []).map(text).join(" ");
  assert.match(text(view), /No transcript sessions scanned/);
  setResponder();
}

const tests = {
  testEffectiveViewShowsStatesAndEscapesNames,
  testSkillToggleControls,
  testMcpToggleShowsOnlyValidAction,
  testCliFooterNotOverwrittenByBackgroundFetch,
  testUsageShowsUnusedItemsAsText,
  testUsageShowsCoverageActiveAndUnusedItems,
  testUsageWithoutSessionsExplainsNoCoverage,
  testSecretApprovalsRequireConfirmationAndRefresh,
  testProvidersViewListsReferencesAndAddsAndRemoves,
  testProfileSaveIsGatedOnPreview,
  testMcpFormUsesJsonArrayValues,
  testMcpPreviewIgnoresStaleResponse,
  testMcpEditClearsPreviewAndGuardsSave,
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
