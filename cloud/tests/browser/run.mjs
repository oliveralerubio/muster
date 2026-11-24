import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright-core';

const baseUrl = process.env.MUSTER_CLOUD_URL;
assert.ok(baseUrl, 'MUSTER_CLOUD_URL is required');
const billingUrl = process.env.MUSTER_BILLING_CLOUD_URL || '';
const paddleControlUrl = process.env.MUSTER_PADDLE_CONTROL_URL || '';

async function paddleControl(method, endpoint, body) {
  assert.ok(paddleControlUrl, 'MUSTER_PADDLE_CONTROL_URL is required');
  const resp = await fetch(`${paddleControlUrl}${endpoint}`, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await resp.text();
  assert.ok(resp.ok, `paddleControl ${method} ${endpoint} failed (${resp.status}): ${text}`);
  return text ? JSON.parse(text) : null;
}

const evidenceDir = process.env.MUSTER_EVIDENCE_DIR || '';
const executablePath =
  process.env.CHROMIUM_PATH ||
  ['/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome'].find((p) =>
    fs.existsSync(p)
  );

async function launchBrowser() {
  return chromium.launch({
    ...(executablePath ? { executablePath } : {}),
    headless: true,
    args: ['--no-sandbox', '--disable-dev-shm-usage'],
  });
}

async function saveScreenshot(page, name) {
  if (!evidenceDir) return;
  fs.mkdirSync(evidenceDir, { recursive: true });
  await page.screenshot({ path: path.join(evidenceDir, `${name}.png`), fullPage: true });
}

async function signInViaIdp(page, { subject, email }) {
  const signInLink = page.getByRole('link', { name: /Sign in/i });
  await signInLink.waitFor({ timeout: 5000 });
  await Promise.all([
    page.waitForURL((u) => new URL(u).pathname === '/authorize', { timeout: 10000 }),
    signInLink.click(),
  ]);
  const idpUrl = new URL(page.url());
  idpUrl.searchParams.set('login_as', subject);
  if (email) {
    idpUrl.searchParams.set('email', email);
  }
  await page.goto(idpUrl.toString(), { waitUntil: 'networkidle' });
}

async function journey1SignInAndAccount(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const baseOrigin = new URL(baseUrl).origin;
  const unexpectedResponses = [];
  const pageErrors = [];
  page.on('response', (res) => {
    const u = new URL(res.url());
    if (u.origin !== baseOrigin) return;
    if (u.pathname === '/v1/me' && res.status() === 401) return;
    if (res.status() >= 400) {
      unexpectedResponses.push(`${res.status()} ${u.pathname}`);
    }
  });
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });

  // Unauthenticated landing / sign-in page
  await page.getByRole('heading', { name: /Muster Cloud/i }).waitFor({ timeout: 5000 });
  const signInLink = page.getByRole('link', { name: /Sign in/i });
  await signInLink.waitFor({ timeout: 5000 });
  await saveScreenshot(page, '01-signin-page');

  // Click Sign in -> real /auth/login -> external IdP -> /auth/callback -> /
  await signInViaIdp(page, { subject: 'alice-sub-1', email: 'alice@example.com' });

  // Verify account email from /v1/me is shown
  await page.getByText('alice@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '02-signed-in-account');

  assert.deepEqual(
    unexpectedResponses,
    [],
    `unexpected app HTTP errors: ${unexpectedResponses.join('; ')}`
  );
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey2EmptyWorkspaces(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'bob-sub-1', email: 'bob@example.com' });

  await page.getByRole('heading', { name: /No workspaces yet/i }).waitFor({ timeout: 5000 });
  await page.getByLabel(/Workspace name/i).waitFor({ timeout: 5000 });
  await page.getByRole('button', { name: /Create workspace/i }).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '03-empty-workspaces');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey3CreateWorkspaceValidationAndIdempotency(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'carol-sub-1', email: 'carol@example.com' });

  const nameInput = page.getByLabel(/Workspace name/i);
  const submitBtn = page.getByRole('button', { name: /Create workspace/i });

  // 1. Validation: whitespace-only name
  await nameInput.fill('   ');
  await submitBtn.click();
  await page.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await nameInput.getAttribute('aria-invalid'), 'true');
  await saveScreenshot(page, '04-create-validation-error');

  // 2. Network drop on first submit -> retry reuses exact same Idempotency-Key
  const seenKeys = [];
  let dropFirst = true;
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    const key = route.request().headers()['idempotency-key'];
    if (key) seenKeys.push(key);
    if (dropFirst) {
      dropFirst = false;
      await route.abort('failed');
      return;
    }
    await route.continue();
  });

  await nameInput.fill('Acme Fleet');
  await submitBtn.dblclick();

  // Accessible error appears after dropped request
  await page.locator('[role="alert"]').waitFor({ timeout: 5000 });

  // Retry same intent
  await submitBtn.click();
  await page.getByRole('heading', { name: 'Acme Fleet' }).waitFor({ timeout: 5000 });

  assert.equal(seenKeys.length, 2, `expected 2 POST attempts, got ${seenKeys.length}`);
  assert.equal(seenKeys[0], seenKeys[1], 'Idempotency-Key must stay identical across retries of the same intent');
  assert.match(seenKeys[0], /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i);

  // Verify only 1 workspace exists on the real server
  const count = await page.evaluate(async () => {
    const r = await fetch('/v1/workspaces', { credentials: 'same-origin' });
    const list = await r.json();
    return list.length;
  });
  assert.equal(count, 1, 'must not create duplicate workspaces');
  await saveScreenshot(page, '05-created-workspace');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey4DetailReloadAndLogout(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'dave-sub-1', email: 'dave@example.com' });

  // Create first workspace
  await page.getByLabel(/Workspace name/i).fill('Prod Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();

  await page.waitForURL(/\/workspaces\/[0-9a-f-]{36}$/, { timeout: 5000 });
  await page.getByRole('heading', { name: 'Prod Fleet' }).waitFor({ timeout: 5000 });
  await page.locator('.meta-list').first().getByText(/owner/i).waitFor({ timeout: 5000 });
  await page.getByText(/14-day trial|Trialing/i).first().waitFor({ timeout: 5000 });
  await saveScreenshot(page, '06-workspace-detail');

  // Persistent reload on /workspaces/{ws}
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByRole('heading', { name: 'Prod Fleet' }).waitFor({ timeout: 5000 });
  await page.getByText(/Trialing/i).first().waitFor({ timeout: 5000 });

  // Navigate to workspace list and create second workspace
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.waitForURL((u) => new URL(u).pathname === '/', { timeout: 5000 });
  await page.getByLabel(/Workspace name/i).fill('Staging Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.getByRole('heading', { name: 'Staging Fleet' }).waitFor({ timeout: 5000 });

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Prod Fleet/i }).waitFor({ timeout: 5000 });
  await page.getByRole('link', { name: /Staging Fleet/i }).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '07-workspace-list');

  // Sign out
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  // Reload after sign out remains signed out
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey5AccessibilityThemeResponsiveAndSecurity(browser) {
  const context = await browser.newContext({ viewport: { width: 375, height: 812 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'eve-sub-1', email: 'eve@example.com' });

  // 1. No secrets in DOM, URL, or localStorage
  const me = await page.evaluate(async () => {
    const r = await fetch('/v1/me', { credentials: 'same-origin' });
    return r.json();
  });
  const domAndStorage = await page.evaluate(() => ({
    html: document.documentElement.innerHTML,
    url: window.location.href,
    storage: JSON.stringify(localStorage),
  }));
  assert.ok(!domAndStorage.html.includes(me.csrf_token), 'csrf_token must not appear in DOM');
  assert.ok(!domAndStorage.url.includes(me.csrf_token), 'csrf_token must not appear in URL');
  assert.ok(!domAndStorage.storage.includes(me.csrf_token), 'csrf_token must not appear in localStorage');

  // 2. Light/dark theme toggle + mobile responsive check (no horizontal overflow)
  const darkBtn = page.getByRole('button', { name: /Switch to dark mode/i });
  await darkBtn.click();
  assert.equal(await page.evaluate(() => document.documentElement.getAttribute('data-theme')), 'dark');
  const overflowMobile = await page.evaluate(
    () => document.documentElement.scrollWidth > window.innerWidth
  );
  assert.equal(overflowMobile, false, 'mobile viewport must not overflow horizontally');
  await saveScreenshot(page, '08-dark-mode-mobile');

  // 3. Keyboard Tab focus
  await page.keyboard.press('Tab');
  const focusedTag = await page.evaluate(() => document.activeElement?.tagName || '');
  assert.ok(['A', 'BUTTON', 'INPUT'].includes(focusedTag), `expected focusable element, got ${focusedTag}`);

  // 4. XSS safety on workspace name
  const xssPayload = '<img src=x onerror=window.__xss=1>';
  await page.getByLabel(/Workspace name/i).fill(xssPayload);
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.getByRole('heading', { name: xssPayload }).waitFor({ timeout: 5000 });
  assert.equal(await page.evaluate(() => window.__xss), undefined, 'XSS payload must not execute');

  // 5. Accessible 404 / error recovery on unknown workspace
  await page.goto(`${baseUrl}/workspaces/00000000-0000-0000-0000-000000000000`, {
    waitUntil: 'networkidle',
  });
  await page.getByRole('heading', { name: /Workspace not found/i }).waitFor({ timeout: 5000 });
  await page.locator('[role="alert"]').waitFor({ timeout: 5000 });
  await saveScreenshot(page, '09-workspace-not-found');
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: xssPayload }).waitFor({ timeout: 5000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function startDeviceAuthorization(clientName) {
  const resp = await fetch(`${baseUrl}/v1/device/authorize`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ client_name: clientName }),
  });
  assert.equal(resp.status, 200, `device/authorize failed: ${resp.status}`);
  return resp.json();
}

async function pollDeviceToken(deviceCode) {
  const resp = await fetch(`${baseUrl}/v1/device/token`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      grant_type: 'urn:ietf:params:oauth:grant-type:device_code',
      device_code: deviceCode,
    }),
  });
  const body = await resp.json();
  return { status: resp.status, body };
}

async function journey6DeviceApproveWithLoginRedirect(browser) {
  const auth = await startDeviceAuthorization('laptop-cli');

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  // Visit verification_uri_complete while unauthenticated
  await page.goto(auth.verification_uri_complete, { waitUntil: 'networkidle' });

  // Must NOT auto-approve on GET
  const pendingBefore = await pollDeviceToken(auth.device_code);
  assert.equal(pendingBefore.status, 400);
  assert.equal(pendingBefore.body.error.code, 'authorization_pending');

  // Sign in preserving return_to=/device?user_code=...
  await signInViaIdp(page, { subject: 'grace-sub-1', email: 'grace@example.com' });

  // Returned to /device?user_code=... with code prefilled
  await page.waitForURL((u) => new URL(u).pathname === '/device', { timeout: 5000 });
  const codeInput = page.getByLabel(/Device code|Verification code/i);
  await codeInput.waitFor({ timeout: 5000 });
  assert.equal((await codeInput.inputValue()).toUpperCase(), auth.user_code);

  // Must not claim client_name before server decision
  const bodyText = await page.locator('main').innerText();
  assert.equal(bodyText.includes('laptop-cli'), false, 'must not claim client_name before decision');

  await saveScreenshot(page, '10-device-confirm');

  // Explicit Approve
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.getByRole('heading', { name: /CLI login approved/i }).waitFor({ timeout: 5000 });
  await page.getByText('laptop-cli').waitFor({ timeout: 5000 });
  await saveScreenshot(page, '11-device-approved');

  // Real poll returns account token with expected scopes (never log token)
  const tokenRes = await pollDeviceToken(auth.device_code);
  assert.equal(tokenRes.status, 200);
  assert.ok(typeof tokenRes.body.access_token === 'string' && tokenRes.body.access_token.startsWith('mca_'));
  assert.deepEqual(tokenRes.body.scope, ['workspaces:read', 'devices:link']);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey7DeviceDenyAndErrorHandling(browser) {
  const auth = await startDeviceAuthorization('other-cli');

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(auth.verification_uri_complete, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'heidi-sub-1', email: 'heidi@example.com' });

  await page.waitForURL((u) => new URL(u).pathname === '/device', { timeout: 5000 });

  // Double-click Deny -> sends once, shows Denied
  await page.getByRole('button', { name: /Deny/i }).dblclick();
  await page.getByRole('heading', { name: /CLI login denied/i }).waitFor({ timeout: 5000 });
  await page.getByText('other-cli').waitFor({ timeout: 5000 });
  await saveScreenshot(page, '12-device-denied');

  // Real poll returns access_denied
  const deniedRes = await pollDeviceToken(auth.device_code);
  assert.equal(deniedRes.status, 400);
  assert.equal(deniedRes.body.error.code, 'access_denied');

  // Visit /device without code -> empty validation + invalid/reused code error
  await page.goto(`${baseUrl}/device`, { waitUntil: 'networkidle' });
  const codeInput = page.getByLabel(/Verification code/i);
  await codeInput.fill('');
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await codeInput.getAttribute('aria-invalid'), 'true');

  // Re-using already-decided code returns accessible server error
  await codeInput.fill(auth.user_code);
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.locator('[role="alert"]').waitFor({ timeout: 5000 });
  await saveScreenshot(page, '13-device-invalid-code');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

function generateMachineIdentity() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
  const rawPub = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32);
  const signingKey = `musterpk1${rawPub.toString('hex')}`;
  const recipient = 'age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p';
  return { signingKey, recipient, privateKey };
}

function signLinkProof({ workspaceId, accountId, machineName, signingKey, recipient, privateKey }) {
  const payload = [
    'muster-cloud-link-v1',
    workspaceId,
    accountId,
    machineName,
    signingKey,
    recipient,
  ].join('\n') + '\n';
  return crypto.sign(null, Buffer.from(payload, 'utf8'), privateKey).toString('hex');
}

