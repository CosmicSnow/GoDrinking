/**
 * @golive/dsh-ui — Host half.
 *
 * Exposes three private RPC endpoints over `connection.rpc` route `/golive`:
 * - `chat.delete`    { sessionId }            → archive + detach + rm -rf session dir
 * - `effort.declare` { provider? }            → declare standard reasoningEfforts
 * - `models.list`    {}                       → flat provider/model catalog (fallback seat)
 * - `models.get`     {}                       → current global default (fallback seat)
 * - `models.save`    { provider, model }      → save global default (fallback seat)
 *
 * Result envelope mirrors the community convention:
 *   { ok: true, value } / { ok: false, error: { code, message } }
 */

function str(v) {
  return typeof v === 'string' ? v : v === undefined || v === null ? '' : String(v);
}

function short(s, n) {
  s = str(s);
  return s.length > n ? s.slice(0, n) : s;
}

function ok(value) {
  return { ok: true, value };
}

function fail(code, message) {
  return { ok: false, error: { code, message: String(message) } };
}

function pidOf(p) {
  if (typeof p === 'string') return p;
  if (p && typeof p === 'object') return str(p.id || p.provider || p.name || p.label);
  return str(p);
}

function midOf(m) {
  if (typeof m === 'string') return m;
  if (m && typeof m === 'object') return str(m.id || m.model || m.name || m.label);
  return str(m);
}

function deepCopy(v) {
  if (v === null || v === undefined) return v;
  if (typeof v !== 'object') return v;
  if (v instanceof Array) {
    const o = [];
    for (let i = 0; i < v.length; i++) o.push(deepCopy(v[i]));
    return o;
  }
  const o = {};
  const ks = Object.keys(v);
  for (let i = 0; i < ks.length; i++) o[ks[i]] = deepCopy(v[ks[i]]);
  return o;
}

function stdLevels() {
  return { minimal: 'minimal', low: 'low', medium: 'medium', high: 'high' };
}

async function chatDelete(ctx, args) {
  const a = args && typeof args === 'object' ? args : {};
  const sid = str(a.sessionId);
  if (!sid) return fail('missing-sessionId', 'missing sessionId');
  if (sid.indexOf('/') >= 0 || sid.indexOf('\\') >= 0 || sid === '.' || sid === '..') {
    return fail('bad-sessionId', 'bad sessionId');
  }
  const workspace = ctx.get('workspaceRegistry');
  const persistence = ctx.get('sessionPersistence');
  const shell = ctx.get('shell');
  try {
    if (workspace !== undefined) {
      try {
        await workspace.archiveSession(sid);
      } catch (e) {
        console.error('golive chat.delete pre-archive failed for ' + sid + ': ' + short(e && e.message ? e.message : e, 200));
      }
      try {
        const list = workspace.list();
        const tables = list && list.tables ? list.tables : list && list.workspaces ? list.workspaces : [];
        for (let i = 0; i < tables.length; i++) {
          const t = tables[i];
          if (t && typeof t.detachSession === 'function') {
            try {
              await t.detachSession(sid);
            } catch (e) {
              console.error('golive chat.delete detach failed for ' + sid + ': ' + short(e && e.message ? e.message : e, 200));
            }
          }
        }
      } catch (e) {
        console.error('golive chat.delete detach scan failed: ' + short(e && e.message ? e.message : e, 200));
      }
    }
  } catch (e) {
    console.error('golive chat.delete archive phase failed for ' + sid + ': ' + short(e && e.message ? e.message : e, 200));
  }
  let dir = null;
  try {
    if (persistence !== undefined && typeof persistence.locate === 'function') {
      const got = persistence.locate({ sessionId: sid });
      const p = got && got.path ? String(got.path) : String(got || '');
      if (p && p.indexOf('/sessions/') >= 0) dir = p;
    }
  } catch (e) {
    console.error('golive chat.delete locate failed for ' + sid + ': ' + short(e && e.message ? e.message : e, 200));
  }
  if (dir === null) return fail('session-dir-not-found', 'session dir not found');
  const last = dir
    .split('/')
    .filter(function (s) {
      return s.length > 0;
    })
    .pop();
  if (last !== sid) return fail('dir-mismatch', 'refusing: dir mismatch ' + last);
  try {
    if (shell === undefined || typeof shell.run !== 'function') return fail('no-shell-service', 'no shell service');
    const spec =
      typeof shell.resolve === 'function' ? shell.resolve({ command: ['rm', '-rf', dir] }) : { command: ['rm', '-rf', dir] };
    const res = await shell.run(spec);
    const code = res && typeof res.exitCode === 'number' ? res.exitCode : -1;
    if (code !== 0) return fail('rm-failed', 'rm exit ' + code + ': ' + short(res && (res.stderr || res.stdout), 200));
    return ok({ removed: dir });
  } catch (e) {
    return fail('internal', short(e && e.message ? e.message : e, 300));
  }
}

