(() => {
  'use strict';

  const THEME_STORAGE_KEY = 'mc_theme';
  const state = {
    me: null,
    workspaces: null,
    loadingWorkspaces: false,
    loadError: null,
    formName: '',
    formError: null,
    submitError: null,
    createInFlight: false,
    createIdempotencyKey: null,
    createIntentBody: null,
    workspaceEpoch: 0,
    activeWorkspace: null,
    loadingDetail: false,
    detailError: null,
    devices: null,
    loadingDevices: false,
    devicesError: null,
    members: null,
    loadingMembers: false,
    membersError: null,
    memberActionError: null,
    removingMemberId: null,
    invitations: null,
    loadingInvitations: false,
    invitationsError: null,
    inviteEmail: '',
    inviteRole: 'member',
    inviteFieldError: null,
    inviteSubmitError: null,
    inviteInFlight: false,
    createdInvite: null,
    deviceUserCode: '',
    deviceFieldError: null,
    deviceSubmitError: null,
    deviceInFlight: false,
    deviceResult: null,
    acceptInFlight: false,
    acceptError: null,
    acceptResult: null,
    revisions: null,
    loadingRevisions: false,
    revisionsError: null,
    loadingMoreRevisions: false,
    hasMoreRevisions: false,
    selectedDiffRev: null,
    diffsByRev: {},
    loadingDiffRev: null,
    diffError: null,
    proposals: null,
    loadingProposals: false,
    proposalsError: null,
    proposalActionError: null,
    rejectingProposalId: null,
    restoreTargetRev: '',
    restorePathsText: '',
    restoreFieldError: null,
    restoreSubmitError: null,
    restoreInFlight: false,
    restoreCreatedMessage: null,
    billing: null,
    loadingBilling: false,
    billingError: null,
    billingActionError: null,
    billingNotice: null,
    checkoutPlan: 'personal',
    checkoutSeats: '2',
    checkoutFieldError: null,
    checkoutInFlight: false,
    checkoutSession: null,
    portalInFlight: false,
    portalUrl: null,
    updatePlanChoice: 'personal',
    updateSeats: '2',
    updatePlanFieldError: null,
    confirmingPlanChange: null,
    updatePlanInFlight: false,
    confirmingCancel: false,
    cancelInFlight: false,
    signOutInFlight: false,
    signOutError: null,
  };

  function resetBillingState() {
    state.billing = null;
    state.loadingBilling = false;
    state.billingError = null;
    state.billingActionError = null;
    state.billingNotice = null;
    state.checkoutPlan = 'personal';
    state.checkoutSeats = '2';
    state.checkoutFieldError = null;
    state.checkoutInFlight = false;
    state.checkoutSession = null;
    state.portalInFlight = false;
    state.portalUrl = null;
    state.updatePlanChoice = 'personal';
    state.updateSeats = '2';
    state.updatePlanFieldError = null;
    state.confirmingPlanChange = null;
    state.updatePlanInFlight = false;
    state.confirmingCancel = false;
    state.cancelInFlight = false;
  }

  function captureBillingContext(wsId) {
    return {
      epoch: state.workspaceEpoch,
      wsId,
      accountId: state.me ? state.me.id : null,
    };
  }

  function isCurrentBillingContext(ctx) {
    return (
      Boolean(ctx && ctx.accountId) &&
      state.workspaceEpoch === ctx.epoch &&
      Boolean(state.me && state.me.id === ctx.accountId) &&
      Boolean(state.activeWorkspace && state.activeWorkspace.id === ctx.wsId) &&
      parseWorkspaceIdFromPath(window.location.pathname) === ctx.wsId
    );
  }

  function el(tag, attrs = {}, children = []) {
    const node = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      if (k === 'className') {
        node.className = v;
      } else if (k === 'textContent') {
        node.textContent = v;
      } else if (k.startsWith('on') && typeof v === 'function') {
        node.addEventListener(k.slice(2).toLowerCase(), v);
      } else {
        node.setAttribute(k, v === true ? '' : String(v));
      }
    }
    for (const child of Array.isArray(children) ? children : [children]) {
      if (child === null || child === undefined) continue;
      node.appendChild(
        typeof child === 'string' ? document.createTextNode(child) : child
      );
    }
    return node;
  }

  function applyTheme(theme) {
    if (theme === 'light' || theme === 'dark') {
      document.documentElement.setAttribute('data-theme', theme);
    } else {
      document.documentElement.removeAttribute('data-theme');
    }
  }

  function currentTheme() {
    const saved = localStorage.getItem(THEME_STORAGE_KEY);
    if (saved === 'light' || saved === 'dark') return saved;
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  function toggleTheme() {
    const next = currentTheme() === 'dark' ? 'light' : 'dark';
    localStorage.setItem(THEME_STORAGE_KEY, next);
    applyTheme(next);
    renderShell();
  }

  function renderHeader() {
    const themeLabel = currentTheme() === 'dark' ? 'Light mode' : 'Dark mode';
    const actions = [
      el('button', {
        type: 'button',
        className: 'btn btn-secondary btn-sm',
        onClick: toggleTheme,
        'aria-label': `Switch to ${themeLabel.toLowerCase()}`,
        textContent: themeLabel,
      }),
    ];

    if (state.me) {
      actions.unshift(
        el('span', { className: 'account-email', textContent: state.me.email }),
        el('button', {
          type: 'button',
          className: 'btn btn-secondary btn-sm',
          disabled: state.signOutInFlight,
          onClick: handleSignOut,
          textContent: state.signOutInFlight ? 'Signing out…' : 'Sign out',
        })
      );
    }

    return el('header', { className: 'topbar' }, [
      el('div', { className: 'topbar-inner' }, [
        el('a', {
          href: '/',
          className: 'brand',
          onClick: (e) => {
            if (!state.me) return;
            e.preventDefault();
            navigateToList();
          },
        }, [
          el('span', { className: 'brand-mark', 'aria-hidden': 'true', textContent: 'M' }),
          el('span', { textContent: 'Muster Cloud' }),
        ]),
        el('div', { className: 'topbar-actions' }, actions),
      ]),
    ]);
  }

  function parseWorkspaceIdFromPath(pathname) {
    const match = /^\/workspaces\/([^/]+)$/.exec(pathname);
    return match ? decodeURIComponent(match[1]) : null;
  }

  function isDeviceRoute(pathname = window.location.pathname) {
    return pathname === '/device';
  }

  function parseInvitationTokenFromPath(pathname = window.location.pathname) {
    const match = /^\/invitations\/([^/]+)$/.exec(pathname);
    return match ? decodeURIComponent(match[1]) : null;
  }

  function syncDeviceCodeFromUrl() {
    const params = new URLSearchParams(window.location.search);
    const code = params.get('user_code');
    if (code) {
      state.deviceUserCode = code.trim().toUpperCase();
    }
  }

  function currentReturnTo() {
    if (isDeviceRoute()) {
      return window.location.pathname + window.location.search;
    }
    if (parseInvitationTokenFromPath()) {
      return window.location.pathname;
    }
    return '/';
  }

  function formatDate(iso) {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleDateString('en-US', {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    });
  }

  function formatPlanStatus(status) {
    switch (status) {
      case 'trialing':
        return { label: 'Trialing (14-day trial)', badgeClass: 'badge badge-trialing' };
      case 'trial_expired':
        return { label: 'Trial expired', badgeClass: 'badge badge-warn' };
      case 'active':
        return { label: 'Active', badgeClass: 'badge badge-trialing' };
      case 'past_due':
        return { label: 'Past due', badgeClass: 'badge badge-warn' };
      case 'restricted':
        return { label: 'Restricted', badgeClass: 'badge badge-warn' };
      case 'canceled':
        return { label: 'Canceled', badgeClass: 'badge badge-neutral' };
      default:
        return { label: status, badgeClass: 'badge badge-neutral' };
    }
  }

  async function handleSignOut() {
    if (state.signOutInFlight || !state.me) return;
    state.signOutInFlight = true;
    state.signOutError = null;
    renderShell();
    try {
      const resp = await fetch('/v1/auth/logout', {
        method: 'POST',
        headers: {
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
      });
      if (!resp.ok && resp.status !== 401) {
        throw new Error(`HTTP ${resp.status}`);
      }
      state.signOutInFlight = false;
      state.me = null;
      state.workspaces = null;
      state.activeWorkspace = null;
      state.detailError = null;
      state.members = null;
      state.membersError = null;
      state.memberActionError = null;
      state.removingMemberId = null;
      state.invitations = null;
      state.invitationsError = null;
      state.createdInvite = null;
      state.inviteEmail = '';
      state.inviteRole = 'member';
      state.inviteFieldError = null;
      state.inviteSubmitError = null;
      state.acceptInFlight = false;
      state.acceptError = null;
      state.acceptResult = null;
      state.revisions = null;
      state.loadingRevisions = false;
      state.revisionsError = null;
      state.loadingMoreRevisions = false;
      state.hasMoreRevisions = false;
      state.selectedDiffRev = null;
      state.diffsByRev = {};
      state.loadingDiffRev = null;
      state.diffError = null;
      state.proposals = null;
      state.loadingProposals = false;
      state.proposalsError = null;
      state.proposalActionError = null;
      state.rejectingProposalId = null;
      state.restoreTargetRev = '';
      state.restorePathsText = '';
      state.restoreFieldError = null;
      state.restoreSubmitError = null;
      state.restoreInFlight = false;
      state.restoreCreatedMessage = null;
      state.workspaceEpoch += 1;
      resetBillingState();
      window.history.pushState({}, '', '/');
      renderShell();
    } catch (_err) {
      state.signOutInFlight = false;
      state.signOutError = 'Could not sign out. Please try again.';
      renderShell();
    }
  }

  function navigateToList() {
    state.activeWorkspace = null;
    state.detailError = null;
    state.devices = null;
    state.devicesError = null;
    state.members = null;
    state.membersError = null;
    state.memberActionError = null;
    state.removingMemberId = null;
    state.invitations = null;
    state.invitationsError = null;
    state.createdInvite = null;
    state.inviteEmail = '';
    state.inviteRole = 'member';
    state.inviteFieldError = null;
    state.inviteSubmitError = null;
    state.acceptInFlight = false;
    state.acceptError = null;
    state.acceptResult = null;
    state.deviceResult = null;
    state.deviceSubmitError = null;
    state.deviceFieldError = null;
    state.revisions = null;
    state.loadingRevisions = false;
    state.revisionsError = null;
    state.loadingMoreRevisions = false;
    state.hasMoreRevisions = false;
    state.selectedDiffRev = null;
    state.diffsByRev = {};
    state.loadingDiffRev = null;
    state.diffError = null;
    state.proposals = null;
    state.loadingProposals = false;
    state.proposalsError = null;
    state.proposalActionError = null;
    state.rejectingProposalId = null;
    state.restoreTargetRev = '';
    state.restorePathsText = '';
    state.restoreFieldError = null;
    state.restoreSubmitError = null;
    state.restoreInFlight = false;
    state.restoreCreatedMessage = null;
    state.workspaceEpoch += 1;
    resetBillingState();
    window.history.pushState({}, '', '/');
    if (!state.workspaces) {
      loadWorkspaces();
    } else {
      renderShell();
    }
  }

  function navigateToWorkspace(id) {
    state.acceptInFlight = false;
    state.acceptError = null;
    state.acceptResult = null;
    window.history.pushState({}, '', `/workspaces/${encodeURIComponent(id)}`);
    loadWorkspaceDetail(id);
  }

  function renderSignedOut() {
    const returnTo = encodeURIComponent(currentReturnTo());
    const inviteToken = parseInvitationTokenFromPath();
    const deviceNotice = isDeviceRoute()
      ? el('p', {
          className: 'muted',
          textContent:
            'Sign in to review and confirm the CLI login request. Only approve codes from a CLI login you started.',
        })
      : inviteToken
        ? el('p', {
            className: 'muted',
            textContent:
              'Sign in to review and accept the workspace invitation using the verified email address that was invited.',
          })
        : null;
    return el('section', { className: 'card hero-card stack-md' }, [
      el('div', { className: 'stack-sm' }, [
        el('h1', { textContent: 'Muster Cloud' }),
        el('p', {
          className: 'muted',
          textContent:
            'Hosted workspace console and fleet synchronization for Muster. Your free CLI, local vault, and signing keys stay under your control.',
        }),
        deviceNotice,
      ]),
      el('ul', { className: 'feature-list' }, [
        el('li', { textContent: 'Start a workspace with a 14-day free trial and no card required.' }),
        el('li', { textContent: 'Sync machine configurations without running your own Git server.' }),
        el('li', { textContent: 'Export your complete fleet history back to the open-source CLI anytime.' }),
      ]),
      el('div', {}, [
        el('a', {
          href: `/auth/login?return_to=${returnTo}`,
          className: 'btn btn-primary',
          textContent: 'Sign in',
        }),
      ]),
    ]);
  }

  function validateWorkspaceName(raw) {
    const name = raw.trim();
    const chars = Array.from(name).length;
    if (chars === 0) {
      return { ok: false, error: 'Enter a workspace name.' };
    }
    if (chars > 64) {
      return { ok: false, error: 'Workspace name must be 64 characters or fewer.' };
    }
    if (/[\u0000-\u001F\u007F]/.test(name)) {
      return { ok: false, error: 'Workspace name cannot contain control characters.' };
    }
    return { ok: true, name };
  }

  function formatDeviceStatus(status) {
    switch (status) {
      case 'approved':
        return { label: 'Approved', badgeClass: 'badge badge-trialing' };
      case 'pending':
        return { label: 'Pending', badgeClass: 'badge badge-warn' };
      case 'revoked':
        return { label: 'Revoked', badgeClass: 'badge badge-warn' };
      case 'linked':
      default:
        return { label: 'Linked', badgeClass: 'badge badge-neutral' };
    }
  }

  async function handleDeviceDecision(decision) {
    if (state.deviceInFlight || !state.me) return;
    const code = state.deviceUserCode.trim();
    if (!code) {
      state.deviceFieldError = 'Enter the verification code shown by your CLI.';
      state.deviceSubmitError = null;
      renderShell();
      const input = document.getElementById('device-user-code');
      if (input) input.focus();
      return;
    }

    state.deviceFieldError = null;
    state.deviceSubmitError = null;
    state.deviceInFlight = true;
    renderShell();

    try {
      const resp = await fetch('/v1/device/approve', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: JSON.stringify({ user_code: code, decision }),
      });
      if (resp.status === 401) {
        state.me = null;
        state.deviceInFlight = false;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        state.deviceSubmitError =
          (data && data.error && data.error.message) ||
          'Could not process this code. Check that it is valid and has not expired.';
        state.deviceInFlight = false;
        renderShell();
        return;
      }
      state.deviceInFlight = false;
      state.deviceResult = data;
      renderShell();
    } catch (_err) {
      state.deviceInFlight = false;
      state.deviceSubmitError = 'Connection failed while sending your decision. Please try again.';
      renderShell();
    }
  }

  function renderDeviceAuth() {
    if (state.deviceResult) {
      const approved = state.deviceResult.status === 'approved';
      return el('section', { className: 'card hero-card stack-md' }, [
        el('div', { className: 'row-between' }, [
          el('h1', { textContent: approved ? 'CLI login approved' : 'CLI login denied' }),
          el('span', {
            className: approved ? 'badge badge-trialing' : 'badge badge-warn',
            textContent: approved ? 'Approved' : 'Denied',
          }),
        ]),
        el('p', {
          textContent: approved
            ? `Authorized client "${state.deviceResult.client_name}" to list your workspaces and link machines.`
            : `Denied the login request from client "${state.deviceResult.client_name}".`,
        }),
        approved
          ? el('p', {
              className: 'muted',
              textContent:
                'This grants account access for workspace listing and machine linking only. It does not transfer signing keys or grant fleet governance approval.',
            })
          : null,
        el('div', {}, [
          el('a', {
            href: '/',
            className: 'btn btn-primary',
            onClick: (e) => {
              e.preventDefault();
              navigateToList();
            },
            textContent: 'All workspaces',
          }),
        ]),
      ]);
    }

    const hasFieldError = Boolean(state.deviceFieldError);
    return el('section', { className: 'card hero-card stack-md' }, [
      el('div', { className: 'stack-sm' }, [
        el('h1', { textContent: 'Authorize CLI login' }),
        el('p', {
          className: 'muted',
          textContent:
            'Confirm that this verification code matches the CLI login you started. Approving grants an account token to list workspaces and link machines; it does not share signing keys or approve a machine for your fleet.',
        }),
      ]),
      state.deviceSubmitError
        ? el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.deviceSubmitError }),
          ])
        : null,
      el('form', {
        className: 'stack-md',
        noValidate: true,
        onSubmit: (e) => {
          e.preventDefault();
          handleDeviceDecision('approve');
        },
      }, [
        el('div', { className: 'field' }, [
          el('label', { for: 'device-user-code', textContent: 'Verification code' }),
          el('input', {
            id: 'device-user-code',
            name: 'user_code',
            type: 'text',
            className: 'input code-input',
            autocomplete: 'off',
            placeholder: 'BCDF-GH23',
            value: state.deviceUserCode,
            'aria-invalid': hasFieldError ? 'true' : null,
            'aria-describedby': hasFieldError ? 'device-code-error' : null,
            onInput: (e) => {
              state.deviceUserCode = e.target.value;
              state.deviceFieldError = null;
              state.deviceSubmitError = null;
            },
          }),
          hasFieldError
            ? el('p', {
                id: 'device-code-error',
                className: 'field-error',
                role: 'alert',
                textContent: state.deviceFieldError,
              })
            : null,
        ]),
        el('div', { className: 'btn-row' }, [
          el('button', {
            type: 'submit',
            className: 'btn btn-primary',
            disabled: state.deviceInFlight,
            textContent: state.deviceInFlight ? 'Submitting…' : 'Approve',
          }),
          el('button', {
            type: 'button',
            className: 'btn btn-danger',
            disabled: state.deviceInFlight,
            onClick: () => handleDeviceDecision('deny'),
            textContent: 'Deny',
          }),
        ]),
      ]),
    ]);
  }

  function canManageInvites(ws) {
    return Boolean(ws && (ws.role === 'owner' || ws.role === 'admin'));
  }

  function validateInviteEmail(raw) {
    const email = raw.trim().toLowerCase();
    if (email.length < 3 || email.length > 255 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      return { ok: false, error: 'Enter a valid email address.' };
    }
    return { ok: true, email };
  }

  function formatInvitationStatus(inv) {
    if (inv.accepted_at) {
      return { label: 'Accepted', badgeClass: 'badge badge-trialing' };
    }
    const exp = Date.parse(inv.expires_at);
    if (!Number.isNaN(exp) && exp <= Date.now()) {
      return { label: 'Expired', badgeClass: 'badge badge-warn' };
    }
    return { label: 'Pending', badgeClass: 'badge badge-neutral' };
  }

  async function handleCreateInvitation(e, ws) {
    e.preventDefault();
    if (state.inviteInFlight || !state.me) return;

    const check = validateInviteEmail(state.inviteEmail);
    if (!check.ok) {
      state.inviteFieldError = check.error;
      state.inviteSubmitError = null;
      renderShell();
      const input = document.getElementById('invite-email');
      if (input) input.focus();
      return;
    }

    const allowedRoles = ws.role === 'owner' ? ['member', 'admin'] : ['member'];
    const role = allowedRoles.includes(state.inviteRole) ? state.inviteRole : 'member';

    state.inviteFieldError = null;
    state.inviteSubmitError = null;
    state.inviteInFlight = true;
    renderShell();

    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(ws.id)}/invitations`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: JSON.stringify({ email: check.email, role }),
      });
      if (resp.status === 401) {
        state.me = null;
        state.inviteInFlight = false;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        state.inviteSubmitError =
          (data && data.error && data.error.message) ||
          'Could not create the invitation. Please try again.';
        state.inviteInFlight = false;
        renderShell();
        return;
      }

      state.inviteInFlight = false;
      state.inviteEmail = '';
      state.inviteRole = 'member';
      state.createdInvite = {
        email: data.invitation.email,
        role: data.invitation.role,
        shareUrl: `${window.location.origin}/invitations/${encodeURIComponent(data.token)}`,
      };
      state.invitations = [data.invitation, ...(state.invitations || [])];
      renderShell();
    } catch (_err) {
      state.inviteInFlight = false;
      state.inviteSubmitError = 'Connection failed while creating the invitation. Please try again.';
      renderShell();
    }
  }

  function renderInviteControls(ws) {
    if (!canManageInvites(ws)) return null;

    const hasFieldError = Boolean(state.inviteFieldError);
    const roleOptions = ws.role === 'owner' ? ['member', 'admin'] : ['member'];
    const selectedRole = roleOptions.includes(state.inviteRole) ? state.inviteRole : 'member';

    const blocks = [
      el('form', {
        className: 'stack-md invite-form',
        noValidate: true,
        onSubmit: (e) => handleCreateInvitation(e, ws),
      }, [
        el('h3', { textContent: 'Invite a member' }),
        state.inviteSubmitError
          ? el('div', { className: 'alert', role: 'alert' }, [
              el('span', { textContent: state.inviteSubmitError }),
            ])
          : null,
        el('div', { className: 'invite-grid' }, [
          el('div', { className: 'field' }, [
            el('label', { for: 'invite-email', textContent: 'Invite email' }),
            el('input', {
              id: 'invite-email',
              name: 'email',
              type: 'email',
              className: 'input',
              autocomplete: 'off',
              placeholder: 'teammate@example.com',
              value: state.inviteEmail,
              'aria-invalid': hasFieldError ? 'true' : null,
              'aria-describedby': hasFieldError ? 'invite-email-error' : null,
              onInput: (e) => {
                state.inviteEmail = e.target.value;
                state.inviteFieldError = null;
                state.inviteSubmitError = null;
              },
            }),
            hasFieldError
              ? el('p', {
                  id: 'invite-email-error',
                  className: 'field-error',
                  role: 'alert',
                  textContent: state.inviteFieldError,
                })
              : null,
          ]),
          el('div', { className: 'field' }, [
            el('label', { for: 'invite-role', textContent: 'Cloud role' }),
            el(
              'select',
              {
                id: 'invite-role',
                name: 'role',
                className: 'input',
                onChange: (e) => {
                  state.inviteRole = e.target.value;
                },
              },
              roleOptions.map((r) =>
                el('option', {
                  value: r,
                  selected: selectedRole === r,
                  textContent: r,
                })
              )
            ),
          ]),
        ]),
        el('div', {}, [
          el('button', {
            type: 'submit',
            className: 'btn btn-primary btn-sm',
            disabled: state.inviteInFlight,
            textContent: state.inviteInFlight ? 'Sending…' : 'Send invite',
          }),
        ]),
      ]),
    ];

    if (state.createdInvite) {
      blocks.push(
        el('div', { className: 'share-box stack-sm' }, [
          el('div', { className: 'row-between' }, [
            el('p', {
              textContent: `One-time shareable link ready for ${state.createdInvite.email} (${state.createdInvite.role}).`,
            }),
            el('button', {
              type: 'button',
              className: 'btn btn-secondary btn-sm',
              onClick: () => {
                state.createdInvite = null;
                renderShell();
              },
              textContent: 'Dismiss',
            }),
          ]),
          el('div', { className: 'field' }, [
            el('label', { for: 'invite-share-link', textContent: 'Shareable invite link' }),
            el('input', {
              id: 'invite-share-link',
              type: 'text',
              className: 'input mono',
              readonly: true,
              value: state.createdInvite.shareUrl,
            }),
          ]),
          el('p', {
            className: 'muted',
            textContent:
              'This link is shown once. The invited person must sign in with their verified email and explicitly accept.',
          }),
        ])
      );
    }

    if (state.loadingInvitations) {
      blocks.push(el('p', { className: 'muted', textContent: 'Loading invitations…' }));
    } else if (state.invitationsError) {
      blocks.push(
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.invitationsError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: () => loadWorkspaceInvitations(ws.id),
            textContent: 'Retry',
          }),
        ])
      );
    } else if (state.invitations && state.invitations.length > 0) {
      blocks.push(
        el('div', { className: 'stack-sm' }, [
          el('h3', { textContent: 'Invitations' }),
          el(
            'div',
            { className: 'member-list' },
            state.invitations.map((inv) => {
              const st = formatInvitationStatus(inv);
              return el('article', { className: 'member-card stack-sm' }, [
                el('div', { className: 'row-between' }, [
                  el('strong', { className: 'member-email', textContent: inv.email }),
                  el('div', { className: 'badge-row' }, [
                    el('span', { className: formatRoleBadge(inv.role), textContent: inv.role }),
                    el('span', { className: st.badgeClass, textContent: st.label }),
                  ]),
                ]),
                el('dl', { className: 'meta-list' }, [
                  el('div', { className: 'meta-item' }, [
                    el('dt', { textContent: 'Expires' }),
                    el('dd', { textContent: formatDate(inv.expires_at) }),
                  ]),
                ]),
              ]);
            })
          ),
        ])
      );
    }

    return el('div', { className: 'stack-md invite-controls' }, blocks);
  }

  function formatRoleBadge(role) {
    switch (role) {
      case 'owner':
        return 'badge badge-trialing';
      case 'admin':
        return 'badge badge-warn';
      default:
        return 'badge badge-neutral';
    }
  }

  function canRemoveMember(ws, m) {
    if (!ws || !state.me) return false;
    if (m.user_id === state.me.id) return true;
    if (ws.role === 'owner') return true;
    if (ws.role === 'admin') return m.role === 'member';
    return false;
  }

  async function handleRemoveMember(ws, m) {
    if (state.removingMemberId || !state.me) return;
    state.removingMemberId = m.user_id;
    state.memberActionError = null;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(ws.id)}/members/${encodeURIComponent(m.user_id)}`,
        {
          method: 'DELETE',
          headers: {
            Accept: 'application/json',
            'X-CSRF-Token': state.me.csrf_token,
          },
          credentials: 'same-origin',
        }
      );
      if (resp.status === 401) {
        state.me = null;
        state.removingMemberId = null;
        renderShell();
        return;
      }
      if (!resp.ok) {
        const data = await resp.json().catch(() => null);
        state.removingMemberId = null;
        state.memberActionError =
          (data && data.error && data.error.message) ||
          'Could not remove this member. Please try again.';
        renderShell();
        return;
      }
      state.removingMemberId = null;
      if (m.user_id === state.me.id) {
        state.workspaces = null;
        navigateToList();
        return;
      }
      state.members = (state.members || []).filter((item) => item.user_id !== m.user_id);
      renderShell();
      await loadWorkspaceDevices(ws.id);
    } catch (_err) {
      state.removingMemberId = null;
      state.memberActionError = 'Connection failed while removing the member. Please try again.';
      renderShell();
    }
  }

  function renderMembersSection(ws) {
    const heading = el('h2', { id: 'members-heading', textContent: 'Members' });
    const notice = el('p', {
      className: 'muted',
      textContent:
        'Cloud roles control console access and billing only. They do not grant fleet signing keys or signed fleet approval. Removing a member revokes transport tokens of machines they linked in this workspace; it does not change signed fleet governance or erase secrets already delivered.',
    });

    if (state.loadingMembers) {
      return el(
        'section',
        { className: 'card stack-sm', 'aria-labelledby': 'members-heading', 'aria-live': 'polite' },
        [heading, notice, el('p', { className: 'muted', textContent: 'Loading members…' })]
      );
    }

    if (state.membersError) {
      return el('section', { className: 'card stack-md', 'aria-labelledby': 'members-heading' }, [
        heading,
        notice,
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.membersError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: () => loadWorkspaceMembers(ws.id),
            textContent: 'Retry',
          }),
        ]),
      ]);
    }

    const members = state.members || [];
    return el('section', { className: 'card stack-md', 'aria-labelledby': 'members-heading' }, [
      heading,
      notice,
      state.memberActionError
        ? el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.memberActionError }),
          ])
        : null,
      el(
        'div',
        { className: 'member-list' },
        members.map((m) => {
          const isSelf = Boolean(state.me && m.user_id === state.me.id);
          const badges = [
            el('span', { className: formatRoleBadge(m.role), textContent: m.role }),
          ];
          if (isSelf) {
            badges.push(el('span', { className: 'badge badge-neutral', textContent: 'You' }));
          }
          const showRemove = canRemoveMember(ws, m);
          const busy = state.removingMemberId === m.user_id;
          return el('article', { className: 'member-card stack-sm' }, [
            el('div', { className: 'row-between' }, [
              el('strong', { className: 'member-email', textContent: m.email }),
              el('div', { className: 'badge-row' }, badges),
            ]),
            el('div', { className: 'row-between' }, [
              el('dl', { className: 'meta-list' }, [
                el('div', { className: 'meta-item' }, [
                  el('dt', { textContent: 'Joined' }),
                  el('dd', { textContent: formatDate(m.joined_at) }),
                ]),
              ]),
              showRemove
                ? el('button', {
                    type: 'button',
                    className: 'btn btn-danger btn-sm',
                    disabled: Boolean(state.removingMemberId),
                    'aria-label': isSelf ? 'Leave workspace' : `Remove ${m.email}`,
                    onClick: () => handleRemoveMember(ws, m),
                    textContent: busy
                      ? isSelf
                        ? 'Leaving…'
                        : 'Removing…'
                      : isSelf
                        ? 'Leave workspace'
                        : 'Remove',
                  })
                : null,
            ]),
          ]);
        })
      ),
      renderInviteControls(ws),
    ]);
  }

  async function loadWorkspaceProposals(id) {
    state.loadingProposals = true;
    state.proposalsError = null;
    state.proposals = null;
    state.proposalActionError = null;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(id)}/proposals`,
        { headers: { Accept: 'application/json' }, credentials: 'same-origin' }
      );
      if (resp.status === 401) {
        state.me = null;
        state.loadingProposals = false;
        renderShell();
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      state.proposals = await resp.json();
      state.loadingProposals = false;
      renderShell();
    } catch (_err) {
      state.loadingProposals = false;
      state.proposalsError = 'Could not load proposals. Please try again.';
      renderShell();
    }
  }

  function canReviewProposals(ws) {
    return Boolean(ws && (ws.role === 'owner' || ws.role === 'admin'));
  }

  function formatProposalKind(kind) {
    switch (kind) {
      case 'conflict':
        return 'Conflict';
      case 'pending_quorum':
        return 'Pending quorum';
      case 'unauthorized':
        return 'Unauthorized';
      case 'enrollment':
        return 'Enrollment';
      case 'restore':
        return 'Restore';
      default:
        return kind;
    }
  }

  function formatProposalStatus(status) {
    switch (status) {
      case 'open':
        return { label: 'Open', badgeClass: 'badge badge-warn' };
      case 'resolved':
        return { label: 'Resolved', badgeClass: 'badge badge-trialing' };
      case 'rejected':
        return { label: 'Rejected', badgeClass: 'badge badge-neutral' };
      case 'superseded':
        return { label: 'Superseded', badgeClass: 'badge badge-neutral' };
      default:
        return { label: status, badgeClass: 'badge badge-neutral' };
    }
  }

  function proposalGuidance(p) {
    switch (p.kind) {
      case 'conflict':
        return 'Push collided with a newer head. Pull latest history or this bundle, rebase and re-sign locally, then push again.';
      case 'pending_quorum':
        return 'Held waiting for additional local admin signatures to meet governance quorum.';
      case 'unauthorized':
        return 'Signing key is not authorized in accepted fleet governance.';
      case 'enrollment':
        return 'An authorized admin machine must approve and sign this machine into the fleet locally.';
      case 'restore':
        return 'Unsigned restore request. An authorized machine must review, sign a forward commit locally, and push to apply.';
      default:
        return null;
    }
  }

  async function handleRejectProposal(ws, proposal) {
    if (state.rejectingProposalId || !state.me) return;
    state.rejectingProposalId = proposal.id;
    state.proposalActionError = null;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(ws.id)}/proposals/${encodeURIComponent(proposal.id)}/resolve`,
        {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            Accept: 'application/json',
            'X-CSRF-Token': state.me.csrf_token,
          },
          credentials: 'same-origin',
          body: JSON.stringify({ result_revision: null }),
        }
      );
      if (resp.status === 401) {
        state.me = null;
        state.rejectingProposalId = null;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        state.rejectingProposalId = null;
        state.proposalActionError =
          (data && data.error && data.error.message) ||
          'Could not reject this proposal. Please try again.';
        renderShell();
        return;
      }
      state.rejectingProposalId = null;
      state.proposals = (state.proposals || []).map((item) =>
        item.id === data.id ? data : item
      );
      renderShell();
    } catch (_err) {
      state.rejectingProposalId = null;
      state.proposalActionError =
        'Connection failed while rejecting the proposal. Please try again.';
      renderShell();
    }
  }

  function selectRestoreTarget(rev) {
    state.restoreTargetRev = rev;
    state.restoreFieldError = null;
    state.restoreSubmitError = null;
    state.restoreCreatedMessage = null;
    state.billing = null;
    state.loadingBilling = false;
    state.billingError = null;
    renderShell();
    const input = document.getElementById('restore-paths');
    if (input) {
      input.focus();
    }
  }

  async function handleCreateRestoreProposal(e, ws) {
    e.preventDefault();
    if (state.restoreInFlight || !state.me) return;
    const revisions = state.revisions || [];
    if (revisions.length === 0) return;

    const targetRev =
      revisions.some((r) => r.revision === state.restoreTargetRev)
        ? state.restoreTargetRev
        : revisions[0].revision;
    const paths = state.restorePathsText
      .split(/[\n,]+/)
      .map((s) => s.trim())
      .filter(Boolean);

    if (paths.length === 0) {
      state.restoreFieldError = 'Enter at least one relative repository path to restore.';
      state.restoreSubmitError = null;
      state.restoreCreatedMessage = null;
      renderShell();
      const input = document.getElementById('restore-paths');
      if (input) input.focus();
      return;
    }

    state.restoreFieldError = null;
    state.restoreSubmitError = null;
    state.restoreCreatedMessage = null;
    state.restoreInFlight = true;
    renderShell();

    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(ws.id)}/proposals`,
        {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            Accept: 'application/json',
            'X-CSRF-Token': state.me.csrf_token,
          },
          credentials: 'same-origin',
          body: JSON.stringify({
            kind: 'restore',
            base_revision: revisions[0].revision,
            payload: {
              target_revision: targetRev,
              paths,
            },
          }),
        }
      );
      if (resp.status === 401) {
        state.me = null;
        state.restoreInFlight = false;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        state.restoreInFlight = false;
        state.restoreSubmitError =
          (data && data.error && data.error.message) ||
          'Could not create restore proposal. Please try again.';
        renderShell();
        return;
      }
      state.restoreInFlight = false;
      state.restorePathsText = '';
      state.restoreCreatedMessage =
        'Restore proposal created (unsigned). It requires local signing and a new forward commit pushed from an authorized machine.';
      state.proposals = [data, ...(state.proposals || [])];
      renderShell();
    } catch (_err) {
      state.restoreInFlight = false;
      state.restoreSubmitError =
        'Connection failed while creating the restore proposal. Please try again.';
      renderShell();
    }
  }

  function renderRestoreForm(ws) {
    const revisions = state.revisions || [];
    if (!canReviewProposals(ws) || revisions.length === 0) return null;

    const selectedRev =
      revisions.some((r) => r.revision === state.restoreTargetRev)
        ? state.restoreTargetRev
        : revisions[0].revision;
    const hasFieldError = Boolean(state.restoreFieldError);

    return el(
      'form',
      {
        className: 'stack-md invite-controls',
        noValidate: true,
        onSubmit: (e) => handleCreateRestoreProposal(e, ws),
      },
      [
        el('div', { className: 'stack-sm' }, [
          el('h3', { textContent: 'Request restore proposal' }),
          el('p', {
            className: 'muted',
            textContent:
              'Submits an unsigned restore proposal only. Clicking this never modifies or reverts files; an authorized client must review, sign a new forward commit locally, and push.',
          }),
        ]),
        state.restoreSubmitError
          ? el('div', { className: 'alert', role: 'alert' }, [
              el('span', { textContent: state.restoreSubmitError }),
            ])
          : null,
        state.restoreCreatedMessage
          ? el('div', { className: 'share-box', role: 'status' }, [
              el('p', { textContent: state.restoreCreatedMessage }),
            ])
          : null,
        el('div', { className: 'field' }, [
          el('label', { for: 'restore-target', textContent: 'Target revision' }),
          el(
            'select',
            {
              id: 'restore-target',
              className: 'input mono',
              onChange: (e) => {
                state.restoreTargetRev = e.target.value;
              },
            },
            revisions.map((r) =>
              el('option', {
                value: r.revision,
                selected: selectedRev === r.revision,
                textContent: `${r.revision.slice(0, 8)} — ${r.message} (Epoch ${r.epoch})`,
              })
            )
          ),
        ]),
        el('div', { className: 'field' }, [
          el('label', { for: 'restore-paths', textContent: 'Paths to restore' }),
          el('input', {
            id: 'restore-paths',
            type: 'text',
            className: 'input mono',
            autocomplete: 'off',
            placeholder: 'settings.toml, vault/default.age',
            value: state.restorePathsText,
            'aria-invalid': hasFieldError ? 'true' : null,
            'aria-describedby': hasFieldError ? 'restore-paths-error' : null,
            onInput: (e) => {
              state.restorePathsText = e.target.value;
              state.restoreFieldError = null;
              state.restoreSubmitError = null;
            },
          }),
          hasFieldError
            ? el('p', {
                id: 'restore-paths-error',
                className: 'field-error',
                role: 'alert',
                textContent: state.restoreFieldError,
              })
            : null,
        ]),
        el('div', {}, [
          el('button', {
            type: 'submit',
            className: 'btn btn-primary btn-sm',
            disabled: state.restoreInFlight,
            textContent: state.restoreInFlight
              ? 'Creating proposal…'
              : 'Create restore proposal',
          }),
        ]),
      ]
    );
  }

  function renderProposalsSection(ws) {
    const heading = el('h2', { id: 'proposals-heading', textContent: 'Proposals' });
    const note = el('p', {
      className: 'muted',
      textContent:
        'Conflicts, pending quorum changes, enrollment requests, and restore requests. Cloud never signs governance or applies changes on your behalf.',
    });

    if (state.loadingProposals) {
      return el(
        'section',
        { className: 'card stack-sm', 'aria-labelledby': 'proposals-heading', 'aria-live': 'polite' },
        [heading, note, el('p', { className: 'muted', textContent: 'Loading proposals…' })]
      );
    }

    if (state.proposalsError) {
      return el('section', { className: 'card stack-md', 'aria-labelledby': 'proposals-heading' }, [
        heading,
        note,
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.proposalsError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: () => loadWorkspaceProposals(ws.id),
            textContent: 'Retry',
          }),
        ]),
      ]);
    }

    const proposals = state.proposals || [];
    const restoreForm = renderRestoreForm(ws);
    if (proposals.length === 0) {
      return el('section', { className: 'card stack-md', 'aria-labelledby': 'proposals-heading' }, [
        heading,
        note,
        el('p', { textContent: 'No proposals or conflicts.' }),
        restoreForm,
      ]);
    }

    const reviewer = canReviewProposals(ws);

    return el('section', { className: 'card stack-md', 'aria-labelledby': 'proposals-heading' }, [
      heading,
      note,
      state.proposalActionError
        ? el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.proposalActionError }),
          ])
        : null,
      el(
        'div',
        { className: 'member-list' },
        proposals.map((p) => {
          const st = formatProposalStatus(p.status);
          const kindLabel = formatProposalKind(p.kind);
          const guidance = proposalGuidance(p);
          const metaItems = [
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Base revision' }),
              el('dd', {
                className: 'mono',
                textContent: p.base_revision && p.base_revision !== 'none' ? p.base_revision.slice(0, 8) : 'none',
              }),
            ]),
          ];
          if (p.revision) {
            metaItems.push(
              el('div', { className: 'meta-item' }, [
                el('dt', { textContent: 'Proposed revision' }),
                el('dd', { className: 'mono', textContent: p.revision.slice(0, 8) }),
              ])
            );
          }
          if (p.epoch !== null && p.epoch !== undefined) {
            metaItems.push(
              el('div', { className: 'meta-item' }, [
                el('dt', { textContent: 'Epoch' }),
                el('dd', { textContent: String(p.epoch) }),
              ])
            );
          }
          metaItems.push(
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Created' }),
              el('dd', { textContent: formatDate(p.created_at) }),
            ])
          );
          if (p.kind === 'enrollment' && p.payload && p.payload.name) {
            metaItems.push(
              el('div', { className: 'meta-item' }, [
                el('dt', { textContent: 'Machine' }),
                el('dd', { textContent: p.payload.name }),
              ])
            );
          }
          if (p.kind === 'restore' && p.payload) {
            if (p.payload.target_revision) {
              metaItems.push(
                el('div', { className: 'meta-item' }, [
                  el('dt', { textContent: 'Target revision' }),
                  el('dd', {
                    className: 'mono',
                    textContent: p.payload.target_revision.slice(0, 8),
                  }),
                ])
              );
            }
            if (Array.isArray(p.payload.paths) && p.payload.paths.length > 0) {
              metaItems.push(
                el('div', { className: 'meta-item meta-item-wide' }, [
                  el('dt', { textContent: 'Paths' }),
                  el('dd', { className: 'mono', textContent: p.payload.paths.join(', ') }),
                ])
              );
            }
          }
          if (p.result_revision) {
            metaItems.push(
              el('div', { className: 'meta-item' }, [
                el('dt', { textContent: 'Result revision' }),
                el('dd', { className: 'mono', textContent: p.result_revision.slice(0, 8) }),
              ])
            );
          }

          const actions = [];
          if (p.revision) {
            actions.push(
              el('a', {
                href: `/v1/workspaces/${encodeURIComponent(ws.id)}/proposals/${encodeURIComponent(p.id)}/bundle`,
                download: `proposal-${p.id.slice(0, 8)}.bundle`,
                className: 'btn btn-secondary btn-sm',
                textContent: 'Download proposal bundle',
              })
            );
          }
          if (reviewer && p.status === 'open') {
            const busy = state.rejectingProposalId === p.id;
            actions.push(
              el('button', {
                type: 'button',
                className: 'btn btn-danger btn-sm',
                disabled: Boolean(state.rejectingProposalId),
                onClick: () => handleRejectProposal(ws, p),
                textContent: busy ? 'Rejecting…' : 'Reject proposal',
              })
            );
          }

          return el('article', { className: 'member-card stack-sm' }, [
            el('div', { className: 'row-between' }, [
              el('strong', { textContent: kindLabel }),
              el('div', { className: 'badge-row' }, [
                el('span', { className: st.badgeClass, textContent: st.label }),
              ]),
            ]),
            guidance ? el('p', { className: 'muted', textContent: guidance }) : null,
            el('dl', { className: 'meta-list' }, metaItems),
            actions.length > 0 ? el('div', { className: 'btn-row' }, actions) : null,
          ]);
        })
      ),
      restoreForm,
    ]);
  }

  const REVISIONS_PAGE_SIZE = 20;

  async function loadWorkspaceRevisions(id) {
    state.loadingRevisions = true;
    state.revisionsError = null;
    state.revisions = null;
    state.hasMoreRevisions = false;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(id)}/revisions?limit=${REVISIONS_PAGE_SIZE}`,
        { headers: { Accept: 'application/json' }, credentials: 'same-origin' }
      );
      if (resp.status === 401) {
        state.me = null;
        state.loadingRevisions = false;
        renderShell();
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const list = await resp.json();
      state.revisions = list;
      state.hasMoreRevisions = list.length === REVISIONS_PAGE_SIZE;
      state.loadingRevisions = false;
      renderShell();
    } catch (_err) {
      state.loadingRevisions = false;
      state.revisionsError = 'Could not load revision history. Please try again.';
      renderShell();
    }
  }

  async function loadMoreRevisions(id) {
    if (state.loadingMoreRevisions || !state.revisions || state.revisions.length === 0) return;
    const oldest = state.revisions[state.revisions.length - 1];
    state.loadingMoreRevisions = true;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(id)}/revisions?before=${encodeURIComponent(oldest.revision)}&limit=${REVISIONS_PAGE_SIZE}`,
        { headers: { Accept: 'application/json' }, credentials: 'same-origin' }
      );
      if (resp.status === 401) {
        state.me = null;
        state.loadingMoreRevisions = false;
        renderShell();
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const older = await resp.json();
      state.revisions = [...state.revisions, ...older];
      state.hasMoreRevisions = older.length === REVISIONS_PAGE_SIZE;
      state.loadingMoreRevisions = false;
      renderShell();
    } catch (_err) {
      state.loadingMoreRevisions = false;
      state.revisionsError = 'Could not load older revisions. Please try again.';
      renderShell();
    }
  }

  async function toggleRevisionDiff(wsId, rev) {
    if (state.selectedDiffRev === rev) {
      state.selectedDiffRev = null;
      state.diffError = null;
      renderShell();
      return;
    }
    state.selectedDiffRev = rev;
    state.diffError = null;
    if (state.diffsByRev[rev]) {
      renderShell();
      return;
    }
    state.loadingDiffRev = rev;
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(wsId)}/revisions/${encodeURIComponent(rev)}/diff`,
        { headers: { Accept: 'application/json' }, credentials: 'same-origin' }
      );
      if (resp.status === 401) {
        state.me = null;
        state.loadingDiffRev = null;
        renderShell();
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      state.diffsByRev[rev] = await resp.json();
      state.loadingDiffRev = null;
      renderShell();
    } catch (_err) {
      state.loadingDiffRev = null;
      state.diffError = 'Could not load diff for this revision. Please try again.';
      renderShell();
    }
  }

  function renderDiffFiles(diff) {
    if (!diff.files || diff.files.length === 0) {
      return el('p', { className: 'muted', textContent: 'No file changes in this commit.' });
    }
    return el(
      'div',
      { className: 'diff-list stack-sm' },
      diff.files.map((file) => {
        const changeBadge =
          file.change === 'deleted'
            ? 'badge badge-warn'
            : file.change === 'added'
              ? 'badge badge-trialing'
              : 'badge badge-neutral';
        return el('div', { className: 'diff-file stack-sm' }, [
          el('div', { className: 'row-between' }, [
            el('span', { className: 'mono', textContent: file.path }),
            el('div', { className: 'badge-row' }, [
              el('span', { className: changeBadge, textContent: file.change }),
              file.is_vault
                ? el('span', { className: 'badge badge-warn', textContent: 'Encrypted (.age)' })
                : null,
            ]),
          ]),
          file.is_vault
            ? el('p', {
                className: 'muted mono',
                textContent: file.patch || '[ciphertext changed]',
              })
            : el('pre', { className: 'diff-patch mono' }, [
                el('code', { textContent: file.patch || 'No text diff.' }),
              ]),
        ]);
      })
    );
  }

  function formatBytes(bytes) {
    const n = Number(bytes || 0);
    if (n <= 0) return '0 B';
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1).replace(/\.0$/, '')} KiB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1).replace(/\.0$/, '')} MiB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(1).replace(/\.0$/, '')} GiB`;
  }

  function formatPlanName(plan) {
    switch (plan) {
      case 'personal':
        return 'Personal';
      case 'team':
        return 'Team';
      case 'trial':
      default:
        return 'Trial';
    }
  }

  function canManageBilling(ws) {
    return Boolean(ws && (ws.role === 'owner' || ws.role === 'admin'));
  }

  function hasCheckoutReturnQuery() {
    const params = new URLSearchParams(window.location.search);
    return params.has('checkout') || params.has('_ptxn') || params.has('ptxn');
  }

  async function loadWorkspaceBilling(id) {
    const ctx = captureBillingContext(id);
    if (!ctx.accountId) return;
    state.loadingBilling = true;
    state.billingError = null;
    if (!state.billing || state.billing.workspace_id !== id) {
      state.billing = null;
    }
    renderShell();
    try {
      const resp = await fetch(
        `/v1/workspaces/${encodeURIComponent(id)}/billing`,
        { headers: { Accept: 'application/json' }, credentials: 'same-origin' }
      );
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        resetBillingState();
        renderShell();
        return;
      }
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const billing = await resp.json();
      if (!isCurrentBillingContext(ctx)) return;
      state.billing = billing;
      state.loadingBilling = false;
      if (billing.plan !== 'trial' && billing.plan_status !== 'canceled') {
        state.checkoutSession = null;
        if (!state.confirmingPlanChange && !state.updatePlanInFlight) {
          state.updatePlanChoice = billing.plan === 'team' ? 'team' : 'personal';
          state.updateSeats = String(billing.seats || 1);
        }
        if (billing.cancel_at_period_end && !state.cancelInFlight) {
          state.confirmingCancel = false;
        }
      } else {
        if (!state.portalInFlight) {
          state.portalUrl = null;
        }
        if (!state.updatePlanInFlight) {
          state.confirmingPlanChange = null;
        }
        if (!state.cancelInFlight) {
          state.confirmingCancel = false;
        }
      }
      if (
        billing.billing_configured &&
        hasCheckoutReturnQuery() &&
        (billing.plan === 'trial' || billing.plan_status === 'canceled')
      ) {
        state.billingNotice =
          'Returned from checkout. Subscription rights activate only after server-side payment verification — use Refresh billing status once payment finishes.';
      }
      renderShell();
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.loadingBilling = false;
      state.billingError = 'Could not load workspace billing details. Please try again.';
      renderShell();
    }
  }

  async function handleStartCheckout(e, wsId) {
    e.preventDefault();
    if (state.checkoutInFlight || !state.me) return;
    const ctx = captureBillingContext(wsId);
    if (!isCurrentBillingContext(ctx)) return;

    const plan = state.checkoutPlan === 'team' ? 'team' : 'personal';
    let seats = 1;
    if (plan === 'team') {
      const rawSeats = String(state.checkoutSeats || '').trim();
      const parsed = Number.parseInt(rawSeats, 10);
      if (!/^\d+$/.test(rawSeats) || Number.isNaN(parsed) || parsed < 1) {
        state.checkoutFieldError = 'Enter a whole number of seats (1 or more).';
        renderShell();
        const input = document.getElementById('billing-checkout-seats');
        if (input) input.focus();
        return;
      }
      seats = parsed;
    }

    state.checkoutFieldError = null;
    state.billingActionError = null;
    state.checkoutInFlight = true;
    renderShell();

    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(wsId)}/billing/checkout`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: JSON.stringify({ plan, seats }),
      });
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        resetBillingState();
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!isCurrentBillingContext(ctx)) return;
      if (!resp.ok) {
        state.billingActionError =
          (data && data.error && data.error.message) ||
          'Could not create a checkout session. Please try again.';
        state.checkoutInFlight = false;
        renderShell();
        return;
      }
      state.checkoutSession = {
        checkout_url: data.checkout_url,
        transaction_id: data.transaction_id,
        plan,
        seats,
      };
      state.billingNotice = null;
      state.checkoutInFlight = false;
      renderShell();
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.checkoutInFlight = false;
      state.billingActionError =
        'Connection failed while creating the checkout session. Please retry.';
      renderShell();
    }
  }

  async function handleOpenPortal(wsId) {
    if (state.portalInFlight || !state.me) return;
    const ctx = captureBillingContext(wsId);
    if (!isCurrentBillingContext(ctx)) return;
    state.portalInFlight = true;
    state.billingActionError = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(wsId)}/billing/portal`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: '{}',
      });
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        resetBillingState();
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!isCurrentBillingContext(ctx)) return;
      if (!resp.ok) {
        state.billingActionError =
          (data && data.error && data.error.message) ||
          'Could not open the billing portal. Please try again.';
        state.portalInFlight = false;
        renderShell();
        return;
      }
      state.portalUrl = data.portal_url;
      state.portalInFlight = false;
      renderShell();
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.portalInFlight = false;
      state.billingActionError =
        'Connection failed while requesting a billing portal session. Please retry.';
      renderShell();
    }
  }

  function handleReviewPlanChange(e, b) {
    e.preventDefault();
    if (state.updatePlanInFlight) return;
    const plan = state.updatePlanChoice === 'team' ? 'team' : 'personal';
    let seats = 1;
    if (plan === 'team') {
      const rawSeats = String(state.updateSeats || '').trim();
      const parsed = Number.parseInt(rawSeats, 10);
      if (!/^\d+$/.test(rawSeats) || Number.isNaN(parsed) || parsed < 1) {
        state.updatePlanFieldError = 'Enter a whole number of seats (1 or more).';
        state.confirmingPlanChange = null;
        renderShell();
        const input = document.getElementById('billing-update-seats');
        if (input) input.focus();
        return;
      }
      seats = parsed;
    }
    if (plan === b.plan && seats === b.seats) {
      state.updatePlanFieldError = 'Select a different plan or seat count to update.';
      state.confirmingPlanChange = null;
      renderShell();
      return;
    }
    state.updatePlanFieldError = null;
    state.billingActionError = null;
    state.confirmingPlanChange = { plan, seats };
    renderShell();
  }

  async function handleConfirmPlanChange(wsId) {
    if (!state.confirmingPlanChange || state.updatePlanInFlight || !state.me) return;
    const ctx = captureBillingContext(wsId);
    if (!isCurrentBillingContext(ctx)) return;
    const { plan, seats } = state.confirmingPlanChange;
    state.updatePlanInFlight = true;
    state.billingActionError = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(wsId)}/billing/plan`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: JSON.stringify({ plan, seats }),
      });
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        resetBillingState();
        renderShell();
        return;
      }
      if (!resp.ok) {
        const data = await resp.json().catch(() => null);
        if (!isCurrentBillingContext(ctx)) return;
        state.billingActionError =
          (data && data.error && data.error.message) ||
          'Could not update the subscription plan. Please try again.';
        state.updatePlanInFlight = false;
        renderShell();
        return;
      }
      state.updatePlanInFlight = false;
      state.confirmingPlanChange = null;
      state.billingNotice = 'Subscription updated cleanly with Paddle.';
      await loadWorkspaceBilling(wsId);
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.updatePlanInFlight = false;
      state.billingActionError =
        'Connection failed while updating the subscription plan. Please retry.';
      renderShell();
    }
  }

  async function handleConfirmCancel(wsId) {
    if (!state.confirmingCancel || state.cancelInFlight || !state.me) return;
    const ctx = captureBillingContext(wsId);
    if (!isCurrentBillingContext(ctx)) return;
    state.cancelInFlight = true;
    state.billingActionError = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(wsId)}/billing/cancel`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
        body: '{}',
      });
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        resetBillingState();
        renderShell();
        return;
      }
      if (!resp.ok) {
        const data = await resp.json().catch(() => null);
        if (!isCurrentBillingContext(ctx)) return;
        state.billingActionError =
          (data && data.error && data.error.message) ||
          'Could not cancel the subscription. Please try again.';
        state.cancelInFlight = false;
        renderShell();
        return;
      }
      state.cancelInFlight = false;
      state.confirmingCancel = false;
      state.billingNotice =
        'Subscription cancellation scheduled for the end of the current billing period.';
      await loadWorkspaceBilling(wsId);
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.cancelInFlight = false;
      state.billingActionError =
        'Connection failed while canceling the subscription. Please retry.';
      renderShell();
    }
  }

  function renderBillingSection(ws) {
    const heading = el('h2', { id: 'billing-heading', textContent: 'Billing' });

    if (!state.billing) {
      if (state.billingError) {
        return el('section', { className: 'card stack-md', 'aria-labelledby': 'billing-heading' }, [
          heading,
          el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.billingError }),
            el('button', {
              type: 'button',
              className: 'btn btn-secondary btn-sm',
              onClick: () => loadWorkspaceBilling(ws.id),
              textContent: 'Retry',
            }),
          ]),
        ]);
      }
      return el(
        'section',
        { className: 'card stack-sm', 'aria-labelledby': 'billing-heading', 'aria-live': 'polite' },
        [heading, el('p', { className: 'muted', textContent: 'Loading billing details…' })]
      );
    }

    const b = state.billing;
    const statusInfo = formatPlanStatus(b.plan_status);
    const planLabel =
      b.plan === 'team'
        ? `${formatPlanName(b.plan)} (${b.seats} ${b.seats === 1 ? 'seat' : 'seats'})`
        : formatPlanName(b.plan);
    const badges = [el('span', { className: statusInfo.badgeClass, textContent: statusInfo.label })];
    if (b.cancel_at_period_end) {
      badges.push(el('span', { className: 'badge badge-warn', textContent: 'Cancels at period end' }));
    }
    if (b.billing_configured) {
      badges.push(
        el('button', {
          type: 'button',
          className: 'btn btn-secondary btn-sm',
          disabled: state.loadingBilling,
          onClick: () => loadWorkspaceBilling(ws.id),
          textContent: state.loadingBilling ? 'Refreshing…' : 'Refresh billing status',
        })
      );
    }

    const metaItems = [
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Plan' }),
        el('dd', { textContent: planLabel }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Status' }),
        el('dd', { textContent: statusInfo.label }),
      ]),
    ];
    if (b.trial_ends_at) {
      metaItems.push(
        el('div', { className: 'meta-item' }, [
          el('dt', { textContent: 'Trial ends' }),
          el('dd', { textContent: formatDate(b.trial_ends_at) }),
        ])
      );
    }
    if (b.current_period_end) {
      metaItems.push(
        el('div', { className: 'meta-item' }, [
          el('dt', { textContent: b.cancel_at_period_end ? 'Access ends' : 'Current period ends' }),
          el('dd', { textContent: formatDate(b.current_period_end) }),
        ])
      );
    }
    metaItems.push(
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Push' }),
        el('dd', { textContent: b.can_push ? 'Allowed' : 'Blocked' }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Pull' }),
        el('dd', { textContent: b.can_pull ? 'Allowed' : 'Unavailable' }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Export' }),
        el('dd', { textContent: b.can_export ? 'Available' : 'Unavailable' }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Machines limit' }),
        el('dd', {
          textContent: Array.isArray(state.devices)
            ? `${state.devices.filter((d) => d.status !== 'revoked').length} / ${b.max_devices}`
            : `${b.max_devices} max`,
        }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Max bundle size' }),
        el('dd', { textContent: formatBytes(b.max_bundle_bytes) }),
      ]),
      el('div', { className: 'meta-item' }, [
        el('dt', { textContent: 'Storage used' }),
        el('dd', {
          textContent: `${formatBytes(b.storage_used_bytes)} of ${formatBytes(b.max_storage_bytes)}`,
        }),
      ])
    );

    const children = [
      el('div', { className: 'row-between' }, [
        heading,
        el('div', { className: 'badge-row' }, badges),
      ]),
      el('dl', { className: 'meta-list' }, metaItems),
    ];

    if (state.billingError) {
      children.push(
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.billingError }),
        ])
      );
    }

    if (state.billingActionError) {
      children.push(
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.billingActionError }),
        ])
      );
    }

    if (state.billingNotice) {
      children.push(
        el('div', { className: 'machine-card', role: 'status' }, [
          el('p', { className: 'muted', textContent: state.billingNotice }),
        ])
      );
    }

    if (!b.billing_configured) {
      children.push(
        el('p', {
          className: 'muted',
          textContent:
            'Billing is not configured on this server. Your workspace keeps its trial status, and pull and Git bundle export remain available.',
        })
      );
    } else if (canManageBilling(ws) && (b.plan === 'trial' || b.plan_status === 'canceled')) {
      const formFields = [
        el('div', { className: 'field' }, [
          el('label', { for: 'billing-checkout-plan', textContent: 'Plan' }),
          el(
            'select',
            {
              id: 'billing-checkout-plan',
              className: 'input',
              value: state.checkoutPlan,
              onChange: (ev) => {
                state.checkoutPlan = ev.target.value;
                state.checkoutFieldError = null;
                renderShell();
              },
            },
            [
              el('option', {
                value: 'personal',
                selected: state.checkoutPlan === 'personal',
                textContent: 'Personal — 10 machines, 25 MiB bundle, 10 GiB storage',
              }),
              el('option', {
                value: 'team',
                selected: state.checkoutPlan === 'team',
                textContent: 'Team — 50 machines & 100 GiB storage per seat',
              }),
            ]
          ),
        ]),
      ];

      if (state.checkoutPlan === 'team') {
        formFields.push(
          el('div', { className: 'field' }, [
            el('label', { for: 'billing-checkout-seats', textContent: 'Seats' }),
            el('input', {
              id: 'billing-checkout-seats',
              className: 'input',
              type: 'number',
              min: '1',
              step: '1',
              value: state.checkoutSeats,
              'aria-invalid': state.checkoutFieldError ? 'true' : null,
              'aria-describedby': state.checkoutFieldError ? 'billing-checkout-seats-err' : null,
              onInput: (ev) => {
                state.checkoutSeats = ev.target.value;
              },
            }),
            state.checkoutFieldError
              ? el('span', {
                  id: 'billing-checkout-seats-err',
                  className: 'field-error',
                  role: 'alert',
                  textContent: state.checkoutFieldError,
                })
              : null,
          ])
        );
      }

      children.push(
        el('form', {
          className: 'invite-controls stack-sm',
          noValidate: true,
          onSubmit: (ev) => handleStartCheckout(ev, ws.id),
        }, [
          el('h3', { textContent: 'Subscribe or upgrade' }),
          el('p', {
            className: 'muted',
            textContent:
              'Checkout opens Paddle for payment. Workspace rights activate only after server-side payment verification.',
          }),
          el('div', { className: 'invite-grid' }, formFields),
          el('div', { className: 'btn-row' }, [
            el('button', {
              type: 'submit',
              className: 'btn btn-primary',
              disabled: state.checkoutInFlight,
              textContent: state.checkoutInFlight ? 'Starting checkout…' : 'Continue to checkout',
            }),
          ]),
        ])
      );

      if (state.checkoutSession) {
        const s = state.checkoutSession;
        const planSummary =
          s.plan === 'team' ? `Team (${s.seats} ${s.seats === 1 ? 'seat' : 'seats'})` : 'Personal';
        children.push(
          el('div', { className: 'share-box stack-sm', role: 'status' }, [
            el('p', {
              textContent: `Checkout session ready for ${planSummary}. Complete payment at Paddle, then refresh billing status to confirm activation.`,
            }),
            el('div', { className: 'btn-row' }, [
              el('a', {
                className: 'btn btn-primary',
                href: s.checkout_url,
                target: '_blank',
                rel: 'noopener noreferrer',
                textContent: 'Open Paddle checkout',
              }),
            ]),
          ])
        );
      }
    } else if (canManageBilling(ws)) {
      const planOptions = [
        el('div', { className: 'field' }, [
          el('label', { for: 'billing-update-plan', textContent: 'Change plan' }),
          el(
            'select',
            {
              id: 'billing-update-plan',
              className: 'input',
              value: state.updatePlanChoice,
              onChange: (ev) => {
                state.updatePlanChoice = ev.target.value;
                state.updatePlanFieldError = null;
                state.confirmingPlanChange = null;
                renderShell();
              },
            },
            [
              el('option', {
                value: 'personal',
                selected: state.updatePlanChoice === 'personal',
                textContent: 'Personal — 10 machines, 25 MiB bundle, 10 GiB storage',
              }),
              el('option', {
                value: 'team',
                selected: state.updatePlanChoice === 'team',
                textContent: 'Team — 50 machines & 100 GiB storage per seat',
              }),
            ]
          ),
        ]),
      ];

      if (state.updatePlanChoice === 'team') {
        planOptions.push(
          el('div', { className: 'field' }, [
            el('label', { for: 'billing-update-seats', textContent: 'Update seats' }),
            el('input', {
              id: 'billing-update-seats',
              className: 'input',
              type: 'number',
              min: '1',
              step: '1',
              value: state.updateSeats,
              'aria-invalid': state.updatePlanFieldError ? 'true' : null,
              'aria-describedby': state.updatePlanFieldError ? 'billing-update-seats-err' : null,
              onInput: (ev) => {
                state.updateSeats = ev.target.value;
                state.confirmingPlanChange = null;
              },
            }),
          ])
        );
      }

      const planFormChildren = [
        el('h3', { textContent: 'Manage subscription' }),
        el('div', { className: 'btn-row' }, [
          el('button', {
            type: 'button',
            className: 'btn btn-secondary',
            disabled: state.portalInFlight,
            onClick: () => handleOpenPortal(ws.id),
            textContent: state.portalInFlight ? 'Opening billing portal…' : 'Open billing portal',
          }),
        ]),
      ];

      if (state.portalUrl) {
        planFormChildren.push(
          el('div', { className: 'share-box stack-sm', role: 'status' }, [
            el('p', { textContent: 'Billing portal session ready.' }),
            el('div', { className: 'btn-row' }, [
              el('a', {
                className: 'btn btn-secondary',
                href: state.portalUrl,
                target: '_blank',
                rel: 'noopener noreferrer',
                textContent: 'Continue to billing portal',
              }),
            ]),
          ])
        );
      }

      planFormChildren.push(
        el('div', { className: 'invite-grid' }, planOptions),
        state.updatePlanFieldError
          ? el('p', {
              id: 'billing-update-seats-err',
              className: 'field-error',
              role: 'alert',
              textContent: state.updatePlanFieldError,
            })
          : null,
        el('div', { className: 'btn-row' }, [
          el('button', {
            type: 'submit',
            className: 'btn btn-secondary',
            disabled: state.updatePlanInFlight,
            textContent: 'Review plan change',
          }),
        ])
      );

      if (state.confirmingPlanChange) {
        const next = state.confirmingPlanChange;
        const nextLabel =
          next.plan === 'team'
            ? `Team (${next.seats} ${next.seats === 1 ? 'seat' : 'seats'})`
            : 'Personal';
        planFormChildren.push(
          el(
            'div',
            {
              className: 'share-box stack-sm',
              role: 'region',
              'aria-label': 'Confirm plan change',
            },
            [
              el('p', {
                textContent: `Confirm changing subscription from ${planLabel} to ${nextLabel}. Paddle applies immediate proration.`,
              }),
              el('div', { className: 'btn-row' }, [
                el('button', {
                  type: 'button',
                  className: 'btn btn-primary',
                  disabled: state.updatePlanInFlight,
                  onClick: () => handleConfirmPlanChange(ws.id),
                  textContent: state.updatePlanInFlight ? 'Updating plan…' : 'Confirm plan change',
                }),
                el('button', {
                  type: 'button',
                  className: 'btn btn-secondary',
                  disabled: state.updatePlanInFlight,
                  onClick: () => {
                    state.confirmingPlanChange = null;
                    renderShell();
                  },
                  textContent: 'Keep current plan',
                }),
              ]),
            ]
          )
        );
      }

      if (b.cancel_at_period_end) {
        planFormChildren.push(
          el('p', {
            className: 'muted',
            textContent:
              'Cancellation is scheduled for the end of the current billing period. Pull and Git bundle export remain available after cancellation.',
          })
        );
      } else if (state.confirmingCancel) {
        planFormChildren.push(
          el(
            'div',
            {
              className: 'share-box stack-sm',
              role: 'region',
              'aria-label': 'Confirm subscription cancellation',
            },
            [
              el('p', {
                textContent:
                  'Cancel subscription at the end of the current billing period? Push will be blocked after the period ends; pull and Git bundle export remain available.',
              }),
              el('div', { className: 'btn-row' }, [
                el('button', {
                  type: 'button',
                  className: 'btn btn-danger',
                  disabled: state.cancelInFlight,
                  onClick: () => handleConfirmCancel(ws.id),
                  textContent: state.cancelInFlight ? 'Canceling…' : 'Confirm cancellation',
                }),
                el('button', {
                  type: 'button',
                  className: 'btn btn-secondary',
                  disabled: state.cancelInFlight,
                  onClick: () => {
                    state.confirmingCancel = false;
                    renderShell();
                  },
                  textContent: 'Keep subscription',
                }),
              ]),
            ]
          )
        );
      } else {
        planFormChildren.push(
          el('div', { className: 'btn-row' }, [
            el('button', {
              type: 'button',
              className: 'btn btn-danger',
              onClick: () => {
                state.confirmingCancel = true;
                state.billingActionError = null;
                renderShell();
              },
              textContent: 'Cancel subscription',
            }),
          ])
        );
      }

      children.push(
        el(
          'form',
          {
            className: 'invite-controls stack-md',
            noValidate: true,
            onSubmit: (ev) => handleReviewPlanChange(ev, b),
          },
          planFormChildren
        )
      );
    }

    return el('section', { className: 'card stack-md', 'aria-labelledby': 'billing-heading' }, children);
  }

  function renderExportSection(ws) {
    const heading = el('h2', {
      id: 'export-heading',
      textContent: 'Export & offline recovery',
    });
    const note = el('p', {
      className: 'muted',
      textContent:
        'Download the full accepted Git history as a portable bundle. Available to any workspace member with no Cloud subscription required.',
    });

    if (state.loadingRevisions) {
      return el(
        'section',
        {
          className: 'card stack-sm',
          'aria-labelledby': 'export-heading',
          'aria-live': 'polite',
        },
        [
          heading,
          note,
          el('p', { className: 'muted', textContent: 'Checking export availability…' }),
        ]
      );
    }

    if (state.revisionsError) {
      return el(
        'section',
        { className: 'card stack-md', 'aria-labelledby': 'export-heading' },
        [
          heading,
          note,
          el('p', {
            className: 'muted',
            textContent: 'Could not verify export status while history failed to load.',
          }),
        ]
      );
    }

    const revisions = state.revisions || [];
    if (revisions.length === 0) {
      return el(
        'section',
        { className: 'card stack-sm', 'aria-labelledby': 'export-heading' },
        [
          heading,
          note,
          el('p', {
            textContent:
              'No accepted history to export yet. Push your first fleet commit with the CLI before exporting a bundle.',
          }),
        ]
      );
    }

    const head = revisions[0];
    const ossCommands = [
      'git clone -b main workspace.bundle muster-config',
      `muster sync --pull --expect-signer ${head.signer}`,
    ].join('\n');

    return el(
      'section',
      { className: 'card stack-md', 'aria-labelledby': 'export-heading' },
      [
        heading,
        note,
        el('dl', { className: 'meta-list' }, [
          el('div', { className: 'meta-item' }, [
            el('dt', { textContent: 'Head revision' }),
            el('dd', { className: 'mono', textContent: head.revision.slice(0, 8) }),
          ]),
          el('div', { className: 'meta-item' }, [
            el('dt', { textContent: 'Epoch' }),
            el('dd', { textContent: String(head.epoch) }),
          ]),
          el('div', { className: 'meta-item meta-item-wide' }, [
            el('dt', { textContent: 'Trusted signer' }),
            el('dd', { className: 'mono', textContent: head.signer }),
          ]),
        ]),
        el('div', { className: 'btn-row' }, [
          el('a', {
            href: `/v1/workspaces/${encodeURIComponent(ws.id)}/export`,
            download: 'workspace.bundle',
            className: 'btn btn-primary btn-sm',
            textContent: 'Download Git bundle',
          }),
        ]),
        el('div', { className: 'stack-sm' }, [
          el('p', {
            className: 'muted',
            textContent:
              'Offline exit with the open-source CLI (unbundle with Git and pin your trusted signer):',
          }),
          el('pre', { className: 'diff-patch mono' }, [
            el('code', { textContent: ossCommands }),
          ]),
        ]),
      ]
    );
  }

  function renderHistorySection(ws) {
    const heading = el('h2', { id: 'history-heading', textContent: 'History' });
    const note = el('p', {
      className: 'muted',
      textContent:
        'Accepted fleet revisions, newest first. Encrypted .age vault files remain redacted on the server.',
    });

    if (state.loadingRevisions) {
      return el(
        'section',
        { className: 'card stack-sm', 'aria-labelledby': 'history-heading', 'aria-live': 'polite' },
        [heading, note, el('p', { className: 'muted', textContent: 'Loading history…' })]
      );
    }

    if (state.revisionsError) {
      return el('section', { className: 'card stack-md', 'aria-labelledby': 'history-heading' }, [
        heading,
        note,
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.revisionsError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: () => loadWorkspaceRevisions(ws.id),
            textContent: 'Retry',
          }),
        ]),
      ]);
    }

    const revisions = state.revisions || [];
    if (revisions.length === 0) {
      return el('section', { className: 'card stack-sm', 'aria-labelledby': 'history-heading' }, [
        heading,
        note,
        el('p', { textContent: 'No revisions synced yet. Push your first fleet commit with the CLI.' }),
      ]);
    }

    return el('section', { className: 'card stack-md', 'aria-labelledby': 'history-heading' }, [
      heading,
      note,
      el(
        'div',
        { className: 'member-list' },
        revisions.map((rev) => {
          const isOpen = state.selectedDiffRev === rev.revision;
          const diff = state.diffsByRev[rev.revision];
          return el('article', { className: 'member-card stack-sm' }, [
            el('div', { className: 'row-between' }, [
              el('strong', { textContent: rev.message }),
              el('div', { className: 'badge-row' }, [
                el('span', { className: 'badge badge-neutral', textContent: `Epoch ${rev.epoch}` }),
                el('span', {
                  className: 'badge badge-neutral mono',
                  textContent: rev.revision.slice(0, 8),
                }),
              ]),
            ]),
            el('div', { className: 'row-between' }, [
              el('dl', { className: 'meta-list' }, [
                el('div', { className: 'meta-item' }, [
                  el('dt', { textContent: 'Accepted' }),
                  el('dd', { textContent: formatDate(rev.accepted_at) }),
                ]),
                el('div', { className: 'meta-item' }, [
                  el('dt', { textContent: 'Signer' }),
                  el('dd', { className: 'mono', textContent: rev.signer }),
                ]),
              ]),
              el('div', { className: 'btn-row' }, [
                el('button', {
                  type: 'button',
                  className: 'btn btn-secondary btn-sm',
                  'aria-expanded': isOpen ? 'true' : 'false',
                  onClick: () => toggleRevisionDiff(ws.id, rev.revision),
                  textContent: isOpen ? 'Hide diff' : 'View diff',
                }),
                canReviewProposals(ws)
                  ? el('button', {
                      type: 'button',
                      className: 'btn btn-secondary btn-sm',
                      onClick: () => selectRestoreTarget(rev.revision),
                      textContent: 'Propose restore',
                    })
                  : null,
              ]),
            ]),
            isOpen
              ? el('div', { className: 'diff-panel stack-sm' }, [
                  state.loadingDiffRev === rev.revision
                    ? el('p', { className: 'muted', textContent: 'Loading diff…' })
                    : state.diffError && !diff
                      ? el('div', { className: 'alert', role: 'alert' }, [
                          el('span', { textContent: state.diffError }),
                        ])
                      : diff
                        ? renderDiffFiles(diff)
                        : null,
                ])
              : null,
          ]);
        })
      ),
      state.hasMoreRevisions
        ? el('div', {}, [
            el('button', {
              type: 'button',
              className: 'btn btn-secondary btn-sm',
              disabled: state.loadingMoreRevisions,
              onClick: () => loadMoreRevisions(ws.id),
              textContent: state.loadingMoreRevisions ? 'Loading…' : 'Load older revisions',
            }),
          ])
        : null,
    ]);
  }

  function renderMachinesSection(ws) {
    if (state.loadingDevices) {
      return el('section', { className: 'card stack-sm', 'aria-live': 'polite' }, [
        el('h2', { textContent: 'Machines' }),
        el('p', { className: 'muted', textContent: 'Loading machines…' }),
      ]);
    }

    if (state.devicesError) {
      return el('section', { className: 'card stack-md' }, [
        el('h2', { textContent: 'Machines' }),
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.devicesError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: () => loadWorkspaceDevices(ws.id),
            textContent: 'Retry',
          }),
        ]),
      ]);
    }

    const devices = state.devices || [];
    if (devices.length === 0) {
      return el('section', { className: 'card stack-sm' }, [
        el('h2', { textContent: 'Machines' }),
        el('p', { textContent: 'No machines linked yet.' }),
        el('p', {
          className: 'muted',
          textContent:
            'Run the CLI login and link command from a machine to connect its transport to this workspace.',
        }),
      ]);
    }

    return el('section', { className: 'card stack-md' }, [
      el('h2', { textContent: 'Machines' }),
      el(
        'div',
        { className: 'machine-list' },
        devices.map((d) => {
          const st = formatDeviceStatus(d.status);
          const badges = [el('span', { className: st.badgeClass, textContent: st.label })];
          if (d.bootstrap) {
            badges.push(el('span', { className: 'badge badge-neutral', textContent: 'Bootstrap claim' }));
          }
          const meta = [
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Linked' }),
              el('dd', { textContent: formatDate(d.created_at) }),
            ]),
          ];
          if (d.last_seen_at) {
            meta.push(
              el('div', { className: 'meta-item' }, [
                el('dt', { textContent: 'Last seen' }),
                el('dd', { textContent: formatDate(d.last_seen_at) }),
              ])
            );
          }
          if (d.last_receipt) {
            meta.push(
              el('div', { className: 'meta-item meta-item-wide' }, [
                el('dt', { textContent: 'Last receipt' }),
                el('dd', {
                  className: 'mono',
                  textContent: `#${d.last_receipt.sequence} ${d.last_receipt.outcome} (${d.last_receipt.revision})`,
                }),
              ])
            );
          }
          meta.push(
            el('div', { className: 'meta-item meta-item-wide' }, [
              el('dt', { textContent: 'Signing key' }),
              el('dd', { className: 'mono', textContent: d.signing_key }),
            ]),
            el('div', { className: 'meta-item meta-item-wide' }, [
              el('dt', { textContent: 'Recipient' }),
              el('dd', { className: 'mono', textContent: d.recipient }),
            ])
          );

          return el('article', { className: 'machine-card stack-sm' }, [
            el('div', { className: 'row-between' }, [
              el('h3', { textContent: d.machine_name }),
              el('div', { className: 'badge-row' }, badges),
            ]),
            d.status !== 'approved'
              ? el('p', {
                  className: 'muted',
                  textContent:
                    d.status === 'revoked'
                      ? 'This machine is no longer authorized by signed fleet governance.'
                      : 'Linked for transport only; not yet approved by signed fleet governance.',
                })
              : null,
            el('dl', { className: 'meta-list' }, meta),
          ]);
        })
      ),
    ]);
  }

  async function handleCreateWorkspace(e) {
    e.preventDefault();
    if (state.createInFlight) return;

    const check = validateWorkspaceName(state.formName);
    if (!check.ok) {
      state.formError = check.error;
      state.submitError = null;
      renderShell();
      const input = document.getElementById('ws-name');
      if (input) input.focus();
      return;
    }

    state.formError = null;
    state.submitError = null;
    const body = JSON.stringify({ name: check.name });
    if (!state.createIdempotencyKey || state.createIntentBody !== body) {
      state.createIdempotencyKey = crypto.randomUUID();
      state.createIntentBody = body;
    }

    state.createInFlight = true;
    renderShell();

    try {
      const resp = await fetch('/v1/workspaces', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
          'Idempotency-Key': state.createIdempotencyKey,
        },
        credentials: 'same-origin',
        body: state.createIntentBody,
      });
      if (resp.status === 401) {
        state.me = null;
        state.createInFlight = false;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        const msg =
          (data && data.error && data.error.message) ||
          'Could not create the workspace. Please try again.';
        state.submitError = msg;
        state.createInFlight = false;
        renderShell();
        return;
      }

      state.createInFlight = false;
      state.formName = '';
      state.createIdempotencyKey = null;
      state.createIntentBody = null;
      state.workspaces = [...(state.workspaces || []).filter((w) => w.id !== data.id), data];
      state.workspaceEpoch += 1;
      state.activeWorkspace = data;
      state.devices = [];
      state.members = null;
      state.invitations = [];
      state.invitationsError = null;
      state.createdInvite = null;
      state.revisions = [];
      state.revisionsError = null;
      state.proposals = [];
      state.proposalsError = null;
      resetBillingState();
      window.history.pushState({}, '', `/workspaces/${data.id}`);
      renderShell();
      const createLoaders = [
        loadWorkspaceMembers(data.id),
        loadWorkspaceRevisions(data.id),
        loadWorkspaceProposals(data.id),
        loadWorkspaceBilling(data.id),
      ];
      if (canManageInvites(data)) {
        createLoaders.push(loadWorkspaceInvitations(data.id));
      }
      await Promise.all(createLoaders);
    } catch (_err) {
      state.createInFlight = false;
      state.submitError = 'Connection failed while creating the workspace. Click Create workspace to retry safely.';
      renderShell();
    }
  }

  function renderCreateForm() {
    const hasFieldError = Boolean(state.formError);
    return el('form', { className: 'stack-md', noValidate: true, onSubmit: handleCreateWorkspace }, [
      state.submitError
        ? el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.submitError }),
          ])
        : null,
      el('div', { className: 'field' }, [
        el('label', { for: 'ws-name', textContent: 'Workspace name' }),
        el('input', {
          id: 'ws-name',
          name: 'name',
          type: 'text',
          className: 'input',
          maxLength: '64',
          autocomplete: 'off',
          placeholder: 'e.g. Acme Fleet',
          value: state.formName,
          'aria-invalid': hasFieldError ? 'true' : null,
          'aria-describedby': hasFieldError ? 'ws-name-error' : null,
          onInput: (e) => {
            state.formName = e.target.value;
            state.formError = null;
            state.submitError = null;
          },
        }),
        hasFieldError
          ? el('p', {
              id: 'ws-name-error',
              className: 'field-error',
              role: 'alert',
              textContent: state.formError,
            })
          : null,
      ]),
      el('div', {}, [
        el('button', {
          type: 'submit',
          className: 'btn btn-primary',
          disabled: state.createInFlight,
          textContent: state.createInFlight ? 'Creating…' : 'Create workspace',
        }),
      ]),
    ]);
  }

  async function handleAcceptInvitation(token) {
    if (state.acceptInFlight || !state.me) return;
    state.acceptInFlight = true;
    state.acceptError = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/invitations/${encodeURIComponent(token)}/accept`, {
        method: 'POST',
        headers: {
          Accept: 'application/json',
          'X-CSRF-Token': state.me.csrf_token,
        },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.acceptInFlight = false;
        renderShell();
        return;
      }
      const data = await resp.json().catch(() => null);
      if (!resp.ok) {
        state.acceptInFlight = false;
        if (resp.status === 404) {
          state.acceptError = 'This invitation link is invalid, expired, or has already been used.';
        } else if (resp.status === 403 && data && data.error && data.error.code === 'invitation_mismatch') {
          state.acceptError =
            `${data.error.message}. Sign out and sign in with the verified email address that was invited.`;
        } else {
          state.acceptError =
            (data && data.error && data.error.message) ||
            'Could not accept this invitation. Please try again.';
        }
        renderShell();
        return;
      }
      state.acceptInFlight = false;
      state.acceptResult = data;
      state.workspaces = null;
      renderShell();
    } catch (_err) {
      state.acceptInFlight = false;
      state.acceptError = 'Connection failed while accepting the invitation. Please try again.';
      renderShell();
    }
  }

  function renderInvitationAccept(token) {
    if (state.acceptResult) {
      const wsId = state.acceptResult.workspace_id;
      const role = state.acceptResult.role;
      return el('section', { className: 'card hero-card stack-md' }, [
        el('div', { className: 'row-between' }, [
          el('h1', { textContent: 'Invitation accepted' }),
          el('span', { className: formatRoleBadge(role), textContent: role }),
        ]),
        el('p', {
          textContent: `You joined this workspace as ${role}.`,
        }),
        el('p', {
          className: 'muted',
          textContent:
            'Cloud roles control console access only. They do not grant fleet signing keys or signed fleet approval.',
        }),
        el('div', { className: 'btn-row' }, [
          el('a', {
            href: `/workspaces/${encodeURIComponent(wsId)}`,
            className: 'btn btn-primary',
            onClick: (e) => {
              e.preventDefault();
              navigateToWorkspace(wsId);
            },
            textContent: 'Open workspace',
          }),
          el('a', {
            href: '/',
            className: 'btn btn-secondary',
            onClick: (e) => {
              e.preventDefault();
              navigateToList();
            },
            textContent: 'All workspaces',
          }),
        ]),
      ]);
    }

    return el('section', { className: 'card hero-card stack-md' }, [
      el('div', { className: 'stack-sm' }, [
        el('h1', { textContent: 'Accept workspace invitation' }),
        el('p', {
          className: 'muted',
          textContent: `Signed in as ${state.me.email}. Accepting grants your account a Cloud role in this workspace; it does not share fleet signing keys or approve a machine in the signed fleet.`,
        }),
      ]),
      state.acceptError
        ? el('div', { className: 'alert', role: 'alert' }, [
            el('span', { textContent: state.acceptError }),
          ])
        : null,
      el('div', { className: 'btn-row' }, [
        el('button', {
          type: 'button',
          className: 'btn btn-primary',
          disabled: state.acceptInFlight,
          onClick: () => handleAcceptInvitation(token),
          textContent: state.acceptInFlight ? 'Accepting…' : 'Accept invitation',
        }),
        el('a', {
          href: '/',
          className: 'btn btn-secondary',
          onClick: (e) => {
            e.preventDefault();
            navigateToList();
          },
          textContent: 'All workspaces',
        }),
      ]),
    ]);
  }

  function renderSignedIn() {
    if (isDeviceRoute()) {
      return renderDeviceAuth();
    }
    const inviteToken = parseInvitationTokenFromPath();
    if (inviteToken) {
      return renderInvitationAccept(inviteToken);
    }

    if (state.signOutError) {
      return el('section', { className: 'stack-md' }, [
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.signOutError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: handleSignOut,
            textContent: 'Retry sign out',
          }),
        ]),
      ]);
    }

    if (state.loadingDetail) {
      return el('section', { className: 'card stack-sm', 'aria-live': 'polite' }, [
        el('h1', { textContent: 'Workspace' }),
        el('p', { className: 'muted', textContent: 'Loading workspace details…' }),
      ]);
    }

    if (state.detailError) {
      return el('section', { className: 'card stack-md' }, [
        el('h1', { textContent: state.detailError.title }),
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.detailError.message }),
          el('div', { className: 'topbar-actions' }, [
            state.detailError.retryId
              ? el('button', {
                  type: 'button',
                  className: 'btn btn-secondary btn-sm',
                  onClick: () => loadWorkspaceDetail(state.detailError.retryId),
                  textContent: 'Retry',
                })
              : null,
            el('a', {
              href: '/',
              className: 'btn btn-primary btn-sm',
              onClick: (e) => {
                e.preventDefault();
                navigateToList();
              },
              textContent: 'All workspaces',
            }),
          ]),
        ]),
      ]);
    }

    if (state.activeWorkspace) {
      const ws = state.activeWorkspace;
      const plan = formatPlanStatus(ws.plan_status);
      return el('div', { className: 'stack-lg' }, [
        el('div', {}, [
          el('a', {
            href: '/',
            className: 'btn btn-secondary btn-sm',
            onClick: (e) => {
              e.preventDefault();
              navigateToList();
            },
            textContent: 'All workspaces',
          }),
        ]),
        el('section', { className: 'card stack-md' }, [
          el('div', { className: 'row-between' }, [
            el('h1', { textContent: ws.name }),
            el('span', { className: plan.badgeClass, textContent: plan.label }),
          ]),
          el('dl', { className: 'meta-list' }, [
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Your role' }),
              el('dd', { textContent: ws.role }),
            ]),
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Plan status' }),
              el('dd', { textContent: plan.label }),
            ]),
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Trial ends' }),
              el('dd', { textContent: formatDate(ws.trial_ends_at) }),
            ]),
            el('div', { className: 'meta-item' }, [
              el('dt', { textContent: 'Created' }),
              el('dd', { textContent: formatDate(ws.created_at) }),
            ]),
            el('div', { className: 'meta-item meta-item-wide' }, [
              el('dt', { textContent: 'Workspace ID' }),
              el('dd', { className: 'mono', textContent: ws.id }),
            ]),
          ]),
        ]),
        renderMachinesSection(ws),
        renderHistorySection(ws),
        renderProposalsSection(ws),
        renderExportSection(ws),
        renderMembersSection(ws),
        renderBillingSection(ws),
      ]);
    }

    if (state.loadingWorkspaces) {
      return el('section', { className: 'card stack-sm', 'aria-live': 'polite' }, [
        el('h1', { textContent: 'Workspaces' }),
        el('p', { className: 'muted', textContent: 'Loading workspaces…' }),
      ]);
    }

    if (state.loadError) {
      return el('section', { className: 'stack-md' }, [
        el('h1', { textContent: 'Workspaces' }),
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: state.loadError }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: loadWorkspaces,
            textContent: 'Retry',
          }),
        ]),
      ]);
    }

    const list = state.workspaces || [];
    if (list.length === 0) {
      return el('section', { className: 'card stack-md' }, [
        el('div', { className: 'stack-sm' }, [
          el('h1', { textContent: 'No workspaces yet' }),
          el('p', {
            className: 'muted',
            textContent:
              'Create your first workspace to host your fleet configuration. Every new workspace starts with a 14-day free trial, no card required.',
          }),
        ]),
        renderCreateForm(),
      ]);
    }

    return el('div', { className: 'stack-lg' }, [
      el('section', { className: 'stack-md' }, [
        el('h1', { textContent: 'Workspaces' }),
        el(
          'div',
          { className: 'workspace-grid' },
          list.map((w) => {
            const plan = formatPlanStatus(w.plan_status);
            return el(
              'a',
              {
                href: `/workspaces/${encodeURIComponent(w.id)}`,
                className: 'card workspace-card',
                onClick: (e) => {
                  e.preventDefault();
                  navigateToWorkspace(w.id);
                },
              },
              [
                el('div', { className: 'row-between' }, [
                  el('h2', { textContent: w.name }),
                  el('span', { className: plan.badgeClass, textContent: plan.label }),
                ]),
                el('div', { className: 'row-between muted' }, [
                  el('span', { textContent: `Role: ${w.role}` }),
                  el('span', { textContent: `Trial ends ${formatDate(w.trial_ends_at)}` }),
                ]),
              ]
            );
          })
        ),
      ]),
      el('section', { className: 'card stack-md' }, [
        el('h2', { textContent: 'Create another workspace' }),
        renderCreateForm(),
      ]),
    ]);
  }

  async function loadWorkspaceInvitations(id) {
    state.loadingInvitations = true;
    state.invitationsError = null;
    state.invitations = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}/invitations`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.loadingInvitations = false;
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      state.invitations = await resp.json();
      state.loadingInvitations = false;
      renderShell();
    } catch (_err) {
      state.loadingInvitations = false;
      state.invitationsError = 'Could not load invitations. Please try again.';
      renderShell();
    }
  }

  async function loadWorkspaceMembers(id) {
    state.loadingMembers = true;
    state.membersError = null;
    state.members = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}/members`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.loadingMembers = false;
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      state.members = await resp.json();
      state.loadingMembers = false;
      renderShell();
    } catch (_err) {
      state.loadingMembers = false;
      state.membersError = 'Could not load workspace members. Please try again.';
      renderShell();
    }
  }

  async function loadWorkspaceDevices(id) {
    const ctx = captureBillingContext(id);
    if (!ctx.accountId) return;
    state.loadingDevices = true;
    state.devicesError = null;
    state.devices = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}/devices`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (!isCurrentBillingContext(ctx)) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        state.loadingDevices = false;
        resetBillingState();
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      const devices = await resp.json();
      if (!isCurrentBillingContext(ctx)) return;
      state.devices = devices;
      state.loadingDevices = false;
      renderShell();
    } catch (_err) {
      if (!isCurrentBillingContext(ctx)) return;
      state.loadingDevices = false;
      state.devicesError = 'Could not load linked machines. Please try again.';
      renderShell();
    }
  }

  async function loadWorkspaceDetail(id) {
    state.loadingDetail = true;
    state.detailError = null;
    state.activeWorkspace = null;
    state.devices = null;
    state.devicesError = null;
    state.members = null;
    state.membersError = null;
    state.memberActionError = null;
    state.removingMemberId = null;
    state.invitations = null;
    state.invitationsError = null;
    state.createdInvite = null;
    state.inviteEmail = '';
    state.inviteRole = 'member';
    state.inviteFieldError = null;
    state.inviteSubmitError = null;
    state.revisions = null;
    state.loadingRevisions = false;
    state.revisionsError = null;
    state.loadingMoreRevisions = false;
    state.hasMoreRevisions = false;
    state.selectedDiffRev = null;
    state.diffsByRev = {};
    state.loadingDiffRev = null;
    state.diffError = null;
    state.proposals = null;
    state.loadingProposals = false;
    state.proposalsError = null;
    state.proposalActionError = null;
    state.rejectingProposalId = null;
    state.restoreTargetRev = '';
    state.restorePathsText = '';
    state.restoreFieldError = null;
    state.restoreSubmitError = null;
    state.restoreInFlight = false;
    state.restoreCreatedMessage = null;
    state.workspaceEpoch += 1;
    const epoch = state.workspaceEpoch;
    const accountId = state.me ? state.me.id : null;
    resetBillingState();
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (state.workspaceEpoch !== epoch || (state.me ? state.me.id : null) !== accountId) return;
      if (resp.status === 401) {
        state.workspaceEpoch += 1;
        state.me = null;
        state.loadingDetail = false;
        resetBillingState();
        renderShell();
        return;
      }
      if (resp.status === 404) {
        state.loadingDetail = false;
        state.detailError = {
          title: 'Workspace not found',
          message: 'This workspace does not exist or you do not have access to it.',
          retryId: null,
        };
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      const ws = await resp.json();
      if (state.workspaceEpoch !== epoch || (state.me ? state.me.id : null) !== accountId) return;
      state.activeWorkspace = ws;
      state.loadingDetail = false;
      renderShell();
      const loaders = [loadWorkspaceDevices(id), loadWorkspaceMembers(id), loadWorkspaceRevisions(id), loadWorkspaceProposals(id), loadWorkspaceBilling(id)];
      if (canManageInvites(state.activeWorkspace)) {
        loaders.push(loadWorkspaceInvitations(id));
      }
      await Promise.all(loaders);
    } catch (_err) {
      if (state.workspaceEpoch !== epoch || (state.me ? state.me.id : null) !== accountId) return;
      state.loadingDetail = false;
      state.detailError = {
        title: 'Could not load workspace',
        message: 'Could not load workspace details. Please try again.',
        retryId: id,
      };
      renderShell();
    }
  }

  async function loadWorkspaces() {
    state.loadingWorkspaces = true;
    state.loadError = null;
    renderShell();
    try {
      const resp = await fetch('/v1/workspaces', {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.workspaces = null;
        state.loadingWorkspaces = false;
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      state.workspaces = await resp.json();
      state.loadingWorkspaces = false;
      renderShell();
    } catch (_err) {
      state.loadingWorkspaces = false;
      state.loadError = 'Could not load your workspaces. Please try again.';
      renderShell();
    }
  }

  function renderShell(contentNode) {
    const app = document.getElementById('app');
    const main = el(
      'main',
      { id: 'main', className: 'main-container', tabindex: '-1' },
      [contentNode || (state.me ? renderSignedIn() : renderSignedOut())]
    );
    app.replaceChildren(renderHeader(), main);
  }

  async function init() {
    applyTheme(localStorage.getItem(THEME_STORAGE_KEY));
    syncDeviceCodeFromUrl();
    try {
      const resp = await fetch('/v1/me', {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`Unexpected status ${resp.status}`);
      }
      state.me = await resp.json();
      const wsId = parseWorkspaceIdFromPath(window.location.pathname);
      if (isDeviceRoute() || parseInvitationTokenFromPath()) {
        renderShell();
      } else if (wsId) {
        await loadWorkspaceDetail(wsId);
      } else {
        await loadWorkspaces();
      }
    } catch (_err) {
      renderShell(
        el('div', { className: 'alert', role: 'alert' }, [
          el('span', { textContent: 'Could not reach Muster Cloud. Please try again.' }),
          el('button', {
            type: 'button',
            className: 'btn btn-secondary btn-sm',
            onClick: init,
            textContent: 'Retry',
          }),
        ])
      );
    }
  }

  window.addEventListener('popstate', () => {
    syncDeviceCodeFromUrl();
    if (!state.me) return;
    const wsId = parseWorkspaceIdFromPath(window.location.pathname);
    state.acceptInFlight = false;
    state.acceptError = null;
    state.acceptResult = null;
    if (isDeviceRoute() || parseInvitationTokenFromPath()) {
      state.workspaceEpoch += 1;
      state.activeWorkspace = null;
      state.detailError = null;
      resetBillingState();
      renderShell();
    } else if (wsId) {
      loadWorkspaceDetail(wsId);
    } else {
      state.workspaceEpoch += 1;
      state.activeWorkspace = null;
      state.detailError = null;
      resetBillingState();
      if (!state.workspaces) {
        loadWorkspaces();
      } else {
        renderShell();
      }
    }
  });

  window.addEventListener('DOMContentLoaded', init);
})();