async function journey8WorkspaceDevicesList(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'frank-sub-1', email: 'frank@example.com' });

  // Create workspace
  await page.getByLabel(/Workspace name/i).fill('Edge Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = new URL(page.url()).pathname.split('/').pop();

  // Empty machines state
  await page.getByRole('heading', { name: /Machines/i }).waitFor({ timeout: 5000 });
  await page.getByText(/No machines linked yet/i).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '14-workspace-devices-empty');

  // Obtain real account token via /device flow API and link a real machine via API
  const auth = await startDeviceAuthorization('edge-cli');
  const me = await page.evaluate(async () => {
    const r = await fetch('/v1/me', { credentials: 'same-origin' });
    return r.json();
  });
  const approveStatus = await page.evaluate(
    async ({ userCode, csrfToken }) => {
      const r = await fetch('/v1/device/approve', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-CSRF-Token': csrfToken,
        },
        credentials: 'same-origin',
        body: JSON.stringify({ user_code: userCode, decision: 'approve' }),
      });
      return r.status;
    },
    { userCode: auth.user_code, csrfToken: me.csrf_token }
  );
  assert.equal(approveStatus, 200);

  const tokenRes = await pollDeviceToken(auth.device_code);
  assert.equal(tokenRes.status, 200);

  const machine = generateMachineIdentity();
  const machineName = 'edge-node-1';
  const proof = signLinkProof({
    workspaceId: wsId,
    accountId: tokenRes.body.account_id,
    machineName,
    signingKey: machine.signingKey,
    recipient: machine.recipient,
    privateKey: machine.privateKey,
  });

  const linkResp = await fetch(`${baseUrl}/v1/workspaces/${wsId}/devices`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${tokenRes.body.access_token}`,
      'Idempotency-Key': crypto.randomUUID(),
    },
    body: JSON.stringify({
      machine_name: machineName,
      signing_key: machine.signingKey,
      recipient: machine.recipient,
      proof,
    }),
  });
  assert.equal(linkResp.status, 201);
  const linked = await linkResp.json();
  assert.equal(linked.bootstrap, true);

  // Reload workspace detail and verify machine appears with accurate status
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByRole('heading', { name: 'edge-node-1' }).waitFor({ timeout: 5000 });
  await page.locator('.badge-row').getByText('Linked', { exact: true }).waitFor({ timeout: 5000 });
  await page.getByText('Bootstrap claim', { exact: true }).waitFor({ timeout: 5000 });
  await page.getByText(/not yet approved by signed fleet governance/i).waitFor({ timeout: 5000 });
  await page.getByText(machine.signingKey).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '15-workspace-devices-linked');

  // Mobile responsive check for long signing_key / recipient
  await page.setViewportSize({ width: 375, height: 812 });
  const overflowMobile = await page.evaluate(
    () => document.documentElement.scrollWidth > window.innerWidth
  );
  assert.equal(overflowMobile, false, 'mobile machines list must not overflow horizontally');
  await saveScreenshot(page, '16-workspace-devices-mobile');

  // No fake governance buttons
  assert.equal(await page.getByRole('button', { name: /Revoke/i }).count(), 0);

  // No secrets in DOM, URL, or localStorage
  const domAndStorage = await page.evaluate(() => ({
    html: document.documentElement.innerHTML,
    url: window.location.href,
    storage: JSON.stringify(localStorage),
  }));
  assert.ok(!domAndStorage.html.includes(linked.device_token), 'device_token must not appear in DOM');
  assert.ok(!domAndStorage.html.includes(tokenRes.body.access_token), 'access_token must not appear in DOM');
  assert.ok(!domAndStorage.storage.includes(linked.device_token), 'device_token must not appear in localStorage');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}


async function journey9TeamMembersVisible(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'ivan-sub-1', email: 'ivan@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Team Alpha');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });

  const membersSection = page.getByRole('region', { name: /Members/i });
  await membersSection.waitFor({ timeout: 3000 });
  await membersSection.getByText('ivan@example.com', { exact: true }).waitFor({ timeout: 3000 });
  await membersSection.locator('.badge-row').getByText('owner', { exact: true }).waitFor({ timeout: 3000 });
  await membersSection.getByText(/Cloud roles control console access/i).waitFor({ timeout: 3000 });
  await membersSection.getByText(/do not grant fleet signing keys or signed fleet approval/i).waitFor({ timeout: 3000 });

  await saveScreenshot(page, '17-team-members-visible');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}


async function journey10InviteMemberAndRoleLimits(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'jack-sub-1', email: 'jack@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Team Beta');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });

  const membersSection = page.getByRole('region', { name: /Members/i });
  await membersSection.waitFor({ timeout: 5000 });

  // Owner sees Invite form with member + admin options, never owner
  const emailInput = membersSection.getByLabel(/Invite email/i);
  const roleSelect = membersSection.getByLabel(/Cloud role/i);
  const inviteBtn = membersSection.getByRole('button', { name: /Send invite/i });
  await emailInput.waitFor({ timeout: 3000 });

  const roleValues = await roleSelect.locator('option').evaluateAll((opts) => opts.map((o) => o.value));
  assert.deepEqual(roleValues, ['member', 'admin']);

  // Validation on invalid email
  await emailInput.fill('not-an-email');
  await inviteBtn.click();
  await membersSection.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await emailInput.getAttribute('aria-invalid'), 'true');

  // Owner invites bob.team@example.com as admin
  await emailInput.fill('bob.team@example.com');
  await roleSelect.selectOption('admin');
  await inviteBtn.click();

  // Shareable one-time link is displayed (not persisted in localStorage or URL)
  const shareInput = membersSection.getByLabel(/Shareable invite link/i);
  await shareInput.waitFor({ timeout: 5000 });
  const shareUrl = await shareInput.inputValue();
  assert.match(shareUrl, /^http:\/\/localhost:\d+\/invitations\/[A-Za-z0-9_-]+$/);
  const inviteToken = shareUrl.split('/').pop();

  const storageDump = await page.evaluate(() => JSON.stringify(localStorage));
  assert.ok(!storageDump.includes(inviteToken), 'invite token must not be saved in localStorage');
  assert.ok(!page.url().includes(inviteToken), 'invite token must not appear in workspace URL');

  // Pending invitation appears in Pending invitations list without exposing token
  await membersSection.getByText('bob.team@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await membersSection.getByText('Pending', { exact: true }).waitFor({ timeout: 5000 });
  await saveScreenshot(page, '18-team-invite-created');

  // Reload clears one-time token from DOM while pending invitation remains
  await page.reload({ waitUntil: 'networkidle' });
  await membersSection.getByText('bob.team@example.com', { exact: true }).waitFor({ timeout: 5000 });
  assert.equal(await membersSection.getByLabel(/Shareable invite link/i).count(), 0);
  const htmlAfterReload = await page.evaluate(() => document.documentElement.innerHTML);
  assert.ok(!htmlAfterReload.includes(inviteToken), 'one-time token must not persist after reload');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}


async function journey11AcceptInvitationExplicitAndMismatch(browser) {
  // Context 1: Owner creates workspace and invites kate@example.com as member
  const ownerCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const ownerPage = await ownerCtx.newPage();
  const ownerErrors = [];
  ownerPage.on('pageerror', (err) => ownerErrors.push(err.message));

  await ownerPage.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(ownerPage, { subject: 'judy-sub-1', email: 'judy@example.com' });
  await ownerPage.getByLabel(/Workspace name/i).fill('Team Gamma');
  await ownerPage.getByRole('button', { name: /Create workspace/i }).click();
  await ownerPage.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = new URL(ownerPage.url()).pathname.split('/').pop();

  const ownerMembers = ownerPage.getByRole('region', { name: /Members/i });
  await ownerMembers.getByLabel(/Invite email/i).fill('kate@example.com');
  await ownerMembers.getByLabel(/Cloud role/i).selectOption('member');
  await ownerMembers.getByRole('button', { name: /Send invite/i }).click();

  const shareUrl = await ownerMembers.getByLabel(/Shareable invite link/i).inputValue();

  // Context 2: Wrong account visits invite link -> no auto-accept, signs in via IdP, gets honest mismatch alert
  const wrongCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const wrongPage = await wrongCtx.newPage();
  const wrongErrors = [];
  wrongPage.on('pageerror', (err) => wrongErrors.push(err.message));

  await wrongPage.goto(shareUrl, { waitUntil: 'networkidle' });
  await wrongPage.getByText(/Sign in to review and accept/i).waitFor({ timeout: 3000 });

  // Verify GET did not alter membership
  const countBefore = await ownerPage.evaluate(async (id) => {
    const r = await fetch(`/v1/workspaces/${id}/members`, { credentials: 'same-origin' });
    return (await r.json()).length;
  }, wsId);
  assert.equal(countBefore, 1, 'visiting invite link unauthenticated must not alter membership');

  await signInViaIdp(wrongPage, { subject: 'wrong-sub-1', email: 'wrong@example.com' });
  await wrongPage.waitForURL((u) => new URL(u).pathname.startsWith('/invitations/'), { timeout: 5000 });

  // Explicit accept as wrong email -> 403 invitation_mismatch shown honestly
  await wrongPage.getByRole('button', { name: /Accept invitation/i }).click();
  const mismatchAlert = wrongPage.locator('[role="alert"]');
  await mismatchAlert.waitFor({ timeout: 5000 });
  const mismatchText = await mismatchAlert.innerText();
  assert.match(mismatchText, /another email/i);
  await saveScreenshot(wrongPage, '19-invite-wrong-email');

  // Context 3: Invited user (kate@example.com, verified via IdP) visits invite link and explicitly accepts
  const kateCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const katePage = await kateCtx.newPage();
  const kateErrors = [];
  const kateUnexpected = [];
  katePage.on('pageerror', (err) => kateErrors.push(err.message));
  katePage.on('response', (res) => {
    const u = new URL(res.url());
    if (u.origin === new URL(baseUrl).origin && !(u.pathname === '/v1/me' && res.status() === 401) && res.status() >= 400) {
      kateUnexpected.push(`${res.status()} ${u.pathname}`);
    }
  });

  await katePage.goto(shareUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(katePage, { subject: 'kate-sub-1', email: 'kate@example.com' });
  await katePage.waitForURL((u) => new URL(u).pathname.startsWith('/invitations/'), { timeout: 5000 });
  await saveScreenshot(katePage, '20-invite-confirm');

  // Still not accepted on GET after login!
  const countBeforeClick = await ownerPage.evaluate(async (id) => {
    const r = await fetch(`/v1/workspaces/${id}/members`, { credentials: 'same-origin' });
    return (await r.json()).length;
  }, wsId);
  assert.equal(countBeforeClick, 1, 'loading invite page after login must not auto-accept');

  await katePage.getByRole('button', { name: /Accept invitation/i }).click();
  await katePage.getByRole('heading', { name: /Invitation accepted/i }).waitFor({ timeout: 5000 });
  await saveScreenshot(katePage, '21-invite-accepted');

  // Open workspace and verify kate sees both members and no invite controls (member role)
  await katePage.getByRole('link', { name: /Open workspace/i }).click();
  await katePage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });
  const kateMembers = katePage.getByRole('region', { name: /Members/i });
  await kateMembers.getByText('judy@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await kateMembers.getByText('kate@example.com', { exact: true }).waitFor({ timeout: 5000 });
  assert.equal(await kateMembers.getByLabel(/Invite email/i).count(), 0, 'member must not see invite form');
  assert.deepEqual(kateUnexpected, [], `member should not trigger 403/4xx requests: ${kateUnexpected.join('; ')}`);

  // Reusing accepted invite link gives honest error
  await katePage.goto(shareUrl, { waitUntil: 'networkidle' });
  await katePage.getByRole('button', { name: /Accept invitation/i }).click();
  await katePage.locator('[role="alert"]').waitFor({ timeout: 5000 });

  assert.deepEqual(ownerErrors, [], `owner page errors: ${ownerErrors.join('; ')}`);
  assert.deepEqual(wrongErrors, [], `wrong page errors: ${wrongErrors.join('; ')}`);
  assert.deepEqual(kateErrors, [], `kate page errors: ${kateErrors.join('; ')}`);
  await ownerCtx.close();
  await wrongCtx.close();
  await kateCtx.close();
}


async function acceptInviteInNewContext(browser, shareUrl, { subject, email }) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (err) => errors.push(err.message));

  await page.goto(shareUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject, email });
  await page.waitForURL((u) => new URL(u).pathname.startsWith('/invitations/'), { timeout: 5000 });
  await page.getByRole('button', { name: /Accept invitation/i }).click();
  await page.getByRole('heading', { name: /Invitation accepted/i }).waitFor({ timeout: 5000 });
  return { ctx, page, errors };
}

async function journey12RemoveMemberAndLastOwnerGuard(browser) {
  const ownerCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const ownerPage = await ownerCtx.newPage();
  const ownerErrors = [];
  ownerPage.on('pageerror', (err) => ownerErrors.push(err.message));

  await ownerPage.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(ownerPage, { subject: 'leo-sub-1', email: 'leo@example.com' });
  await ownerPage.getByLabel(/Workspace name/i).fill('Team Delta');
  await ownerPage.getByRole('button', { name: /Create workspace/i }).click();
  await ownerPage.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = new URL(ownerPage.url()).pathname.split('/').pop();

  const ownerMembers = ownerPage.getByRole('region', { name: /Members/i });
  await ownerMembers.getByLabel(/Invite email/i).fill('mia@example.com');
  await ownerMembers.getByLabel(/Cloud role/i).selectOption('member');
  await ownerMembers.getByRole('button', { name: /Send invite/i }).click();
  const miaShareUrl = await ownerMembers.getByLabel(/Shareable invite link/i).inputValue();
  await ownerMembers.getByRole('button', { name: /Dismiss/i }).click();

  await ownerMembers.getByLabel(/Invite email/i).fill('noah@example.com');
  await ownerMembers.getByLabel(/Cloud role/i).selectOption('admin');
  await ownerMembers.getByRole('button', { name: /Send invite/i }).click();
  const noahShareUrl = await ownerMembers.getByLabel(/Shareable invite link/i).inputValue();

  const mia = await acceptInviteInNewContext(browser, miaShareUrl, {
    subject: 'mia-sub-1',
    email: 'mia@example.com',
  });
  const noah = await acceptInviteInNewContext(browser, noahShareUrl, {
    subject: 'noah-sub-1',
    email: 'noah@example.com',
  });

  // Owner reloads to view all 3 members + honest removal boundary
  await ownerPage.reload({ waitUntil: 'networkidle' });
  const ownerMemberList = ownerMembers.locator('.member-list').first();
  await ownerMemberList.getByText('mia@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await ownerMemberList.getByText('noah@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await ownerMembers.getByText(/revokes transport tokens of machines they linked/i).waitFor({ timeout: 3000 });
  await ownerMembers.getByText(/does not change signed fleet governance or erase secrets already delivered/i).waitFor({ timeout: 3000 });

  // Last-owner guard: owner cannot remove/leave while last owner -> 409 last_owner shown honestly
  await ownerMembers.getByRole('button', { name: /Leave workspace/i }).click();
  const lastOwnerAlert = ownerMembers.locator('[role="alert"]');
  await lastOwnerAlert.waitFor({ timeout: 5000 });
  assert.match(await lastOwnerAlert.innerText(), /last owner/i);
  await ownerMemberList.getByText('leo@example.com', { exact: true }).waitFor({ timeout: 3000 });
  await saveScreenshot(ownerPage, '22-team-last-owner-error');

  // Owner removes mia@example.com
  await ownerMembers.getByRole('button', { name: 'Remove mia@example.com' }).click();
  await ownerMemberList.getByText('mia@example.com', { exact: true }).waitFor({ state: 'detached', timeout: 5000 });
  await ownerPage.reload({ waitUntil: 'networkidle' });
  await ownerMemberList.getByText('noah@example.com', { exact: true }).waitFor({ timeout: 5000 });
  assert.equal(await ownerMemberList.getByText('mia@example.com', { exact: true }).count(), 0);
  await saveScreenshot(ownerPage, '23-team-member-removed');

  // Removed member loses workspace access
  await mia.page.goto(`${baseUrl}/workspaces/${wsId}`, { waitUntil: 'networkidle' });
  await mia.page.getByRole('heading', { name: /Workspace not found/i }).waitFor({ timeout: 5000 });

  // Admin view: can only invite 'member', cannot remove owner, can leave workspace
  await noah.page.getByRole('link', { name: /Open workspace/i }).click();
  await noah.page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });
  const noahMembers = noah.page.getByRole('region', { name: /Members/i });
  await noahMembers.getByText('leo@example.com', { exact: true }).waitFor({ timeout: 5000 });
  const noahRoleOptions = await noahMembers
    .getByLabel(/Cloud role/i)
    .locator('option')
    .evaluateAll((opts) => opts.map((o) => o.value));
  assert.deepEqual(noahRoleOptions, ['member']);
  assert.equal(await noahMembers.getByRole('button', { name: 'Remove leo@example.com' }).count(), 0);

  await noahMembers.getByRole('button', { name: /Leave workspace/i }).click();
  await noah.page.waitForURL((u) => new URL(u).pathname === '/', { timeout: 5000 });
  await noah.page.getByRole('heading', { name: /No workspaces yet/i }).waitFor({ timeout: 5000 });

  assert.deepEqual(ownerErrors, [], `owner page errors: ${ownerErrors.join('; ')}`);
  assert.deepEqual(mia.errors, [], `mia page errors: ${mia.errors.join('; ')}`);
  assert.deepEqual(noah.errors, [], `noah page errors: ${noah.errors.join('; ')}`);
  await ownerCtx.close();
  await mia.ctx.close();
  await noah.ctx.close();
}


function loadFixtures() {
  const raw = process.env.MUSTER_BROWSER_FIXTURES;
  assert.ok(raw, "MUSTER_BROWSER_FIXTURES must be set");
  return JSON.parse(raw);
}

async function journey13HistoryAndRedactedDiff(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(page, { subject: "olivia-sub-1", email: "olivia@example.com" });

  // Open pre-seeded workspace with real signed history
  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const historySection = page.getByRole("region", { name: /^History$/i });
  await historySection.waitFor({ timeout: 5000 });

  const revHead = fixtures.revisions[0];
  const revGenesis = fixtures.revisions[1];
  await historySection.getByText(revHead.message, { exact: true }).waitFor({ timeout: 5000 });
  await historySection.getByText(revGenesis.message, { exact: true }).waitFor({ timeout: 5000 });
  await historySection.getByText(revHead.revision.slice(0, 8)).waitFor({ timeout: 5000 });

  // Verify newest-first order
  const textOrder = await historySection.innerText();
  assert.ok(
    textOrder.indexOf(revHead.message) < textOrder.indexOf(revGenesis.message),
    "expected newest revision first in history"
  );

  // Open diff for newest revision: text patch visible, .age vault redacted
  const revCard = historySection.locator("article", { hasText: revHead.message });
  await revCard.getByRole("button", { name: /View diff/i }).click();

  await historySection.getByText("settings.toml", { exact: true }).waitFor({ timeout: 5000 });
  await historySection.getByText(/model = "opus"/).waitFor({ timeout: 5000 });
  await historySection.getByText("vault/default.age", { exact: true }).waitFor({ timeout: 5000 });
  await historySection.getByText(/\[ciphertext changed\]/).waitFor({ timeout: 5000 });

  const mainText = await page.locator("main").innerText();
  assert.equal(mainText.includes("SUPER_SECRET_CIPHERTEXT_MARKER_999"), false, "must never render vault ciphertext");
  assert.equal(mainText.includes("synthetic-ciphertext"), false, "must never render vault ciphertext");

  await saveScreenshot(page, "24-history-redacted-diff");

  // Also verify member role (peter) can read history and diff
  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on("pageerror", (err) => memberErrors.push(err.message));

  await memberPage.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(memberPage, { subject: "peter-sub-1", email: "peter@example.com" });
  await memberPage.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const memberHistory = memberPage.getByRole("region", { name: /^History$/i });
  await memberHistory.getByText(revHead.message, { exact: true }).waitFor({ timeout: 5000 });
  await memberHistory.locator("article", { hasText: revHead.message }).getByRole("button", { name: /View diff/i }).click();
  await memberHistory.getByText(/\[ciphertext changed\]/).waitFor({ timeout: 5000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join("; ")}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join("; ")}`);
  await context.close();
  await memberCtx.close();
}


async function journey14ProposalsAndConflictsVisible(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(page, { subject: "olivia-sub-1", email: "olivia@example.com" });

  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const proposalsSection = page.getByRole("region", { name: /^Proposals$/i });
  await proposalsSection.waitFor({ timeout: 5000 });

  // Enrollment proposal visible with machine name and honest local signing requirement
  const enrollCard = proposalsSection.locator("article", { hasText: fixtures.enrollmentMachineName });
  await enrollCard.waitFor({ timeout: 5000 });
  await enrollCard.getByText(/Enrollment/i).waitFor({ timeout: 3000 });
  await enrollCard.getByText(/^Open$/i).waitFor({ timeout: 3000 });
  assert.equal(await enrollCard.getByRole("link", { name: /Download proposal bundle/i }).count(), 0);
  assert.equal(await enrollCard.getByRole("button", { name: /Approve enrollment/i }).count(), 0);

  // Conflict proposal visible with real Open status and downloadable retained bundle
  const conflictCard = proposalsSection.locator("article", { hasText: fixtures.conflictRevision.slice(0, 8) });
  await conflictCard.waitFor({ timeout: 5000 });
  await conflictCard.getByText(/Conflict/i).waitFor({ timeout: 3000 });
  await conflictCard.getByText(/^Open$/i).waitFor({ timeout: 3000 });

  const [bundleDownload] = await Promise.all([
    page.waitForEvent("download"),
    conflictCard.getByRole("link", { name: /Download proposal bundle/i }).click(),
  ]);
  const bundlePath = await bundleDownload.path();
  const bundleHead = fs.readFileSync(bundlePath, "utf8").slice(0, 32);
  assert.ok(bundleHead.startsWith("# v2 git bundle"), `expected git bundle, got: ${bundleHead}`);

  // Owner rejects the conflict proposal via real API
  await conflictCard.getByRole("button", { name: /Reject proposal/i }).click();
  await conflictCard.getByText(/^Rejected$/i).waitFor({ timeout: 5000 });
  assert.equal(
    await conflictCard.getByRole("link", { name: /Download proposal bundle/i }).count(),
    0,
    "closed proposal must not show Download proposal bundle"
  );
  await page.reload({ waitUntil: "networkidle" });
  const conflictReloaded = page
    .getByRole("region", { name: /^Proposals$/i })
    .locator("article", { hasText: fixtures.conflictRevision.slice(0, 8) });
  await conflictReloaded.getByText(/^Rejected$/i).waitFor({ timeout: 5000 });
  assert.equal(await conflictReloaded.getByRole("button", { name: /Reject proposal/i }).count(), 0);
  assert.equal(
    await conflictReloaded.getByRole("link", { name: /Download proposal bundle/i }).count(),
    0,
    "closed proposal after reload must not show Download proposal bundle"
  );

  await saveScreenshot(page, "25-proposals-and-conflicts");

  // Pending quorum workspace
  await page.getByRole("link", { name: /All workspaces/i }).click();
  await page.getByRole("link", { name: new RegExp(fixtures.quorumWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.quorumWorkspaceId}$`), { timeout: 5000 });

  const quorumProposals = page.getByRole("region", { name: /^Proposals$/i });
  const pendingCard = quorumProposals.locator("article", { hasText: fixtures.pendingQuorumRevision.slice(0, 8) });
  await pendingCard.waitFor({ timeout: 5000 });
  await pendingCard.getByText(/Pending quorum/i).waitFor({ timeout: 3000 });
  await pendingCard.getByText(/^Open$/i).waitFor({ timeout: 3000 });
  await pendingCard.getByRole("link", { name: /Download proposal bundle/i }).waitFor({ timeout: 3000 });
  await saveScreenshot(page, "26-proposals-pending-quorum");

  // Member role sees proposals read-only without Reject controls
  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on("pageerror", (err) => memberErrors.push(err.message));

  await memberPage.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(memberPage, { subject: "peter-sub-1", email: "peter@example.com" });
  await memberPage.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const memberProposals = memberPage.getByRole("region", { name: /^Proposals$/i });
  await memberProposals.locator("article", { hasText: fixtures.enrollmentMachineName }).waitFor({ timeout: 5000 });
  assert.equal(await memberProposals.getByRole("button", { name: /Reject proposal/i }).count(), 0);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join("; ")}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join("; ")}`);
  await context.close();
  await memberCtx.close();
}


async function journey15RequestRestoreProposal(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(page, { subject: "olivia-sub-1", email: "olivia@example.com" });
  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const historySection = page.getByRole("region", { name: /^History$/i });
  const revHead = fixtures.revisions[0];
  const revGenesis = fixtures.revisions[1];

  // Owner clicks "Propose restore" on the genesis revision
  const genesisCard = historySection.locator("article", { hasText: revGenesis.message });
  await genesisCard.waitFor({ timeout: 5000 });
  await genesisCard.getByRole("button", { name: /Propose restore/i }).click();

  const proposalsSection = page.getByRole("region", { name: /^Proposals$/i });
  const targetSelect = proposalsSection.getByLabel(/Target revision/i);
  assert.equal(await targetSelect.inputValue(), revGenesis.revision);

  // Empty paths validation
  const pathsInput = proposalsSection.getByLabel(/Paths to restore/i);
  await pathsInput.fill("   ");
  await proposalsSection.getByRole("button", { name: /Create restore proposal/i }).click();
  await proposalsSection.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await pathsInput.getAttribute("aria-invalid"), "true");

  // Traversal path rejected by real server 400
  await pathsInput.fill("../secret");
  await proposalsSection.getByRole("button", { name: /Create restore proposal/i }).click();
  await proposalsSection.locator('[role="alert"]').waitFor({ timeout: 5000 });

  // Valid relative path creates an unsigned restore proposal (never claims applied)
  await pathsInput.fill("settings.toml");
  await proposalsSection.getByRole("button", { name: /Create restore proposal/i }).click();

  await proposalsSection.getByText(/requires local signing/i).waitFor({ timeout: 5000 });
  const restoreCard = proposalsSection.locator("article", { hasText: "settings.toml" });
  await restoreCard.waitFor({ timeout: 5000 });
  await restoreCard.getByText(/^Restore$/i).waitFor({ timeout: 3000 });
  await restoreCard.getByText(/^Open$/i).waitFor({ timeout: 3000 });
  await restoreCard.getByText(revGenesis.revision.slice(0, 8)).waitFor({ timeout: 3000 });

  const pageText = await page.locator("main").innerText();
  assert.equal(/restored successfully|files reverted/i.test(pageText), false, "must never claim restore applied files");

  await saveScreenshot(page, "27-restore-proposal-created");

  // Persists across reload and accepted head remains unchanged
  await page.reload({ waitUntil: "networkidle" });
  await page.getByRole("region", { name: /^Proposals$/i }).locator("article", { hasText: "settings.toml" }).waitFor({ timeout: 5000 });
  const historyText = await page.getByRole("region", { name: /^History$/i }).innerText();
  assert.ok(historyText.indexOf(revHead.message) < historyText.indexOf(revGenesis.message));

  // Member role does not see restore creation controls
  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on("pageerror", (err) => memberErrors.push(err.message));

  await memberPage.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(memberPage, { subject: "peter-sub-1", email: "peter@example.com" });
  await memberPage.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  await memberPage.getByRole("region", { name: /^Proposals$/i }).locator("article", { hasText: "settings.toml" }).waitFor({ timeout: 5000 });
  assert.equal(await memberPage.getByRole("button", { name: /Create restore proposal/i }).count(), 0);
  assert.equal(await memberPage.getByRole("button", { name: /Propose restore/i }).count(), 0);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join("; ")}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join("; ")}`);
  await context.close();
  await memberCtx.close();
}


async function verifyExportRecovery(exportSection, download, revision) {
  const cli = process.env.MUSTER_CLI_BIN;
  assert.ok(cli, "MUSTER_CLI_BIN must point to the built CLI");
  const command = await exportSection.locator("pre code").textContent();
  const args = command.match(/^muster sync --pull --remote "\$PWD\/workspace\.bundle" --expect-signer (musterpk1[0-9a-f]{64})$/);
  assert.ok(args, `expected a standalone recovery command, got: ${command}`);
  assert.equal(args[1], revision.signer);
  await exportSection.getByText(revision.revision.slice(0, 8), { exact: true }).waitFor({ timeout: 3000 });
  await exportSection.getByText(/from the folder where you downloaded/i).waitFor({ timeout: 3000 });
  assert.equal(download.suggestedFilename(), "workspace.bundle");

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "muster-export-recover-"));
  try {
    const home = path.join(tmpDir, "home");
    const configHome = path.join(home, "config");
    const bundlePath = path.join(tmpDir, "workspace.bundle");
    fs.mkdirSync(home);
    await download.saveAs(bundlePath);
    const result = JSON.parse(execFileSync(cli, [
      "--format", "json", "sync", "--pull", "--remote", bundlePath, "--expect-signer", args[1],
    ], {
      cwd: tmpDir,
      env: { PATH: process.env.PATH, HOME: home, XDG_CONFIG_HOME: configHome, GIT_CONFIG_NOSYSTEM: "1" },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }));
    assert.equal(result.ok, true);
    assert.equal(result.result.joined, true);
    assert.equal(result.result.applied, false);
    const head = execFileSync("git", ["-C", path.join(configHome, "muster"), "rev-parse", "HEAD"], { encoding: "utf8" });
    assert.equal(head.trim(), revision.revision);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

async function journey16ExportBundleAndOssInstructions(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(page, { subject: "olivia-sub-1", email: "olivia@example.com" });

  // 1. Fresh empty workspace shows honest empty export state (no broken 204 link)
  await page.getByLabel(/Workspace name/i).fill("Blank Export Workspace");
  await page.getByRole("button", { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });

  const emptyExport = page.getByRole("region", { name: /Export & offline recovery/i });
  await emptyExport.waitFor({ timeout: 5000 });
  await emptyExport.getByText(/No accepted history to export yet/i).waitFor({ timeout: 5000 });
  assert.equal(await emptyExport.getByText("Download Git bundle", { exact: true }).count(), 0);

  // 2. Populated workspace exposes full downloadable bundle + honest OSS recovery commands
  await page.getByRole("link", { name: /All workspaces/i }).click();
  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const exportSection = page.getByRole("region", { name: /Export & offline recovery/i });
  await exportSection.waitFor({ timeout: 5000 });

  const revHead = fixtures.revisions[0];
  await exportSection.getByText(revHead.signer, { exact: true }).waitFor({ timeout: 5000 });
  await exportSection.getByText(/no Cloud subscription/i).waitFor({ timeout: 3000 });

  const [download] = await Promise.all([
    page.waitForEvent("download"),
    exportSection.getByText("Download Git bundle", { exact: true }).click(),
  ]);
  await verifyExportRecovery(exportSection, download, revHead);
  const bundlePath = await download.path();
  assert.ok(bundlePath, "downloaded bundle must exist");

  // Real offline OSS verification via git clone
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "muster-export-verify-"));
  try {
    const cloneDir = path.join(tmpDir, "muster-config");
    execFileSync("git", ["clone", "-q", "-b", "main", bundlePath, cloneDir], { stdio: "pipe" });
    const clonedHead = execFileSync("git", ["-C", cloneDir, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
    assert.equal(clonedHead, revHead.revision);
    const gitLog = execFileSync("git", ["-C", cloneDir, "log", "--oneline"], { encoding: "utf8" });
    assert.ok(gitLog.includes("update config and vault"));
    assert.ok(gitLog.includes("genesis"));
    assert.ok(fs.existsSync(path.join(cloneDir, "settings.toml")));
    assert.ok(fs.existsSync(path.join(cloneDir, "vault/default.age")));
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }

  await saveScreenshot(page, "28-export-and-oss-recovery");

  // 3. Serve a different signed export under the same URL to exercise stale page metadata.
  const nextHead = fixtures.paginationRevisions[0];
  assert.notEqual(nextHead.signer, revHead.signer);
  await page.route(`**/v1/workspaces/${fixtures.historyWorkspaceId}/export`, async (route) => {
    const response = await route.fetch({ url: `${baseUrl}/v1/workspaces/${fixtures.paginationWorkspaceId}/export` });
    await route.fulfill({ response });
  });
  const [nextDownload] = await Promise.all([
    page.waitForEvent("download"),
    exportSection.getByRole("button", { name: /Download Git bundle/i }).click(),
  ]);
  await exportSection.getByText(nextHead.signer, { exact: true }).waitFor({ timeout: 3000 });
  await verifyExportRecovery(exportSection, nextDownload, nextHead);
  assert.equal(await exportSection.getByText(revHead.signer, { exact: true }).count(), 0);

  // 4. Member role can also export full bundle
  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on("pageerror", (err) => memberErrors.push(err.message));

  await memberPage.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(memberPage, { subject: "peter-sub-1", email: "peter@example.com" });
  await memberPage.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const memberExport = memberPage.getByRole("region", { name: /Export & offline recovery/i });
  const [memberDownload] = await Promise.all([
    memberPage.waitForEvent("download"),
    memberExport.getByRole("button", { name: /Download Git bundle/i }).click(),
  ]);
  const memberBundleHead = fs.readFileSync(await memberDownload.path(), "utf8").slice(0, 32);
  assert.ok(memberBundleHead.startsWith("# v2 git bundle"));

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join("; ")}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join("; ")}`);
  await context.close();
  await memberCtx.close();
}