async function effortDeclare(ctx, args) {
  const a = args && typeof args === 'object' ? args : {};
  const wantProvider = typeof a.provider === 'string' && a.provider ? a.provider : null;
  const settings = ctx.get('settings');
  const adm = ctx.get('agentDefaultModel');
  if (settings === undefined) return fail('no-settings-service', 'no settings service');
  let section = null;
  try {
    section = settings.get('llm-pi-ai');
  } catch (e) {
    return fail('settings-get', short(e && e.message ? e.message : e, 200));
  }
  if (!section || typeof section !== 'object' || !section.providers || typeof section.providers !== 'object') {
    return fail('no-llm-pi-ai-providers', 'no llm-pi-ai providers');
  }
  let target = wantProvider;
  if (target === null && adm !== undefined) {
    try {
      const sel = adm.currentSelection();
      if (sel && typeof sel === 'object' && typeof sel.provider === 'string') target = sel.provider;
    } catch (e) {}
  }
  if (target === null) return fail('no-provider', 'no provider');
  const entry = section.providers[target];
  if (!entry || typeof entry !== 'object' || !(entry.models instanceof Array)) {
    return fail('provider-not-found', 'provider not found: ' + target);
  }
  const patched = [];
  const next = deepCopy(section);
  const models = next.providers[target].models;
  for (let i = 0; i < models.length; i++) {
    const m = models[i];
    if (!m || typeof m !== 'object' || typeof m.id !== 'string') continue;
    if (m.reasoningEfforts === undefined) {
      m.reasoningEfforts = stdLevels();
      patched.push(m.id);
    }
  }
  if (patched.length === 0) return ok({ patched: [], provider: target, note: 'already-declared' });
  try {
    await settings.replace('llm-pi-ai', next);
  } catch (e) {
    return fail('settings-replace', short(e && e.message ? e.message : e, 200));
  }
  return ok({ patched, provider: target });
}

async function modelsList(ctx) {
  try {
    const llm = ctx.get('llm');
    if (llm === undefined) return ok({ models: [] });
    const providers = llm.listProviders();
    const out = [];
    for (let i = 0; i < providers.length; i++) {
      const pid = pidOf(providers[i]);
      if (!pid) continue;
      let list = [];
      try {
        list = await llm.listModels(pid);
      } catch (e) {
        continue;
      }
      for (let j = 0; j < list.length; j++) {
        const mid = midOf(list[j]);
        if (!mid) continue;
        out.push({ provider: pid, model: mid, label: pid + '/' + mid });
      }
    }
    out.sort(function (a, b) {
      return a.label < b.label ? -1 : a.label > b.label ? 1 : 0;
    });
    return ok({ models: out });
  } catch (e) {
    return ok({ models: [] });
  }
}

async function modelsGet(ctx) {
  try {
    const adm = ctx.get('agentDefaultModel');
    if (adm === undefined) return ok({ current: null });
    const sel = adm.currentSelection();
    if (!sel || typeof sel !== 'object') return ok({ current: null });
    const provider = str(sel.provider);
    const model = str(sel.model);
    if (!provider || !model) return ok({ current: null });
    return ok({ current: { provider, model, label: provider + '/' + model } });
  } catch (e) {
    return ok({ current: null });
  }
}

async function modelsSave(ctx, args) {
  try {
    const adm = ctx.get('agentDefaultModel');
    if (adm === undefined) return fail('no-default-model-service', 'no default model service');
    const a = args && typeof args === 'object' ? args : {};
    const provider = str(a.provider);
    const model = str(a.model);
    if (!provider || !model) return fail('missing-selection', 'missing selection');
    await adm.saveSelection({ provider, model });
    return ok({ saved: true });
  } catch (e) {
    return fail('internal', short(e && e.message ? e.message : e, 300));
  }
}

export function apply(ctx) {
  async function handle(endpoint, payload) {
    try {
      if (endpoint === 'chat.delete') return await chatDelete(ctx, payload);
      if (endpoint === 'effort.declare') return await effortDeclare(ctx, payload);
      if (endpoint === 'models.list') return await modelsList(ctx);
      if (endpoint === 'models.get') return await modelsGet(ctx);
      if (endpoint === 'models.save') return await modelsSave(ctx, payload);
      return fail('bad-request', 'unknown endpoint: ' + str(endpoint));
    } catch (e) {
      return fail('internal', short(e && e.message ? e.message : e, 300));
    }
  }
  ctx.inject(['connection'], (c) => c.connection.rpc.handle('/golive', handle, { authority: 'loopback' }));
}
