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
  for (const child of children.flat(Infinity)) {
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

// Diff rendering shared by the plan preview and every edit preview.
function diffPre(diff) {
  return el("pre", { class: "diff" }, diff || "(no changes)");
}

function failMessage(data) {
  return (data.diagnostics || []).map((d) => d.message).join("\n") || data.error || "failed";
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
  actionsEl.replaceChildren(...actions.flat(Infinity).filter(Boolean));
  viewEl.replaceChildren(...blocks.flat(Infinity).filter(Boolean));
}

const views = {
  async dashboard(token) {
    const data = await api("status");
    const schedule = await api("schedule");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    const machines = r.machines || [];
    const pending = (r.pending && r.pending.members) || [];
    const drift = r.drift || {};
    const driftCount = (drift.members || []).length + (drift.machines || []).length;
    const s = schedule.result || {};
    const interval = el("input", { type: "number", min: "1", value: "60", "aria-label": "Minutes between syncs" });
    render("Dashboard (fleet panel)", [button("Sync now", () => act("Pull, plan and apply now?", "sync"), "primary")],
      el("h2", {}, "Last sync"),
      el("p", {}, r.sync ? `${r.sync.ts} · ${r.sync.result}` : "Never"),
      table(["Machines", "Pending members", "Drift"], [[machines.length, pending.length, driftCount]]),
      el("h2", {}, "Periodic sync"),
      el("p", {}, s.enabled ? `Enabled via ${s.backend}.` : "Not scheduled on this machine."),
      el("div", { class: "skill-form" },
        interval,
        button("Enable", () => act(`Enable periodic sync every ${interval.value} minute(s)?`, "schedule/enable", { interval: Number(interval.value) || undefined })),
        s.enabled ? button("Disable", () => act("Disable periodic sync?", "schedule/disable"), "danger") : ""),
      diagnostics(schedule),
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

  async providers(token) {
    const data = await api("providers");
    if (token !== undefined && token !== navToken) return;
    const providers = (data.result || {}).providers || [];
    const field = (id, label, attrs) => {
      const input = el("input", Object.assign({ id, type: "text", autocomplete: "off" }, attrs || {}));
      return [el("label", { for: id }, label), input];
    };
    const kind = el("select", { id: "provider-kind" },
      ["anthropic", "openai", "openai-compatible", "ollama", "google"].map((value) => el("option", { value }, value)));
    const form = el("form", { id: "provider-form", class: "skill-form" },
      field("provider-id", "ID", { required: "", pattern: "[a-z0-9-]+" }),
      el("label", { for: "provider-kind" }, "Kind"), kind,
      field("provider-url", "Base URL", { type: "url" }),
      field("provider-model", "Default model"),
      field("provider-key", "Key reference (not a key value)", { placeholder: "native://app/key or env://VARIABLE" }),
      el("button", { type: "submit", class: "primary" }, "Add provider"));
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const body = { id: $("#provider-id").value.trim(), kind: kind.value };
      for (const [name, id] of [["base-url", "provider-url"], ["default-model", "provider-model"], ["key", "provider-key"]]) {
        const value = $("#" + id).value.trim();
        if (value) body[name] = value;
      }
      await act("Add or replace this AI provider?", "provider/add", body);
    });
    render("AI providers", [],
      table(["ID", "Kind", "Base URL", "Default model", "Key reference", "Receiving agents", ""],
        providers.map((p) => [p.id, p.kind, p.base_url || "", p.default_model || "", p.key || "", (p.agents || []).join(", "),
          button("Remove", () => act(`Remove provider ${p.id}?`, "provider/rm", { id: p.id }), "danger")])),
      el("h2", {}, "Add provider"), form,
      el("p", {}, "Subscription logins stay on this machine. Run muster provider login claude or muster provider login codex in your terminal."),
      diagnostics(data));
  },

  async machines(token) {
    const data = await api("machines");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    const machines = r.machines || [];
    const pending = r.pending || [];
    const quorum = (p) => {
      const approvals = p.approvals || [];
      if (p.approvals_required === undefined) return "";
      return `${approvals.length}/${p.approvals_required}`
        + (approvals.length ? ` (${approvals.join(", ")})` : "");
    };
    render("Machines", [],
      el("h2", {}, "Enrolled"),
      table(["Name", "Platform", "Role", "Profile", "Agents", "Last sync", ""],
        machines.map((m) => [m.name, m.platform || "", m.role || "", m.profile || "",
          (m.agents || []).join(", "), m.last_sync || "never",
          m.is_current ? "this machine" : button("Remove", () => act(`Remove machine ${m.name}?`, "machine/rm", { name: m.name }))])),
      el("h2", {}, "Pending enrollment requests"),
      table(["Name", "Platform", "Version", "Agents", "Approvals", ""],
        pending.map((p) => [p.name, p.platform || "", p.version || "",
          (p.agents || []).join(", "), quorum(p),
          el("span", { class: "actions" },
            button("Approve", () => act(`Approve enrollment of machine ${p.name}?`, "approve", { name: p.name })),
            button("Reject", () => act(`Reject the enrollment request of machine ${p.name}? Its recorded approvals are discarded.`, "reject", { name: p.name }), "danger"))])),
      diagnostics(data));
  },

  async team(token) {
    const members = await api("members");
    const groups = await api("groups");
    if (token !== undefined && token !== navToken) return;
    const memberRows = ((members.result && members.result.members) || []).map((m) => [
      m.name, m.role, m.status, (m.groups || []).join(", "), m.profile || "",
      m.status === "pending"
        ? el("span", { class: "actions" },
            button("Approve", () => act(`Approve member ${m.name}?`, "member/approve", { name: m.name })),
            button("Reject", () => act(`Reject member ${m.name}? Their invitation and recorded approvals are discarded.`, "member/reject", { name: m.name }), "danger"))
        : "",
    ]);
    const groupRows = ((groups.result && groups.result.groups) || []).map((g) => [
      g.name || g.group || "", JSON.stringify(g.readers || {}), g.profile || "",
    ]);
    render("Members & groups", [],
      el("h2", {}, "Members"), table(["Name", "Role", "Status", "Groups", "Profile", ""], memberRows),
      el("h2", {}, "Groups"), table(["Group", "Readers", "Profile"], groupRows),
      diagnostics(members), diagnostics(groups));
  },

  async secrets(token) {
    const data = await api("secrets");
    if (token !== undefined && token !== navToken) return;
    const secrets = Array.isArray(data.result) ? data.result : [];
    render("Secrets", [button("New secret", () => openSecretEdit(), "primary")],
      el("p", {}, "Metadata only. Revealing a value needs explicit confirmation and is recorded."),
      table(["Reference", "Updated", "By", "Readers", ""],
        secrets.map((s) => [s.reference, s.updated_at || "", s.updated_by || "",
          (s.readers || []).map((r) => r.machine || r.recipient).join(", "),
          el("div", { class: "actions" }, button("Reveal…", () => openReveal(s.reference)),
            button("Change value", () => openSecretEdit(s.reference)))])),
      diagnostics(data));
  },

  async skills(token) {
    const data = await api("skills");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    const skills = r.skills || [];
    updateSkillsDot(r);
    const badgeClass = {
      local: "", up_to_date: "success", update_available: "info",
      deprecated: "warning", upstream_removed: "danger", unknown: "",
    };
    const statusLabel = {
      local: "own", up_to_date: "up to date", update_available: "update available",
      deprecated: "deprecated", upstream_removed: "removed upstream", unknown: "unknown",
    };
    const rows = skills.map((s) => {
      const status = el("span", { class: "badge " + (badgeClass[s.status] || "") }, statusLabel[s.status] || s.status);
      if (s.reason) status.setAttribute("title", s.reason);
      const actions = [];
      if (s.status === "upstream_removed") {
        actions.push(button("Keep as own", () => act(`Keep ${s.name} as an own skill and stop following ${s.origin}?`, "skills/rm", { name: s.name, detach: true })));
        actions.push(button("Delete", () => act(`Delete skill ${s.name} and its files?`, "skills/rm", { name: s.name }), "danger"));
      } else {
        if (s.origin !== "local") actions.push(button("Update", () => updateSkill(s)));
        actions.push(button("Remove", () => act(`Delete skill ${s.name} and its files?`, "skills/rm", { name: s.name }), "danger"));
      }
      return [
        s.name,
        el("span", { class: "badge" + (s.origin === "local" ? "" : " info") }, s.origin === "local" ? "own" : s.origin),
        s.version || "",
        s.updated_at || "",
        status,
        s.modified ? el("span", { class: "badge warning", title: "Edited locally since it was vendored" }, "modified") : "",
        el("span", { class: "actions" }, actions),
      ];
    });
    const name = el("input", { type: "text", placeholder: "new-skill", "aria-label": "Skill name" });
    const description = el("input", { type: "text", placeholder: "Description", "aria-label": "Skill description" });
    const url = el("input", { type: "url", placeholder: "https://github.com/org/skills", "aria-label": "Repository URL" });
    const path = el("input", { type: "text", placeholder: "path (optional)", "aria-label": "Skills directory in the repository" });
    const include = el("input", { type: "text", placeholder: "only: a,b (optional)", "aria-label": "Skills to include" });
    render("Skills", [
      button("Check repositories", async () => { await api("skills/check", {}); await show("skills"); }, "primary"),
      button("Update all", () => updateAll()),
    ],
      el("p", {}, r.checked_at ? `Last upstream check: ${r.checked_at}.` : "Upstream sources not checked yet on this machine."),
      table(["Name", "Origin", "Version", "Updated", "Status", "", ""], rows),
      el("h2", {}, "Add an own skill"),
      el("div", { class: "skill-form" }, name, description,
        button("Create", () => act(`Create skill ${name.value}?`, "skills/add", { name: name.value, description: description.value || undefined }))),
      el("h2", {}, "Subscribe to a repository"),
      el("div", { class: "skill-form" }, url, path, include,
        button("Subscribe", () => act(`Subscribe to ${url.value}?`, "skills/add", {
          url: url.value,
          path: path.value || undefined,
          include: include.value ? include.value.split(",").map((x) => x.trim()).filter(Boolean) : undefined,
        }))),
      diagnostics(data));
  },

  async profiles(token) {
    const [planData, listData, mcpData] = await Promise.all([api("plan"), api("profiles"), api("mcp-servers")]);
    if (token !== undefined && token !== navToken) return;
    const plan = planData.result || {};
    const profiles = (listData.result && listData.result.profiles) || [];
    const servers = (mcpData.result && mcpData.result.servers) || [];
    const usage = (u) => {
      const used = u || {};
      const parts = [];
      if (used.groups && used.groups.length) parts.push(`groups: ${used.groups.join(", ")}`);
      if (used.members && used.members.length) parts.push(`members: ${used.members.join(", ")}`);
      if (used.machines && used.machines.length) parts.push(`machines: ${used.machines.join(", ")}`);
      return parts.join("; ") || "unused";
    };
    const planBlocks = (plan.agents || []).flatMap((agent) => [
      el("h2", {}, `${agent.id} (${agent.status})`),
      table(["File", "Action", "Drift", "Diff"], (agent.files || []).map((f) => [
        f.path, f.action, f.drift ? "yes" : "no",
        f.diff ? diffPre(f.diff) : (f.private ? "private (redacted)" : ""),
      ])),
    ]);
    render("Profiles & diff preview", [
      button("New profile", () => openProfileEdit(null), "primary"),
      button("Apply (sync)", () => act("Apply this plan now?", "sync")),
      button("Refresh", () => show("profiles")),
    ],
      el("p", {}, "Agent profiles and MCP servers declared in muster.toml. Every edit shows its diff before saving."),
      el("h2", {}, "Agent profiles"),
      table(["Name", "Assigned to", ""], profiles.map((p) => [
        p.name, usage(p.used_by),
        el("span", { class: "actions" },
          button("Edit", () => openProfileEdit(p.name)),
          button("Remove", () => act(`Remove profile ${p.name}?`, "profile/rm", { name: p.name }), "danger")),
      ])),
      diagnostics(listData),
      el("h2", {}, "MCP servers"),
      table(["Name", "Transport", "Target", "Env / headers", ""], servers.map((s) => {
        const server = s.server || {};
        const settings = Object.entries(server.env || {}).map(([k, v]) => `${k}=${v}`)
          .concat(Object.entries(server.headers || {}).map(([k, v]) => `${k}=${v}`));
        return [s.name, s.transport, server.command || server.url || "", settings.join(", "),
          el("span", { class: "actions" },
            button("Edit", () => openMcpEdit(s)),
            button("Remove", () => act(`Remove MCP server ${s.name}?`, "mcp-server/rm", { name: s.name }), "danger"))];
      })),
      mcpForm(),
      el("pre", { id: "mcp-diff", hidden: true }, ""),
      el("p", { id: "mcp-error", class: "form-error", role: "alert" }),
      diagnostics(mcpData),
      el("h2", {}, "Plan preview"),
      el("p", {}, `${plan.changes || 0} change(s) planned for ${plan.target || "global"}.`),
      planBlocks,
      diagnostics(planData));
  },

  async history(token) {
    const data = await api("history");
    if (token !== undefined && token !== navToken) return;
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

  async doctor(token) {
    const data = await api("doctor");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    render("Doctor", [button("Apply safe fixes", () => act("Run doctor --fix?", "doctor", { fix: true }), "primary")],
      el("p", {}, r.healthy ? "Healthy." : "Problems found."),
      table(["Check", "Status"], (r.checks || []).map((c) => [c.name, c.status])),
      diagnostics(data));
  },
};

// Skills: the tab shows a dot while anything needs a decision.
function updateSkillsDot(result) {
  $("#skills-dot").hidden = !(result && result.attention > 0);
}

function fileSummary(files) {
  const part = (label, list) => (list && list.length ? `${label}: ${list.join(", ")}` : null);
  return [part("added", files.added), part("changed", files.changed), part("removed", files.removed)]
    .filter(Boolean).join("; ") || "no file changes";
}

// Preview with a dry run, then confirm with the per-file summary.
async function confirmUpdate(body) {
  const preview = await api("skills/update", Object.assign({ dry_run: true }, body));
  if (!preview.ok) {
    alert((preview.diagnostics || []).map((d) => d.message).join("\n") || "update failed");
    return;
  }
  const updates = (preview.result && preview.result.updated) || [];
  if (!updates.length) {
    alert("Nothing to update.");
    return;
  }
  const lines = updates.map((u) => `${u.name}${u.version ? " " + u.version : ""}: ${fileSummary(u.files || {})}`);
  await act("Apply these updates?\n\n" + lines.join("\n"), "skills/update", body);
}

function updateSkill(skill) {
  if (skill.modified && !confirm(`${skill.name} was edited locally. Overwrite the local changes?`)) return;
  return confirmUpdate(skill.modified ? { name: skill.name, force: true } : { name: skill.name });
}

function updateAll() {
  return confirmUpdate({ all: true });
}

api("skills").then((data) => updateSkillsDot(data.result)).catch(() => {});

let current = "dashboard";
// Navigation token: guards against tab-switch races where slow inflight
// fetches from an earlier view resolve after the user navigated elsewhere.
let navToken = 0;
const tabs = Array.from(document.querySelectorAll("#nav [role=tab]"));

async function show(name) {
  current = name;
  const token = ++navToken;
  for (const tab of tabs) {
    const selected = tab.dataset.view === name;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
  }
  try {
    await views[name](token);
    if (token !== navToken) return;
  } catch (err) {
    if (token !== navToken) return;
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
  $("#secret-edit-title").textContent = reference ? "Change value" : "New secret";
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

// Profile editing: a dialog with the profile TOML, a dry-run diff preview and
// a confirmed save. Secret references pass through verbatim; values are never
// resolved or sent anywhere else. Save stays disabled until the exact current
// payload has been previewed; any edit disables it again until a re-preview,
// and a preview response that arrives after the form moved on is ignored.
const profileDialog = $("#profile-edit");
let profileEditing = null;

// The payload the dialog would act on, and a token identifying it exactly.
function profileBody(dryRun) {
  return {
    name: $("#profile-name").value.trim(),
    toml: $("#profile-toml").value,
    ...(dryRun ? { dry_run: true } : { confirm: true }),
  };
}
function profileToken() {
  const body = profileBody(false);
  return JSON.stringify({ name: body.name, toml: body.toml });
}
// Token of the last successful preview; anything else keeps Save disabled.
let profilePreviewToken = null;

async function openProfileEdit(name) {
  profileEditing = name;
  profilePreviewToken = null;
  $("#profile-form").reset();
  $("#profile-diff").hidden = true;
  $("#profile-diff").textContent = "";
  $("#profile-error").textContent = "";
  $("#profile-save").disabled = true;
  $("#profile-name").value = name || "";
  $("#profile-name").readOnly = Boolean(name);
  $("#profile-edit-title").textContent = name ? `Edit profile ${name}` : "New profile";
  if (name) {
    const data = await api(`profile?name=${encodeURIComponent(name)}&toml=true`);
    if (!data.ok) {
      $("#profile-error").textContent = failMessage(data);
      profileDialog.showModal();
      return;
    }
    $("#profile-toml").value = (data.result && data.result.toml) || "";
  }
  profileDialog.showModal();
}

// Shared by profile and MCP previews: run the command with dry_run and show
// the returned diff.
function showDiff(pre, data) {
  if (!data.ok) {
    pre.hidden = true;
    pre.textContent = "";
    return failMessage(data);
  }
  pre.textContent = (data.result && data.result.diff) || "(no changes)";
  pre.hidden = false;
  return null;
}

$("#profile-preview").addEventListener("click", async () => {
  const token = profileToken();
  const data = await api("profile/set", profileBody(true));
  // The form changed while the request was inflight: the response describes a
  // payload nobody previewed, so ignore it and keep Save disabled.
  if (profileToken() !== token) {
    $("#profile-save").disabled = true;
    return;
  }
  const problem = showDiff($("#profile-diff"), data);
  if (problem) $("#profile-error").textContent = problem;
  // Save unlocks only after a successful preview of the exact current payload.
  profilePreviewToken = problem ? null : token;
  $("#profile-save").disabled = Boolean(problem);
});

// Any edit after a preview invalidates it: Save is disabled until re-previewed.
for (const id of ["profile-name", "profile-toml"]) {
  $("#" + id).addEventListener("input", () => { $("#profile-save").disabled = true; });
}

$("#profile-cancel").addEventListener("click", () => profileDialog.close());
profileDialog.addEventListener("close", () => $("#profile-form").reset());
$("#profile-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if ($("#profile-save").disabled) return;
  // Belt and braces: only the exact previewed payload may be acted on.
  if (profileToken() !== profilePreviewToken) {
    $("#profile-save").disabled = true;
    $("#profile-error").textContent = "The form changed since the preview. Preview the diff again.";
    return;
  }
  const body = profileBody(false);
  if (!confirm(`Save profile ${body.name}?`)) return;
  const data = await api("profile/set", body);
  if (data.ok) {
    profilePreviewToken = null;
    profileDialog.close();
    await show("profiles");
  } else {
    $("#profile-error").textContent = failMessage(data);
  }
});

// MCP server form: list fields hold JSON arrays, what the API itself accepts
// as KEY=VALUE lists. JSON keeps every value verbatim: " padded ", "" and
// commas survive the edit → preview → save round trip unchanged. Values that
// look like secret material must be references (e.g. native://group/name);
// they are stored verbatim and resolved only at apply time.
function jsonList(text, label) {
  if (!text.trim()) return [];
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error(`${label} must be a JSON array of strings, e.g. ["arg1", "arg2"].`);
  }
  if (!Array.isArray(parsed) || parsed.some((item) => typeof item !== "string")) {
    throw new Error(`${label} must be a JSON array of strings.`);
  }
  return parsed;
}

function jsonPairs(text, label) {
  const list = jsonList(text, label);
  if (list.some((entry) => !entry.includes("=") || entry.startsWith("="))) {
    throw new Error(`${label} entries must be KEY=VALUE strings.`);
  }
  return list;
}

function field(id, label, placeholder) {
  return el("label", { class: "field" }, label + " ",
    el("input", { id, type: "text", placeholder, "aria-label": label }));
}

function fieldArea(id, label, placeholder) {
  return el("label", { class: "field" }, label + " ",
    el("textarea", { id, rows: 2, spellcheck: "false", placeholder, "aria-label": label }));
}

function mcpForm() {
  const boxes = [
    field("mcp-name", "Name", "docs"),
    field("mcp-command", "Command (stdio)", "npx"),
    field("mcp-url", "URL (remote)", "https://example.com/mcp"),
    fieldArea("mcp-args", "Args (JSON array)", '["-y", "server-name"]'),
    fieldArea("mcp-env", "Env KEY=reference (JSON array)", '["API_KEY=native://app/api-key"]'),
    fieldArea("mcp-header", "Headers KEY=reference (JSON array)", '["Authorization=native://app/token"]'),
  ];
  const save = button("Save", saveMcp, "primary");
  save.id = "mcp-save";
  // Like the profile dialog: Save stays disabled until the exact current
  // values have been previewed, and any edit disables it again.
  save.disabled = true;
  for (const box of boxes) {
    box.querySelector("input, textarea").addEventListener("input", () => { save.disabled = true; });
  }
  const row = el("div", { class: "skill-form" }, boxes,
    button("Preview diff", previewMcp),
    save);
  return [el("h2", {}, "Add or replace an MCP server"), row];
}

function mcpPayload() {
  const command = $("#mcp-command").value.trim();
  const url = $("#mcp-url").value.trim();
  return {
    name: $("#mcp-name").value.trim(),
    command: command || undefined,
    url: url || undefined,
    args: jsonList($("#mcp-args").value, "Args"),
    env: jsonPairs($("#mcp-env").value, "Env"),
    header: jsonPairs($("#mcp-header").value, "Headers"),
  };
}
// Token of the form's exact current payload; null when the list fields do not
// parse, so unparseable values can never match a preview.
let mcpPreviewToken = null;
function mcpToken() {
  try {
    return JSON.stringify(mcpPayload());
  } catch {
    return null;
  }
}

// Values that look like secret material must stay references; Muster stores
// exactly what is typed, so a raw value would end up in the manifest.
function looksLikeSecretValue(body) {
  return [...body.env, ...body.header].some((entry) => {
    const [key, value] = entry.split("=");
    return /token|secret|password|credential|api[_-]?key/i.test(key || "")
      && value && !value.includes("://");
  });
}

async function previewMcp() {
  let payload;
  try {
    payload = mcpPayload();
  } catch (err) {
    $("#mcp-diff").hidden = true;
    $("#mcp-diff").textContent = "";
    $("#mcp-error").textContent = err.message;
    mcpPreviewToken = null;
    $("#mcp-save").disabled = true;
    return;
  }
  const token = JSON.stringify(payload);
  const data = await api("mcp-server/add", Object.assign({ dry_run: true }, payload));
  // The form changed while the request was inflight: the response describes a
  // payload nobody previewed, so ignore it and keep Save disabled.
  if (mcpToken() !== token) {
    $("#mcp-save").disabled = true;
    return;
  }
  const problem = showDiff($("#mcp-diff"), data);
  $("#mcp-error").textContent = problem || "";
  // Save unlocks only after a successful preview of the exact current values.
  mcpPreviewToken = problem ? null : token;
  $("#mcp-save").disabled = Boolean(problem);
}

async function saveMcp() {
  if ($("#mcp-save").disabled) return;
  let body;
  try {
    body = mcpPayload();
  } catch (err) {
    $("#mcp-save").disabled = true;
    $("#mcp-error").textContent = err.message;
    return;
  }
  // Belt and braces: only the exact previewed payload may be acted on.
  if (JSON.stringify(body) !== mcpPreviewToken) {
    $("#mcp-save").disabled = true;
    $("#mcp-error").textContent = "The form changed since the preview. Preview the diff again.";
    return;
  }
  if (!body.name) {
    $("#mcp-error").textContent = "A server name is required.";
    return;
  }
  if (looksLikeSecretValue(body)
    && !confirm("Some values look like secret values, not references (e.g. native://group/name). "
      + "Muster stores them verbatim and only resolves references at apply time. Save anyway?")) return;
  if (!confirm(`Save MCP server ${body.name}?`)) return;
  const data = await api("mcp-server/add", Object.assign({ confirm: true }, body));
  if (data.ok) {
    mcpPreviewToken = null;
    $("#mcp-error").textContent = "";
    await show("profiles");
  } else {
    $("#mcp-error").textContent = failMessage(data);
  }
}

function openMcpEdit(server) {
  const s = server.server || {};
  // JSON arrays fill the fields verbatim: " padded ", "" and commas survive.
  $("#mcp-name").value = server.name || "";
  $("#mcp-command").value = s.command || "";
  $("#mcp-url").value = s.url || "";
  $("#mcp-args").value = JSON.stringify(s.args || [], null, 2);
  $("#mcp-env").value = JSON.stringify(
    Object.entries(s.env || {}).map(([k, v]) => `${k}=${v}`), null, 2);
  $("#mcp-header").value = JSON.stringify(
    Object.entries(s.headers || {}).map(([k, v]) => `${k}=${v}`), null, 2);
  // Any earlier preview was for other values: clear its state and lock Save.
  $("#mcp-error").textContent = "";
  mcpPreviewToken = null;
  $("#mcp-diff").hidden = true;
  $("#mcp-diff").textContent = "";
  $("#mcp-save").disabled = true;
  $("#mcp-name").focus();
}

show("dashboard");
