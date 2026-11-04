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

// The view load in progress. When the panel cannot talk to muster at all
// (server gone, session expired), the failure is recorded on the load that
// sent the request, so a stale request never marks a newer view as failed.
// A failed view shows the problem instead of empty tables, which would
// wrongly read as "nothing configured".
let loadAttempt = { failure: null };

// While the server is unreachable (restarting, laptop asleep) the panel shows
// a "Reconnecting" banner and probes with exponential backoff; once the server
// answers, the banner goes away and the current view reloads by itself. A 401
// is different: the server is up but the session is gone, so retrying cannot
// help and the message tells the user to reopen the panel.
let reconnectTimer = null;
let reconnectDelay = 1000;
const RECONNECT_MAX = 15000;

function setConnBanner(message) {
  const banner = $("#conn");
  if (!banner) return;
  banner.hidden = !message;
  banner.textContent = message || "";
}

function startReconnect() {
  if (reconnectTimer !== null) return;
  reconnectDelay = 1000;
  setConnBanner("Reconnecting to Muster…");
  const probe = async () => {
    reconnectTimer = null;
    let status = 0;
    try {
      status = (await fetch("/api/status")).status;
    } catch (err) {
      reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX);
      setConnBanner(`Muster is not reachable. Retrying in ${Math.round(reconnectDelay / 1000)}s…`);
      reconnectTimer = setTimeout(probe, reconnectDelay);
      return;
    }
    if (status === 401 || status === 403) {
      setConnBanner("Session expired. Reopen the panel with `muster ui`.");
      return;
    }
    setConnBanner("");
    await show(current);
  };
  reconnectTimer = setTimeout(probe, reconnectDelay);
}

// Background calls pass `quiet` so they never rewrite the "CLI equivalent"
// footer that belongs to the view or the action the user is looking at.
async function api(path, body, quiet) {
  const options = body === undefined
    ? {}
    : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  const attempt = loadAttempt;
  let response;
  try {
    response = await fetch("/api/" + path, options);
  } catch (err) {
    attempt.failure = "Muster is not reachable. Is `muster ui` still running? (" + err.message + ")";
    startReconnect();
    return { ok: false, error: attempt.failure, diagnostics: [] };
  }
  // Any answer means the server is back: drop a pending probe and its banner
  // (e.g. the user pressed Retry before the backoff timer fired).
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
    setConnBanner("");
  }
  const data = await response.json().catch(() => ({ ok: false, diagnostics: [] }));
  if (response.status === 401 || response.status === 403) {
    attempt.failure = "Session expired. Reopen the panel with `muster ui`.";
    setConnBanner(attempt.failure);
    return { ok: false, error: attempt.failure, diagnostics: [] };
  }
  if (!quiet && body !== undefined) showCli(data.cli);
  return data;
}

