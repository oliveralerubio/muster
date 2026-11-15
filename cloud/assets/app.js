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
    signOutInFlight: false,
    signOutError: null,
  };

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
      state.activeWorkspace = data;
      state.devices = [];
      state.members = null;
      state.invitations = [];
      state.invitationsError = null;
      state.createdInvite = null;
      window.history.pushState({}, '', `/workspaces/${data.id}`);
      renderShell();
      await loadWorkspaceMembers(data.id);
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
        renderMembersSection(ws),
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
    state.loadingDevices = true;
    state.devicesError = null;
    state.devices = null;
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}/devices`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.loadingDevices = false;
        renderShell();
        return;
      }
      if (!resp.ok) {
        throw new Error(`HTTP ${resp.status}`);
      }
      state.devices = await resp.json();
      state.loadingDevices = false;
      renderShell();
    } catch (_err) {
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
    renderShell();
    try {
      const resp = await fetch(`/v1/workspaces/${encodeURIComponent(id)}`, {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      });
      if (resp.status === 401) {
        state.me = null;
        state.loadingDetail = false;
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
      state.activeWorkspace = await resp.json();
      state.loadingDetail = false;
      renderShell();
      const loaders = [loadWorkspaceDevices(id), loadWorkspaceMembers(id)];
      if (canManageInvites(state.activeWorkspace)) {
        loaders.push(loadWorkspaceInvitations(id));
      }
      await Promise.all(loaders);
    } catch (_err) {
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
      state.activeWorkspace = null;
      state.detailError = null;
      renderShell();
    } else if (wsId) {
      loadWorkspaceDetail(wsId);
    } else {
      state.activeWorkspace = null;
      state.detailError = null;
      if (!state.workspaces) {
        loadWorkspaces();
      } else {
        renderShell();
      }
    }
  });

  window.addEventListener('DOMContentLoaded', init);
})();
