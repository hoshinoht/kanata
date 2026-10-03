(() => {
  'use strict';
  const $ = (id) => document.getElementById(id);
  let data = null, selected = null, pending = null, busy = false, session = null;
  const text = (tag, value, className) => { const node = document.createElement(tag); node.textContent = value; if (className) node.className = className; return node; };
  const format = (value) => value == null ? 'Unavailable' : Number(value).toLocaleString();
  const date = (value) => value ? new Date(value).toLocaleString() : 'Never';
  function notice(message, error = false) { $('notice').textContent = message; $('notice').className = error ? 'error' : ''; $('notice').hidden = !message; }
  function showLocked() { session = null; data = null; selected = null; $('dashboard').hidden = true; $('login').hidden = false; $('lock').hidden = true; $('keys').replaceChildren(); $('key-form').reset(); $('secret-value').value = ''; if ($('secret-dialog').open) $('secret-dialog').close(); }
  async function api(path, body = {}) {
    const response = await fetch(`/api/${path}`, { method: 'POST', credentials: 'omit', cache: 'no-store', headers: { 'Content-Type': 'application/json', 'X-Kanata-Portal': '1', ...(session ? { Authorization: `Bearer ${session}` } : {}) }, body: JSON.stringify(body) });
    const result = await response.json();
    if (!response.ok) { if (response.status === 401) showLocked(); throw new Error(result.error || 'Portal request failed'); }
    return result;
  }
  async function refresh() {
    data = await api('snapshot');
    $('dashboard').hidden = false; $('login').hidden = true; $('lock').hidden = false;
    const active = data.keys.filter((key) => !key.expired && !key.revoked_at).length;
    const expired = data.keys.filter((key) => key.expired && !key.revoked_at).length;
    const revoked = data.keys.filter((key) => key.revoked_at).length;
    const stats = [['Active keys', active], ['Expired', expired], ['Revoked', revoked], ['Recorded requests', data.usage_configured ? data.keys.reduce((n, key) => n + (key.requests || 0), 0) : null]];
    $('stats').replaceChildren(...stats.map(([label, value]) => { const el = text('div', '', 'stat'); el.append(text('span', label), text('strong', format(value))); return el; }));
    $('reload-note').textContent = data.reload_note; $('usage-note').textContent = data.usage_note;
    renderKeys(); if (selected) select(data.keys.find((key) => key.id === selected));
  }
  function renderKeys() {
    const query = $('filter').value.trim().toLowerCase();
    const keys = data.keys.filter((key) => key.id.toLowerCase().includes(query));
    $('keys').replaceChildren(...keys.map((key) => {
      const button = text('button', '', `key-row${selected === key.id ? ' selected' : ''}`); button.type = 'button';
      const status = key.revoked_at ? 'revoked' : key.expired ? 'expired' : 'active';
      button.append(text('strong', key.id), text('small', `${key.owner ? 'Owner · ' : ''}${key.scopes.length} scope${key.scopes.length === 1 ? '' : 's'} · ${key.public_access ? 'Public + private' : 'Private'}`), text('span', status, `status ${status}`));
      button.addEventListener('click', () => select(key)); return button;
    }));
    if (!keys.length) $('keys').append(text('p', query ? 'No matching keys.' : 'No keys yet. Create one to grant access.', 'small'));
  }
  function scopes(key) {
    const inputs = data.routes.map((route, index) => {
      const input = document.createElement('input'); input.type = 'checkbox'; input.name = 'scope'; input.value = String(index);
      input.checked = !!key?.scopes.some((scope) => scope.model_alias === route.model_alias && scope.operation === route.operation);
      input.disabled = !!key?.revoked_at; input.addEventListener('change', scopeWarning); return input;
    });
    const renderModel = (group) => {
      const badge = text('em', group.exposure === 'never_public' ? 'Never public' : group.exposure === 'public' ? 'Public' : 'Private');
      const label = text('span', '');
      const name = group.levels.length ? group.alias.replace(/^gpt-(\d+(?:\.\d+)?)-(.+)$/i, (_, version, model) => `GPT-${version}-${model.replace(/(^|-)[a-z]/g, (part) => part.toUpperCase())}`) : group.alias;
      label.append(text('b', name), text('small', group.operation));
      if (!group.levels.length) {
        const row = text('label', '', 'check scope'); row.append(inputs[group.indices[0]], label, badge); return row;
      }
      const row = text('div', '', 'scope scope-group'); const heading = text('div', '', 'scope-heading'); heading.append(label, badge); row.append(heading);
      const levels = document.createElement('fieldset'); levels.className = 'effort-levels'; levels.append(text('legend', 'Allowed reasoning levels'));
      for (const level of group.levels) {
        const choice = text('label', '', 'check effort-choice'); const visible = document.createElement('input'); visible.type = 'checkbox'; visible.name = 'effort';
        visible.checked = level.indices.some((index) => inputs[index].checked); visible.disabled = !!key?.revoked_at;
        visible.setAttribute('aria-label', `${name} ${level.effort} reasoning`);
        visible.addEventListener('change', () => { for (const index of level.indices) inputs[index].checked = visible.checked; scopeWarning(); });
        choice.append(visible, text('span', level.effort)); levels.append(choice);
      }
      for (const index of group.indices) { inputs[index].hidden = true; row.append(inputs[index]); }
      row.append(levels); return row;
    };
    $('scopes').replaceChildren(...providerGroups(data.routes).map((provider) => {
      const section = text('section', '', 'provider-group'); section.setAttribute('aria-label', `${provider.label} routes`);
      const heading = text('div', '', 'provider-heading');
      const modelCount = new Set(provider.models.map((model) => model.alias)).size;
      heading.append(text('h3', provider.label), text('span', `${modelCount} model${modelCount === 1 ? '' : 's'}`));
      section.append(heading);
      if (provider.models.every((model) => model.exposure === 'never_public')) section.append(text('p', 'Private access only', 'provider-note'));
      const models = text('div', '', 'provider-routes'); models.append(...provider.models.map(renderModel)); section.append(models);
      return section;
    }));
    scopeWarning();
  }
  function scopeWarning() {
    const never = [...document.querySelectorAll('input[name=scope]:checked')].some((input) => data.routes[Number(input.value)].exposure === 'never_public');
    $('private-confirm').hidden = !never; $('confirm-private').required = never;
  }
  function select(key = null) {
    selected = key?.id || null; $('key-form').reset(); $('empty').hidden = true; $('key-form').hidden = false;
    $('editor-title').textContent = key ? 'Key details' : 'New application key'; $('key-id').value = key?.id || ''; $('key-id').disabled = !!key;
    $('key-status').textContent = key ? key.revoked_at ? 'REVOKED' : key.expired ? 'EXPIRED' : 'ACTIVE' : 'NEW';
    $('key-details').textContent = key ? `Created ${date(key.created_at)}${key.owner ? ' · Owner key' : ''}. ${key.missing_routes.length ? 'Some saved scopes reference missing routes. Saving replaces them with the selection below.' : ''}` : 'Use a unique name for the app or person receiving this key.';
    $('expiry').querySelector('[value=keep]').hidden = !key; $('expiry').value = key ? 'keep' : '7';
    $('expiry-note').textContent = key ? `Current expiry: ${date(key.expires_at)}` : 'Shorter expiries reduce the lifetime of a misplaced secret.';
    $('max-flight').value = key?.max_in_flight || ''; $('rate-count').value = key?.rate_limit?.requests || ''; $('rate-window').value = key?.rate_limit?.per_ms || '';
    $('quota-action').querySelector('[value=keep]').hidden = !key; $('quota-action').value = key ? 'keep' : 'clear';
    $('quota-action').querySelector('[value=set]').disabled = !data.usage_configured;
    $('quota-action').disabled = !!key?.revoked_at;
    $('quota-availability').textContent = data.usage_configured ? 'Saving other settings keeps the current daily quota unless you change this selection.' : 'Daily quotas need [keys] usage_dir in the gateway configuration.';
    $('daily-requests').value = key?.daily_quota?.requests ?? ''; $('daily-tokens').value = key?.daily_quota?.tokens ?? ''; $('reservation-tokens').value = key?.daily_quota?.reservation_tokens ?? '';
    quotaFields();
    $('owner').checked = !!key?.owner; $('owner-wrap').hidden = !!key; $('save').textContent = key ? 'Save changes' : 'Create key';
    $('save').hidden = !!key?.revoked_at; $('rotate').hidden = !key || !!key.revoked_at; $('revoke').hidden = !key || !!key.revoked_at;
    $('usage').replaceChildren();
    if (key) {
      const items = [['Requests', format(key.requests)], ['Last used', date(key.last_used_at)], ['Input tokens', format(key.tokens?.input_tokens)], ['Output tokens', format(key.tokens?.output_tokens)], ['Reported attempts', format(key.tokens?.reported)], ['Missing token usage', format(key.tokens?.missing)]];
      if (key.daily_quota) items.push(['Daily request allowance', key.daily_quota.requests == null ? 'Not set' : format(key.daily_quota.requests)], ['Daily token allowance', key.daily_quota.tokens == null ? 'Not set' : format(key.daily_quota.tokens)], ['Tokens reserved per request', key.daily_quota.reservation_tokens == null ? 'Not set' : format(key.daily_quota.reservation_tokens)]);
      for (const [label, value] of items) { const el = text('div', `${label}: `); el.append(text('b', value)); $('usage').append(el); }
    }
    scopes(key); renderKeys();
  }
  function limits() {
    const max = $('max-flight').value, requests = $('rate-count').value, window = $('rate-window').value;
    if (!!requests !== !!window) throw new Error('Set both the rate limit request count and its window, or leave both empty.');
    return { max_in_flight: max ? Number(max) : null, rate_limit: requests ? { requests: Number(requests), per_ms: Number(window) } : null };
  }
  function quotaFields() {
    const editing = $('quota-action').value === 'set';
    $('quota-fields').hidden = !editing;
    for (const id of ['daily-requests', 'daily-tokens', 'reservation-tokens']) $(id).disabled = !editing;
  }
  function quotaChange() {
    const action = $('quota-action').value;
    if (action === 'keep') return {};
    if (action === 'clear') return selected ? { clear_quota: true } : {};
    const number = (id) => {
      if (!$(id).value) return null;
      const value = Number($(id).value);
      if (!Number.isSafeInteger(value) || value <= 0) throw new Error('Daily quota values must be positive whole numbers no greater than 9,007,199,254,740,991.');
      return value;
    };
    const requests = number('daily-requests'), tokens = number('daily-tokens'), reservation_tokens = number('reservation-tokens');
    if (requests == null && tokens == null) throw new Error('Set a daily request or token allowance, or choose No daily quota.');
    if ((tokens == null) !== (reservation_tokens == null)) throw new Error('Set both the daily token allowance and tokens reserved per request.');
    if (tokens != null && reservation_tokens > tokens) throw new Error('The per-request reservation cannot exceed the daily token allowance.');
    return { daily_quota: { requests, tokens, reservation_tokens } };
  }
  async function change(request) {
    if (busy) return; busy = true;
    $('save').disabled = true; $('confirm-action').disabled = true;
    try {
      const result = await api('change', { revision: data.revision, ...request });
      if ($('confirm-dialog').open) $('confirm-dialog').close();
      selected = result.id;
      if (result.secret) {
        $('secret-title').textContent = request.action === 'rotate' ? 'Key rotated' : 'Key created'; $('secret-value').value = result.secret; result.secret = null;
        $('secret-dialog').showModal();
      }
      notice(result.warning || 'Saved. The gateway normally reloads key changes within 2 seconds.', !!result.warning);
      await refresh();
    } catch (error) { notice(error.message, true); }
    finally { busy = false; $('save').disabled = false; $('confirm-action').disabled = $('confirm-id').value !== selected; }
  }
  $('login-form').addEventListener('submit', async (event) => {
    event.preventDefault(); const code = $('code').value.trim(); $('code').value = '';
    try { const result = await api('login', { code }); session = result.session; notice(''); await refresh(); } catch (error) { notice(error.message, true); }
  });
  $('key-form').addEventListener('submit', (event) => {
    event.preventDefault();
    try {
      const request = { action: selected ? 'edit' : 'new', id: $('key-id').value, scopes: [...document.querySelectorAll('input[name=scope]:checked')].map((input) => { const route = data.routes[Number(input.value)]; return { model_alias: route.model_alias, operation: route.operation }; }), limits: limits(), confirm_private: $('confirm-private').checked };
      if ($('expiry').value !== 'keep') request.expires = $('expiry').value;
      if (!selected) request.owner = $('owner').checked;
      Object.assign(request, quotaChange());
      change(request);
    } catch (error) { notice(error.message, true); }
  });
  $('quota-action').addEventListener('change', quotaFields);
  $('filter').addEventListener('input', () => { if (data) renderKeys(); });
  $('refresh').addEventListener('click', () => refresh().catch((error) => notice(error.message, true)));
  $('new').addEventListener('click', () => { notice(''); select(); $('key-id').focus(); });
  $('lock').addEventListener('click', async () => {
    try { await api('logout'); showLocked(); notice('Portal locked. Restart the terminal command to get a new login code.'); } catch (error) { notice(error.message, true); }
  });
  function confirm(action) {
    pending = action; $('confirm-id').value = ''; $('confirm-action').disabled = true;
    $('confirm-title').textContent = action === 'rotate' ? `Rotate ${selected}?` : `Revoke ${selected}?`;
    $('confirm-text').textContent = action === 'rotate' ? 'The old secret will stop working after the gateway reloads. Update every client using this key.' : 'This permanently revokes the key. Its ID cannot be reused. Connected clients will lose access after the gateway reloads.';
    $('rotate-expiry').hidden = action !== 'rotate'; $('rotate-expiry-label').hidden = action !== 'rotate';
    $('confirm-action').textContent = action === 'rotate' ? 'Rotate secret' : 'Revoke key'; $('confirm-dialog').showModal(); $('confirm-id').focus();
  }
  $('rotate').addEventListener('click', () => confirm('rotate')); $('revoke').addEventListener('click', () => confirm('revoke'));
  $('confirm-id').addEventListener('input', () => { $('confirm-action').disabled = busy || $('confirm-id').value !== selected; });
  $('confirm-action').addEventListener('click', () => { const request = { action: pending, id: selected, confirm: $('confirm-id').value }; if (pending === 'rotate') request.expires = $('rotate-expiry').value; change(request); });
  $('cancel-action').addEventListener('click', () => $('confirm-dialog').close());
  $('close-secret').addEventListener('click', () => $('secret-dialog').close());
  $('secret-dialog').addEventListener('close', () => { $('secret-value').value = ''; $('copy-secret').textContent = 'Copy secret'; });
  $('copy-secret').addEventListener('click', async () => { try { await navigator.clipboard.writeText($('secret-value').value); $('copy-secret').textContent = 'Copied'; } catch { $('secret-value').focus(); $('secret-value').select(); $('copy-secret').textContent = 'Select and copy'; } });
  window.addEventListener('pagehide', () => { showLocked(); $('code').value = ''; });
  showLocked();
})();