// Backends send RFC 3339 stamps with nanoseconds; show them as local time.
function formatTime(value) {
  if (value === null || value === undefined || value === "") return "";
  const text = String(value);
  const d = new Date(text.replace(/(\.\d{3})\d+/, "$1"));
  return Number.isNaN(d.getTime()) ? text : d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

function timeCell(value) {
  if (!value) return el("span", { class: "muted" }, "—");
  return el("time", { class: "data", datetime: String(value), title: String(value) }, formatTime(value));
}

// Non-blocking status message; replaces alert() for failures.
let toastTimer = null;
function hideToast() {
  const toast = $("#toast");
  if (!toast) return;
  toast.hidden = true;
  try { if (toast.hidePopover) toast.hidePopover(); } catch (err) { /* not open */ }
}
function toast(message, kind, sticky) {
  const box = $("#toast");
  if (!box) return;
  box.setAttribute("class", "toast " + (kind || "error"));
  box.replaceChildren(el("span", {}, message), button("Dismiss", hideToast));
  box.hidden = false;
  // A modal <dialog> lives in the top layer, above any z-index; a manual
  // popover joins the top layer too so the toast stays visible over it.
  try {
    box.setAttribute("popover", "manual");
    if (box.hidePopover) box.hidePopover();
    if (box.showPopover) box.showPopover();
  } catch (err) { /* popover unsupported: z-index fallback applies */ }
  clearTimeout(toastTimer);
  // Errors and actionable instructions stay until dismissed.
  if (!sticky && (kind || "error") === "info") toastTimer = setTimeout(hideToast, 6000);
}

// Disable a control while its request is in flight; `gate` says whether it
// must stay disabled afterwards (preview-gated Save buttons).
const pendingLabels = { "Sync now": "Syncing...", "Enable": "Enabling..." };
async function busy(node, run, gate) {
  const wasFocused = document.activeElement === node;
  const plain = node.children && node.children.length === 0;
  const original = plain ? node.textContent : null;
  node.disabled = true;
  node.setAttribute("aria-busy", "true");
  if (plain && original) node.textContent = pendingLabels[original.trim()] || "Working...";
  try {
    return await run();
  } finally {
    node.setAttribute("aria-busy", "false");
    if (plain && original && node.textContent !== original) node.textContent = original;
    node.disabled = gate ? gate() : false;
    // A re-render may have replaced the button: keep keyboard focus in the page.
    if (wasFocused && !node.isConnected) {
      const replacement = document.activeElement === document.body || !document.activeElement;
      const main = $("#main");
      if (replacement && main) main.focus();
    }
  }
}

// A response's diagnostics, with a failed request added as an error row.
function collectDiagnostics(data) {
  const list = (data.diagnostics || []).slice();
  if (data.error) list.push({ severity: "error", code: "http", message: data.error });
  return list;
}

function diagnosticsTable(list) {
  const tone = { error: "danger", warning: "warning" };
  return table(["Severity", "Code", "Message", "Suggestion"],
    list.map((d) => [el("span", { class: "badge " + (tone[d.severity] || "") }, d.severity),
      el("code", { class: "diag-code" }, d.code), d.message, d.suggestion || ""]));
}

function diagnostics(data) {
  const list = collectDiagnostics(data);
  return list.length ? diagnosticsTable(list) : null;
}

function table(headers, rows, empty, cls) {
  if (!rows.length) return empty instanceof Node ? empty : el("p", { class: "empty" }, empty || "Nothing to show.");
  const isActions = (cell) => cell instanceof Node
    && (cell.tagName === "BUTTON" || (cell.getAttribute("class") || "").includes("actions"));
  const label = headers.filter(Boolean).join(", ");
  return el("div", { class: "table-wrap" + (cls ? " " + cls : ""), role: "region", "aria-label": label },
    el("table", {},
      el("thead", {}, el("tr", {}, headers.map((h) =>
        el("th", { scope: "col" }, h || el("span", { class: "sr-only" }, "Actions"))))),
      el("tbody", {}, rows.map((row) => el("tr", {}, row.map((cell) =>
        el("td", isActions(cell) ? { class: "cell-actions" } : {}, cell)))))));
}

// Diff rendering shared by the plan preview and every edit preview.
function diffPre(diff) {
  return el("pre", { class: "diff" }, diff || "(no changes)");
}

function failMessage(data) {
  return (data.diagnostics || []).map((d) => d.message).join("\n") || data.error || "failed";
}

// Every panel button is disabled while its handler is in flight, so a double
// click cannot submit twice. `gate` keeps preview-gated Save buttons locked.
function button(label, onclick, cls, gate) {
  const node = el("button", { type: "button", class: "btn " + (cls || ""),
    "data-action": String(label).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") }, label);
  node.addEventListener("click", async () => {
    if (node.disabled || node.dataset.locked === "1") return undefined;
    // A handler that asks for confirmation (act) claims the trigger and shows
    // progress only once the request is really in flight.
    const ctx = { node, gate, claimed: false };
    node.dataset.locked = "1";
    triggerCtx = ctx;
    let pending;
    try {
      pending = onclick();
    } catch (err) {
      pending = Promise.reject(err);
    } finally {
      triggerCtx = null;
    }
    try {
      if (ctx.claimed) return await pending;
      return await busy(node, () => pending, gate);
    } finally {
      delete node.dataset.locked;
    }
  });
  return node;
}
let triggerCtx = null;

// In-app confirmation: an accessible modal <dialog> (focus trapped on Confirm, Escape cancels).
function askConfirm(message, title, actionLabel) {
  if (typeof document === "undefined" || !document.body || typeof document.body.appendChild !== "function") {
    return Promise.resolve(typeof confirm === "function" ? confirm(message) : true);
  }
  return new Promise((resolve) => {
    const opener = document.activeElement;
    const ok = el("button", { type: "button", class: "btn primary", id: "confirm-ok" }, actionLabel || "Confirm");
    const cancel = el("button", { type: "button", class: "btn", id: "confirm-cancel" }, "Cancel");
    const dlg = el("dialog", { class: "confirm-dialog", "aria-labelledby": "confirm-title", "aria-describedby": "confirm-desc" },
      el("h2", { id: "confirm-title" }, title || "Please confirm"),
      el("p", { class: "confirm-message", id: "confirm-desc" }, message),
      el("div", { class: "actions confirm-actions" }, cancel, ok));
    let result = false;
    ok.addEventListener("click", () => { result = true; dlg.close(); });
    cancel.addEventListener("click", () => dlg.close());
    dlg.addEventListener("keydown", (e) => {
      if (e.key === "Escape") { e.preventDefault(); dlg.close(); return; }
      if (e.key !== "Tab") return;
      const first = e.shiftKey ? ok : cancel;
      const last = e.shiftKey ? cancel : ok;
      if (document.activeElement === last) { e.preventDefault(); first.focus(); }
    });
    dlg.addEventListener("close", () => {
      dlg.remove();
      if (opener && opener.isConnected && typeof opener.focus === "function") opener.focus();
      resolve(result);
    });
    document.body.appendChild(dlg);
    if (typeof dlg.showModal === "function") dlg.showModal();
    ok.focus();
  });
}
async function openConfirm({ title, message, onConfirm }) {
  const confirmed = await askConfirm(message, title);
  if (confirmed && onConfirm) return onConfirm();
  return confirmed;
}

// Stable identity of the focused control, so focus can return to its replacement.
function focusKey() {
  const a = document.activeElement;
  if (!a || a === document.body) return null;
  if (a.id) return "#" + CSS.escape(a.id);
  if (a.dataset && a.dataset.action) return `[data-action="${a.dataset.action}"]`;
  return null;
}
function restoreFocus(key) {
  const target = key && document.querySelector(key);
  if (target && typeof target.focus === "function") { target.focus(); return; }
  const main = $("#main");
  if (main) main.focus();
}

// Run a state-changing command after an explicit confirmation, then reload.
async function act(question, path, body, trigger) {
  const ctx = trigger ? { node: trigger } : triggerCtx;
  triggerCtx = null;
  if (ctx && !trigger) ctx.claimed = true;
  const key = focusKey();
  const label = ctx && ctx.node.textContent ? ctx.node.textContent.trim() : "";
  if (!(await askConfirm(question, label || undefined, label || undefined))) return;
  const run = async () => {
    const data = await api(path, Object.assign({ confirm: true }, body || {}));
    if (!data.ok) toast(failMessage(data));
    await show(current);
    restoreFocus(key);
    // The reload shows the view's own command; the action just run is what the
    // user wants to learn from, so it takes the footer back.
    if (data.cli) showCli(data.cli);
    return data;
  };
  return ctx ? busy(ctx.node, run, ctx.gate) : run();
}

// A labelled form control used by the inline admin forms.
function labelled(label, control) {
  return el("label", { class: "field" }, label + " ", control);
}

function textInput(label, attrs) {
  return el("input", Object.assign({ type: "text", autocomplete: "off", "aria-label": label }, attrs || {}));
}

function selectInput(label, values) {
  return el("select", { "aria-label": label }, values.map((v) => el("option", { value: v }, v || "(none)")));
}

// One-line description under each page title.
const descriptions = {
  "Proposals": "Configuration changes proposed for review before they touch the config repository.",
  "Find context": "Search the text agents receive, to see which file and layer it comes from.",
  "Dashboard": "Fleet health, periodic sync and anything that needs a decision.",
  "Machines": "Devices enrolled in this fleet and requests waiting for approval.",
  "Members & groups": "People who can read the vault, and the groups that scope access.",
  "AI providers": "Endpoints and keys that agents on this fleet receive.",
  "Secrets": "Vault entries by reference. Values stay encrypted until you reveal them.",
  "Skills": "Skills installed for your agents, their sources and update status.",
  "Effective": "The configuration each agent actually receives on this machine.",
  "Profiles & diff preview": "Agent profiles and MCP servers declared in muster.toml.",
  "History & generations": "Applied generations you can roll back to, and the audit trail.",
  "Doctor": "Health checks for this machine and the fleet.",
};

// The small count chip next to the title ("1 machine"); empty hides it.
function setCount(text) {
  const chip = $("#page-count");
  if (!chip) return;
  chip.textContent = text || "";
  chip.hidden = !text;
}

function render(title, actions, ...blocks) {
  $("#title").textContent = title;
  const desc = $("#page-desc");
  if (desc) desc.textContent = descriptions[title] || "";
  setCount("");
  actionsEl.replaceChildren(...actions.flat(Infinity).filter(Boolean));
  viewEl.replaceChildren(...blocks.flat(Infinity).filter(Boolean));
  syncScrollRegions();
}

// A table wrapper joins the tab order only while it really scrolls sideways.
function syncScrollRegions() {
  if (typeof viewEl.querySelectorAll !== "function") return;
  for (const wrap of viewEl.querySelectorAll(".table-wrap")) {
    if (wrap.scrollWidth > wrap.clientWidth) wrap.setAttribute("tabindex", "0");
    else wrap.removeAttribute("tabindex");
  }
}
if (typeof addEventListener === "function") addEventListener("resize", syncScrollRegions);

// Last sync as clean tabular text (the coloured pill is reserved for the Status column).
function lastSyncCell(value) {
  if (!value) return el("span", { class: "muted data" }, "Never");
  const d = new Date(value);
  const text = Number.isNaN(d.getTime()) ? String(value)
    : d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
  return el("time", { class: "data sync-time", datetime: String(value), title: String(value) }, text);
}

// Raw drift reasons from the backend, shown as state (not action) labels.
// Unknown future reasons fall back to a humanized form of the code.
const DRIFT_REASON_LABELS = {
  machines_drifted: "Machines out of sync",
  profile_not_applied: "Profile not applied",
  profile_outdated: "Profile outdated",
  sync_pending: "Sync pending",
  approval_pending: "Awaiting approval",
  skills_missing: "Skills missing",
  subagents_missing: "Subagents missing",
  instructions_missing: "Instructions missing",
  identity_missing: "Identity missing",
  hash_mismatch: "Content mismatch",
  git_missing: "Git not installed",
};
function driftReasonLabel(reason) {
  if (DRIFT_REASON_LABELS[reason]) return DRIFT_REASON_LABELS[reason];
  const text = String(reason || "").replace(/_/g, " ");
  return text.charAt(0).toUpperCase() + text.slice(1);
}

// Friendly empty state: a title and a helper line (strings or nodes).
function emptyCard(title, ...help) {
  return el("div", { class: "empty-card" }, el("strong", {}, title), el("p", {}, ...help));
}

// Machine identity shared by Dashboard and Machines: name over platform, plus an optional status pill.
const MACHINE_STATUS_KIND = { current: "live", active: "live", enrolled: "idle", pending: "wait", drifted: "wait", revoked: "alert" };
function machineState(status) {
  return el("span", { class: "state state-" + (MACHINE_STATUS_KIND[status] || "idle") },
    el("span", { class: "state-dot", "aria-hidden": "true" }), sentenceCase(status));
}

// Display-only: capitalise the first letter of a status label; data values stay as sent.
function sentenceCase(text) {
  const s = String(text);
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : s;
}

// Section header shared by Dashboard and Machines: h2, optional count pill, same rhythm everywhere.
function sectionHead(title, id, badge) {
  return el("div", { class: "section-head" }, el("h2", id ? { id } : {}, title), badge || null);
}

function machineIdentity(m, current) {
  return el("div", { class: "name-cell" },
    el("div", { class: "name-line" },
      el("span", { class: "board-name" }, m.name),
      current ? el("span", { class: "badge info" }, "this machine") : null),
    el("code", { class: "data muted" }, m.platform || "unknown platform"));
}

const views = {
  async dashboard(token) {
    const [data, schedule] = await Promise.all([api("status"), api("schedule")]);
    if (token !== undefined && token !== navToken) return;
    // Both fetches race to set the footer hint; the dashboard's own command wins.
    showCli("muster status");
    const r = data.result || {};
    const machines = r.machines || [];
    const pending = (r.pending && r.pending.members) || [];
    const drift = r.drift || {};
    const driftCount = (drift.members || []).length + (drift.machines || []).length;
    const diags = [...collectDiagnostics(data), ...collectDiagnostics(schedule)];
    const flagged = diags.filter((d) => d.severity === "warning" || d.severity === "error").length;
    const attentionCount = pending.length + driftCount + flagged;
    const s = schedule.result || {};
    const defaultInterval = s.interval_minutes || 60;
    const interval = el("input", { type: "number", min: "1", value: String(defaultInterval), "aria-label": "Minutes between syncs" });
    const intervalError = el("span", { id: "ops-interval-error", class: "error-text", role: "alert" });
    const clearIntervalError = () => {
      interval.removeAttribute("aria-invalid");
      interval.removeAttribute("aria-errormessage");
      intervalError.textContent = "";
    };
    interval.addEventListener("input", clearIntervalError);
    const enableBtn = button(s.enabled ? "Update" : "Enable", () => {
      const raw = interval.value.trim();
      const val = Number(raw);
      if (!raw || !Number.isInteger(val) || val < 1) {
        interval.setAttribute("aria-invalid", "true");
        interval.setAttribute("aria-errormessage", "ops-interval-error");
        intervalError.textContent = "Enter a whole number of minutes, 1 or more.";
        interval.focus();
        toast("Please enter a valid interval of 1 or more minutes", "error", true);
        return undefined;
      }
      clearIntervalError();
      return act(`Enable periodic sync every ${val} minute(s)?`, "schedule/enable", { interval: val });
    });
    enableBtn.id = "ops-enable";
    const state = (kind, label) => el("span", { class: "state state-" + kind },
      el("span", { class: "state-dot", "aria-hidden": "true" }), sentenceCase(label));
    const resultKind = (text) => (/^(ok|success|synced|applied|current)/i.test(text) ? "live"
      : /fail|error|drift|denied|revoked/i.test(text) ? "alert"
        : /pending|waiting/i.test(text) ? "wait" : "idle");
    const plate = (label, figure, sub, attention, fallback) => el("div", { class: "plate" + (attention ? " plate-attention" : "") },
      el("dt", { class: "plate-label" }, label),
      el("dd", { class: "plate-figure num" + (fallback ? " is-fallback" : "") }, figure),
      el("dd", { class: "plate-sub" }, sub));

    // Last sync: the local time of r.sync.ts as the figure, the date and result as its state.
    const syncDate = r.sync ? new Date(r.sync.ts) : null;
    const syncValid = Boolean(syncDate) && !Number.isNaN(syncDate.getTime());
    const syncFigure = !r.sync ? "Never"
      : el("time", { datetime: r.sync.ts, title: r.sync.ts },
        syncValid ? syncDate.toLocaleTimeString(undefined, { timeStyle: "short" }) : String(r.sync.ts));
    const syncSub = !r.sync ? "No sync recorded"
      : [syncValid ? syncDate.toLocaleDateString(undefined, { dateStyle: "medium" }) : "",
        r.sync.result ? state(resultKind(String(r.sync.result)), r.sync.result) : null];

    const byStatus = {};
    for (const m of machines) byStatus[m.status] = (byStatus[m.status] || 0) + 1;
    const fleetSub = machines.length
      ? Object.entries(byStatus).map(([status, count]) => `${count} ${status}`).join(", ")
      : "None enrolled";
    const attentionSub = attentionCount
      ? [pending.length ? `${pending.length} pending` : null,
        driftCount ? "Drift detected" : null,
        flagged ? `${flagged} ${flagged === 1 ? "diagnostic" : "diagnostics"}` : null].filter(Boolean).join(", ")
      : "Fleet converged";

    const fleet = machines.length
      ? table(["Machine", "Version", "Status", "Last sync"],
        machines.map((m) => [
          machineIdentity(m, m.is_current),
          el("code", { class: "data" }, m.version || "—"),
          machineState(m.status),
          lastSyncCell(m.last_sync)]))
      : el("div", { class: "empty-state" },
        el("p", {}, "No machines in fleet. Run ", el("code", {}, "muster machine add"), " to enroll one, or open the Machines tab."),
        button("Open Machines", () => show("machines")));
    if (machines.length) fleet.setAttribute("class", "table-wrap board fleet-table");

    const periodicText = s.enabled
      ? (s.interval_minutes ? `Every ${s.interval_minutes} min` : "Enabled") + (s.backend ? ` via ${s.backend}` : "")
      : "Off";

    const driftRows = [
      ...(drift.members || []).map((d) => [sentenceCase("member"), el("code", {}, d.name), el("span", { class: "reason" }, driftReasonLabel(d.reason))]),
      ...(drift.machines || []).map((d) => [sentenceCase("machine"), el("code", {}, d.name), el("span", { class: "reason" }, driftReasonLabel(d.reason))]),
    ];
    const attentionBlocks = [
      pending.length
        ? [el("h3", {}, `Pending approvals (${pending.length})`),
          table(["Name"], pending.map((p) => [el("code", {}, typeof p === "string" ? p : p.name)]), null, "narrow")]
        : null,
      driftCount
        ? table(["Kind", "Name", "Reason"], driftRows, null, "narrow drift-table")
        : null,
      diags.length
        ? [el("h3", {}, `Diagnostics (${diags.length})`), diagnosticsTable(diags)]
        : null,
    ].filter(Boolean);

    const attentionSection = el("section", { class: "attention", "aria-labelledby": "attention-title" },
        sectionHead("Needs attention", "attention-title",
          attentionCount ? el("span", { class: "badge warning count", "aria-label": `${attentionCount} items` }, String(attentionCount)) : null),
        el("p", { class: "section-desc" }, attentionBlocks.length ? "Drift, pending approvals and diagnostics that need a decision." : "Pending approvals, drift and diagnostics appear here."),
        attentionBlocks.length
          ? attentionBlocks
          : el("p", { class: "quiet", role: "status" }, "Nothing needs attention: no pending approvals, no drift and no diagnostics."));
    render("Dashboard", [button("Sync now", () => act("Pull, plan and apply now?", "sync"), "primary")],
      el("dl", { class: "plates", "aria-label": "Fleet status" },
        plate("Machines", String(machines.length), fleetSub),
        plate("Needs attention", String(attentionCount), attentionSub, attentionCount > 0),
        plate("Last sync", syncFigure, syncSub, false, !r.sync)),
      driftCount > 0 ? attentionSection : null,
      sectionHead("Fleet"),
      fleet,
      sectionHead("Sync operations", "ops-title"),
      el("section", { class: "ops", "aria-labelledby": "ops-title" },
        el("div", { class: "ops-periodic" },
         el("div", { class: "ops-info" },
          el("div", { class: "ops-heading" },
            el("span", { class: "ops-title" }, "Periodic sync"),
            state(s.enabled ? "live" : "idle", periodicText)),
          el("span", { class: "ops-desc" }, "Run periodic background sync on this machine")),
         el("div", { class: "ops-form" },
          el("label", { class: "ops-interval" },
            interval, el("span", { class: "muted" }, "min")),
          intervalError,
          el("div", { class: "ops-controls" },
            enableBtn,
            s.enabled ? button("Disable", () => act("Disable periodic sync?", "schedule/disable")) : "")))),
      driftCount > 0 ? null : attentionSection);
    setCount(`${machines.length} ${machines.length === 1 ? "machine" : "machines"}`);
  },

  async providers(token) {
    const data = await api("providers");
    if (token !== undefined && token !== navToken) return;
    showCli("muster provider list");
    const providers = (data.result || {}).providers || [];
    const field = (id, label, attrs) => el("label", { class: "field", for: id }, label + " ",
      el("input", Object.assign({ id, type: "text", autocomplete: "off" }, attrs || {})));
    const kind = el("select", { id: "provider-kind" },
      ["anthropic", "openai", "openai-compatible", "ollama", "google"].map((value) => el("option", { value }, value)));
    const kindField = el("label", { class: "field", for: "provider-kind" }, "Kind ", kind);
    const form = el("form", { id: "provider-form", class: "skill-form" },
      field("provider-id", "ID", { required: "", pattern: "[a-z0-9-]+" }),
      kindField,
      field("provider-url", "Base URL", { type: "url" }),
      field("provider-model", "Default model"),
      field("provider-key", "Key reference (not a key value)", { placeholder: "native://app/key or env://VARIABLE" }),
      el("button", { type: "submit", class: "btn primary" }, "Add provider"));
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const body = { id: $("#provider-id").value.trim(), kind: kind.value };
      for (const [name, id] of [["base-url", "provider-url"], ["default-model", "provider-model"], ["key", "provider-key"]]) {
        const value = $("#" + id).value.trim();
        if (value) body[name] = value;
      }
      const submit = form.querySelector("button");
      await act("Add or replace this AI provider?", "provider/add", body, submit);
    });

    const agentList = ["claude", "codex", "opencode", "pi", "hermes", "antigravity"];
    const useProv = selectInput("Provider", providers.map((p) => p.id));
    const useAgent = selectInput("Agent", agentList);
    const useForm = el("div", { class: "skill-form" },
      labelled("Provider", useProv),
      labelled("Agent", useAgent),
      button("Set as default", () => {
        if (!useProv.value) return toast("Select a provider.");
        return act(`Set ${useProv.value} as default provider for ${useAgent.value}?`, "provider/use", { id: useProv.value, agent: useAgent.value });
      }, "primary"),
      button("Clear default", () => {
        return act(`Clear default provider for ${useAgent.value}?`, "provider/use", { agent: useAgent.value, clear: true });
      }));

    render("AI providers", [],
      table(["ID", "Kind", "Base URL", "Default model", "Key reference", "Receiving agents", ""],
        providers.map((p) => [p.id, p.kind, p.base_url || "", p.default_model || "", p.key || "", (p.agents || []).join(", "),
          button("Remove", () => act(`Remove provider ${p.id}?`, "provider/rm", { id: p.id }), "danger")]),
        "No providers configured."),
      el("h2", {}, "Default provider for agent"),
      useForm,
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
    const tags = (list) => ((list || []).length
      ? el("div", { class: "tags" }, list.map((a) => el("span", { class: "tag" }, a)))
      : el("span", { class: "muted" }, "—"));
    const hasRole = machines.some((m) => m.role);
    const hasProfile = machines.some((m) => m.profile);
    const hasActions = machines.some((m) => !m.is_current);
    const nameCell = (m, current) => machineIdentity(m, current);
    const statusCell = (m) => (m.status ? machineState(m.status) : el("span", { class: "muted" }, "—"));
    const enrollCommand = "muster machine add <host>";
    const enroll = button("Copy enroll command", async () => {
      let copied = false;
      try {
        await navigator.clipboard.writeText(enrollCommand);
        copied = true;
      } catch (err) { /* clipboard unavailable: the command is still shown in the toast */ }
      toast((copied ? "Copied: " : "Run on the new machine: ") + enrollCommand, "info", true);
    }, "primary");
    enroll.title = "Copies " + enrollCommand;
    const assignMachine = selectInput("Machine", machines.map((m) => m.name));
    const assignRole = textInput("Role", { placeholder: "role (optional)" });
    const assignProfile = textInput("Profile", { placeholder: "profile (optional)" });
    const assignForm = machines.length
      ? el("div", { class: "skill-form" },
        labelled("Machine", assignMachine), labelled("Role", assignRole), labelled("Profile", assignProfile),
        button("Assign", () => {
          const body = { name: assignMachine.value };
          if (assignRole.value.trim()) body.role = assignRole.value.trim();
          if (assignProfile.value.trim()) body.profile = assignProfile.value.trim();
          if (!body.role && !body.profile) return toast("Enter a role or a profile to assign.");
          return act(`Assign ${[body.role && "role " + body.role, body.profile && "profile " + body.profile].filter(Boolean).join(" and ")} to ${body.name}?`, "machine/assign", body);
        }, "primary"))
      : null;
    render("Machines", [enroll],
      sectionHead("Enrolled"),
      table(["Machine", "Version", hasRole ? "Role" : null, hasProfile ? "Profile" : null, "Agents", "Status", "Last sync", hasActions ? "Actions" : null].filter((h) => h !== null),
        machines.map((m) => [nameCell(m, m.is_current),
          el("code", { class: "data" }, m.version || "—"),
          hasRole ? (m.role || el("span", { class: "muted" }, "—")) : null,
          hasProfile ? (m.profile || el("span", { class: "muted" }, "—")) : null,
          tags(m.agents),
          statusCell(m),
          lastSyncCell(m.last_sync),
          hasActions ? (m.is_current ? "" : button("Remove", () => act(`Remove machine ${m.name}?`, "machine/rm", { name: m.name }), "danger")) : null].filter((c) => c !== null)),
        emptyCard("No machines enrolled yet", "Run ", el("code", {}, "muster init"), " on this machine, then ", el("code", {}, "muster machine add"), " to enroll more."),
        "machines-table" + (!hasRole && !hasProfile ? " machines-basic" : "") + (hasActions ? " actions-end" : "")),
      assignForm ? [sectionHead("Assign role or profile"), assignForm] : null,
      sectionHead("Pending enrollment requests"),
      el("p", { class: "section-desc" }, "Machines waiting for approval before they can join."),
      table(["Machine", "Version", "Agents", "Approvals", ""],
        pending.map((p) => [nameCell(p, false), el("code", { class: "data" }, p.version || "—"),
          tags(p.agents), quorum(p),
          el("span", { class: "actions" },
            button("Approve", () => act(`Approve enrollment of machine ${p.name}?`, "approve", { name: p.name })),
            button("Reject", () => act(`Reject the enrollment request of machine ${p.name}? Its recorded approvals are discarded.`, "reject", { name: p.name }), "danger"))]),
        emptyCard("No pending requests", "New machines run ", el("code", {}, "muster init"), " to request enrollment.")),
      diagnostics(data));
    setCount(`${machines.length} ${machines.length === 1 ? "machine" : "machines"}`
      + (pending.length ? `, ${pending.length} pending` : ""));
  },

  async team(token) {
    const [members, groups] = await Promise.all([api("members"), api("groups")]);
    if (token !== undefined && token !== navToken) return;
    showCli("muster member list");
    const mList = (members.result && members.result.members) || [];
    const gList = (groups.result && groups.result.groups) || [];

    const memberRows = mList.map((m) => {
      const actions = [];
      if (m.status === "pending") {
        actions.push(button("Approve", () => act(`Approve member ${m.name}?`, "member/approve", { name: m.name })));
        actions.push(button("Reject", () => act(`Reject member ${m.name}? Their invitation and recorded approvals are discarded.`, "member/reject", { name: m.name }), "danger"));
      } else {
        const nextRole = m.role === "admin" ? "member" : "admin";
        actions.push(button("Set profile", () => {
          const next = prompt(`Profile for ${m.name} (empty to clear):`, m.profile || "");
          if (next === null) return undefined;
          return act(`${next.trim() ? "Assign profile " + next.trim() : "Clear profile"} for ${m.name}?`, "member/profile", { name: m.name, profile: next.trim() });
        }));
        actions.push(button(`Make ${nextRole}`, () => act(`Change role of ${m.name} to ${nextRole}?`, "member/role", { name: m.name, role: nextRole })));
        actions.push(button("Remove", () => act(`Revoke member ${m.name} and all their machines?`, "member/rm", { name: m.name }), "danger"));
      }
      return [
        m.name, m.role, m.status, (m.groups || []).join(", "), m.profile || "",
        el("span", { class: "actions" }, actions),
      ];
    });

    const formatReaders = (readers) => {
      if (!readers || typeof readers !== "object") return "";
      const parts = [];
      const known = new Set(["machines", "members", "groups", "recipients"]);
      for (const key of ["machines", "members", "groups", "recipients"]) {
        const val = readers[key];
        if (Array.isArray(val) && val.length) parts.push(`${key}: ${val.join(", ")}`);
      }
      for (const [k, v] of Object.entries(readers)) {
        if (known.has(k)) continue;
        if (v && (!Array.isArray(v) || v.length)) {
          parts.push(`${k}: ${Array.isArray(v) ? v.join(", ") : v}`);
        }
      }
      return parts.join("; ") || "none";
    };

    const groupRows = gList.map((g) => {
      const gName = g.name || g.group || "";
      return [
        gName, formatReaders(g.readers), g.profile || "",
      ];
    });

    // Add member form
    const newMemberName = textInput("Member name", { placeholder: "alice", pattern: "[a-z0-9-]+" });
    const newMemberRole = selectInput("Role", ["member", "admin"]);
    const newMemberProfile = textInput("Profile (optional)", { placeholder: "profile (optional)" });
    const addMemberForm = el("div", { class: "skill-form" },
      labelled("Name", newMemberName),
      labelled("Role", newMemberRole),
      labelled("Profile", newMemberProfile),
      button("Invite member", () => {
        const name = newMemberName.value.trim();
        if (!name) return toast("Enter a member name.");
        const role = newMemberRole.value;
        const profile = newMemberProfile.value.trim();
        return (async () => {
          if (!confirm(`Invite ${name} as ${role}${profile ? " with profile " + profile : ""}?`)) return;
          const added = await api("member/add", { confirm: true, name, role });
          if (!added.ok) toast(failMessage(added));
          else if (profile) {
            const set = await api("member/profile", { confirm: true, name, profile });
            if (!set.ok) toast(failMessage(set));
          }
          await show(current);
        })();
      }, "primary"));

    // Create group form
    const newGroupName = textInput("Group name", { placeholder: "dev", pattern: "[a-z0-9-]+" });
    const newGroupProfile = textInput("Profile (optional)", { placeholder: "profile (optional)" });
    const createGroupForm = el("div", { class: "skill-form" },
      labelled("Group", newGroupName),
      labelled("Profile", newGroupProfile),
      button("Create group", () => {
        const name = newGroupName.value.trim();
        if (!name) return toast("Enter a group name.");
        const body = { group: name };
        if (newGroupProfile.value.trim()) body.profile = newGroupProfile.value.trim();
        return act(`Create group ${name}?`, "group/create", body);
      }, "primary"));

    // Grant / revoke reader form
    const grantGroup = selectInput("Group", gList.map((g) => g.name || g.group || ""));
    const grantReader = textInput("Reader", { placeholder: "alice or vps (member, machine or group)" });
    const grantForm = gList.length
      ? el("div", { class: "skill-form" },
        labelled("Group", grantGroup),
        labelled("Reader", grantReader),
        button("Grant access", () => {
          const reader = grantReader.value.trim();
          if (!reader) return toast("Enter a reader (member, machine or group name).");
          return act(`Grant ${reader} access to group ${grantGroup.value}?`, "group/grant", { group: grantGroup.value, reader });
        }, "primary"),
        button("Revoke access", () => {
          const reader = grantReader.value.trim();
          if (!reader) return toast("Enter a reader to revoke.");
          return act(`Revoke ${reader} access from group ${grantGroup.value}?`, "group/revoke", { group: grantGroup.value, reader });
        }, "danger"))
      : null;

    render("Members & groups", [],
      el("h2", {}, "Members"), table(["Name", "Role", "Status", "Groups", "Profile", ""], memberRows, "No members configured."),
      el("h3", {}, "Invite member"), addMemberForm,
      el("h2", {}, "Groups"), table(["Group", "Readers", "Profile"], groupRows, "No groups configured."),
      el("h3", {}, "Create group"), createGroupForm,
      grantForm ? [el("h3", {}, "Grant or revoke group access"), grantForm] : null,
      diagnostics(members), diagnostics(groups));
  },

  async secrets(token) {
    const [data, requests, leases] = await Promise.all([
      api("secrets"), api("secret/requests"), api("secret/leases"),
    ]);
    if (token !== undefined && token !== navToken) return;
    showCli("muster secret list");
    const secrets = Array.isArray(data.result) ? data.result : [];
    render("Secrets", [
      button("New secret", () => openSecretEdit(), "primary"),
      button("Check providers", async () => {
        const check = await api("secret/check");
        if (!check.ok) toast(failMessage(check));
        else {
          const list = (check.result && check.result.providers) || [];
          const bad = list.filter((p) => p.available === false);
          const summary = `${list.length - bad.length} of ${list.length} secret providers available.`;
          if (bad.length) {
            toast(summary + "\n" + bad.map((p) => `${p.provider}: ${p.message || p.code || "unavailable"}`).join("\n"));
          } else {
            toast(summary, "info");
          }
        }
      }),
    ],
      el("p", {}, "Metadata only. Revealing a value needs explicit confirmation and is recorded."),
      table(["Reference", "Updated", "By", "Readers", ""],
        secrets.map((s) => [s.reference, timeCell(s.updated_at), s.updated_by || "",
          (s.readers || []).map((r) => r.machine || r.recipient).join(", "),
          el("div", { class: "actions" },
            button("Reveal…", () => openReveal(s.reference)),
            button("Change value", () => openSecretEdit(s.reference)),
            button("Delete", () => act(`Delete secret ${s.reference}?`, "secret/rm", { path: s.reference }), "danger"))]),
        "No secrets yet. Use New secret to add one."),
      el("h2", {}, "Secret approvals"),
      el("p", {}, "Approve access for the configured duration. Revoking prevents new reveals; agents may retain keys already received."),
      table(["Pending reference", "Agent", "Requested", ""],
        (requests.result || []).map((r) => [r.reference, r.agent, timeCell(new Date(r.requested_at * 1000).toISOString()),
          el("div", { class: "actions" },
            button("Approve", () => secretApproval("approve", r.id)),
            button("Deny", () => secretApproval("deny", r.id), "danger"))]),
        "No pending requests."),
      el("h3", {}, "Active approvals"),
      table(["Approved reference", "Agent", "Expires", ""],
        (leases.result || []).map((l) => [l.reference, l.agent, timeCell(new Date(l.expires_at * 1000).toISOString()),
          button("Revoke", () => secretApproval("revoke", l.id))]),
        "No active approvals."),
      diagnostics(data), diagnostics(requests), diagnostics(leases));
  },

  async effective(token) {
    const data = await api("effective");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    // `instructions` is a list of sources (user scope plus, with --project,
    // the project scope); a single object is tolerated from older payloads.
    const instructions = Array.isArray(r.instructions)
      ? r.instructions
      : r.instructions && r.instructions.source ? [r.instructions] : [];
    const skills = Array.isArray(r.skills) ? r.skills : [];
    const mcp = Array.isArray(r.mcp) ? r.mcp : [];
    const subagents = Array.isArray(r.subagents) ? r.subagents : [];
    const entries = [
      ...instructions.map((i) => ["Instructions", i.source, i.layer, i]),
      ...skills.map((s) => ["Skill", s.name, s.origin, s]),
      ...mcp.map((s) => ["MCP", s.name, s.layer, s]),
      ...subagents.map((s) => ["Subagent", s.name, s.layer, s]),
    ];
    const extras = r.extra_on_disk || {};
    const totals = r.totals || {};
    const ids = [...new Set([...Object.keys(totals), ...Object.keys(extras),
      ...entries.flatMap((entry) => Object.keys(entry[3].adapters || {}))])].sort();
    const rows = entries.map(([kind, name, origin, entry]) => [kind, name || "", origin || "",
      ...ids.map((id) => {
        const state = (entry.adapters || {})[id]?.state;
        return state ? el("span", { class: "badge " + (state === "drifted" || state === "missing" ? "danger" : "") }, state) : "—";
      })]);
    const layers = Array.isArray(r.layers) ? r.layers : [];
    const warnings = Array.isArray(r.warnings) ? r.warnings : [];
    render("Effective", [],
      el("h2", {}, "Layers"),
      table(["Kind", "Name"], layers.map((layer) => [layer.kind || "", layer.name || ""]), "No layers."),
      el("h2", {}, "Configuration by adapter"),
      table(["Type", "Name", "Origin", ...ids], rows, "No configuration to show."),
      el("h2", {}, "Approximate tokens"),
      table(["Adapter", "Tokens"], Object.entries(totals).map(([id, count]) => [id, count]), "No token totals."),
      el("h2", {}, "Unmanaged extras on disk"),
      table(["Adapter", "Names"], Object.entries(extras).map(([id, names]) => [id, Array.isArray(names) ? names.join(", ") : ""]), "No unmanaged extras."),
      warnings.length ? el("div", {}, el("h2", {}, "Warnings"), el("ul", {}, warnings.map((warning) => el("li", {}, typeof warning === "string" ? warning : JSON.stringify(warning))))) : null,
      diagnostics(data));
  },

  async skills(token) {
    const [data, usage] = await Promise.all([api("skills"), api("usage")]);
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
      // `enabled` is absent from older payloads: treat that as enabled.
      const enabled = s.enabled !== false;
      const actions = [
        enabled
          ? button("Disable", () => act(`Disable skill ${s.name}?`, "disable", { item: `skill:${s.name}` }))
          : button("Enable", () => act(`Enable skill ${s.name}?`, "enable", { item: `skill:${s.name}` })),
        button("Edit", () => openSkillEdit(s.name)),
      ];
      if (s.origin === "local" && (!s.locked || s.modified)) {
        actions.push(button("Pin", () => act(`Approve the current contents of ${s.name} (pin in skills.lock)?`, "skills/pin", { name: s.name })));
      }
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
        timeCell(s.updated_at),
        el("span", { class: "state state-" + (enabled ? "live" : "idle") },
          el("span", { class: "state-dot", "aria-hidden": "true" }), enabled ? "Enabled" : "Disabled"),
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
      button("Check repositories", async () => {
        const checked = await api("skills/check", {});
        if (!checked.ok) toast(failMessage(checked));
        await show("skills");
      }, "primary"),
      button("Update all", () => updateAll()),
    ],
      el("p", {}, r.checked_at ? `Last upstream check: ${formatTime(r.checked_at)}.` : "Upstream sources not checked yet on this machine."),
      table(["Name", "Origin", "Version", "Updated", "State", "Status", "", ""], rows, "No skills yet. Add one below."),
      el("h2", {}, "Usage"),
      (() => {
        const u = usage.result || {};
        const scanned = Object.entries(u.sessions_scanned || {})
          .map(([a, count]) => `${a}: ${count}`)
          .join(", ");
        const unsupp = (Array.isArray(u.sources) ? u.sources : [])
          .filter((s) => s.status === "unsupported")
          .map((s) => s.agent)
          .join(", ");
        const coverageMsg = (Object.values(u.sessions_scanned || {}).reduce((a, b) => a + b, 0) === 0)
          ? "No transcript sessions scanned yet on this machine."
          : `Sessions scanned: ${scanned || "none"}${unsupp ? ` (unsupported: ${unsupp})` : ""}.`;
        const items = (u.items || []).filter((it) => it.uses > 0).map((it) => [it.kind, it.name, String(it.uses), timeCell(it.last_used)]);
        const unused = (u.unused || []).map((it) => [it.kind, it.name, it.suggestion || "—"]);
        return el("div", { class: "usage-section" },
          el("p", { class: "coverage-note" }, coverageMsg),
          el("h3", {}, "Active items"),
          table(["Kind", "Name", "Uses", "Last used"], items, "No active item usage recorded."),
          el("h3", {}, "Unused items"),
          table(["Kind", "Name", "Suggestion"], unused, "No unused items flagged for pruning."));
      })(),
      diagnostics(usage),
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
      ]), "No files in this plan."),
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
      ]), "No profiles yet."),
      diagnostics(listData),
      el("h2", {}, "MCP servers"),
      table(["Name", "Transport", "Target", "Env / headers", "State", ""], servers.map((s) => {
        const server = s.server || {};
        const settings = Object.entries(server.env || {}).map(([k, v]) => `${k}=${v}`)
          .concat(Object.entries(server.headers || {}).map(([k, v]) => `${k}=${v}`));
        const enabled = s.enabled !== false;
        const actions = [
          enabled
            ? button("Disable", () => act(`Disable MCP server ${s.name}?`, "disable", { item: `mcp:${s.name}` }))
            : button("Enable", () => act(`Enable MCP server ${s.name}?`, "enable", { item: `mcp:${s.name}` })),
          button("Edit", () => openMcpEdit(s)),
          button("Remove", () => act(`Remove MCP server ${s.name}?`, "mcp-server/rm", { name: s.name }), "danger"),
        ];
        return [s.name, s.transport, server.command || server.url || "", settings.join(", "),
          el("span", { class: "state state-" + (enabled ? "live" : "idle") },
            el("span", { class: "state-dot", "aria-hidden": "true" }), enabled ? "Enabled" : "Disabled"),
          el("span", { class: "actions" }, actions)];
      }), "No MCP servers declared."),
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
        g.number, timeCell(g.created_at), g.rollback_of ? `${g.kind} of ${g.rollback_of}` : g.kind, (g.files || []).length,
        button("Roll back", () => act(`Restore generation ${g.number}?`, "rollback", { generation: g.number })),
      ]), "No generations yet."),
      el("h2", {}, "Events"),
      table(["When", "Action", "Subject"], (r.events || []).slice().reverse().map((e) => [timeCell(e.at), e.action, e.subject]), "No events recorded."),
      diagnostics(data));
  },

  async proposals(token) {
    const data = await api("proposals");
    if (token !== undefined && token !== navToken) return;
    const list = Array.isArray(data.result) ? data.result : [];
    const path = textInput("File path", { placeholder: "skills/review/SKILL.md" });
    const message = textInput("Message", { placeholder: "Why this change (optional)" });
    const contents = el("textarea", { rows: "6", spellcheck: "false", "aria-label": "Proposed file contents", placeholder: "Proposed file contents" });
    const form = el("div", { class: "skill-form" },
      labelled("Path", path), labelled("Message", message), labelled("Contents", contents),
      button("Propose change", () => {
        if (!path.value.trim()) return toast("Enter the file path in the config repository.");
        const body = { path: path.value.trim(), contents: contents.value };
        if (message.value.trim()) body.message = message.value.trim();
        return act(`Record a proposal for ${body.path}?`, "proposal/create", body);
      }, "primary"));
    render("Proposals", [],
      table(["ID", "Path", "Message", "Created", ""], list.map((p) => [
        el("code", {}, p.id || ""), p.path || "", p.message || "", timeCell(p.created_at),
        button("Accept", () => act(`Apply proposal ${p.id} to the config repository?`, "proposal/accept", { id: p.id }), "primary"),
      ]), "No pending proposals."),
      el("h2", {}, "New proposal"), form,
      diagnostics(data));
    setCount(list.length ? `${list.length} pending` : "");
  },

  async why(token) {
    const query = textInput("Text to find", { placeholder: "text or regular expression" });
    const regex = el("input", { type: "checkbox", "aria-label": "Regular expression" });
    const results = el("div", { class: "why-results" }, el("p", { class: "quiet" }, "Enter text to find where agents get it from."));
    const run = async () => {
      const text = query.value.trim();
      if (!text) return;
      const params = new URLSearchParams({ text });
      if (regex.checked) params.set("regex", "true");
      const data = await api("why?" + params.toString());
      showCli(data.cli || `muster why ${text}`);
      const matches = ((data.result || {}).matches) || [];
      results.replaceChildren(...[
        data.ok ? null : el("p", { class: "form-error", role: "alert" }, failMessage(data)),
        table(["File", "Line", "Layer", "Agents", "Excerpt"], matches.map((m) => [
          el("code", {}, m.file || ""), String(m.line || ""), m.layer || m.kind || "",
          (m.agents || []).join(", "), el("code", {}, m.excerpt || "")]),
        data.ok ? "No matches." : ""),
      ].filter(Boolean));
    };
    query.addEventListener("keydown", (event) => { if (event.key === "Enter") run(); });
    if (token !== undefined && token !== navToken) return;
    render("Find context", [],
      el("div", { class: "skill-form" },
        labelled("Search", query),
        el("label", { class: "check" }, regex, " Regular expression"),
        button("Search", run, "primary")),
      results);
  },

  async doctor(token) {
    const data = await api("doctor");
    if (token !== undefined && token !== navToken) return;
    const r = data.result || {};
    render("Doctor", [button("Apply safe fixes", () => act("Run doctor --fix?", "doctor", { fix: true }), "primary")],
      el("p", {}, r.healthy ? "Healthy." : "Problems found."),
      table(["Check", "Status"], (r.checks || []).map((c) => [c.name, c.status]), "No checks reported."),
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
    toast(failMessage(preview));
    return;
  }
  const updates = (preview.result && preview.result.updated) || [];
  if (!updates.length) {
    toast("Nothing to update.", "info");
    return;
  }
  const lines = updates.map((u) => `${u.name}${u.version ? " " + u.version : ""}: ${fileSummary(u.files || {})}`);
  await act("Apply these updates?\n\n" + lines.join("\n"), "skills/update", body);
}

