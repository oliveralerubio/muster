"use strict";

// The one-time token was consumed by the page load; drop it from the address bar.
if (location.search) history.replaceState(null, "", "/");

const $ = (sel) => document.querySelector(sel);
const viewEl = $("#view");
const actionsEl = $("#actions");

function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "onclick") node.addEventListener("click", v);
    else node.setAttribute(k, v);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined) continue;
    node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function showCli(cli) {
  if (cli) $("#cli").textContent = cli;
}

async function api(path, body) {
  const options = body === undefined
    ? {}
    : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  const response = await fetch("/api/" + path, options);
  const data = await response.json().catch(() => ({ ok: false, diagnostics: [] }));
  showCli(data.cli);
  return data;
}

function diagnostics(data) {
  const list = data.diagnostics || [];
  if (data.error) list.push({ severity: "error", code: "http", message: data.error });
  if (!list.length) return null;
  return table(["Severity", "Code", "Message", "Suggestion"],
    list.map((d) => [d.severity, d.code, d.message, d.suggestion || ""]));
}

function table(headers, rows) {
  if (!rows.length) return el("p", {}, "Nothing to show.");
  return el("table", {},
    el("thead", {}, el("tr", {}, headers.map((h) => el("th", { scope: "col" }, h)))),
    el("tbody", {}, rows.map((row) => el("tr", {}, row.map((cell) => el("td", {}, cell))))));
}

function button(label, onclick, cls) {
  return el("button", { type: "button", class: cls || "", onclick }, label);
}

// Run a state-changing command after an explicit confirmation, then reload.
async function act(question, path, body) {
  if (!confirm(question)) return;
  const data = await api(path, Object.assign({ confirm: true }, body || {}));
  if (!data.ok) alert((data.diagnostics || []).map((d) => d.message).join("\n") || data.error || "failed");
  await show(current);
}

function render(title, actions, ...blocks) {
  $("#title").textContent = title;
  actionsEl.replaceChildren(...actions);
  viewEl.replaceChildren(...blocks.flat().filter(Boolean));
}

const views = {
  async dashboard() {
    const data = await api("status");
    const r = data.result || {};
    const machines = r.machines || [];
    const pending = (r.pending && r.pending.members) || [];
    const drift = r.drift || {};
    const driftCount = (drift.members || []).length + (drift.machines || []).length;
    render("Dashboard (fleet panel)", [button("Sincronizar ahora", () => act("Pull, plan and apply now?", "sync"), "primary")],
      el("h2", {}, "Última sincronización"),
      el("p", {}, r.sync ? `${r.sync.ts} · ${r.sync.result}` : "Nunca"),
      table(["Machines", "Pending members", "Drift"], [[machines.length, pending.length, driftCount]]),
      el("h2", {}, "Fleet"),
      table(["Name", "Platform", "Version", "Status", "Last sync"],
        machines.map((m) => [m.name, m.platform || "", m.version || "", m.status, m.last_sync || "never"])),
      el("h2", {}, "Pending members"),
      table(["Name"], pending.map((p) => [typeof p === "string" ? p : p.name])),
      el("h2", {}, "Drift"),
      table(["Kind", "Name", "Reason"], [
        ...(drift.members || []).map((d) => ["member", d.name, d.reason]),
        ...(drift.machines || []).map((d) => ["machine", d.name, d.reason]),
      ]),
      diagnostics(data));
  },

  async machines() {
    const data = await api("machines");
    const machines = (data.result && data.result.machines) || [];
    render("Machines", [],
      table(["Name", "Platform", "Role", "Profile", "Agents", "Last sync", ""],
        machines.map((m) => [m.name, m.platform || "", m.role || "", m.profile || "",
          (m.agents || []).join(", "), m.last_sync || "never",
          m.is_current ? "this machine" : button("Remove", () => act(`Remove machine ${m.name}?`, "machine/rm", { name: m.name }))])),
      diagnostics(data));
  },

  async team() {
    const members = await api("members");
    const groups = await api("groups");
    const memberRows = ((members.result && members.result.members) || []).map((m) => [
      m.name, m.role, m.status, (m.groups || []).join(", "), m.profile || "",
      m.status === "pending" ? button("Approve", () => act(`Approve member ${m.name}?`, "member/approve", { name: m.name })) : "",
    ]);
    const groupRows = ((groups.result && groups.result.groups) || []).map((g) => [
      g.name || g.group || "", JSON.stringify(g.readers || {}), g.profile || "",
    ]);
    render("Members & groups", [],
      el("h2", {}, "Members"), table(["Name", "Role", "Status", "Groups", "Profile", ""], memberRows),
      el("h2", {}, "Groups"), table(["Group", "Readers", "Profile"], groupRows),
      diagnostics(members), diagnostics(groups));
  },

  async secrets() {
    const data = await api("secrets");
    const secrets = Array.isArray(data.result) ? data.result : [];
    render("Secrets", [button("Nuevo secreto", () => openSecretEdit(), "primary")],
      el("p", {}, "Metadata only. Revealing a value needs explicit confirmation and is recorded."),
      table(["Reference", "Updated", "By", "Readers", ""],
        secrets.map((s) => [s.reference, s.updated_at || "", s.updated_by || "",
          (s.readers || []).map((r) => r.machine || r.recipient).join(", "),
          el("div", { class: "actions" }, button("Reveal…", () => openReveal(s.reference)),
            button("Cambiar valor", () => openSecretEdit(s.reference)))])),
      diagnostics(data));
  },

  async profiles() {
    const data = await api("plan");
    const r = data.result || {};
    const blocks = (r.agents || []).map((agent) => [
      el("h2", {}, `${agent.id} (${agent.status})`),
      table(["File", "Action", "Drift", "Diff"], (agent.files || []).map((f) => [
        f.path, f.action, f.drift ? "yes" : "no",
        f.diff ? el("pre", {}, f.diff) : (f.private ? "private (redacted)" : ""),
      ])),
    ]);
    render("Profiles & diff preview", [
      button("Apply (sync)", () => act("Apply this plan now?", "sync"), "primary"),
      button("Refresh", () => show("profiles")),
    ], el("p", {}, `${r.changes || 0} change(s) planned for ${r.target || "global"}.`), blocks, diagnostics(data));
  },

  async history() {
    const data = await api("history");
    const r = data.result || {};
    const generations = (r.generations || []).slice().reverse();
    render("History & generations", [],
      el("h2", {}, "Generations"),
      table(["#", "Created", "Kind", "Files", ""], generations.map((g) => [
        g.number, g.created_at, g.rollback_of ? `${g.kind} of ${g.rollback_of}` : g.kind, (g.files || []).length,
        button("Roll back", () => act(`Restore generation ${g.number}?`, "rollback", { generation: g.number })),
      ])),
      el("h2", {}, "Events"),
      table(["When", "Action", "Subject"], (r.events || []).slice().reverse().map((e) => [e.at, e.action, e.subject])),
      diagnostics(data));
  },

  async doctor() {
    const data = await api("doctor");
    const r = data.result || {};
    render("Doctor", [button("Apply safe fixes", () => act("Run doctor --fix?", "doctor", { fix: true }), "primary")],
      el("p", {}, r.healthy ? "Healthy." : "Problems found."),
      table(["Check", "Status"], (r.checks || []).map((c) => [c.name, c.status])),
      diagnostics(data));
  },
};