async function journey17BillingUnconfiguredHonestStateAndLimits(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(page, { subject: "olivia-sub-1", email: "olivia@example.com" });

  // 1. Fresh workspace on unconfigured billing server
  await page.getByLabel(/Workspace name/i).fill("Trial Unconfigured Fleet");
  await page.getByRole("button", { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });

  const billingSection = page.getByRole("region", { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.getByText(/Billing is not configured/i).waitFor({ timeout: 5000 });
  await billingSection.locator("dd", { hasText: /^Trial$/i }).waitFor({ timeout: 3000 });
  await billingSection.locator("dd", { hasText: /Trialing/i }).waitFor({ timeout: 3000 });
  await billingSection.locator("dd", { hasText: /^10 MiB$/i }).waitFor({ timeout: 3000 });
  await billingSection.locator("dd", { hasText: /^0 B of 1 GiB$/i }).waitFor({ timeout: 3000 });
  await billingSection.locator(".meta-item", { hasText: /^Push/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });
  await billingSection.locator(".meta-item", { hasText: /^Pull/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });
  await billingSection.locator(".meta-item", { hasText: /^Export/i }).getByText(/^Available$/i).waitFor({ timeout: 3000 });

  // Must NEVER render simulated payment controls; Delete workspace is owner-only
  assert.equal(await billingSection.getByRole("button", { name: /checkout|subscribe|portal|cancel subscription|update plan/i }).count(), 0);
  await page.getByRole("region", { name: /^Delete workspace$/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole("button", { name: /^Delete workspace$/i }).count(), 1);
  await saveScreenshot(page, "29-billing-unconfigured");

  // 2. Populated workspace reflects non-zero storage_used_bytes from real Git/proposal data
  await page.getByRole("link", { name: /All workspaces/i }).click();
  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const seededBilling = page.getByRole("region", { name: /^Billing$/i });
  await seededBilling.waitFor({ timeout: 5000 });
  const storageDd = seededBilling.locator("dd", { hasText: /of 1 GiB/i });
  await storageDd.waitFor({ timeout: 5000 });
  const storageText = await storageDd.innerText();
  assert.equal(storageText.startsWith("0 B "), false, `expected non-zero storage usage, got: ${storageText}`);

  // 3. Member role sees billing info read-only and never sees Delete workspace controls
  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on("pageerror", (err) => memberErrors.push(err.message));

  await memberPage.goto(`${baseUrl}/`, { waitUntil: "networkidle" });
  await signInViaIdp(memberPage, { subject: "peter-sub-1", email: "peter@example.com" });
  await memberPage.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const memberBilling = memberPage.getByRole("region", { name: /^Billing$/i });
  await memberBilling.waitFor({ timeout: 5000 });
  await memberBilling.getByText(/of 1 GiB/i).waitFor({ timeout: 5000 });
  assert.equal(await memberPage.getByRole("region", { name: /Delete workspace/i }).count(), 0);
  assert.equal(await memberPage.getByRole("button", { name: /delete workspace/i }).count(), 0);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join("; ")}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join("; ")}`);
  await context.close();
  await memberCtx.close();
}

async function journey18BillingCheckoutNoDuplicatesAndServerReconcile(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-billing-1', email: 'olivia@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Paid Billing Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  // Invite Peter as a read-only member and accept via real invitation flow
  const membersSection = page.getByRole('region', { name: /^Members$/i });
  await membersSection.waitFor({ timeout: 5000 });
  await membersSection.getByLabel(/Invite email/i).fill('peter@example.com');
  await membersSection.getByLabel(/Cloud role/i).selectOption('member');
  await membersSection.getByRole('button', { name: /Send invite/i }).click();
  const inviteLinkInput = membersSection.getByLabel(/Shareable invite link/i);
  await inviteLinkInput.waitFor({ timeout: 5000 });
  const inviteUrl = await inviteLinkInput.inputValue();

  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on('pageerror', (err) => memberErrors.push(err.message));
  await memberPage.goto(inviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(memberPage, { subject: 'peter-billing-1', email: 'peter@example.com' });
  await memberPage.getByRole('button', { name: /Accept invitation/i }).click();
  await memberPage.getByRole('link', { name: /Open workspace/i }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  // Member sees billing summary read-only with zero mutation controls
  const memberBilling = memberPage.getByRole('region', { name: /^Billing$/i });
  await memberBilling.waitFor({ timeout: 5000 });
  await memberBilling.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 3000 });
  assert.equal(
    await memberBilling.getByRole('button', { name: /checkout|portal|cancel|update plan|review plan/i }).count(),
    0
  );

  // Owner checks configured billing panel: no "not configured" notice
  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  assert.equal(await billingSection.getByText(/Billing is not configured/i).count(), 0);

  // Client validation: Team with invalid seat count 0 does not call Paddle
  const beforeCalls = (await paddleControl('GET', '/calls/create_transaction')).count;
  await billingSection.getByLabel(/^Plan$/i).selectOption('team');
  const seatsInput = billingSection.getByLabel(/Seats/i);
  await seatsInput.fill('0');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();
  await billingSection.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await seatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal((await paddleControl('GET', '/calls/create_transaction')).count, beforeCalls);

  // Select Personal and double-click Continue to checkout -> exactly ONE Paddle transaction
  await billingSection.getByLabel(/^Plan$/i).selectOption('personal');
  const checkoutBtn = billingSection.getByRole('button', { name: /Continue to checkout/i });
  await Promise.all([checkoutBtn.click(), checkoutBtn.click()]);

  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  const checkoutHref = await paddleLink.getAttribute('href');
  assert.ok(checkoutHref && checkoutHref.includes('sandbox-checkout.paddle.com/checkout/txn_'), checkoutHref);
  const txnId = checkoutHref.split('/').pop();
  assert.equal((await paddleControl('GET', '/calls/create_transaction')).count, beforeCalls + 1);

  // Clicking Continue to checkout again with same plan reuses open transaction (still 1 Paddle call)
  await checkoutBtn.click();
  await billingSection.getByRole('link', { name: /Open Paddle checkout/i }).waitFor({ timeout: 5000 });
  assert.equal((await paddleControl('GET', '/calls/create_transaction')).count, beforeCalls + 1);

  // Return URL (?checkout=success) MUST NEVER grant entitlements locally
  await page.goto(`${billingUrl}/workspaces/${wsId}?checkout=success&_ptxn=${encodeURIComponent(txnId)}`, {
    waitUntil: 'networkidle',
  });
  const afterReturnBilling = page.getByRole('region', { name: /^Billing$/i });
  await afterReturnBilling.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 5000 });
  await afterReturnBilling.getByRole('button', { name: /Refresh billing status/i }).click();
  await afterReturnBilling.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 3000 });

  // Complete payment via external Paddle fixture + real signed webhook
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: 'sub_01browserpersonal00000001',
    customer_id: 'ctm_01browserpersonal00000001',
  });

  // Refresh billing status reflects server-reconciled Personal Active limits
  await afterReturnBilling.getByRole('button', { name: /Refresh billing status/i }).click();
  await afterReturnBilling.locator('dd', { hasText: /^Personal$/i }).waitFor({ timeout: 5000 });
  await afterReturnBilling.locator('dd', { hasText: /^Active$/i }).waitFor({ timeout: 3000 });
  await afterReturnBilling.locator('.meta-item', { hasText: /^Current period ends/i }).waitFor({ timeout: 3000 });
  await afterReturnBilling.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^0 \/ 10$/).waitFor({ timeout: 3000 });
  await afterReturnBilling.locator('.meta-item', { hasText: /^Max bundle size/i }).getByText(/^25 MiB$/).waitFor({ timeout: 3000 });
  await afterReturnBilling.locator('.meta-item', { hasText: /^Storage used/i }).getByText(/^0 B of 10 GiB$/).waitFor({ timeout: 3000 });
  assert.equal(await afterReturnBilling.getByRole('button', { name: /Continue to checkout/i }).count(), 0);
  await saveScreenshot(page, '30-billing-personal-active');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join('; ')}`);
  await context.close();
  await memberCtx.close();
}