async function updateSkill(skill) {
  if (skill.modified && !(await askConfirm(`${skill.name} was edited locally. Overwrite the local changes?`))) return;
  return confirmUpdate(skill.modified ? { name: skill.name, force: true } : { name: skill.name });
}

function updateAll() {
  return confirmUpdate({ all: true });
}

api("skills", undefined, true).then((data) => updateSkillsDot(data.result)).catch(() => {});

let current = "dashboard";
// Navigation token: guards against tab-switch races where slow inflight
// fetches from an earlier view resolve after the user navigated elsewhere.
let navToken = 0;
const tabs = Array.from(document.querySelectorAll("#nav [role=tab]"));

// Under 60rem the sidebar is a top bar: a menu button opens and closes the list.
const menuBtn = $("#menu");
const sidePanel = $("#side-body");
function setMenu(open) {
  if (!menuBtn || !sidePanel) return;
  menuBtn.setAttribute("aria-expanded", String(open));
  sidePanel.classList.toggle("open", open);
}
if (menuBtn) {
  menuBtn.addEventListener("click", () => setMenu(menuBtn.getAttribute("aria-expanded") !== "true"));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && menuBtn.getAttribute("aria-expanded") === "true") {
      setMenu(false);
      menuBtn.focus();
    }
  });
}

