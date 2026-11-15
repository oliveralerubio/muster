import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright-core';

const baseUrl = process.env.MUSTER_CLOUD_URL;
assert.ok(baseUrl, 'MUSTER_CLOUD_URL is required');

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
  await page.getByText(/owner/i).waitFor({ timeout: 5000 });
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

const journeys = [
  ['journey_1_signin_and_account', journey1SignInAndAccount],
  ['journey_2_empty_workspaces', journey2EmptyWorkspaces],
  ['journey_3_create_workspace_validation_and_idempotency', journey3CreateWorkspaceValidationAndIdempotency],
  ['journey_4_detail_reload_and_logout', journey4DetailReloadAndLogout],
  ['journey_5_accessibility_theme_responsive_and_security', journey5AccessibilityThemeResponsiveAndSecurity],
  ['journey_6_device_approve_with_login_redirect', journey6DeviceApproveWithLoginRedirect],
  ['journey_7_device_deny_and_error_handling', journey7DeviceDenyAndErrorHandling],
  ['journey_8_workspace_devices_list', journey8WorkspaceDevicesList],
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
