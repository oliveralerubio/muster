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