const VIEW_CLI = {
  dashboard: "muster status",
  machines: "muster machine list",
  team: "muster member list",
  secrets: "muster secret list",
  providers: "muster provider list",
  skills: "muster skill list",
  effective: "muster effective",
  profiles: "muster profile list",
  proposals: "muster proposal list",
  why: 'muster why "<query>"',
  history: "muster rollback",
  doctor: "muster doctor",
};

async function show(name, opts) {
  current = name;
  const token = ++navToken;
  for (const tab of tabs) {
    const selected = tab.dataset.view === name;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
    // On narrow screens the tab row scrolls: keep the active tab fully visible.
    if (selected && tab.scrollIntoView) tab.scrollIntoView({ block: "nearest", inline: "nearest" });
  }
  const menuWasOpen = menuBtn && menuBtn.getAttribute("aria-expanded") === "true";
  const keepMenu = Boolean(opts && opts.keepMenu);
  if (!keepMenu) setMenu(false);
  if (menuWasOpen && !keepMenu) { const main = $("#main"); if (main) main.focus(); }
  viewEl.setAttribute("aria-labelledby", "tab-" + name);
  const announcer = $("#status-announcer");
  if (announcer) {
    const label = tabs.find((tab) => tab.dataset.view === name);
    announcer.textContent = label ? label.textContent.trim() + " view" : "";
  }
  // The previous content stays in place while loading, so the layout holds still.
  viewEl.setAttribute("aria-busy", "true");
  actionsEl.setAttribute("aria-busy", "true");
  viewEl.inert = true;
  actionsEl.inert = true;
  // Name the destination right away; the old content stays dimmed until replaced.
  const destTab = tabs.find((tab) => tab.dataset.view === name);
  const destName = destTab && destTab.textContent ? destTab.textContent.trim() : "";
  const titleEl = $("#title");
  if (destName && titleEl) titleEl.textContent = destName;
  const attempt = loadAttempt = { failure: null };
  const title = tabs.find((tab) => tab.dataset.view === name);
  const failed = (message) => render(title ? title.textContent.trim() : name, [],
    el("div", { class: "load-error", role: "alert" },
      el("p", {}, message),
      button("Retry", () => show(name), "primary")));
  try {
    await views[name](token);
    if (token === navToken && attempt.failure) failed(attempt.failure);
    // The footer names the command behind the view on screen, whatever
    // background requests finished last.
    else if (token === navToken && VIEW_CLI[name]) showCli(VIEW_CLI[name]);
  } catch (err) {
    if (token !== navToken) return;
    failed("Failed to load: " + err.message);
  }
  if (token === navToken) { viewEl.setAttribute("aria-busy", "false"); actionsEl.setAttribute("aria-busy", "false"); viewEl.inert = false; actionsEl.inert = false; }
}

