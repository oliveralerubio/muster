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
  await page.reload({ waitUntil: "networkidle" });
  const conflictReloaded = page
    .getByRole("region", { name: /^Proposals$/i })
    .locator("article", { hasText: fixtures.conflictRevision.slice(0, 8) });
  await conflictReloaded.getByText(/^Rejected$/i).waitFor({ timeout: 5000 });
  assert.equal(await conflictReloaded.getByRole("button", { name: /Reject proposal/i }).count(), 0);

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
  assert.equal(await emptyExport.getByRole("link", { name: /Download Git bundle/i }).count(), 0);

  // 2. Populated workspace exposes full downloadable bundle + honest OSS recovery commands
  await page.getByRole("link", { name: /All workspaces/i }).click();
  await page.getByRole("link", { name: new RegExp(fixtures.historyWorkspaceName, "i") }).click();
  await page.waitForURL(new RegExp(`/workspaces/${fixtures.historyWorkspaceId}$`), { timeout: 5000 });

  const exportSection = page.getByRole("region", { name: /Export & offline recovery/i });
  await exportSection.waitFor({ timeout: 5000 });

  const revHead = fixtures.revisions[0];
  await exportSection.getByText(revHead.signer, { exact: true }).waitFor({ timeout: 5000 });
  await exportSection.getByText(/git clone -b main workspace\.bundle/i).waitFor({ timeout: 3000 });
  await exportSection.getByText(new RegExp(`--expect-signer ${revHead.signer}`)).waitFor({ timeout: 3000 });
  await exportSection.getByText(/no Cloud subscription/i).waitFor({ timeout: 3000 });

  const [download] = await Promise.all([
    page.waitForEvent("download"),
    exportSection.getByRole("link", { name: /Download Git bundle/i }).click(),
  ]);
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

  // 3. Member role can also export full bundle
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
    memberExport.getByRole("link", { name: /Download Git bundle/i }).click(),
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

  // Must NEVER render simulated payment controls or premature workspace delete UI
  assert.equal(await billingSection.getByRole("button", { name: /checkout|subscribe|portal|cancel subscription|update plan/i }).count(), 0);
  assert.equal(await page.getByRole("button", { name: /delete workspace/i }).count(), 0);
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

  // 3. Member role sees billing info read-only
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