let current = "dashboard";
const tabs = Array.from(document.querySelectorAll("#nav [role=tab]"));

async function show(name) {
  current = name;
  for (const tab of tabs) {
    const selected = tab.dataset.view === name;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
  }
  try {
    await views[name]();
  } catch (err) {
    render(name, [], el("p", {}, "Failed to load: " + err.message));
  }
}

// Keyboard: arrows/Home/End move between tabs; digits 1-7 jump to a view.
$("#nav").addEventListener("keydown", (e) => {
  const i = tabs.indexOf(document.activeElement);
  if (i < 0) return;
  const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
  if (next === undefined) return;
  e.preventDefault();
  const tab = tabs[(next + tabs.length) % tabs.length];
  tab.focus();
  show(tab.dataset.view);
});
for (const tab of tabs) tab.addEventListener("click", () => show(tab.dataset.view));
document.addEventListener("keydown", (e) => {
  if (e.target.closest("input, textarea, dialog") || e.altKey || e.ctrlKey || e.metaKey) return;
  const n = Number(e.key);
  if (n >= 1 && n <= tabs.length) {
    tabs[n - 1].focus();
    show(tabs[n - 1].dataset.view);
  }
});

// Theme: system → light → dark, remembered locally.
function applyTheme(theme) {
  if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
  $("#theme").textContent = "Theme: " + (theme || "system");
}
applyTheme(localStorage.getItem("muster-theme"));
$("#theme").addEventListener("click", () => {
  const order = [null, "light", "dark"];
  const next = order[(order.indexOf(localStorage.getItem("muster-theme")) + 1) % order.length];
  if (next) localStorage.setItem("muster-theme", next);
  else localStorage.removeItem("muster-theme");
  applyTheme(next);
});

// Secret reveal: a dialog, then an explicit {"confirm": true} POST.
const dialog = $("#reveal");
let revealName = null;
function openReveal(reference) {
  revealName = reference;
  $("#reveal-name").textContent = reference;
  $("#reveal-value").textContent = "";
  showCli(`muster secret get ${reference} --reveal --yes`);
  dialog.showModal();
}
$("#reveal-confirm").addEventListener("click", async () => {
  const data = await api("secret/reveal", { name: revealName, confirm: true });
  $("#reveal-value").textContent = data.ok
    ? data.result
    : (data.diagnostics || []).map((d) => d.message).join("\n");
});
dialog.addEventListener("close", () => { $("#reveal-value").textContent = ""; });

const editDialog = $("#secret-edit");
let secretAction = "add";
function openSecretEdit(reference) {
  secretAction = reference ? "rotate" : "add";
  $("#secret-form").reset();
  $("#secret-edit-title").textContent = reference ? "Cambiar valor" : "Nuevo secreto";
  $("#secret-path").value = reference || "";
  $("#secret-path").readOnly = Boolean(reference);
  $("#secret-error").textContent = "";
  editDialog.showModal();
}
$("#secret-cancel").addEventListener("click", () => editDialog.close());
editDialog.addEventListener("close", () => $("#secret-form").reset());
$("#secret-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = $("#secret-input");
  const body = { path: $("#secret-path").value, value: input.value, confirm: $("#secret-confirm").checked };
  input.value = "";
  $("#secret-save").disabled = true;
  try {
    const pending = api("secret/" + secretAction, body);
    body.value = "";
    const data = await pending;
    if (data.ok) {
      editDialog.close();
      await show("secrets");
    } else {
      $("#secret-error").textContent = data.error || (data.diagnostics || []).map((d) => d.message).join("\n") || "failed";
    }
  } catch (err) {
    $("#secret-error").textContent = "Failed to save: " + err.message;
  } finally {
    body.value = "";
    input.value = "";
    $("#secret-save").disabled = false;
  }
});

show("dashboard");