// Keyboard: arrows (the list is vertical)/Home/End move between tabs; Alt+digits 1-9 jump to a view.
$("#nav").addEventListener("keydown", (e) => {
  const i = tabs.indexOf(document.activeElement);
  if (i < 0) return;
  const next = { ArrowRight: i + 1, ArrowDown: i + 1, ArrowLeft: i - 1, ArrowUp: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
  if (next === undefined) return;
  e.preventDefault();
  const tab = tabs[(next + tabs.length) % tabs.length];
  tab.focus();
  show(tab.dataset.view, { keepMenu: true });
});
// Enter/Space on a tab commits the choice: close the mobile menu, focus the page.
$("#nav").addEventListener("keydown", (e) => {
  if ((e.key !== "Enter" && e.key !== " ") || !tabs.includes(document.activeElement)) return;
  if (!menuBtn || menuBtn.getAttribute("aria-expanded") !== "true") return;
  e.preventDefault();
  show(document.activeElement.dataset.view);
});
for (const tab of tabs) tab.addEventListener("click", () => show(tab.dataset.view));
document.addEventListener("keydown", (e) => {
  const typing = e.target && typeof e.target.closest === "function"
    && e.target.closest("input, textarea, select, dialog, [contenteditable]");
  if (typing || e.metaKey || !e.altKey) return;
  const n = Number(e.key);
  if (n >= 1 && n <= tabs.length) {
    e.preventDefault();
    tabs[n - 1].focus();
    show(tabs[n - 1].dataset.view);
  }
});

// Theme: Light / Auto (system) / Dark segmented control, remembered locally.
const themeEl = $("#theme");
function applyTheme(theme) {
  if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
  if (themeEl && typeof themeEl.querySelectorAll === "function") {
    for (const b of themeEl.querySelectorAll("button")) {
      b.setAttribute("aria-pressed", String((b.dataset.themeChoice || "system") === (theme || "system")));
    }
  }
}
applyTheme(localStorage.getItem("muster-theme"));
themeEl.addEventListener("click", (event) => {
  const target = event.target && typeof event.target.closest === "function"
    ? event.target.closest("[data-theme-choice]") : null;
  if (!target) return;
  const choice = target.dataset.themeChoice;
  const next = choice === "light" || choice === "dark" ? choice : null;
  if (next) localStorage.setItem("muster-theme", next);
  else localStorage.removeItem("muster-theme");
  applyTheme(next);
});

// Secret reveal: a dialog, then an explicit {"confirm": true} POST.
const dialog = $("#reveal");
let revealName = null;
let revealOpener = null;
function openReveal(reference) {
  revealOpener = document.activeElement;
  revealName = reference;
  $("#reveal-name").textContent = reference;
  $("#reveal-value").textContent = "";
  showCli(`muster secret get ${reference} --reveal --yes`);
  dialog.showModal();
  $("#reveal-confirm").focus();
}
$("#reveal-confirm").addEventListener("click", async () => {
  const confirmBtn = $("#reveal-confirm");
  if (confirmBtn.disabled) return;
  await busy(confirmBtn, async () => {
    try {
      const data = await api("secret/reveal", { name: revealName, confirm: true });
      $("#reveal-value").textContent = data.ok ? data.result : failMessage(data);
    } catch (err) {
      $("#reveal-value").textContent = "Failed to reveal: " + err.message;
    }
  });
});
dialog.addEventListener("close", () => {
  $("#reveal-value").textContent = "";
  if (revealOpener && revealOpener.isConnected && typeof revealOpener.focus === "function") revealOpener.focus();
  else { const main = $("#main"); if (main) main.focus(); }
});

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

$("#profile-preview").addEventListener("click", () => {
  const previewBtn = $("#profile-preview");
  if (previewBtn.disabled) return undefined;
  return busy(previewBtn, previewProfile);
});

async function previewProfile() {
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
}

// Any edit after a preview invalidates it: Save is disabled until re-previewed.
for (const id of ["profile-name", "profile-toml"]) {
  $("#" + id).addEventListener("input", () => { $("#profile-save").disabled = true; });
}

$("#profile-cancel").addEventListener("click", () => profileDialog.close());
profileDialog.addEventListener("close", () => $("#profile-form").reset());
$("#profile-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const saveBtn = $("#profile-save");
  if (saveBtn.disabled) return;
  // Belt and braces: only the exact previewed payload may be acted on.
  if (profileToken() !== profilePreviewToken) {
    $("#profile-save").disabled = true;
    $("#profile-error").textContent = "The form changed since the preview. Preview the diff again.";
    return;
  }
  const body = profileBody(false);
  if (!(await askConfirm(`Save profile ${body.name}?`))) return;
  // Locked while saving; afterwards only a still-previewed payload may unlock it.
  await busy(saveBtn, async () => {
    const data = await api("profile/set", body);
    if (data.ok) {
      profilePreviewToken = null;
      profileDialog.close();
      await show("profiles");
    } else {
      $("#profile-error").textContent = failMessage(data);
    }
  }, () => profilePreviewToken === null || profileToken() !== profilePreviewToken);
});