async function journey19BillingTeamPortalPlanChangeAndExplicitCancel(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-team-1', email: 'olivia@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Team Billing Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  // Subscribe to Team (2 seats)
  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.getByLabel(/^Plan$/i).selectOption('team');
  await billingSection.getByLabel(/Seats/i).fill('2');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();

  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  const txnId = (await paddleLink.getAttribute('href')).split('/').pop();

  const subId = 'sub_01browserteam00000000001';
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: subId,
    customer_id: 'ctm_01browserteam00000000001',
  });

  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(2 seats\)$/i }).waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^0 \/ 100$/).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Storage used/i }).getByText(/^0 B of 200 GiB$/).waitFor({ timeout: 3000 });

  // Customer portal generates a fresh URL each time requested
  const portalBefore = (await paddleControl('GET', '/calls/portal_session')).count;
  await billingSection.getByRole('button', { name: /Open billing portal/i }).click();
  const portalLink = billingSection.getByRole('link', { name: /Continue to billing portal/i });
  await portalLink.waitFor({ timeout: 5000 });
  const portalHref1 = await portalLink.getAttribute('href');
  assert.ok(portalHref1 && portalHref1.includes('customer-portal.paddle.com/cpls_'), portalHref1);
  assert.equal((await paddleControl('GET', '/calls/portal_session')).count, portalBefore + 1);

  await billingSection.getByRole('button', { name: /Open billing portal/i }).click();
  await billingSection.waitForFunction(
    (prevHref) => {
      const a = Array.from(document.querySelectorAll('a')).find((n) =>
        /Continue to billing portal/i.test(n.textContent || '')
      );
      return a && a.getAttribute('href') && a.getAttribute('href') !== prevHref;
    },
    portalHref1,
    { timeout: 5000 }
  );
  assert.equal((await paddleControl('GET', '/calls/portal_session')).count, portalBefore + 2);

  // Plan/seat change requires validation + explicit inline confirmation
  const updateBefore = (await paddleControl('GET', '/calls/update')).count;
  const updatePlanSelect = billingSection.getByLabel(/Change plan/i);
  const updateSeatsInput = billingSection.getByLabel(/Update seats/i);
  await updatePlanSelect.selectOption('team');
  await updateSeatsInput.fill('0');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.locator('[role="alert"]').waitFor({ timeout: 3000 });
  assert.equal(await updateSeatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal((await paddleControl('GET', '/calls/update')).count, updateBefore);

  await updateSeatsInput.fill('4');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).waitFor({ timeout: 3000 });
  await billingSection.getByRole('button', { name: /Keep current plan/i }).click();
  assert.equal(await billingSection.getByRole('button', { name: /Confirm plan change/i }).count(), 0);
  assert.equal((await paddleControl('GET', '/calls/update')).count, updateBefore);

  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(4 seats\)$/i }).waitFor({ timeout: 5000 });
  await billingSection.locator('[role="status"]', { hasText: /Subscription updated cleanly with Paddle/i }).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^0 \/ 200$/).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Storage used/i }).getByText(/^0 B of 400 GiB$/).waitFor({ timeout: 3000 });
  assert.equal((await paddleControl('GET', '/calls/update')).count, updateBefore + 1);

  // Past-due webhook shows Past due while preserving Push: Allowed
  await paddleControl('POST', '/update-subscription', {
    subscription_id: subId,
    event_type: 'subscription.past_due',
    status: 'past_due',
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Past due$/i }).waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Push/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });

  await paddleControl('POST', '/update-subscription', {
    subscription_id: subId,
    event_type: 'subscription.updated',
    status: 'active',
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Active$/i }).waitFor({ timeout: 5000 });

  // Cancel requires explicit inline confirmation
  const cancelBefore = (await paddleControl('GET', '/calls/cancel')).count;
  await billingSection.getByRole('button', { name: /Cancel subscription/i }).click();
  await billingSection.getByRole('button', { name: /Confirm cancellation/i }).waitFor({ timeout: 3000 });
  await billingSection.getByRole('button', { name: /Keep subscription/i }).click();
  assert.equal(await billingSection.getByRole('button', { name: /Confirm cancellation/i }).count(), 0);
  assert.equal((await paddleControl('GET', '/calls/cancel')).count, cancelBefore);

  await billingSection.getByRole('button', { name: /Cancel subscription/i }).click();
  await billingSection.getByRole('button', { name: /Confirm cancellation/i }).click();
  await billingSection.getByText(/Cancels at period end/i).waitFor({ timeout: 5000 });
  await billingSection
    .locator('[role="status"]', {
      hasText: /Subscription cancellation scheduled for the end of the current billing period/i,
    })
    .waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Access ends/i }).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Push/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });
  assert.equal(await billingSection.getByRole('button', { name: /^Cancel subscription$/i }).count(), 0);
  assert.equal((await paddleControl('GET', '/calls/cancel')).count, cancelBefore + 1);
  await saveScreenshot(page, '31-billing-team-canceling');

  // Final canceled webhook -> Push: Blocked, Pull & Export remain available, checkout re-enabled
  await paddleControl('POST', '/update-subscription', {
    subscription_id: subId,
    event_type: 'subscription.canceled',
    status: 'canceled',
    cancel_at_period_end: false,
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Canceled$/i }).waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Push/i }).getByText(/^Blocked$/i).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Pull/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Export/i }).getByText(/^Available$/i).waitFor({ timeout: 3000 });
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).waitFor({ timeout: 3000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey20BillingProviderFailureRetryRolesAndMobile(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-retry-1', email: 'olivia@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Resilient Billing Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });

  // 1. Provider outage during checkout -> honest alert, button stays usable, retry succeeds
  await paddleControl('POST', '/fail-next', { count: 1, path_contains: '/transactions' });
  await billingSection.getByLabel(/^Plan$/i).selectOption('personal');
  const checkoutBtn = billingSection.getByRole('button', { name: /Continue to checkout/i });
  await checkoutBtn.click();
  await billingSection.locator('[role="alert"]').waitFor({ timeout: 5000 });
  assert.equal(await checkoutBtn.isDisabled(), false);

  await checkoutBtn.click();
  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  const txnId = (await paddleLink.getAttribute('href')).split('/').pop();
  assert.equal(await billingSection.locator('[role="alert"]').count(), 0);

  const subId = 'sub_01browserresilient00001';
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: subId,
    customer_id: 'ctm_01browserresilient00001',
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Personal$/i }).waitFor({ timeout: 5000 });

  // 2. Provider outage on portal -> honest alert, retry succeeds
  await paddleControl('POST', '/fail-next', { count: 1, path_contains: '/portal-sessions' });
  const portalBtn = billingSection.getByRole('button', { name: /Open billing portal/i });
  await portalBtn.click();
  await billingSection.locator('[role="alert"]').waitFor({ timeout: 5000 });
  await portalBtn.click();
  await billingSection.getByRole('link', { name: /Continue to billing portal/i }).waitFor({ timeout: 5000 });
  assert.equal(await billingSection.locator('[role="alert"]').count(), 0);

  // 3. Provider outage on plan update -> honest alert, retry confirms Team \(3 seats\)
  await billingSection.getByLabel(/Change plan/i).selectOption('team');
  await billingSection.getByLabel(/Update seats/i).fill('3');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).waitFor({ timeout: 3000 });

  await paddleControl('POST', '/fail-next', { count: 1, path_contains: '/subscriptions/' });
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).click();
  await billingSection.locator('[role="alert"]').waitFor({ timeout: 5000 });
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(3 seats\)$/i }).waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^0 \/ 150$/).waitFor({ timeout: 3000 });
  await billingSection.locator('.meta-item', { hasText: /^Storage used/i }).getByText(/^0 B of 300 GiB$/).waitFor({ timeout: 3000 });

  // 4. Member sees active Team subscription read-only with zero mutation controls
  const membersSection = page.getByRole('region', { name: /^Members$/i });
  await membersSection.getByLabel(/Invite email/i).fill('member-active@example.com');
  await membersSection.getByLabel(/Cloud role/i).selectOption('member');
  await membersSection.getByRole('button', { name: /Send invite/i }).click();
  const inviteUrl = await membersSection.getByLabel(/Shareable invite link/i).inputValue();

  const memberCtx = await browser.newContext({ viewport: { width: 375, height: 812 } });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on('pageerror', (err) => memberErrors.push(err.message));
  await memberPage.goto(inviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(memberPage, { subject: 'member-active-1', email: 'member-active@example.com' });
  await memberPage.getByRole('button', { name: /Accept invitation/i }).click();
  await memberPage.getByRole('link', { name: /Open workspace/i }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  const memberBilling = memberPage.getByRole('region', { name: /^Billing$/i });
  await memberBilling.locator('dd', { hasText: /^Team \(3 seats\)$/i }).waitFor({ timeout: 5000 });
  assert.equal(
    await memberBilling.getByRole('button', { name: /checkout|portal|cancel|review plan|confirm/i }).count(),
    0
  );

  // Admin role CAN manage billing (can open billing portal)
  await membersSection.getByLabel(/Invite email/i).fill('admin-billing@example.com');
  await membersSection.getByLabel(/Cloud role/i).selectOption('admin');
  await membersSection.getByRole('button', { name: /Send invite/i }).click();
  await membersSection.locator('.share-box', { hasText: 'admin-billing@example.com' }).waitFor({ timeout: 5000 });
  const adminInviteUrl = await membersSection.getByLabel(/Shareable invite link/i).inputValue();
  assert.notEqual(adminInviteUrl, inviteUrl, 'admin invite URL must differ from member invite URL');

  const adminCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const adminPage = await adminCtx.newPage();
  const adminErrors = [];
  adminPage.on('pageerror', (err) => adminErrors.push(err.message));
  await adminPage.goto(adminInviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(adminPage, { subject: 'admin-billing-1', email: 'admin-billing@example.com' });
  await adminPage.getByRole('button', { name: /Accept invitation/i }).click();
  await adminPage.getByRole('link', { name: /Open workspace/i }).click();
  await adminPage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  const adminBilling = adminPage.getByRole('region', { name: /^Billing$/i });
  await adminBilling.getByRole('button', { name: /Open billing portal/i }).click();
  await adminBilling.getByRole('link', { name: /Continue to billing portal/i }).waitFor({ timeout: 5000 });
  assert.deepEqual(adminErrors, [], `unexpected admin errors: ${adminErrors.join('; ')}`);
  await adminCtx.close();

  // 5. Mobile viewport check (375x812) as owner on active Team billing card -> zero horizontal overflow
  await page.setViewportSize({ width: 375, height: 812 });
  await billingSection.scrollIntoViewIfNeeded();
  const overflow = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  assert.ok(
    overflow.scrollWidth <= overflow.clientWidth,
    `expected no horizontal overflow on mobile 375px, got scrollWidth=${overflow.scrollWidth} clientWidth=${overflow.clientWidth}`
  );
  await saveScreenshot(page, '32-billing-mobile');

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join('; ')}`);
  await context.close();
  await memberCtx.close();
}

async function journey21BillingNavigationIsolationAndStaleResponseGuard(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-iso-1', email: 'olivia@example.com' });

  // Create wsA, wsB, wsC
  await page.getByLabel(/Workspace name/i).fill('Iso Fleet A');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsAId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByLabel(/Workspace name/i).fill('Iso Fleet B');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsBId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByLabel(/Workspace name/i).fill('Iso Fleet C');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsCId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  // Open wsA, activate Personal subscription, open portal + review plan + review cancel
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Iso Fleet A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });

  const billingA = page.getByRole('region', { name: /^Billing$/i });
  await billingA.getByLabel(/^Plan$/i).selectOption('personal');
  await billingA.getByRole('button', { name: /Continue to checkout/i }).click();
  const checkoutLinkA = billingA.getByRole('link', { name: /Open Paddle checkout/i });
  await checkoutLinkA.waitFor({ timeout: 5000 });
  const txnA = (await checkoutLinkA.getAttribute('href')).split('/').pop();

  // Switching to wsB must NOT leak wsA's pending checkoutSession
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Iso Fleet B/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  const billingB = page.getByRole('region', { name: /^Billing$/i });
  await billingB.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 5000 });
  assert.equal(
    await billingB.getByRole('link', { name: /Open Paddle checkout/i }).count(),
    0,
    'wsB must not show wsA checkout link'
  );

  // Browser Back -> Forward must not leak wsA state into wsB
  await page.goBack({ waitUntil: 'networkidle' });
  await page.goBack({ waitUntil: 'networkidle' });
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnA,
    subscription_id: 'sub_01browseriso000000000001',
    customer_id: 'ctm_01browseriso000000000001',
  });
  await billingA.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingA.locator('dd', { hasText: /^Personal$/i }).waitFor({ timeout: 5000 });

  await billingA.getByRole('button', { name: /Open billing portal/i }).click();
  await billingA.getByRole('link', { name: /Continue to billing portal/i }).waitFor({ timeout: 5000 });
  await billingA.getByLabel(/Change plan/i).selectOption('team');
  await billingA.getByRole('button', { name: /Review plan change/i }).click();
  await billingA.getByRole('button', { name: /Confirm plan change/i }).waitFor({ timeout: 3000 });
  await billingA.getByRole('button', { name: /Cancel subscription/i }).click();
  await billingA.getByRole('button', { name: /Confirm cancellation/i }).waitFor({ timeout: 3000 });

  // Go forward back to wsB (or navigate to wsB) -> zero portal/confirmations from wsA
  await page.goForward({ waitUntil: 'networkidle' });
  await page.goForward({ waitUntil: 'networkidle' });
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  await billingB.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 5000 });
  assert.equal(await billingB.getByRole('link', { name: /Continue to billing portal/i }).count(), 0);
  assert.equal(await billingB.getByRole('button', { name: /Confirm plan change/i }).count(), 0);
  assert.equal(await billingB.getByRole('button', { name: /Confirm cancellation/i }).count(), 0);

  // Delayed real POST /checkout on wsB must be discarded after navigating to wsC
  let releaseHeld = null;
  let heldDonePromise = null;
  await page.route(`**/v1/workspaces/${wsBId}/billing/checkout`, async (route) => {
    heldDonePromise = new Promise((resolve) => {
      releaseHeld = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await billingB.getByLabel(/^Plan$/i).selectOption('personal');
  await billingB.getByRole('button', { name: /Continue to checkout/i }).click();
  await billingB.getByRole('button', { name: /Starting checkout/i }).waitFor({ timeout: 3000 });

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Iso Fleet C/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsCId}$`), { timeout: 5000 });
  const billingC = page.getByRole('region', { name: /^Billing$/i });
  await billingC.locator('dd', { hasText: /^Trial$/i }).waitFor({ timeout: 5000 });

  assert.ok(releaseHeld, 'expected delayed wsB checkout route');
  const checkoutRespWait = page.waitForResponse(
    (r) => r.url().includes(`/v1/workspaces/${wsBId}/billing/checkout`) && r.status() === 200
  );
  await releaseHeld();
  await heldDonePromise;
  await checkoutRespWait;
  await page.waitForTimeout(100);
  assert.equal(
    await billingC.getByRole('link', { name: /Open Paddle checkout/i }).count(),
    0,
    'delayed wsB checkout response must not render inside wsC'
  );
  await page.unroute(`**/v1/workspaces/${wsBId}/billing/checkout`);

  // Delayed real GET /billing for wsC (Trial) arriving while viewing wsA (Personal Active) must be discarded
  let releaseGetC = null;
  await page.route(`**/v1/workspaces/${wsCId}/billing`, async (route) => {
    await new Promise((resolve) => {
      releaseGetC = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Iso Fleet C/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsCId}$`), { timeout: 5000 });
  await page.waitForFunction(() => Boolean(document.body), { timeout: 2000 });

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Iso Fleet A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await billingA.locator('dd', { hasText: /^Personal$/i }).waitFor({ timeout: 5000 });

  assert.ok(releaseGetC, 'expected delayed wsC GET /billing route');
  const getCRespWait = page.waitForResponse(
    (r) => r.url().includes(`/v1/workspaces/${wsCId}/billing`) && r.status() === 200
  );
  await releaseGetC();
  await getCRespWait;
  await page.waitForTimeout(100);
  await billingA.locator('dd', { hasText: /^Personal$/i }).waitFor({ timeout: 3000 });
  await billingA.locator('dd', { hasText: /^Active$/i }).waitFor({ timeout: 3000 });
  await page.unroute(`**/v1/workspaces/${wsCId}/billing`);

  // Delayed request across Sign out must not leak errors/links into signed-out state or Account B
  let releasePortalA = null;
  await page.route(`**/v1/workspaces/${wsAId}/billing/portal`, async (route) => {
    await new Promise((resolve) => {
      releasePortalA = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await billingA.getByRole('button', { name: /Open billing portal/i }).click();
  await billingA.getByRole('button', { name: /Opening billing portal/i }).waitFor({ timeout: 3000 });

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  assert.ok(releasePortalA, 'expected delayed portal route across logout');
  const portalA401Wait = page.waitForResponse((r) =>
    r.url().includes(`/v1/workspaces/${wsAId}/billing/portal`) && r.status() === 401
  );
  await releasePortalA();
  await portalA401Wait;
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(
    await page.getByRole('link', { name: /Continue to billing portal/i }).count(),
    0,
    'delayed portal response after sign out must not render'
  );
  await page.unroute(`**/v1/workspaces/${wsAId}/billing/portal`);

  await signInViaIdp(page, { subject: 'bob-iso-2', email: 'bob@example.com' });
  await page.getByText('bob@example.com', { exact: true }).waitFor({ timeout: 5000 });
  assert.equal(
    await page.getByRole('link', { name: /Iso Fleet A/i }).count(),
    0,
    'Account B must remain completely isolated from Account A workspaces'
  );

  // Concurrent Refresh billing status during pending confirmation AND in-flight plan change
  await page.getByRole('button', { name: /Sign out/i }).click();
  await signInViaIdp(page, { subject: 'olivia-iso-1', email: 'olivia@example.com' });
  await page.getByRole('link', { name: /Iso Fleet A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });

  await billingA.getByLabel(/Change plan/i).selectOption('team');
  await billingA.getByLabel(/Update seats/i).fill('3');
  await billingA.getByRole('button', { name: /Review plan change/i }).click();
  const confirmPlanBtn = billingA.getByRole('button', { name: /Confirm plan change/i });
  await confirmPlanBtn.waitFor({ timeout: 3000 });

  // Concurrent refresh BEFORE confirming keeps pending confirmation intact
  await Promise.all([
    page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/billing`) && r.request().method() === 'GET'),
    billingA.getByRole('button', { name: /Refresh billing status/i }).click(),
  ]);
  await confirmPlanBtn.waitFor({ timeout: 3000 });
  assert.equal(await billingA.getByLabel(/Change plan/i).inputValue(), 'team');
  assert.equal(await billingA.getByLabel(/Update seats/i).inputValue(), '3');

  // Concurrent refresh WHILE POST /billing/plan is in flight keeps button disabled (no double submit!)
  let releasePlanUpdate = null;
  await page.route(`**/v1/workspaces/${wsAId}/billing/plan`, async (route) => {
    await new Promise((resolve) => {
      releasePlanUpdate = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await confirmPlanBtn.click();
  const updatingBtn = billingA.getByRole('button', { name: /Updating plan/i });
  await updatingBtn.waitFor({ timeout: 3000 });
  assert.equal(await updatingBtn.isDisabled(), true);

  await Promise.all([
    page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/billing`) && r.request().method() === 'GET'),
    billingA.getByRole('button', { name: /Refresh billing status/i }).click(),
  ]);
  assert.equal(
    await billingA.getByRole('button', { name: /Updating plan/i }).isDisabled(),
    true,
    'concurrent refresh must NOT unlock an in-flight plan update'
  );

  const planRespWait = page.waitForResponse(
    (r) => r.url().includes(`/v1/workspaces/${wsAId}/billing/plan`) && r.status() === 200
  );
  await releasePlanUpdate();
  await planRespWait;
  await billingA.locator('dd', { hasText: /^Team \(3 seats\)$/i }).waitFor({ timeout: 5000 });
  await page.unroute(`**/v1/workspaces/${wsAId}/billing/plan`);

  // Concurrent refresh while POST /billing/cancel is pending confirmation AND in flight keeps button disabled
  await billingA.getByRole('button', { name: /Cancel subscription/i }).click();
  const confirmCancelBtn = billingA.getByRole('button', { name: /Confirm cancellation/i });
  await confirmCancelBtn.waitFor({ timeout: 3000 });

  await Promise.all([
    page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/billing`) && r.request().method() === 'GET'),
    billingA.getByRole('button', { name: /Refresh billing status/i }).click(),
  ]);
  await confirmCancelBtn.waitFor({ timeout: 3000 });

  let releaseCancel = null;
  await page.route(`**/v1/workspaces/${wsAId}/billing/cancel`, async (route) => {
    await new Promise((resolve) => {
      releaseCancel = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await confirmCancelBtn.click();
  const cancelingBtn = billingA.getByRole('button', { name: /Canceling/i });
  await cancelingBtn.waitFor({ timeout: 3000 });
  assert.equal(await cancelingBtn.isDisabled(), true);

  await Promise.all([
    page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/billing`) && r.request().method() === 'GET'),
    billingA.getByRole('button', { name: /Refresh billing status/i }).click(),
  ]);
  assert.equal(
    await billingA.getByRole('button', { name: /Canceling/i }).isDisabled(),
    true,
    'concurrent refresh must NOT unlock an in-flight cancel mutation'
  );

  const cancelRespWait = page.waitForResponse(
    (r) => r.url().includes(`/v1/workspaces/${wsAId}/billing/cancel`) && r.status() === 200
  );
  await releaseCancel();
  await cancelRespWait;
  await billingA.getByText(/Cancels at period end/i).waitFor({ timeout: 5000 });
  await page.unroute(`**/v1/workspaces/${wsAId}/billing/cancel`);

  await saveScreenshot(page, '33-billing-race-isolation');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey22BillingMachinesLimitExcludesRevokedDevices(browser) {
  const fixtures = loadFixtures();
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-sub-1', email: 'olivia@example.com' });

  await page.getByRole('link', { name: new RegExp(fixtures.quotaWorkspaceName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.quotaWorkspaceId}$`), { timeout: 5000 });

  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^2 \/ 3$/).waitFor({ timeout: 5000 });

  await page.getByText('owner-box', { exact: true }).waitFor({ timeout: 3000 });
  await page.getByText('quinn-box', { exact: true }).waitFor({ timeout: 3000 });

  // Remove member Quinn -> backend revokes quinn-box (status='revoked')
  const membersSection = page.getByRole('region', { name: /^Members$/i });
  await membersSection.getByRole('button', { name: /^Remove quinn@example\.com$/i }).click();

  // Revoked machine stays visible in Machines list with Revoked badge, while Machines limit drops to 1 / 3
  const quinnCard = page.locator('.machine-card', { hasText: 'quinn-box' });
  await quinnCard.getByText(/^Revoked$/i).waitFor({ timeout: 5000 });
  await billingSection.locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^1 \/ 3$/).waitFor({ timeout: 5000 });

  // Reloading persists both the revoked machine entry and 1 / 3 quota
  await page.reload({ waitUntil: 'networkidle' });
  await page.locator('.machine-card', { hasText: 'quinn-box' }).getByText(/^Revoked$/i).waitFor({ timeout: 5000 });
  await page.getByRole('region', { name: /^Billing$/i }).locator('.meta-item', { hasText: /^Machines limit/i }).getByText(/^1 \/ 3$/).waitFor({ timeout: 5000 });

  await saveScreenshot(page, '34-billing-revoked-device-quota');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey23WorkspaceAsyncIsolationMembersHistoryProposalsInvites(browser) {
  const fixtures = loadFixtures();
  const wsAId = fixtures.historyWorkspaceId;
  const wsAName = fixtures.historyWorkspaceName;
  const wsBId = fixtures.quorumWorkspaceId;
  const wsBName = fixtures.quorumWorkspaceName;
  const head2 = fixtures.revisions[0].revision;

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-sub-1', email: 'olivia@example.com' });

  // 1. Hold real GET /members, /revisions, /proposals for wsA while navigating wsA -> wsB
  let releaseMembersA = null;
  let releaseRevisionsA = null;
  let releaseProposalsA = null;
  await page.route(`**/v1/workspaces/${wsAId}/members`, async (route) => {
    await new Promise((resolve) => {
      releaseMembersA = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await page.route(`**/v1/workspaces/${wsAId}/revisions*`, async (route) => {
    if (route.request().url().includes('/diff')) {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseRevisionsA = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await page.route(`**/v1/workspaces/${wsAId}/proposals`, async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseProposalsA = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await page.getByRole('link', { name: new RegExp(wsAName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await page.waitForFunction(() => true);
  await page.getByText(/Loading members/i).waitFor({ timeout: 3000 });
  assert.ok(releaseMembersA && releaseRevisionsA && releaseProposalsA, 'expected all three wsA GETs to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsBName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });

  const membersB = page.getByRole('region', { name: /^Members$/i });
  const historyB = page.getByRole('region', { name: /^History$/i });
  const proposalsB = page.getByRole('region', { name: /^Proposals$/i });
  await membersB.getByText('olivia@example.com', { exact: true }).waitFor({ timeout: 5000 });
  await proposalsB.locator('article', { hasText: fixtures.pendingQuorumRevision.slice(0, 8) }).waitFor({ timeout: 5000 });

  // Release held wsA GET responses while viewing wsB -> none may overwrite wsB
  const respMembersA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/members`) && r.status() === 200);
  const respRevisionsA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/revisions`) && r.status() === 200);
  const respProposalsA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/proposals`) && r.status() === 200);
  await releaseMembersA();
  await releaseRevisionsA();
  await releaseProposalsA();
  await Promise.all([respMembersA, respRevisionsA, respProposalsA]);
  await page.waitForTimeout(100);

  assert.equal(await membersB.getByText('peter@example.com').count(), 0, 'stale wsA members leaked into wsB');
  assert.equal(await historyB.getByText(/update config and vault/i).count(), 0, 'stale wsA revisions leaked into wsB');
  assert.equal(await proposalsB.getByText(/beta-box/i).count(), 0, 'stale wsA proposals leaked into wsB');
  await page.unroute(`**/v1/workspaces/${wsAId}/members`);
  await page.unroute(`**/v1/workspaces/${wsAId}/revisions*`);
  await page.unroute(`**/v1/workspaces/${wsAId}/proposals`);

  // 2. Hold real GET /diff for wsA while navigating back to wsB via browser Back
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsAName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  const historyA = page.getByRole('region', { name: /^History$/i });
  await historyA.getByText(/update config and vault/i).waitFor({ timeout: 5000 });

  let releaseDiffA = null;
  await page.route(`**/v1/workspaces/${wsAId}/revisions/${head2}/diff`, async (route) => {
    await new Promise((resolve) => {
      releaseDiffA = async () => {
        await route.continue();
        resolve();
      };
    });
  });
  await historyA.locator('article', { hasText: 'update config and vault' }).getByRole('button', { name: /View diff/i }).click();
  await historyA.getByText(/Loading diff/i).waitFor({ timeout: 3000 });
  assert.ok(releaseDiffA, 'expected wsA diff GET to be held');

  await page.goBack({ waitUntil: 'networkidle' });
  await page.goBack({ waitUntil: 'networkidle' });
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  await proposalsB.locator('article', { hasText: fixtures.pendingQuorumRevision.slice(0, 8) }).waitFor({ timeout: 5000 });

  const respDiffA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/revisions/${head2}/diff`) && r.status() === 200);
  await releaseDiffA();
  await respDiffA;
  await page.waitForTimeout(100);
  assert.equal(await historyB.getByText('settings.toml').count(), 0, 'stale wsA diff leaked into wsB');
  await page.unroute(`**/v1/workspaces/${wsAId}/revisions/${head2}/diff`);

  // 3. Hold real POST /invitations (returns shareable invite token) in wsA while navigating wsA -> wsB
  await page.goForward({ waitUntil: 'networkidle' });
  await page.goForward({ waitUntil: 'networkidle' });
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  const membersA = page.getByRole('region', { name: /^Members$/i });
  await membersA.getByLabel(/Invite email/i).waitFor({ timeout: 5000 });

  let releaseInviteCreateA = null;
  await page.route(`**/v1/workspaces/${wsAId}/invitations`, async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseInviteCreateA = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await membersA.getByLabel(/Invite email/i).fill('delayed-invite@example.com');
  await membersA.getByRole('button', { name: /Send invite/i }).click();
  await membersA.getByRole('button', { name: /Sending/i }).waitFor({ timeout: 3000 });
  assert.ok(releaseInviteCreateA, 'expected POST /invitations on wsA to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsBName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  await membersB.getByLabel(/Invite email/i).waitFor({ timeout: 5000 });
  assert.equal(
    await membersB.getByRole('button', { name: /Send invite/i }).isDisabled(),
    false,
    'Send invite in wsB must not stay locked by in-flight invite on wsA'
  );

  const respInviteA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/invitations`) && r.status() === 201);
  await releaseInviteCreateA();
  await respInviteA;
  await page.waitForTimeout(100);
  assert.equal(
    await membersB.getByLabel(/Shareable invite link/i).count(),
    0,
    'stale wsA invitation token/link must not render inside wsB'
  );
  assert.equal(
    await membersB.getByText('delayed-invite@example.com').count(),
    0,
    'stale wsA invitation row must not render inside wsB'
  );
  await page.unroute(`**/v1/workspaces/${wsAId}/invitations`);

  // 4. Stale real 409 error from wsA (last-owner Leave workspace) must not show alert in wsB
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsAName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await membersA.getByRole('button', { name: /Leave workspace/i }).waitFor({ timeout: 5000 });

  let releaseMemberErrA = null;
  await page.route(`**/v1/workspaces/${wsAId}/members/*`, async (route) => {
    if (route.request().method() !== 'DELETE') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseMemberErrA = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await membersA.getByRole('button', { name: /Leave workspace/i }).click();
  await membersA.locator('button', { hasText: /Leaving/i }).waitFor({ timeout: 3000 });
  assert.ok(releaseMemberErrA, 'expected 409 DELETE /members/* on wsA to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsBName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  await membersB.getByText('olivia@example.com', { exact: true }).waitFor({ timeout: 5000 });

  const respMemberErrA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/members/`) && r.status() === 409);
  await releaseMemberErrA();
  await respMemberErrA;
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[role="alert"]').count(), 0, 'stale 409 error from wsA must not render in wsB');
  await page.unroute(`**/v1/workspaces/${wsAId}/members/*`);

  // 4b. Hold real POST /proposals (restore proposal) on wsA while navigating wsA -> wsB
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsAName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  const proposalsA = page.getByRole('region', { name: /^Proposals$/i });
  await proposalsA.getByLabel(/Paths to restore/i).waitFor({ timeout: 5000 });

  let releaseRestoreA = null;
  await page.route(`**/v1/workspaces/${wsAId}/proposals`, async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseRestoreA = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await proposalsA.getByLabel(/Paths to restore/i).fill('settings.toml');
  await proposalsA.getByRole('button', { name: /Create restore proposal/i }).click();
  await proposalsA.getByRole('button', { name: /Creating proposal/i }).waitFor({ timeout: 3000 });
  assert.ok(releaseRestoreA, 'expected POST /proposals (restore) on wsA to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsBName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });
  await proposalsB.locator('article', { hasText: fixtures.pendingQuorumRevision.slice(0, 8) }).waitFor({ timeout: 5000 });

  const respRestoreA = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/proposals`) && r.request().method() === 'POST' && r.status() === 201);
  await releaseRestoreA();
  await respRestoreA;
  await page.waitForTimeout(100);
  assert.equal(
    await page.getByText(/Restore proposal created \(unsigned\)/i).count(),
    0,
    'stale wsA restore confirmation must not render inside wsB'
  );
  assert.equal(
    await proposalsB.getByText(/^Restore$/i).count(),
    0,
    'stale wsA restore proposal card must not render inside wsB'
  );
  await page.unroute(`**/v1/workspaces/${wsAId}/proposals`);

  // 5. Stale real 401 across Sign out -> must not leak error banner or corrupt signed-out state
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: new RegExp(wsAName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await membersA.getByLabel(/Invite email/i).waitFor({ timeout: 5000 });

  let releaseInvite401A = null;
  await page.route(`**/v1/workspaces/${wsAId}/invitations`, async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseInvite401A = async () => {
        await route.continue();
        resolve();
      };
    });
  });

  await membersA.getByLabel(/Invite email/i).fill('stale-401@example.com');
  await membersA.getByRole('button', { name: /Send invite/i }).click();
  await membersA.getByRole('button', { name: /Sending/i }).waitFor({ timeout: 3000 });
  assert.ok(releaseInvite401A, 'expected 401 POST /invitations on wsA to be held');

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  const respInvite401 = page.waitForResponse((r) => r.url().includes(`/v1/workspaces/${wsAId}/invitations`) && r.status() === 401);
  await releaseInvite401A();
  await respInvite401;
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.locator('[role="alert"]').count(), 0, 'stale 401 after sign out must not render error alert');
  await page.unroute(`**/v1/workspaces/${wsAId}/invitations`);

  await saveScreenshot(page, '35-workspace-async-isolation');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey24SessionAccountOwnershipAndReset(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  const alice = { subject: 'alice-sess-1', email: 'alice-sess@example.com' };
  const bob = { subject: 'bob-sess-1', email: 'bob-sess@example.com' };

  // Setup: sign in as Alice, create one base workspace, toggle dark theme
  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  await page.getByLabel(/Workspace name/i).fill('Alice Base Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  await page.getByRole('button', { name: /Switch to dark mode/i }).click();
  assert.equal(await page.evaluate(() => document.documentElement.getAttribute('data-theme')), 'dark');
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Alice Base Fleet/i }).waitFor({ timeout: 5000 });

  // 1a. Stale GET /v1/workspaces (real 200 fetched before logout, fulfilled after logout)
  let releaseList200 = null;
  let resolveList200Fetched = null;
  const list200Fetched = new Promise((r) => {
    resolveList200Fetched = r;
  });
  let holdNextList = true;
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'GET' || !holdNextList) {
      await route.continue();
      return;
    }
    holdNextList = false;
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseList200 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveList200Fetched();
    });
  });

  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByText(/Loading workspaces/i).waitFor({ timeout: 5000 });
  await list200Fetched;
  assert.ok(releaseList200, 'expected GET /v1/workspaces 200 to be held');

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  await releaseList200();
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole('link', { name: /Alice Base Fleet/i }).count(), 0);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/workspaces');

  // 1b. Real stale 401 on GET /v1/workspaces (held before route.continue, released after logout)
  await signInViaIdp(page, alice);
  assert.equal(await page.evaluate(() => document.documentElement.getAttribute('data-theme')), 'dark');
  let releaseList401 = null;
  let resolveList401Held = null;
  const list401Held = new Promise((r) => {
    resolveList401Held = r;
  });
  let holdList401 = true;
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'GET' || !holdList401) {
      await route.continue();
      return;
    }
    holdList401 = false;
    await new Promise((resolve) => {
      releaseList401 = async () => {
        await route.continue();
        resolve();
      };
      resolveList401Held();
    });
  });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByText(/Loading workspaces/i).waitFor({ timeout: 5000 });
  await list401Held;
  assert.ok(releaseList401, 'expected GET /v1/workspaces to be held before 401');
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  const respList401 = page.waitForResponse(
    (r) => new URL(r.url()).pathname === '/v1/workspaces' && r.request().method() === 'GET' && r.status() === 401
  );
  await releaseList401();
  await respList401;
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/workspaces');

  // 2a. Stale POST /v1/workspaces (real 201 after logout) + form/idempotency reset + same-account reauth
  await signInViaIdp(page, alice);
  const seenKeysSession1 = [];
  let abortCreateOnce = true;
  let releaseCreate201 = null;
  let resolveCreate201Fetched = null;
  const create201Fetched = new Promise((r) => {
    resolveCreate201Fetched = r;
  });
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    const key = route.request().headers()['idempotency-key'];
    if (key) seenKeysSession1.push(key);
    if (abortCreateOnce) {
      abortCreateOnce = false;
      await route.abort('failed');
      return;
    }
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseCreate201 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveCreate201Fetched();
    });
  });

  await page.getByLabel(/Workspace name/i).fill('Dirty Retry Name');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.locator('[role="alert"]').waitFor({ timeout: 5000 });
  assert.equal(seenKeysSession1.length, 1);

  await page.getByLabel(/Workspace name/i).fill('Delayed Created Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.getByRole('button', { name: /Creating/i }).waitFor({ timeout: 3000 });
  await create201Fetched;
  assert.ok(releaseCreate201, 'expected POST /v1/workspaces 201 to be held');

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  await releaseCreate201();
  await page.waitForTimeout(100);
  assert.equal(new URL(page.url()).pathname, '/', 'stale 201 create must not navigate after logout');
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole('heading', { name: 'Delayed Created Fleet' }).count(), 0);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/workspaces');

  // Same-account reauthentication: completed mutation observed, theme preserved, old form/idempotency cleared, fresh retry works
  await signInViaIdp(page, alice);
  assert.equal(await page.evaluate(() => document.documentElement.getAttribute('data-theme')), 'dark');
  await page.getByRole('link', { name: /Delayed Created Fleet/i }).waitFor({ timeout: 5000 });
  assert.equal(await page.getByLabel(/Workspace name/i).inputValue(), '');
  assert.equal(await page.locator('[role="alert"]').count(), 0);

  const seenKeysSession2 = [];
  let dropSession2Once = true;
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    const key = route.request().headers()['idempotency-key'];
    if (key) seenKeysSession2.push(key);
    if (dropSession2Once) {
      dropSession2Once = false;
      await route.abort('failed');
      return;
    }
    await route.continue();
  });

  await page.getByLabel(/Workspace name/i).fill('Fresh Retry Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.locator('[role="alert"]').waitFor({ timeout: 5000 });
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.getByRole('heading', { name: 'Fresh Retry Fleet' }).waitFor({ timeout: 5000 });
  assert.equal(seenKeysSession2.length, 2);
  assert.equal(seenKeysSession2[0], seenKeysSession2[1], 'retry in new session must reuse new key');
  assert.notEqual(seenKeysSession2[0], seenKeysSession1[0], 'new session must not reuse previous session idempotency key');
  await page.unroute('**/v1/workspaces');

  // 2b. Real stale 401 on POST /v1/workspaces (held before route.continue, released after logout)
  await page.getByRole('link', { name: /All workspaces/i }).click();
  let releaseCreate401 = null;
  let resolveCreate401Held = null;
  const create401Held = new Promise((r) => {
    resolveCreate401Held = r;
  });
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue();
      return;
    }
    await new Promise((resolve) => {
      releaseCreate401 = async () => {
        await route.continue();
        resolve();
      };
      resolveCreate401Held();
    });
  });
  await page.getByLabel(/Workspace name/i).fill('Held 401 Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.getByRole('button', { name: /Creating/i }).waitFor({ timeout: 3000 });
  await create401Held;
  assert.ok(releaseCreate401, 'expected POST /v1/workspaces to be held before 401');
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  const respCreate401 = page.waitForResponse(
    (r) => new URL(r.url()).pathname === '/v1/workspaces' && r.request().method() === 'POST' && r.status() === 401
  );
  await releaseCreate401();
  await respCreate401;
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/workspaces');

  // 3. Stale device decision (real 200, real 400 error, real 401) + reset + route re-derivation
  const auth1 = await startDeviceAuthorization('stale-device-cli');
  const auth2 = await startDeviceAuthorization('fresh-device-cli');

  await page.goto(auth1.verification_uri_complete, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  await page.waitForURL((u) => new URL(u).pathname === '/device', { timeout: 5000 });

  let releaseDevice200 = null;
  let resolveDevice200Fetched = null;
  const device200Fetched = new Promise((r) => {
    resolveDevice200Fetched = r;
  });
  await page.route('**/v1/device/approve', async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseDevice200 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveDevice200Fetched();
    });
  });
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.getByRole('button', { name: /Submitting/i }).waitFor({ timeout: 3000 });
  await device200Fetched;
  assert.ok(releaseDevice200, 'expected POST /v1/device/approve 200 to be held');

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  await releaseDevice200();
  await page.waitForTimeout(100);
  assert.equal(await page.getByRole('heading', { name: /CLI login approved/i }).count(), 0);
  assert.equal((await page.locator('main').innerText()).includes('stale-device-cli'), false);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/device/approve');

  // Observe completed server mutation via real device token poll
  const devToken1 = await pollDeviceToken(auth1.device_code);
  assert.equal(devToken1.status, 200);
  assert.ok(devToken1.body.access_token.startsWith('mca_'));

  // Stale 400 device error across logout
  await page.goto(`${baseUrl}/device`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  let releaseDevice400 = null;
  let resolveDevice400Fetched = null;
  const device400Fetched = new Promise((r) => {
    resolveDevice400Fetched = r;
  });
  await page.route('**/v1/device/approve', async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseDevice400 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveDevice400Fetched();
    });
  });
  await page.getByLabel(/Verification code/i).fill('ZZZZ-9999');
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.getByRole('button', { name: /Submitting/i }).waitFor({ timeout: 3000 });
  await device400Fetched;
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await releaseDevice400();
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/device/approve');

  // Stale 401 on device decision + fresh usable device approval on reauth
  await page.goto(auth2.verification_uri_complete, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  assert.equal((await page.getByLabel(/Verification code/i).inputValue()).toUpperCase(), auth2.user_code);
  let releaseDevice401 = null;
  let resolveDevice401Held = null;
  const device401Held = new Promise((r) => {
    resolveDevice401Held = r;
  });
  await page.route('**/v1/device/approve', async (route) => {
    await new Promise((resolve) => {
      releaseDevice401 = async () => {
        await route.continue();
        resolve();
      };
      resolveDevice401Held();
    });
  });
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.getByRole('button', { name: /Submitting/i }).waitFor({ timeout: 3000 });
  await device401Held;
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  const respDev401 = page.waitForResponse((r) => r.url().includes('/v1/device/approve') && r.status() === 401);
  await releaseDevice401();
  await respDev401;
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/device/approve');

  await page.goto(auth2.verification_uri_complete, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  await page.getByRole('button', { name: /Approve/i }).click();
  await page.getByRole('heading', { name: /CLI login approved/i }).waitFor({ timeout: 5000 });
  await page.getByText('fresh-device-cli').waitFor({ timeout: 5000 });
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  // 4. Stale explicit invitation acceptance (403 mismatch, 200 accept, 401)
  const bobCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const bobPage = await bobCtx.newPage();
  await bobPage.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(bobPage, bob);
  await bobPage.getByLabel(/Workspace name/i).fill('Invite Session Fleet');
  await bobPage.getByRole('button', { name: /Create workspace/i }).click();
  await bobPage.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const bobMembers = bobPage.getByRole('region', { name: /^Members$/i });
  await bobMembers.getByLabel(/Invite email/i).fill('carol-sess@example.com');
  await bobMembers.getByRole('button', { name: /Send invite/i }).click();
  const mismatchInviteUrl = await bobMembers.getByLabel(/Shareable invite link/i).inputValue();
  await bobMembers.getByRole('button', { name: /Dismiss/i }).click();

  await bobMembers.getByLabel(/Invite email/i).fill(alice.email);
  await bobMembers.getByRole('button', { name: /Send invite/i }).click();
  const aliceInviteUrl = await bobMembers.getByLabel(/Shareable invite link/i).inputValue();

  // Create second workspace for 401 invite test
  await bobPage.getByRole('link', { name: /All workspaces/i }).click();
  await bobPage.getByLabel(/Workspace name/i).fill('Invite 401 Fleet');
  await bobPage.getByRole('button', { name: /Create workspace/i }).click();
  await bobPage.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const bobMembers2 = bobPage.getByRole('region', { name: /^Members$/i });
  await bobMembers2.getByLabel(/Invite email/i).fill(alice.email);
  await bobMembers2.getByRole('button', { name: /Send invite/i }).click();
  const aliceInvite401Url = await bobMembers2.getByLabel(/Shareable invite link/i).inputValue();
  await bobCtx.close();

  // 4a. Stale 403 invitation_mismatch held across Sign out
  await page.goto(mismatchInviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  let releaseInvite403 = null;
  let resolveInvite403Fetched = null;
  const invite403Fetched = new Promise((r) => {
    resolveInvite403Fetched = r;
  });
  await page.route('**/v1/invitations/*/accept', async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseInvite403 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveInvite403Fetched();
    });
  });
  await page.getByRole('button', { name: /Accept invitation/i }).click();
  await page.getByRole('button', { name: /Accepting/i }).waitFor({ timeout: 3000 });
  await invite403Fetched;
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await releaseInvite403();
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/invitations/*/accept');

  // 4b. Stale 200 invitation accept held across Sign out, then observed on reauth
  await page.goto(aliceInviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  let releaseInvite200 = null;
  let resolveInvite200Fetched = null;
  const invite200Fetched = new Promise((r) => {
    resolveInvite200Fetched = r;
  });
  await page.route('**/v1/invitations/*/accept', async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseInvite200 = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveInvite200Fetched();
    });
  });
  await page.getByRole('button', { name: /Accept invitation/i }).click();
  await page.getByRole('button', { name: /Accepting/i }).waitFor({ timeout: 3000 });
  await invite200Fetched;
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await releaseInvite200();
  await page.waitForTimeout(100);
  assert.equal(await page.getByRole('heading', { name: /Invitation accepted/i }).count(), 0);
  assert.equal(await page.getByRole('link', { name: /Open workspace/i }).count(), 0);
  await page.unroute('**/v1/invitations/*/accept');

  await signInViaIdp(page, alice);
  await page.getByRole('link', { name: /Invite Session Fleet/i }).waitFor({ timeout: 5000 });
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  // 4c. Real stale 401 on invitation accept
  await page.goto(aliceInvite401Url, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  let releaseInvite401 = null;
  let resolveInvite401Held = null;
  const invite401Held = new Promise((r) => {
    resolveInvite401Held = r;
  });
  await page.route('**/v1/invitations/*/accept', async (route) => {
    await new Promise((resolve) => {
      releaseInvite401 = async () => {
        await route.continue();
        resolve();
      };
      resolveInvite401Held();
    });
  });
  await page.getByRole('button', { name: /Accept invitation/i }).click();
  await page.getByRole('button', { name: /Accepting/i }).waitFor({ timeout: 3000 });
  await invite401Held;
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  const respInv401 = page.waitForResponse((r) => r.url().includes('/accept') && r.status() === 401);
  await releaseInvite401();
  await respInv401;
  await page.waitForTimeout(100);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/invitations/*/accept');

  // 5a. Obsolete bootstrap (init): held 401 Retry #1 must not wipe out signed-in Retry #2
  let mePhase = 'abort_initial';
  let releaseBootstrap401 = null;
  let resolveBootstrap401Fetched = null;
  const bootstrap401Fetched = new Promise((r) => {
    resolveBootstrap401Fetched = r;
  });
  await page.route('**/v1/me', async (route) => {
    if (mePhase === 'abort_initial') {
      mePhase = 'hold_retry_401';
      await route.abort('failed');
      return;
    }
    if (mePhase === 'hold_retry_401') {
      mePhase = 'passthrough';
      const response = await route.fetch();
      assert.equal(response.status(), 401);
      await new Promise((resolve) => {
        releaseBootstrap401 = async () => {
          await route.fulfill({ response });
          resolve();
        };
        resolveBootstrap401Fetched();
      });
      return;
    }
    await route.continue();
  });

  await page.goto(`${baseUrl}/`, { waitUntil: 'domcontentloaded' });
  await page.getByText(/Could not reach Muster Cloud/i).waitFor({ timeout: 5000 });
  const retryBtn = page.getByRole('button', { name: /Retry/i });
  await retryBtn.click();
  await bootstrap401Fetched;
  assert.ok(releaseBootstrap401, 'expected first Retry /v1/me 401 to be held');

  // Sign in via second tab sharing the browser context cookies
  const tab2 = await context.newPage();
  await tab2.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(tab2, alice);
  await tab2.getByText(alice.email, { exact: true }).waitFor({ timeout: 5000 });

  // Second Retry on original page establishes current session in-document
  await retryBtn.click();
  await page.getByText(alice.email, { exact: true }).waitFor({ timeout: 5000 });
  await page.getByRole('link', { name: /Alice Base Fleet/i }).waitFor({ timeout: 5000 });

  // Release obsolete 401 from Retry #1 -> must NOT clear state.me or render Sign in
  await releaseBootstrap401();
  await page.waitForTimeout(100);
  await page.getByText(alice.email, { exact: true }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole('link', { name: /Sign in/i }).count(), 0);
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/me');

  // 5a-2. Obsolete bootstrap 200 (Alice) must not overwrite newer session (Bob) in same document
  const page3 = await context.newPage();
  let p3MePhase = 'abort_initial';
  let releaseBootstrapAlice200 = null;
  let resolveBootstrapAliceFetched = null;
  const bootstrapAliceFetched = new Promise((r) => {
    resolveBootstrapAliceFetched = r;
  });
  await page3.route('**/v1/me', async (route) => {
    if (p3MePhase === 'abort_initial') {
      p3MePhase = 'hold_retry_200';
      await route.abort('failed');
      return;
    }
    if (p3MePhase === 'hold_retry_200') {
      p3MePhase = 'passthrough';
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      await new Promise((resolve) => {
        releaseBootstrapAlice200 = async () => {
          await route.fulfill({ response });
          resolve();
        };
        resolveBootstrapAliceFetched();
      });
      return;
    }
    await route.continue();
  });
  await page3.goto(`${baseUrl}/`, { waitUntil: 'domcontentloaded' });
  await page3.getByText(/Could not reach Muster Cloud/i).waitFor({ timeout: 5000 });
  const p3RetryBtn = page3.getByRole('button', { name: /Retry/i });
  await p3RetryBtn.click();
  await bootstrapAliceFetched;
  assert.ok(releaseBootstrapAlice200, 'expected stale 200 /v1/me to be held');

  // Switch tab2 from Alice to Bob
  await tab2.getByRole('button', { name: /Sign out/i }).click();
  await tab2.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await signInViaIdp(tab2, bob);
  await tab2.getByText(bob.email, { exact: true }).waitFor({ timeout: 5000 });

  // Retry #2 on page3 signs in as Bob
  await p3RetryBtn.click();
  await page3.getByText(bob.email, { exact: true }).waitFor({ timeout: 5000 });
  await page3.getByRole('link', { name: /Invite Session Fleet/i }).waitFor({ timeout: 5000 });

  // Release stale 200 Alice -> Bob UI must remain intact
  await releaseBootstrapAlice200();
  await page3.waitForTimeout(100);
  await page3.getByText(bob.email, { exact: true }).waitFor({ timeout: 3000 });
  assert.equal(await page3.getByText(alice.email, { exact: true }).count(), 0);
  assert.equal(await page3.getByRole('link', { name: /Alice Base Fleet/i }).count(), 0);
  await page3.unroute('**/v1/me');
  await page3.close();

  // Sign back in as Alice on tab2 for logout tests
  await tab2.getByRole('button', { name: /Sign out/i }).click();
  await tab2.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await signInViaIdp(tab2, alice);
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByText(alice.email, { exact: true }).waitFor({ timeout: 5000 });

  // 5b. Failed logout -> Retry sign out works; obsolete logout failure does not overwrite expired state
  let abortLogoutOnce = true;
  await page.route('**/v1/auth/logout', async (route) => {
    if (abortLogoutOnce) {
      abortLogoutOnce = false;
      await route.abort('failed');
      return;
    }
    await route.continue();
  });
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByText(/Could not sign out/i).waitFor({ timeout: 5000 });
  await page.getByText(alice.email, { exact: true }).waitFor({ timeout: 3000 });
  await page.getByRole('button', { name: /Retry sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await page.unroute('**/v1/auth/logout');

  // Obsolete logout failure after session already expired via real 401
  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, alice);
  await page.getByRole('link', { name: /Alice Base Fleet/i }).waitFor({ timeout: 5000 });

  let releaseObsoleteLogout = null;
  let resolveObsoleteLogoutHeld = null;
  const obsoleteLogoutHeld = new Promise((r) => {
    resolveObsoleteLogoutHeld = r;
  });
  await page.route('**/v1/auth/logout', async (route) => {
    await new Promise((resolve) => {
      releaseObsoleteLogout = async () => {
        await route.abort('failed');
        resolve();
      };
      resolveObsoleteLogoutHeld();
    });
  });
  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('button', { name: /Signing out/i }).waitFor({ timeout: 3000 });
  await obsoleteLogoutHeld;
  assert.ok(releaseObsoleteLogout, 'expected POST /v1/auth/logout to be held');

  // Sign out on tab2 so server session is truly invalidated
  await tab2.reload({ waitUntil: 'networkidle' });
  await tab2.getByRole('button', { name: /Sign out/i }).click();
  await tab2.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });
  await tab2.close();

  // Trigger real 401 on page while old logout is in flight -> page resets to signed out
  await page.getByRole('link', { name: /Alice Base Fleet/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  // Release held logout failure -> obsolete logout must NOT publish signOutError
  await releaseObsoleteLogout();
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  await page.unroute('**/v1/auth/logout');

  await saveScreenshot(page, '36-session-account-ownership');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function extractHistoryRows(historySection) {
  return historySection.locator('article').evaluateAll((articles) =>
    articles.map((article) => ({
      message: article.querySelector('strong')?.textContent?.trim() || '',
      shortRev: article.querySelector('.badge.mono')?.textContent?.trim() || '',
    }))
  );
}

async function journey25HistoryPaginationAndIsolation(browser) {
  const fixtures = loadFixtures();
  assert.equal(fixtures.paginationRevisions.length, 22);
  assert.equal(
    new Set(fixtures.paginationRevisions.map((r) => r.revision)).size,
    22,
    'expected 22 distinct pagination revisions'
  );
  const expectedFirst20 = fixtures.paginationRevisions.slice(0, 20).map((r) => ({
    message: r.message,
    shortRev: r.revision.slice(0, 8),
  }));
  const expectedAll22 = fixtures.paginationRevisions.map((r) => ({
    message: r.message,
    shortRev: r.revision.slice(0, 8),
  }));
  const expectedDestRows = fixtures.revisions.map((r) => ({
    message: r.message,
    shortRev: r.revision.slice(0, 8),
  }));

  const pageTwoNonGenesis = fixtures.paginationRevisions[20];
  assert.equal(pageTwoNonGenesis.message, 'touch rev-02.toml');
  assert.equal(fixtures.paginationRevisions[21].message, 'genesis');

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  let releaseSecondPage = null;
  let releasedSecondPage = false;
  const revisionsRoutePattern = `**/v1/workspaces/${fixtures.paginationWorkspaceId}/revisions*`;
  let interceptNextSecondPage = false;
  let resolveSecondPageHeld = null;
  const secondPageHeld = new Promise((r) => {
    resolveSecondPageHeld = r;
  });

  const routeHandler = async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    if (
      interceptNextSecondPage &&
      req.method() === 'GET' &&
      url.pathname === `/v1/workspaces/${fixtures.paginationWorkspaceId}/revisions` &&
      url.searchParams.get('before') === fixtures.paginationRevisions[19].revision &&
      url.searchParams.get('limit') === '20'
    ) {
      interceptNextSecondPage = false;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      await new Promise((resolve) => {
        releaseSecondPage = async () => {
          try {
            await route.fulfill({ response });
          } finally {
            resolve();
          }
        };
        resolveSecondPageHeld();
      });
      return;
    }
    await route.continue();
  };

  try {
    await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
    await signInViaIdp(page, { subject: 'olivia-sub-1', email: 'olivia@example.com' });

    // Open pagination workspace -> page 1 shows newest 20 revisions and "Load older revisions"
    await page.getByRole('link', { name: new RegExp(fixtures.paginationWorkspaceName, 'i') }).click();
    await page.waitForURL(new RegExp(`/workspaces/${fixtures.paginationWorkspaceId}$`), { timeout: 5000 });

    const historySection = page.getByRole('region', { name: /^History$/i });
    await historySection.waitFor({ timeout: 5000 });
    await historySection.getByText(expectedFirst20[0].message, { exact: true }).waitFor({ timeout: 5000 });
    const loadOlderBtn = historySection.getByRole('button', { name: /Load older revisions/i });
    await loadOlderBtn.waitFor({ timeout: 5000 });
    assert.deepEqual(
      await extractHistoryRows(historySection),
      expectedFirst20,
      'initial history page must match newest 20 revisions in exact order'
    );

    // Load older revisions -> all 22 rows in order including genesis, controls disappear
    await loadOlderBtn.click();
    await historySection.getByText('genesis', { exact: true }).waitFor({ timeout: 5000 });
    assert.equal(
      await historySection.getByRole('button', { name: /Load older revisions/i }).count(),
      0,
      'Load older revisions button must disappear after loading final page'
    );
    assert.equal(
      await historySection.getByRole('button', { name: /Loading/i }).count(),
      0,
      'Loading control must disappear after loading final page'
    );
    assert.equal(
      await historySection.getByText(/Loading/i).count(),
      0,
      'Loading indicator must not linger after pagination completes'
    );
    assert.deepEqual(
      await extractHistoryRows(historySection),
      expectedAll22,
      'loaded history must match all 22 revisions in exact acceptance order'
    );

    // Open a non-genesis revision from the second page and verify known file + patch
    const olderCard = historySection.locator('article', {
      hasText: pageTwoNonGenesis.revision.slice(0, 8),
    });
    await olderCard.getByRole('button', { name: /View diff/i }).click();
    await olderCard.getByText('rev-02.toml', { exact: true }).waitFor({ timeout: 5000 });
    await olderCard.getByText(/marker = "page-marker-02"/).waitFor({ timeout: 5000 });

    // Revisit pagination workspace via in-app links to restore page one
    await page.getByRole('link', { name: /All workspaces/i }).click();
    await page.getByRole('link', { name: new RegExp(fixtures.paginationWorkspaceName, 'i') }).click();
    await page.waitForURL(new RegExp(`/workspaces/${fixtures.paginationWorkspaceId}$`), { timeout: 5000 });

    await historySection.getByText(expectedFirst20[0].message, { exact: true }).waitFor({ timeout: 5000 });
    await loadOlderBtn.waitFor({ timeout: 5000 });
    assert.deepEqual(await extractHistoryRows(historySection), expectedFirst20);

    // Hold second-page GET (?before=...&limit=20) with actual upstream response, navigate away, release
    await page.route(revisionsRoutePattern, routeHandler);
    interceptNextSecondPage = true;

    await loadOlderBtn.click();
    await historySection.getByRole('button', { name: /Loading/i }).waitFor({ timeout: 5000 });
    await secondPageHeld;
    assert.ok(releaseSecondPage, 'expected second-page revisions GET with before+limit to be held');

    // Navigate via in-app links to History & Export Fleet
    await page.getByRole('link', { name: /All workspaces/i }).click();
    await page.getByRole('link', { name: new RegExp(fixtures.historyWorkspaceName, 'i') }).click();
    await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

    const destHistory = page.getByRole('region', { name: /^History$/i });
    await destHistory.getByText(expectedDestRows[0].message, { exact: true }).waitFor({ timeout: 5000 });
    await destHistory.getByText(expectedDestRows[1].message, { exact: true }).waitFor({ timeout: 5000 });

    // Verify destination history before releasing held second-page response
    assert.deepEqual(
      await extractHistoryRows(destHistory),
      expectedDestRows,
      'destination history before release must contain only destination revisions'
    );
    assert.equal(await destHistory.getByRole('button', { name: /Load older revisions/i }).count(), 0);
    assert.equal(await destHistory.getByRole('button', { name: /Loading/i }).count(), 0);
    assert.equal(await destHistory.getByText(/Loading history/i).count(), 0);
    assert.equal(await destHistory.locator('[role="alert"]').count(), 0);

    // Release unchanged upstream second-page response and wait for completion + browser processing
    const secondPageRespPromise = page.waitForResponse((r) => {
      const u = new URL(r.url());
      return (
        r.request().method() === 'GET' &&
        u.pathname === `/v1/workspaces/${fixtures.paginationWorkspaceId}/revisions` &&
        u.searchParams.get('before') === fixtures.paginationRevisions[19].revision &&
        u.searchParams.get('limit') === '20' &&
        r.status() === 200
      );
    });
    releasedSecondPage = true;
    await releaseSecondPage();
    await secondPageRespPromise;
    await page.evaluate(() => new Promise((r) => setTimeout(r, 100)));

    // Verify destination history after release: no stale rows, no lingering loading, no errors
    assert.deepEqual(
      await extractHistoryRows(destHistory),
      expectedDestRows,
      'stale second-page response must not alter destination workspace history'
    );
    assert.equal(await destHistory.getByText(/rev-\d{2}\.toml|touch rev-/).count(), 0);
    assert.equal(await destHistory.getByRole('button', { name: /Load older revisions/i }).count(), 0);
    assert.equal(await destHistory.getByRole('button', { name: /Loading/i }).count(), 0);
    assert.equal(await destHistory.getByText(/Loading history/i).count(), 0);
    assert.equal(await destHistory.locator('[role="alert"]').count(), 0);
    assert.equal(await page.locator('[role="alert"]').count(), 0);

    // Revisit pagination workspace and test failed "Load older revisions" keeps loaded rows & export intact and retries from same cursor
    await page.unroute(revisionsRoutePattern, routeHandler);
    await page.getByRole('link', { name: /All workspaces/i }).click();
    await page.getByRole('link', { name: new RegExp(fixtures.paginationWorkspaceName, 'i') }).click();
    await page.waitForURL(new RegExp(`/workspaces/${fixtures.paginationWorkspaceId}$`), { timeout: 5000 });

    await historySection.getByText(expectedFirst20[0].message, { exact: true }).waitFor({ timeout: 5000 });
    await loadOlderBtn.waitFor({ timeout: 5000 });
    const exportSection = page.getByRole('region', { name: /Export & offline recovery/i });
    await exportSection.getByRole('button', { name: /Download Git bundle/i }).waitFor({ timeout: 5000 });

    const seenSecondPageBeforeCursors = [];
    let failNextSecondPage = true;
    const cursorRouteHandler = async (route) => {
      const req = route.request();
      const url = new URL(req.url());
      if (
        req.method() === 'GET' &&
        url.pathname === `/v1/workspaces/${fixtures.paginationWorkspaceId}/revisions` &&
        url.searchParams.has('before')
      ) {
        seenSecondPageBeforeCursors.push(url.searchParams.get('before'));
        if (failNextSecondPage) {
          failNextSecondPage = false;
          await route.fulfill({
            status: 503,
            contentType: 'application/json',
            body: JSON.stringify({ error: { code: 'unavailable', message: 'temporary db outage' } }),
          });
          return;
        }
      }
      await route.continue();
    };
    await page.route(revisionsRoutePattern, cursorRouteHandler);

    await loadOlderBtn.click();
    const inlineAlert = historySection.locator('[role="alert"]');
    await inlineAlert.waitFor({ timeout: 5000 });
    const retryMoreBtn = inlineAlert.getByRole('button', { name: /^Retry$/i });
    await retryMoreBtn.waitFor({ timeout: 3000 });

    // Loaded revisions and export section must remain visible while inline error is shown
    assert.deepEqual(
      await extractHistoryRows(historySection),
      expectedFirst20,
      'loaded revisions must remain visible after Load older revisions fails'
    );
    assert.equal(
      await exportSection.getByRole('button', { name: /Download Git bundle/i }).count(),
      1,
      'export download must remain visible when only Load older revisions fails'
    );

    // Retry must resume from the exact same cursor (not reload from scratch)
    await retryMoreBtn.click();
    await historySection.getByText('genesis', { exact: true }).waitFor({ timeout: 5000 });
    assert.equal(await inlineAlert.count(), 0, 'inline error must clear after successful retry');
    assert.deepEqual(
      await extractHistoryRows(historySection),
      expectedAll22,
      'retrying Load older revisions must append remaining revisions in exact order'
    );
    assert.deepEqual(
      seenSecondPageBeforeCursors,
      [fixtures.paginationRevisions[19].revision, fixtures.paginationRevisions[19].revision],
      'retry must resume from the same before cursor'
    );
    await page.unroute(revisionsRoutePattern, cursorRouteHandler);

    await saveScreenshot(page, '37-history-pagination-isolation');
    assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  } finally {
    if (releaseSecondPage && !releasedSecondPage) {
      releasedSecondPage = true;
      await releaseSecondPage().catch(() => {});
    }
    await page.unroute(revisionsRoutePattern, routeHandler).catch(() => {});
    await context.close();
  }
}

async function journey26WorkspaceDeletionLifecycleRolesAndIdempotency(browser) {
  const fixtures = loadFixtures();
  const wsId = fixtures.historyWorkspaceId;
  const wsName = fixtures.historyWorkspaceName;

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-sub-1', email: 'olivia@example.com' });
  await page.getByRole('link', { name: new RegExp(wsName, 'i') }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  // Invite an admin so we verify owner vs admin vs member deletion UI rules
  const membersSection = page.getByRole('region', { name: /^Members$/i });
  await membersSection.waitFor({ timeout: 5000 });
  await membersSection.getByLabel(/Invite email/i).fill('admin-del@example.com');
  await membersSection.getByLabel(/Cloud role/i).selectOption('admin');
  await membersSection.getByRole('button', { name: /Send invite/i }).click();
  const adminInviteLink = membersSection.getByLabel(/Shareable invite link/i);
  await adminInviteLink.waitFor({ timeout: 5000 });
  const adminInviteUrl = await adminInviteLink.inputValue();

  const adminCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const adminPage = await adminCtx.newPage();
  const adminErrors = [];
  adminPage.on('pageerror', (err) => adminErrors.push(err.message));
  await adminPage.goto(adminInviteUrl, { waitUntil: 'networkidle' });
  await signInViaIdp(adminPage, { subject: 'admin-del-1', email: 'admin-del@example.com' });
  await adminPage.getByRole('button', { name: /Accept invitation/i }).click();
  await adminPage.getByRole('link', { name: /Open workspace/i }).click();
  await adminPage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  const memberCtx = await browser.newContext({ viewport: { width: 1280, height: 800 }, acceptDownloads: true });
  const memberPage = await memberCtx.newPage();
  const memberErrors = [];
  memberPage.on('pageerror', (err) => memberErrors.push(err.message));
  await memberPage.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(memberPage, { subject: 'peter-sub-1', email: 'peter@example.com' });
  await memberPage.getByRole('link', { name: new RegExp(wsName, 'i') }).click();
  await memberPage.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  // 1. Admin and Member see ZERO Delete workspace section/button/input
  await adminPage.getByRole('region', { name: /^Members$/i }).waitFor({ timeout: 5000 });
  await memberPage.getByRole('region', { name: /^Members$/i }).waitFor({ timeout: 5000 });
  assert.equal(await adminPage.getByRole('region', { name: /Delete workspace/i }).count(), 0);
  assert.equal(await adminPage.getByRole('button', { name: /delete workspace/i }).count(), 0);
  assert.equal(await memberPage.getByRole('region', { name: /Delete workspace/i }).count(), 0);
  assert.equal(await memberPage.getByRole('button', { name: /delete workspace/i }).count(), 0);

  // 2. Owner sees Delete workspace section with explanation & link to Export section
  const deleteSection = page.getByRole('region', { name: /^Delete workspace$/i });
  await deleteSection.waitFor({ timeout: 5000 });
  const deleteText = await deleteSection.innerText();
  assert.doesNotMatch(deleteText, /member changes/i, 'deletion text must not say member changes are refused');
  assert.match(deleteText, /member invitations/i, 'deletion text must state member invitations are refused');
  await deleteSection.getByText(/30-day/i).waitFor({ timeout: 3000 });
  await deleteSection.getByText(/export/i).first().waitFor({ timeout: 3000 });
  await deleteSection.getByText(/irreversible/i).waitFor({ timeout: 3000 });
  const exportJumpLink = deleteSection.getByRole('link', { name: /export/i });
  await exportJumpLink.waitFor({ timeout: 3000 });
  assert.equal(await exportJumpLink.getAttribute('href'), '#export-section');
  assert.ok((await page.locator('#export-section').count()) === 1);

  // 3. Client-side validation refuses wrong names with ZERO network DELETE requests
  const networkDeleteAttempts = [];
  page.on('request', (req) => {
    if (req.method() === 'DELETE' && new URL(req.url()).pathname === `/v1/workspaces/${wsId}`) {
      networkDeleteAttempts.push(req);
    }
  });

  const confirmInput = deleteSection.getByLabel(/workspace name/i);
  const deleteBtn = deleteSection.getByRole('button', { name: /^Delete workspace$/i });
  for (const wrong of ['', wsName.toLowerCase(), `${wsName} `]) {
    await confirmInput.fill(wrong);
    await deleteBtn.click();
    await deleteSection.locator('[role="alert"]').waitFor({ timeout: 3000 });
    assert.equal(await confirmInput.getAttribute('aria-invalid'), 'true');
  }
  assert.equal(networkDeleteAttempts.length, 0, 'wrong confirmation name must send zero DELETE requests');

  // 4. Right name + network drop -> error shown, never claims success; retry double-click reuses one Idempotency-Key and sends one request
  const seenDeletes = [];
  let dropFirstDelete = true;
  await page.route(`**/v1/workspaces/${wsId}`, async (route) => {
    if (route.request().method() !== 'DELETE') {
      await route.continue();
      return;
    }
    const headers = route.request().headers();
    seenDeletes.push({
      key: headers['idempotency-key'],
      body: route.request().postDataJSON(),
    });
    if (dropFirstDelete) {
      dropFirstDelete = false;
      await route.abort('failed');
      return;
    }
    await route.continue();
  });

  await confirmInput.fill(wsName);
  await deleteBtn.click();
  await deleteSection.locator('[role="alert"]').waitFor({ timeout: 5000 });
  assert.equal(await page.getByText(/Deletion scheduled/i).count(), 0, 'must never claim success on failed request');

  await deleteBtn.dblclick();
  const statusRegion = page.getByRole('region', { name: /Workspace deletion/i });
  await statusRegion.waitFor({ timeout: 5000 });
  await statusRegion.getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
  await page.unroute(`**/v1/workspaces/${wsId}`);

  assert.equal(seenDeletes.length, 2, `expected 2 DELETE attempts (1 aborted + 1 retry), got ${seenDeletes.length}`);
  assert.equal(seenDeletes[0].key, seenDeletes[1].key, 'retry must reuse exact same Idempotency-Key');
  assert.match(seenDeletes[0].key, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i);
  assert.deepEqual(seenDeletes[0].body, {
    confirm_workspace_id: wsId,
    confirm_workspace_name: wsName,
  });

  // 5. After 202: scheduled status with dates, invitations & writes disabled, member removals/departures stay enabled, export & diff still work
  for (const label of ['Requested', 'Export until', 'Primary purge due', 'Backup policy cap']) {
    const item = statusRegion.locator('.meta-item', { hasText: new RegExp(`^${label}`, 'i') });
    await item.waitFor({ timeout: 3000 });
    const val = (await item.locator('dd').innerText()).trim();
    assert.ok(val.length > 0, `expected non-empty ${label} timestamp`);
  }
  await statusRegion.locator('.meta-item', { hasText: /^Writes/i }).getByText(/^Refused$/i).waitFor({ timeout: 3000 });
  await statusRegion.locator('.meta-item', { hasText: /^Pull/i }).getByText(/^Allowed$/i).waitFor({ timeout: 3000 });
  await statusRegion.locator('.meta-item', { hasText: /^Export(?! until)/i }).getByText(/^Available$/i).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole('button', { name: /^Delete workspace$/i }).count(), 0);

  // Invitations are disabled, while member removal & leaving remain enabled during deletion_scheduled
  assert.equal(await membersSection.getByLabel(/Invite email/i).isDisabled(), true);
  assert.equal(await membersSection.getByRole('button', { name: /Send invite/i }).isDisabled(), true);
  assert.equal(await membersSection.getByRole('button', { name: /Remove peter@example\.com/i }).isDisabled(), false);
  assert.equal(await membersSection.getByRole('button', { name: /Leave workspace/i }).isDisabled(), false);

  const proposalsSection = page.getByRole('region', { name: /^Proposals$/i });
  assert.equal(await proposalsSection.getByRole('button', { name: /Create restore proposal/i }).isDisabled(), true);
  assert.equal(await proposalsSection.getByLabel(/Paths to restore/i).isDisabled(), true);
  const rejectBtns = proposalsSection.getByRole('button', { name: /Reject proposal/i });
  assert.ok((await rejectBtns.count()) >= 1);
  assert.equal(await rejectBtns.first().isDisabled(), true);

  const historySection = page.getByRole('region', { name: /^History$/i });
  assert.equal(await historySection.getByRole('button', { name: /Propose restore/i }).first().isDisabled(), true);

  // Export & read diff still work after 202
  const exportSection = page.locator('#export-section');
  const [ownerDownload] = await Promise.all([
    page.waitForEvent('download'),
    exportSection.getByRole('button', { name: /Download Git bundle/i }).click(),
  ]);
  const ownerBundleHead = fs.readFileSync(await ownerDownload.path(), 'utf8').slice(0, 32);
  assert.ok(ownerBundleHead.startsWith('# v2 git bundle'), `expected git bundle, got: ${ownerBundleHead}`);

  await historySection.getByRole('button', { name: /View diff/i }).first().click();
  await historySection.getByText('settings.toml', { exact: true }).waitFor({ timeout: 5000 });

  // 6. Status survives reload for owner, admin, and member; admin/member see no delete button and can still export & leave/remove
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
  assert.equal(await page.getByRole('button', { name: /^Delete workspace$/i }).count(), 0);
  assert.equal(await membersSection.getByRole('button', { name: /Send invite/i }).isDisabled(), true);
  assert.equal(await membersSection.getByRole('button', { name: /Remove peter@example\.com/i }).isDisabled(), false);

  await adminPage.reload({ waitUntil: 'networkidle' });
  await adminPage.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
  assert.equal(await adminPage.getByRole('button', { name: /delete workspace/i }).count(), 0);
  assert.equal(await adminPage.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Send invite/i }).isDisabled(), true);
  assert.equal(await adminPage.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Leave workspace/i }).isDisabled(), false);

  await memberPage.reload({ waitUntil: 'networkidle' });
  await memberPage.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
  assert.equal(await memberPage.getByRole('button', { name: /delete workspace/i }).count(), 0);
  assert.equal(await memberPage.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Leave workspace/i }).isDisabled(), false);
  const [memberDownload] = await Promise.all([
    memberPage.waitForEvent('download'),
    memberPage.locator('#export-section').getByRole('button', { name: /Download Git bundle/i }).click(),
  ]);
  const memberBundleHead = fs.readFileSync(await memberDownload.path(), 'utf8').slice(0, 32);
  assert.ok(memberBundleHead.startsWith('# v2 git bundle'));
  await saveScreenshot(page, '38-workspace-deletion-scheduled');

  // Admin can leave and Owner can remove a member during deletion_scheduled
  await adminPage.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Leave workspace/i }).click();
  await adminPage.waitForURL(`${baseUrl}/`, { timeout: 5000 });
  assert.equal(await adminPage.getByRole('link', { name: new RegExp(wsName, 'i') }).count(), 0);

  await page.reload({ waitUntil: 'networkidle' });
  const ownerMemberList = membersSection.locator('.member-list').first();
  await membersSection.getByRole('button', { name: /Remove peter@example\.com/i }).click();
  await ownerMemberList.getByText('peter@example.com', { exact: true }).waitFor({ state: 'detached', timeout: 5000 });
  assert.equal(await membersSection.locator('[role="alert"]').count(), 0);

  // 7. Billing write controls (including billing portal) disabled & real 409 workspace_deletion_scheduled shown in stale tab
  if (billingUrl) {
    const bCtx1 = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const bPage1 = await bCtx1.newPage();
    const bPage2 = await bCtx1.newPage();
    const bErrors = [];
    bPage1.on('pageerror', (err) => bErrors.push(err.message));
    bPage2.on('pageerror', (err) => bErrors.push(err.message));

    await bPage1.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
    await signInViaIdp(bPage1, { subject: 'olivia-del-bill-1', email: 'olivia@example.com' });
    await bPage1.getByLabel(/Workspace name/i).fill('Billing Deletion Fleet');
    await bPage1.getByRole('button', { name: /Create workspace/i }).click();
    await bPage1.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
    const bWsUrl = bPage1.url();

    await bPage2.goto(bWsUrl, { waitUntil: 'networkidle' });
    const b2Billing = bPage2.getByRole('region', { name: /^Billing$/i });
    await b2Billing.getByRole('button', { name: /Continue to checkout/i }).waitFor({ timeout: 5000 });

    const b1Delete = bPage1.getByRole('region', { name: /^Delete workspace$/i });
    await b1Delete.getByLabel(/workspace name/i).fill('Billing Deletion Fleet');
    await b1Delete.getByRole('button', { name: /^Delete workspace$/i }).click();
    await bPage1.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
    assert.equal(await bPage1.getByRole('region', { name: /^Billing$/i }).getByRole('button', { name: /Continue to checkout/i }).isDisabled(), true);

    // Stale tab submits checkout -> real 409 workspace_deletion_scheduled from server
    await b2Billing.getByRole('button', { name: /Continue to checkout/i }).click();
    await b2Billing.locator('[role="alert"]').getByText(/scheduled for deletion/i).waitFor({ timeout: 5000 });
    await bPage2.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });

    // Subscribed workspace: Open billing portal disabled on deletion_scheduled & stale tab gets real 409
    await bPage1.getByRole('link', { name: /All workspaces/i }).click();
    await bPage1.getByLabel(/Workspace name/i).fill('Billing Portal Deletion Fleet');
    await bPage1.getByRole('button', { name: /Create workspace/i }).click();
    await bPage1.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
    const bPortalWsUrl = bPage1.url();
    const b1PortalBilling = bPage1.getByRole('region', { name: /^Billing$/i });
    await b1PortalBilling.getByLabel(/^Plan$/i).selectOption('personal');
    await b1PortalBilling.getByRole('button', { name: /Continue to checkout/i }).click();
    const pLink = b1PortalBilling.getByRole('link', { name: /Open Paddle checkout/i });
    await pLink.waitFor({ timeout: 5000 });
    const pTxnId = (await pLink.getAttribute('href')).split('/').pop();
    await paddleControl('POST', '/complete-checkout', {
      transaction_id: pTxnId,
      subscription_id: 'sub_01browserportaldel000001',
      customer_id: 'ctm_01browserportaldel000001',
    });
    await b1PortalBilling.getByRole('button', { name: /Refresh billing status/i }).click();
    await b1PortalBilling.locator('dd', { hasText: /^Active$/i }).waitFor({ timeout: 5000 });

    const bPortalStale = await bCtx1.newPage();
    bPortalStale.on('pageerror', (err) => bErrors.push(err.message));
    await bPortalStale.goto(bPortalWsUrl, { waitUntil: 'networkidle' });
    const bStaleBilling = bPortalStale.getByRole('region', { name: /^Billing$/i });
    await bStaleBilling.getByRole('button', { name: /Open billing portal/i }).waitFor({ timeout: 5000 });
    assert.equal(await bStaleBilling.getByRole('button', { name: /Open billing portal/i }).isDisabled(), false);

    const b1PortalDel = bPage1.getByRole('region', { name: /^Delete workspace$/i });
    await b1PortalDel.getByLabel(/workspace name/i).fill('Billing Portal Deletion Fleet');
    await b1PortalDel.getByRole('button', { name: /^Delete workspace$/i }).click();
    await bPage1.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
    assert.equal(await b1PortalBilling.getByRole('button', { name: /Open billing portal/i }).isDisabled(), true);
    assert.equal(await b1PortalBilling.getByRole('button', { name: /Review plan change/i }).isDisabled(), true);
    assert.equal(await b1PortalBilling.getByRole('button', { name: /Cancel subscription/i }).isDisabled(), true);

    await bStaleBilling.getByRole('button', { name: /Open billing portal/i }).click();
    await bStaleBilling.locator('[role="alert"]').getByText(/scheduled for deletion/i).waitFor({ timeout: 5000 });
    await bPortalStale.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });
    assert.equal(await bStaleBilling.getByRole('button', { name: /Open billing portal/i }).isDisabled(), true);

    assert.deepEqual(bErrors, [], `unexpected billing deletion page errors: ${bErrors.join('; ')}`);
    await bCtx1.close();
  }

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  assert.deepEqual(adminErrors, [], `unexpected admin errors: ${adminErrors.join('; ')}`);
  assert.deepEqual(memberErrors, [], `unexpected member errors: ${memberErrors.join('; ')}`);
  await context.close();
  await adminCtx.close();
  await memberCtx.close();
}