// MCP server form: list fields hold JSON arrays, what the API itself accepts
// as KEY=VALUE lists. JSON keeps every value verbatim: " padded ", "" and
// commas survive the edit -> preview -> save round trip unchanged. Values that
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
  // After a request the lock holds unless the exact previewed values are still in the form.
  const save = button("Save", saveMcp, "primary",
    () => mcpPreviewToken === null || mcpToken() !== mcpPreviewToken);
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
    && !(await askConfirm("Some values look like secret values, not references (e.g. native://group/name). "
      + "Muster stores them verbatim and only resolves references at apply time. Save anyway?"))) return;
  if (!(await askConfirm(`Save MCP server ${body.name}?`))) return;
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

// Skill editor: pick a file inside the skill, edit it, save. The server
// confines every path to the skills directory and caps the size; the panel only
// ever sends the relative path it was given by `skills/files`.
const skillDialog = $("#skill-edit");
let skillEditing = null;
let skillFileLoaded = null;
let skillFileSaved = "";

async function loadSkillFile(path) {
  $("#skill-error").textContent = "";
  const data = await api(`skills/file?name=${encodeURIComponent(skillEditing)}&path=${encodeURIComponent(path)}`);
  if (!data.ok) {
    $("#skill-error").textContent = failMessage(data);
    $("#skill-text").value = "";
    skillFileLoaded = null;
    return;
  }
  skillFileLoaded = path;
  skillFileSaved = (data.result && data.result.content) || "";
  $("#skill-text").value = skillFileSaved;
}

// Unsaved edits are lost on file switch or close; ask first.
function skillEditsDiscardable() {
  return skillFileLoaded === null
    || $("#skill-text").value === skillFileSaved
    || confirm(`Discard unsaved changes to ${skillFileLoaded}?`);
}

async function openSkillEdit(name) {
  skillEditing = name;
  skillFileLoaded = null;
  $("#skill-edit-title").textContent = `Edit skill ${name}`;
  $("#skill-error").textContent = "";
  $("#skill-text").value = "";
  const data = await api(`skills/files?name=${encodeURIComponent(name)}`);
  const select = $("#skill-file");
  const files = ((data.result || {}).files || []).map((f) => f.path);
  select.replaceChildren(...files.map((path) => el("option", { value: path }, path)));
  if (!data.ok || !files.length) {
    $("#skill-error").textContent = data.ok ? "This skill has no files." : failMessage(data);
    skillDialog.showModal();
    return;
  }
  const first = files.includes("SKILL.md") ? "SKILL.md" : files[0];
  select.value = first;
  skillDialog.showModal();
  await loadSkillFile(first);
}

const skillFileEl = $("#skill-file");
if (skillFileEl) skillFileEl.addEventListener("change", () => {
  if (!skillEditsDiscardable()) { skillFileEl.value = skillFileLoaded; return; }
  loadSkillFile(skillFileEl.value);
});
const skillCloseEl = $("#skill-close");
if (skillCloseEl) skillCloseEl.addEventListener("click", () => {
  if (skillDialog && skillEditsDiscardable()) skillDialog.close();
});
// Escape closes a <dialog> via "cancel"; guard it the same way.
if (skillDialog) skillDialog.addEventListener("cancel", (event) => {
  if (!skillEditsDiscardable()) event.preventDefault();
});
if (skillDialog) skillDialog.addEventListener("close", () => { if (current === "skills") show("skills"); });
const skillFormEl = $("#skill-form");
if (skillFormEl) skillFormEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  const saveBtn = $("#skill-save");
  if (!saveBtn || saveBtn.disabled || skillFileLoaded === null) return;
  if (!confirm(`Save ${skillFileLoaded} in skill ${skillEditing}?`)) return;
  await busy(saveBtn, async () => {
    const data = await api("skills/file", {
      name: skillEditing, path: skillFileLoaded, content: $("#skill-text").value, confirm: true,
    });
    if (data.ok) {
      $("#skill-error").textContent = "";
      skillFileSaved = $("#skill-text").value;
      toast(`Saved ${skillFileLoaded}.`, "info");
      await show("skills");
    } else {
      $("#skill-error").textContent = failMessage(data);
    }
  });
});

show("dashboard");


async function secretApproval(action, id) {
  if (!(await askConfirm(`${action === "approve" ? "Approve secret use" : action === "deny" ? "Deny secret request" : "Revoke secret approval"}?`))) return;
  const data = await api("secret/" + action, { id, confirm: true });
  if (!data.ok) { toast(failMessage(data)); return; }
  await show("secrets");
}