async function journey27WorkspaceDeletionStaleResponseAndIsolation(browser) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-del-iso-1', email: 'olivia@example.com' });

  async function createWs(name) {
    await page.getByLabel(/Workspace name/i).fill(name);
    await page.getByRole('button', { name: /Create workspace/i }).click();
    await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
    const id = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];
    await page.getByRole('link', { name: /All workspaces/i }).click();
    await page.getByRole('link', { name: new RegExp(name, 'i') }).waitFor({ timeout: 5000 });
    return id;
  }

  const wsAId = await createWs('Del Iso A');
  const wsBId = await createWs('Del Iso B');
  const wsCId = await createWs('Del Iso C');
  const wsDId = await createWs('Del Iso D');

  // 1. Stale DELETE 202 for wsA arriving after switching to wsB must NOT taint wsB
  await page.getByRole('link', { name: /Del Iso A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  let releaseDelA = null;
  let resolveDelAHeld = null;
  const delAHeld = new Promise((r) => {
    resolveDelAHeld = r;
  });
  await page.route(`**/v1/workspaces/${wsAId}`, async (route) => {
    if (route.request().method() !== 'DELETE') {
      await route.continue();
      return;
    }
    const response = await route.fetch();
    assert.equal(response.status(), 202);
    await new Promise((resolve) => {
      releaseDelA = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveDelAHeld();
    });
  });

  const delSecA = page.getByRole('region', { name: /^Delete workspace$/i });
  await delSecA.getByLabel(/workspace name/i).fill('Del Iso A');
  await delSecA.getByRole('button', { name: /^Delete workspace$/i }).click();
  await delSecA.getByRole('button', { name: /Scheduling deletion/i }).waitFor({ timeout: 3000 });
  await delAHeld;
  assert.ok(releaseDelA, 'expected DELETE wsA to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso B/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsBId}$`), { timeout: 5000 });

  await releaseDelA();
  await page.waitForTimeout(100);
  await page.getByRole('region', { name: /^Delete workspace$/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.getByRole('region', { name: /Workspace deletion/i }).count(), 0);
  assert.equal(await page.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Send invite/i }).isDisabled(), false);
  await page.unroute(`**/v1/workspaces/${wsAId}`);

  // Confirm wsA actually transitioned on the server
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await page.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });

  // 2. Stale GET /lifecycle for wsA arriving after switching to wsD must NOT overwrite wsD
  let releaseLcA = null;
  let resolveLcAHeld = null;
  const lcAHeld = new Promise((r) => {
    resolveLcAHeld = r;
  });
  await page.route(`**/v1/workspaces/${wsAId}/lifecycle`, async (route) => {
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseLcA = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveLcAHeld();
    });
  });

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso A/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsAId}$`), { timeout: 5000 });
  await lcAHeld;
  assert.ok(releaseLcA, 'expected GET /lifecycle wsA to be held');

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso D/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsDId}$`), { timeout: 5000 });

  await releaseLcA();
  await page.waitForTimeout(100);
  assert.equal(await page.getByRole('region', { name: /Workspace deletion/i }).count(), 0);
  assert.equal(await page.getByRole('region', { name: /^Members$/i }).getByRole('button', { name: /Send invite/i }).isDisabled(), false);
  await page.unroute(`**/v1/workspaces/${wsAId}/lifecycle`);

  // 3. Stale DELETE across Sign out must not leak alerts or state into signed-out or next session
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso C/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsCId}$`), { timeout: 5000 });

  let releaseDelC = null;
  let resolveDelCHeld = null;
  const delCHeld = new Promise((r) => {
    resolveDelCHeld = r;
  });
  await page.route(`**/v1/workspaces/${wsCId}`, async (route) => {
    if (route.request().method() !== 'DELETE') {
      await route.continue();
      return;
    }
    const response = await route.fetch();
    await new Promise((resolve) => {
      releaseDelC = async () => {
        await route.fulfill({ response });
        resolve();
      };
      resolveDelCHeld();
    });
  });

  const delSecC = page.getByRole('region', { name: /^Delete workspace$/i });
  await delSecC.getByLabel(/workspace name/i).fill('Del Iso C');
  await delSecC.getByRole('button', { name: /^Delete workspace$/i }).click();
  await delSecC.getByRole('button', { name: /Scheduling deletion/i }).waitFor({ timeout: 3000 });
  await delCHeld;

  await page.getByRole('button', { name: /Sign out/i }).click();
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 5000 });

  await releaseDelC();
  await page.waitForTimeout(100);
  await page.getByRole('link', { name: /Sign in/i }).waitFor({ timeout: 3000 });
  assert.equal(await page.locator('[role="alert"]').count(), 0);
  assert.equal(await page.getByRole('region', { name: /Workspace deletion/i }).count(), 0);
  await page.unroute(`**/v1/workspaces/${wsCId}`);

  // Sign back in -> form state reset cleanly on active wsD, wsC shows server truth
  await signInViaIdp(page, { subject: 'olivia-del-iso-1', email: 'olivia@example.com' });
  await page.getByRole('link', { name: /Del Iso D/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsDId}$`), { timeout: 5000 });
  assert.equal(await page.getByRole('region', { name: /^Delete workspace$/i }).getByLabel(/workspace name/i).inputValue(), '');

  // wsC's DELETE reached the server before sign-out, so it must show the scheduled deletion
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Del Iso C/i }).click();
  await page.waitForURL(new RegExp(`/workspaces/${wsCId}$`), { timeout: 5000 });
  await page.getByRole('region', { name: /Workspace deletion/i }).getByText(/^Scheduled$/i).waitFor({ timeout: 5000 });

  await saveScreenshot(page, '39-workspace-deletion-isolation');
  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey28BillingConfirmationWaitsForRefreshedState(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-pending-confirm-1', email: 'olivia@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Pending Confirm Billing Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.getByLabel(/^Plan$/i).selectOption('team');
  await billingSection.getByLabel(/Seats/i).fill('2');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();

  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  const txnId = (await paddleLink.getAttribute('href')).split('/').pop();

  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: 'sub_01browserpending00000001',
    customer_id: 'ctm_01browserpending00000001',
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(2 seats\)$/i }).waitFor({ timeout: 5000 });

  // 1. Plan change where refreshed GET /billing initially returns stale Team (2 seats)
  const prePlanBilling = await page.evaluate(async (id) => {
    const r = await fetch(`/v1/workspaces/${id}/billing`, { credentials: 'same-origin' });
    return r.json();
  }, wsId);
  assert.equal(prePlanBilling.plan, 'team');
  assert.equal(prePlanBilling.seats, 2);

  let returnStalePlanGet = true;
  const billingGetRoute = `**/v1/workspaces/${wsId}/billing`;
  const stalePlanHandler = async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    if (
      returnStalePlanGet &&
      req.method() === 'GET' &&
      url.pathname === `/v1/workspaces/${wsId}/billing`
    ) {
      returnStalePlanGet = false;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(prePlanBilling),
      });
      return;
    }
    await route.continue();
  };
  await page.route(billingGetRoute, stalePlanHandler);

  await billingSection.getByLabel(/Update seats/i).fill('4');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing/plan` &&
        r.status() === 200
    ),
    page.waitForResponse(
      (r) =>
        r.request().method() === 'GET' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing` &&
        r.status() === 200
    ),
    billingSection.getByRole('button', { name: /Confirm plan change/i }).click(),
  ]);

  const statusBox = billingSection.locator('[role="status"]').first();
  await billingSection
    .locator('[role="status"]', { hasText: /Plan change submitted, waiting for Paddle confirmation/i })
    .waitFor({ timeout: 5000 });
  const pendingPlanText = await statusBox.innerText();
  assert.match(pendingPlanText, /submitted, waiting for Paddle confirmation/i);
  assert.equal(
    /Subscription updated cleanly/i.test(pendingPlanText),
    false,
    'must not show plan success banner while refreshed state still shows 2 seats'
  );
  await page.unroute(billingGetRoute, stalePlanHandler);

  // Next refresh returns real Team (4 seats) -> resolves to success banner
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(4 seats\)$/i }).waitFor({ timeout: 5000 });
  await billingSection
    .locator('[role="status"]', { hasText: /Subscription updated cleanly with Paddle/i })
    .waitFor({ timeout: 5000 });

  // 2. Cancel where refreshed GET /billing initially returns stale cancel_at_period_end=false
  const preCancelBilling = await page.evaluate(async (id) => {
    const r = await fetch(`/v1/workspaces/${id}/billing`, { credentials: 'same-origin' });
    return r.json();
  }, wsId);
  assert.equal(preCancelBilling.cancel_at_period_end, false);

  let returnStaleCancelGet = true;
  const staleCancelHandler = async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    if (
      returnStaleCancelGet &&
      req.method() === 'GET' &&
      url.pathname === `/v1/workspaces/${wsId}/billing`
    ) {
      returnStaleCancelGet = false;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(preCancelBilling),
      });
      return;
    }
    await route.continue();
  };
  await page.route(billingGetRoute, staleCancelHandler);

  await billingSection.getByRole('button', { name: /Cancel subscription/i }).click();
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing/cancel` &&
        r.status() === 200
    ),
    page.waitForResponse(
      (r) =>
        r.request().method() === 'GET' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing` &&
        r.status() === 200
    ),
    billingSection.getByRole('button', { name: /Confirm cancellation/i }).click(),
  ]);

  await billingSection
    .locator('[role="status"]', { hasText: /Cancellation submitted, waiting for Paddle confirmation/i })
    .waitFor({ timeout: 5000 });
  const pendingCancelText = await statusBox.innerText();
  assert.match(pendingCancelText, /submitted, waiting for Paddle confirmation/i);
  assert.equal(
    /Subscription cancellation scheduled/i.test(pendingCancelText),
    false,
    'must not show cancellation success banner while refreshed state has cancel_at_period_end=false'
  );
  await page.unroute(billingGetRoute, staleCancelHandler);

  // Next refresh returns real cancel_at_period_end=true -> resolves to cancellation success banner
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.getByText(/Cancels at period end/i).waitFor({ timeout: 5000 });
  await billingSection
    .locator('[role="status"]', {
      hasText: /Subscription cancellation scheduled for the end of the current billing period/i,
    })
    .waitFor({ timeout: 5000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

// After a plan change or cancellation the console keeps asking the server
// (bounded backoff) until the change shows, with no manual Refresh; if it
// never shows it says so honestly and stops asking, and leaving the
// workspace stops it too. Time is driven by Playwright's clock.
async function journey30BillingConfirmationPollsUntilConfirmedOrHonestlyGivesUp(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  await context.clock.install();
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'pia-poll-confirm-1', email: 'pia@example.com' });

  await page.getByLabel(/Workspace name/i).fill('Poll Confirm Billing Fleet');
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];
  const billingPath = `/v1/workspaces/${wsId}/billing`;

  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.getByLabel(/^Plan$/i).selectOption('team');
  await billingSection.getByLabel(/Seats/i).fill('2');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();
  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  const txnId = (await paddleLink.getAttribute('href')).split('/').pop();
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: 'sub_01browserpoll0000000001',
    customer_id: 'ctm_01browserpoll0000000001',
  });
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(2 seats\)$/i }).waitFor({ timeout: 5000 });

  let billingGets = 0;
  page.on('request', (req) => {
    if (req.method() === 'GET' && new URL(req.url()).pathname === billingPath) billingGets += 1;
  });
  const currentBilling = () =>
    page.evaluate(async (path) => {
      const r = await fetch(path, { credentials: 'same-origin' });
      return r.json();
    }, billingPath);
  // Answers the console's GET /billing with `stale` while `remaining()` > 0.
  const staleRoute = (stale, remaining) => async (route) => {
    const req = route.request();
    if (req.method() === 'GET' && new URL(req.url()).pathname === billingPath && remaining()) {
      await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(stale) });
      return;
    }
    await route.continue();
  };
  // Lets fake time pass in rounds (each fires at most one poll, whose
  // request then completes in real time) until `locator` shows.
  const advanceUntil = async (locator, rounds, what) => {
    for (let i = 0; i < rounds; i += 1) {
      await page.clock.runFor(30_000);
      try {
        await locator.waitFor({ timeout: 1500 });
        return;
      } catch (_err) {
        // not yet: next round
      }
    }
    assert.fail(`${what} did not show after ${rounds} rounds of 30 s`);
  };
  const statusWith = (re) => billingSection.locator('[role="status"]', { hasText: re });
  const confirmPlanChange = async (seats) => {
    await billingSection.getByLabel(/Update seats/i).fill(String(seats));
    await billingSection.getByRole('button', { name: /Review plan change/i }).click();
    await Promise.all([
      page.waitForResponse(
        (r) =>
          r.request().method() === 'POST' &&
          new URL(r.url()).pathname === `${billingPath}/plan` &&
          r.status() === 200
      ),
      billingSection.getByRole('button', { name: /Confirm plan change/i }).click(),
    ]);
    await statusWith(/Plan change submitted, waiting for Paddle confirmation/i).waitFor({
      timeout: 5000,
    });
  };

  // 1. The server's re-read lands after two stale reads: the console finds
  //    it on its own, without a manual Refresh.
  const twoSeats = await currentBilling();
  let staleLeft = 2;
  const routeTwoStale = staleRoute(twoSeats, () => (staleLeft > 0 ? staleLeft-- : 0));
  await page.route(`**${billingPath}`, routeTwoStale);
  await confirmPlanChange(4);
  await advanceUntil(
    statusWith(/Subscription updated cleanly with Paddle/i),
    8,
    'plan change confirmation'
  );
  await billingSection.locator('dd', { hasText: /^Team \(4 seats\)$/i }).waitFor({ timeout: 5000 });
  await page.unroute(`**${billingPath}`, routeTwoStale);

  // 2. Leaving the workspace while still unconfirmed stops the polling.
  const fourSeats = await currentBilling();
  const routeAlwaysStalePlan = staleRoute(fourSeats, () => true);
  await page.route(`**${billingPath}`, routeAlwaysStalePlan);
  await confirmPlanChange(5);
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByRole('link', { name: /Poll Confirm Billing Fleet/i }).waitFor({ timeout: 5000 });
  const getsAfterLeaving = billingGets;
  for (let i = 0; i < 6; i += 1) {
    await page.clock.runFor(30_000);
    await page.waitForTimeout(200);
  }
  assert.equal(billingGets, getsAfterLeaving, 'billing was polled after leaving the workspace');
  await page.unroute(`**${billingPath}`, routeAlwaysStalePlan);
  await page.getByRole('link', { name: /Poll Confirm Billing Fleet/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(5 seats\)$/i }).waitFor({ timeout: 5000 });

  // 3. Paddle never confirms the cancellation while the console waits: an
  //    honest notice replaces the waiting one and the polling stops.
  const notCanceled = await currentBilling();
  assert.equal(notCanceled.cancel_at_period_end, false);
  const routeAlwaysStaleCancel = staleRoute(notCanceled, () => true);
  await page.route(`**${billingPath}`, routeAlwaysStaleCancel);
  await billingSection.getByRole('button', { name: /Cancel subscription/i }).click();
  const getsBeforeCancel = billingGets;
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        new URL(r.url()).pathname === `${billingPath}/cancel` &&
        r.status() === 200
    ),
    billingSection.getByRole('button', { name: /Confirm cancellation/i }).click(),
  ]);
  await statusWith(/Cancellation submitted, waiting for Paddle confirmation/i).waitFor({
    timeout: 5000,
  });
  const honest = statusWith(/Paddle has not confirmed the cancellation yet/i);
  await advanceUntil(honest, 10, 'honest unconfirmed notice');
  assert.match(await honest.innerText(), /will appear here once Paddle confirms/i);
  assert.equal(
    await statusWith(/waiting for Paddle confirmation/i).count(),
    0,
    'the waiting notice must be replaced'
  );
  const pollsForCancel = billingGets - getsBeforeCancel;
  assert.ok(pollsForCancel <= 8, `polling was not bounded: ${pollsForCancel} reads`);
  const getsAfterGivingUp = billingGets;
  for (let i = 0; i < 6; i += 1) {
    await page.clock.runFor(60_000);
    await page.waitForTimeout(200);
  }
  assert.equal(billingGets, getsAfterGivingUp, 'billing was still polled after giving up');
  await page.unroute(`**${billingPath}`, routeAlwaysStaleCancel);

  // A later manual refresh still resolves it once Paddle's state shows.
  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.getByText(/Cancels at period end/i).waitFor({ timeout: 5000 });
  await statusWith(
    /Subscription cancellation scheduled for the end of the current billing period/i
  ).waitFor({ timeout: 5000 });

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey29BillingSeatLimitsAndWorkspacePlanStatus(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  const wsName = 'Seat and Status Fleet';
  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'olivia-seats-status-1', email: 'olivia@example.com' });

  await page.getByLabel(/Workspace name/i).fill(wsName);
  await page.getByRole('button', { name: /Create workspace/i }).click();
  await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
  const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];

  // 1. Trialing workspace: detail header & list card both show Trialing and Trial ends
  const headerCard = page.locator('main section.card').first();
  await headerCard.locator('.meta-item', { hasText: /^Plan status/i }).getByText(/Trialing/i).waitFor({ timeout: 5000 });
  await headerCard.locator('.meta-item', { hasText: /^Trial ends/i }).waitFor({ timeout: 3000 });

  await page.getByRole('link', { name: /All workspaces/i }).click();
  const listCard = page.locator('.workspace-card', { hasText: wsName });
  await listCard.getByText(/Trialing/i).waitFor({ timeout: 5000 });
  await listCard.getByText(/Trial ends/i).waitFor({ timeout: 3000 });

  await listCard.click();
  await page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  // 2. Checkout seats input enforces 1..1000 client-side and surfaces server details.field="seats"
  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  await billingSection.waitFor({ timeout: 5000 });
  await billingSection.getByLabel(/^Plan$/i).selectOption('team');

  const checkoutSeatsInput = billingSection.locator('#billing-checkout-seats');
  assert.equal(await checkoutSeatsInput.getAttribute('min'), '1');
  assert.equal(await checkoutSeatsInput.getAttribute('max'), '1000');

  const checkoutCallsBefore = (await paddleControl('GET', '/calls/create_transaction')).count;
  await checkoutSeatsInput.fill('1001');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();
  await billingSection.locator('#billing-checkout-seats-err').waitFor({ timeout: 3000 });
  assert.equal(await checkoutSeatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal((await paddleControl('GET', '/calls/create_transaction')).count, checkoutCallsBefore);

  // Bypass client check via outbound body rewrite to verify real server 400 details.field="seats" handling
  const checkoutRoute = `**/v1/workspaces/${wsId}/billing/checkout`;
  let rewriteCheckoutSeats = true;
  const checkoutRewriteHandler = async (route) => {
    const req = route.request();
    if (rewriteCheckoutSeats && req.method() === 'POST') {
      rewriteCheckoutSeats = false;
      const body = req.postDataJSON();
      await route.continue({ postData: JSON.stringify({ ...body, seats: 1001 }) });
      return;
    }
    await route.continue();
  };
  await page.route(checkoutRoute, checkoutRewriteHandler);
  await checkoutSeatsInput.fill('2');
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing/checkout` &&
        r.status() === 400
    ),
    billingSection.getByRole('button', { name: /Continue to checkout/i }).click(),
  ]);
  await page.unroute(checkoutRoute, checkoutRewriteHandler);
  const checkoutServerErr = billingSection.locator('#billing-checkout-seats-err');
  await checkoutServerErr.waitFor({ timeout: 3000 });
  assert.match(await checkoutServerErr.innerText(), /1000/);
  assert.equal(await checkoutSeatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal((await paddleControl('GET', '/calls/create_transaction')).count, checkoutCallsBefore);

  // Boundary 1000 seats succeeds cleanly
  await checkoutSeatsInput.fill('1000');
  await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();
  const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
  await paddleLink.waitFor({ timeout: 5000 });
  await billingSection.getByText(/Team \(1000 seats\)/i).waitFor({ timeout: 3000 });
  const txnId = (await paddleLink.getAttribute('href')).split('/').pop();
  const subId = 'sub_01browsermaxseats0000001';
  await paddleControl('POST', '/complete-checkout', {
    transaction_id: txnId,
    subscription_id: subId,
    customer_id: 'ctm_01browsermaxseats0000001',
  });

  await billingSection.getByRole('button', { name: /Refresh billing status/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(1000 seats\)$/i }).waitFor({ timeout: 5000 });

  // 3. Active workspace detail header & list card show Active and NEVER show "Trial ends"
  await headerCard.locator('.meta-item', { hasText: /^Plan status/i }).getByText(/^Active$/i).waitFor({ timeout: 5000 });
  assert.equal(await headerCard.getByText(/Trial ends/i).count(), 0, 'Active detail header must not show Trial ends');

  await page.reload({ waitUntil: 'networkidle' });
  await headerCard.locator('.meta-item', { hasText: /^Plan status/i }).getByText(/^Active$/i).waitFor({ timeout: 5000 });
  assert.equal(await headerCard.getByText(/Trial ends/i).count(), 0);

  await page.getByRole('link', { name: /All workspaces/i }).click();
  await listCard.getByText(/^Active$/i).waitFor({ timeout: 5000 });
  assert.equal(await listCard.getByText(/Trial ends/i).count(), 0, 'Active list card must not show Trial ends');
  await listCard.click();
  await page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });

  // 4. Update-plan seats input enforces 1..1000 client-side and surfaces server details.field="seats"
  const updateBefore = (await paddleControl('GET', '/calls/update')).count;
  const updateSeatsInput = billingSection.locator('#billing-update-seats');
  await updateSeatsInput.waitFor({ timeout: 5000 });
  assert.equal(await updateSeatsInput.getAttribute('min'), '1');
  assert.equal(await updateSeatsInput.getAttribute('max'), '1000');

  await updateSeatsInput.fill('1001');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.locator('#billing-update-seats-err').waitFor({ timeout: 3000 });
  assert.equal(await updateSeatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal(await billingSection.getByRole('button', { name: /Confirm plan change/i }).count(), 0);
  assert.equal((await paddleControl('GET', '/calls/update')).count, updateBefore);

  // Bypass client check on confirm to verify real server 400 details.field="seats" on /billing/plan
  const planRoute = `**/v1/workspaces/${wsId}/billing/plan`;
  let rewritePlanSeats = true;
  const planRewriteHandler = async (route) => {
    const req = route.request();
    if (rewritePlanSeats && req.method() === 'POST') {
      rewritePlanSeats = false;
      const body = req.postDataJSON();
      await route.continue({ postData: JSON.stringify({ ...body, seats: 1001 }) });
      return;
    }
    await route.continue();
  };
  await page.route(planRoute, planRewriteHandler);
  await updateSeatsInput.fill('2');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await Promise.all([
    page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        new URL(r.url()).pathname === `/v1/workspaces/${wsId}/billing/plan` &&
        r.status() === 400
    ),
    billingSection.getByRole('button', { name: /Confirm plan change/i }).click(),
  ]);
  await page.unroute(planRoute, planRewriteHandler);
  const updateServerErr = billingSection.locator('#billing-update-seats-err');
  await updateServerErr.waitFor({ timeout: 3000 });
  assert.match(await updateServerErr.innerText(), /1000/);
  assert.equal(await updateSeatsInput.getAttribute('aria-invalid'), 'true');
  assert.equal((await paddleControl('GET', '/calls/update')).count, updateBefore);

  // Valid update to 3 seats succeeds cleanly
  await updateSeatsInput.fill('3');
  await billingSection.getByRole('button', { name: /Review plan change/i }).click();
  await billingSection.getByRole('button', { name: /Confirm plan change/i }).click();
  await billingSection.locator('dd', { hasText: /^Team \(3 seats\)$/i }).waitFor({ timeout: 5000 });

  // 5. Real subscription statuses (past_due, paused->restricted, canceled) render in both detail header and list without Trial ends
  const transitions = [
    { event_type: 'subscription.past_due', status: 'past_due', label: /^Past due$/i },
    { event_type: 'subscription.paused', status: 'paused', label: /^Restricted$/i },
    { event_type: 'subscription.canceled', status: 'canceled', label: /^Canceled$/i },
  ];
  for (const t of transitions) {
    await paddleControl('POST', '/update-subscription', {
      subscription_id: subId,
      event_type: t.event_type,
      status: t.status,
      ...(t.status === 'canceled' ? { cancel_at_period_end: false } : {}),
    });
    await page.reload({ waitUntil: 'networkidle' });
    await headerCard.locator('.meta-item', { hasText: /^Plan status/i }).getByText(t.label).waitFor({ timeout: 5000 });
    await headerCard.locator('.badge-row').getByText(t.label).waitFor({ timeout: 3000 });
    assert.equal(await headerCard.getByText(/Trial ends/i).count(), 0, `${t.status} detail header must not show Trial ends`);

    await page.getByRole('link', { name: /All workspaces/i }).click();
    await listCard.getByText(t.label).waitFor({ timeout: 5000 });
    assert.equal(await listCard.getByText(/Trial ends/i).count(), 0, `${t.status} list card must not show Trial ends`);

    await listCard.click();
    await page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });
  }

  // 6. trial_expired displays Trial expired and DOES show Trial ends in both list and detail header
  const wsListRoute = '**/v1/workspaces';
  const wsDetailRoute = `**/v1/workspaces/${wsId}`;
  const wsBillingRoute = `**/v1/workspaces/${wsId}/billing`;
  await page.route(wsListRoute, async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue();
      return;
    }
    const resp = await route.fetch();
    const list = await resp.json();
    await route.fulfill({
      response: resp,
      body: JSON.stringify(list.map((w) => (w.id === wsId ? { ...w, plan_status: 'trial_expired' } : w))),
    });
  });
  await page.route(wsDetailRoute, async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue();
      return;
    }
    const resp = await route.fetch();
    const ws = await resp.json();
    await route.fulfill({
      response: resp,
      body: JSON.stringify({ ...ws, plan_status: 'trial_expired' }),
    });
  });
  await page.route(wsBillingRoute, async (route) => {
    if (route.request().method() !== 'GET') {
      await route.continue();
      return;
    }
    const resp = await route.fetch();
    const b = await resp.json();
    await route.fulfill({
      response: resp,
      body: JSON.stringify({
        ...b,
        plan: 'trial',
        plan_status: 'trial_expired',
        trial_ends_at: '2026-01-01T00:00:00Z',
        current_period_end: null,
        can_push: false,
      }),
    });
  });

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await listCard.getByText(/^Trial expired$/i).waitFor({ timeout: 5000 });
  await listCard.getByText(/Trial ends/i).waitFor({ timeout: 3000 });
  await listCard.click();
  await page.waitForURL(new RegExp(`/workspaces/${wsId}$`), { timeout: 5000 });
  await headerCard.locator('.meta-item', { hasText: /^Plan status/i }).getByText(/^Trial expired$/i).waitFor({ timeout: 5000 });
  await headerCard.locator('.meta-item', { hasText: /^Trial ends/i }).waitFor({ timeout: 3000 });
  await page.unroute(wsListRoute);
  await page.unroute(wsDetailRoute);
  await page.unroute(wsBillingRoute);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

async function journey31BillingCheckoutAlreadyPaidPollsUntilSubscriptionAppears(browser) {
  assert.ok(billingUrl, 'MUSTER_BILLING_CLOUD_URL is required');
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  await context.clock.install();
  const page = await context.newPage();
  const pageErrors = [];
  page.on('pageerror', (err) => pageErrors.push(err.message));

  await page.goto(`${billingUrl}/`, { waitUntil: 'networkidle' });
  await signInViaIdp(page, { subject: 'quinn-paid-checkout-1', email: 'quinn@example.com' });

  let billingPath = '';
  let billingGets = 0;
  page.on('request', (req) => {
    if (req.method() === 'GET' && new URL(req.url()).pathname === billingPath) billingGets += 1;
  });
  const billingSection = page.getByRole('region', { name: /^Billing$/i });
  const statusWith = (re) => billingSection.locator('[role="status"]', { hasText: re });
  // Answers the console's GET /billing with the trial state while `remaining()` > 0.
  const trialRoute = (trial, remaining) => async (route) => {
    const req = route.request();
    if (req.method() === 'GET' && new URL(req.url()).pathname === billingPath && remaining()) {
      await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(trial) });
      return;
    }
    await route.continue();
  };
  // Lets fake time pass in rounds (each fires at most one poll, whose
  // request then completes in real time) until `locator` shows.
  const advanceUntil = async (locator, rounds, what) => {
    for (let i = 0; i < rounds; i += 1) {
      await page.clock.runFor(30_000);
      try {
        await locator.waitFor({ timeout: 1500 });
        return;
      } catch (_err) {
        // not yet: next round
      }
    }
    assert.fail(`${what} did not show after ${rounds} rounds of 30 s`);
  };
  // A workspace whose open checkout the customer pays at Paddle while this
  // console still offers it. Returns the billing the console last saw (trial).
  const paidCheckoutWorkspace = async (name, n) => {
    await page.getByLabel(/Workspace name/i).fill(name);
    await page.getByRole('button', { name: /Create workspace/i }).click();
    await page.waitForURL(/\/workspaces\/([0-9a-f-]{36})$/, { timeout: 5000 });
    const wsId = page.url().match(/\/workspaces\/([0-9a-f-]{36})$/)[1];
    billingPath = `/v1/workspaces/${wsId}/billing`;
    await billingSection.waitFor({ timeout: 5000 });
    await billingSection.getByRole('button', { name: /Continue to checkout/i }).click();
    const paddleLink = billingSection.getByRole('link', { name: /Open Paddle checkout/i });
    await paddleLink.waitFor({ timeout: 5000 });
    const txnId = (await paddleLink.getAttribute('href')).split('/').pop();
    const trial = await page.evaluate(async (path) => {
      const r = await fetch(path, { credentials: 'same-origin' });
      return r.json();
    }, billingPath);
    assert.equal(trial.plan, 'trial');
    await paddleControl('POST', '/complete-checkout', {
      transaction_id: txnId,
      subscription_id: `sub_01browserpaid${n}000000000`,
      customer_id: `ctm_01browserpaid${n}000000000`,
    });
    return trial;
  };
  // "Continue to checkout" answered 409 subscription_exists, as the server
  // does for a checkout that was paid meanwhile.
  const continueToCheckout409 = async () => {
    const route409 = async (route) => {
      if (route.request().method() !== 'POST') {
        await route.continue();
        return;
      }
      await route.fulfill({
        status: 409,
        contentType: 'application/json',
        body: JSON.stringify({
          error: {
            code: 'subscription_exists',
            message: 'a checkout of this workspace was paid; its subscription is being set up',
          },
        }),
      });
    };
    await page.route(`**${billingPath}/checkout`, route409);
    await Promise.all([
      page.waitForResponse(
        (r) =>
          r.request().method() === 'POST' &&
          new URL(r.url()).pathname === `${billingPath}/checkout` &&
          r.status() === 409
      ),
      billingSection.getByRole('button', { name: /Continue to checkout/i }).click(),
    ]);
    await page.unroute(`**${billingPath}/checkout`, route409);
  };

  // 1. 409 subscription_exists: the console reloads billing on its own, keeps
  //    polling while the subscription is not there yet, shows it once it
  //    appears and then stops, with no manual Refresh.
  const trial = await paidCheckoutWorkspace('Paid Checkout Poll Fleet', 1);
  let trialLeft = 3;
  const routeTrial = trialRoute(trial, () => (trialLeft > 0 ? trialLeft-- : 0));
  await page.route(`**${billingPath}`, routeTrial);
  const getsBefore = billingGets;
  await continueToCheckout409();
  await billingSection.locator('[role="alert"]', { hasText: /subscription is being set up/i }).waitFor({
    timeout: 5000,
  });
  // The reload is immediate: no clock time has passed.
  const reloaded = Date.now();
  while (billingGets === getsBefore) {
    assert.ok(Date.now() - reloaded < 5000, 'billing was not reloaded right after the 409');
    await page.waitForTimeout(50);
  }
  await statusWith(/waiting for the subscription to be set up/i).waitFor({ timeout: 5000 });
  await advanceUntil(billingSection.locator('dd', { hasText: /^Personal$/i }), 8, 'the subscription');
  await billingSection.locator('dd', { hasText: /^Active$/i }).waitFor({ timeout: 3000 });
  assert.ok(billingGets - getsBefore >= 3, `billing was not polled: ${billingGets - getsBefore} reads`);
  assert.equal(
    await billingSection.locator('[role="alert"]', { hasText: /subscription is being set up/i }).count(),
    0,
    'the stale 409 error must go once the subscription shows'
  );
  await page.unroute(`**${billingPath}`, routeTrial);
  const getsAfterFound = billingGets;
  for (let i = 0; i < 6; i += 1) {
    await page.clock.runFor(60_000);
    await page.waitForTimeout(200);
  }
  assert.equal(billingGets, getsAfterFound, 'billing was still polled after the subscription showed');

  // 2. Leaving the workspace while the subscription is still missing stops it.
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByLabel(/Workspace name/i).waitFor({ timeout: 5000 });
  const trial2 = await paidCheckoutWorkspace('Paid Checkout Leave Fleet', 2);
  const routeAlwaysTrial = trialRoute(trial2, () => true);
  await page.route(`**${billingPath}`, routeAlwaysTrial);
  await continueToCheckout409();
  await statusWith(/waiting for the subscription to be set up/i).waitFor({ timeout: 5000 });
  await page.getByRole('link', { name: /All workspaces/i }).click();
  await page.getByLabel(/Workspace name/i).waitFor({ timeout: 5000 });
  const getsAfterLeaving = billingGets;
  for (let i = 0; i < 6; i += 1) {
    await page.clock.runFor(30_000);
    await page.waitForTimeout(200);
  }
  assert.equal(billingGets, getsAfterLeaving, 'billing was polled after leaving the workspace');
  await page.unroute(`**${billingPath}`, routeAlwaysTrial);

  assert.deepEqual(pageErrors, [], `unexpected page errors: ${pageErrors.join('; ')}`);
  await context.close();
}

const journeys = [
  ['journey_1_signin_and_account', journey1SignInAndAccount],
  ['journey_2_empty_workspaces', journey2EmptyWorkspaces],
  ['journey_3_create_workspace_validation_and_idempotency', journey3CreateWorkspaceValidationAndIdempotency],
  ['journey_4_detail_reload_and_logout', journey4DetailReloadAndLogout],
  ['journey_5_accessibility_theme_responsive_and_security', journey5AccessibilityThemeResponsiveAndSecurity],
  ['journey_6_device_approve_with_login_redirect', journey6DeviceApproveWithLoginRedirect],
  ['journey_7_device_deny_and_error_handling', journey7DeviceDenyAndErrorHandling],
  ['journey_8_workspace_devices_list', journey8WorkspaceDevicesList],
  ['journey_9_team_members_visible', journey9TeamMembersVisible],
  ['journey_10_invite_member_and_role_limits', journey10InviteMemberAndRoleLimits],
  ['journey_11_accept_invitation_explicit_and_mismatch', journey11AcceptInvitationExplicitAndMismatch],
  ['journey_12_remove_member_and_last_owner_guard', journey12RemoveMemberAndLastOwnerGuard],
  ['journey_13_history_and_redacted_diff', journey13HistoryAndRedactedDiff],
  ['journey_14_proposals_and_conflicts_visible', journey14ProposalsAndConflictsVisible],
  ['journey_15_request_restore_proposal', journey15RequestRestoreProposal],
  ['journey_16_export_bundle_and_oss_instructions', journey16ExportBundleAndOssInstructions],
  ['journey_17_billing_unconfigured_honest_state_and_limits', journey17BillingUnconfiguredHonestStateAndLimits],
  ['journey_18_billing_checkout_no_duplicates_and_server_reconcile', journey18BillingCheckoutNoDuplicatesAndServerReconcile],
  ['journey_19_billing_team_portal_plan_change_and_explicit_cancel', journey19BillingTeamPortalPlanChangeAndExplicitCancel],
  ['journey_20_billing_provider_failure_retry_roles_and_mobile', journey20BillingProviderFailureRetryRolesAndMobile],
  ['journey_21_billing_navigation_isolation_and_stale_response_guard', journey21BillingNavigationIsolationAndStaleResponseGuard],
  ['journey_22_billing_machines_limit_excludes_revoked_devices', journey22BillingMachinesLimitExcludesRevokedDevices],
  ['journey_23_workspace_async_isolation_members_history_proposals_invites', journey23WorkspaceAsyncIsolationMembersHistoryProposalsInvites],
  ['journey_24_session_account_ownership_and_reset', journey24SessionAccountOwnershipAndReset],
  ['journey_25_history_pagination_and_isolation', journey25HistoryPaginationAndIsolation],
  ['journey_26_workspace_deletion_lifecycle_roles_and_idempotency', journey26WorkspaceDeletionLifecycleRolesAndIdempotency],
  ['journey_27_workspace_deletion_stale_response_and_isolation', journey27WorkspaceDeletionStaleResponseAndIsolation],
  ['journey_28_billing_confirmation_waits_for_refreshed_state', journey28BillingConfirmationWaitsForRefreshedState],
  ['journey_29_billing_seat_limits_and_workspace_plan_status', journey29BillingSeatLimitsAndWorkspacePlanStatus],
  [
    'journey_30_billing_confirmation_polls_until_confirmed_or_honestly_gives_up',
    journey30BillingConfirmationPollsUntilConfirmedOrHonestlyGivesUp,
  ],
  [
    'journey_31_billing_checkout_already_paid_polls_until_subscription_appears',
    journey31BillingCheckoutAlreadyPaidPollsUntilSubscriptionAppears,
  ],
];

async function main() {
  const filter = process.env.MUSTER_BROWSER_FILTER || '';
  const selected = filter ? journeys.filter(([name]) => name.includes(filter)) : journeys;
  assert.ok(selected.length > 0, `no journeys matched filter: ${filter}`);

  const browser = await launchBrowser();
  try {
    for (const [name, fn] of selected) {
      const start = Date.now();
      await fn(browser);
      console.log(`PASS ${name} (${Date.now() - start}ms)`);
    }
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
