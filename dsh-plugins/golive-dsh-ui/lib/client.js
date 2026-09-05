/* global document, window, localStorage */
window.__ModuleLoader__.load({
  id: '@golive/dsh-ui',
  factory: (require) => {
    const module = { exports: {} };
    const React = require('react');

    const DOC = typeof document !== 'undefined' ? document : null;
    const LS = (() => {
      try {
        if (typeof localStorage !== 'undefined') return localStorage;
      } catch (e) {}
      return null;
    })();

    const CSS = [
      '.nsel-wrap { position: relative; display: inline-block; }',
      '.nsel-trigger { display: inline-flex; align-items: center; gap: 7px; max-width: 260px; padding: 4px 10px; border: 1px solid var(--dsw-alias-border-l1); border-radius: 8px; background: var(--dsw-alias-bg-layer-1); color: var(--dsw-alias-label-primary); font-size: 12px; cursor: pointer; white-space: nowrap; }',
      '.nsel-trigger:hover:not(:disabled) { border-color: var(--dsw-alias-border-l2); background: var(--dsw-alias-bg-layer-2); }',
      '.nsel-trigger:disabled { opacity: 0.55; cursor: default; }',
      '.nsel-tlabel { overflow: hidden; text-overflow: ellipsis; }',
      '.nsel-teffort { color: var(--dsw-alias-label-secondary); overflow: hidden; text-overflow: ellipsis; }',
      '.nsel-chev { opacity: 0.6; }',
      '.nsel-menu { position: absolute; bottom: calc(100% + 8px); right: 0; width: 320px; max-height: 400px; display: flex; flex-direction: column; border: 1px solid var(--dsw-alias-border-l1); border-radius: 12px; background: var(--dsw-alias-bg-overlay); box-shadow: 0 12px 32px rgba(0,0,0,0.25); z-index: 50; overflow: hidden; padding: 6px; }',
      '.nsel-cell { display: flex; align-items: center; gap: 8px; width: 100%; text-align: left; padding: 9px 10px; border: 0; border-radius: 8px; background: transparent; color: var(--dsw-alias-label-primary); font-size: 12.5px; cursor: pointer; }',
      '.nsel-cell:hover { background: var(--dsw-alias-bg-layer-2); }',
      '.nsel-celllabel { flex: none; color: var(--dsw-alias-label-secondary); }',
      '.nsel-cellvalue { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; text-align: right; }',
      '.nsel-cellchev { flex: none; opacity: 0.5; }',
      '.nsel-search { padding: 2px 2px 6px 2px; }',
      '.nsel-input { width: 100%; box-sizing: border-box; padding: 7px 10px; border: 1px solid var(--dsw-alias-border-l1); border-radius: 8px; background: var(--dsw-alias-bg-base); color: var(--dsw-alias-label-primary); font-size: 13px; outline: none; }',
      '.nsel-input:focus { border-color: var(--dsw-alias-brand-primary); }',
      '.nsel-back { display: flex; align-items: center; gap: 6px; width: 100%; text-align: left; padding: 7px 10px; border: 0; border-radius: 8px; background: transparent; color: var(--dsw-alias-label-secondary); font-size: 12px; cursor: pointer; }',
      '.nsel-back:hover { background: var(--dsw-alias-bg-layer-2); color: var(--dsw-alias-label-primary); }',
      '.nsel-scroll { overflow-y: auto; display: flex; flex-direction: column; gap: 2px; }',
      '.nsel-grouptitle { padding: 8px 10px 3px 10px; font-size: 11px; font-weight: 600; letter-spacing: 0.04em; text-transform: uppercase; color: var(--dsw-alias-label-secondary); }',
      '.nsel-opt { display: flex; align-items: center; gap: 8px; width: 100%; text-align: left; padding: 7px 10px; border: 0; border-radius: 8px; background: transparent; color: var(--dsw-alias-label-primary); font-size: 12.5px; cursor: pointer; }',
      '.nsel-opt:hover:not(:disabled) { background: var(--dsw-alias-bg-layer-2); }',
      '.nsel-opt.selected { background: var(--dsw-alias-bg-layer-2); }',
      '.nsel-opt:disabled { opacity: 0.55; cursor: default; }',
      '.nsel-copy { flex: 1; min-width: 0; display: flex; flex-direction: column; align-items: flex-start; gap: 1px; }',
      '.nsel-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 100%; }',
      '.nsel-desc { font-size: 11px; color: var(--dsw-alias-label-secondary); }',
      '.nsel-check { flex: none; width: 16px; color: var(--dsw-alias-brand-primary); font-size: 14px; }',
      '.nsel-fav { flex: none; width: 28px; border: 0; border-radius: 7px; background: transparent; color: var(--dsw-alias-label-secondary); font-size: 14px; line-height: 1; cursor: pointer; padding: 2px 0; }',
      '.nsel-fav:hover { background: var(--dsw-alias-bg-layer-2); color: var(--dsw-alias-brand-primary); }',
      '.nsel-fav.on { color: var(--dsw-alias-brand-primary); }',
      '.nsel-row { display: flex; align-items: stretch; gap: 2px; }',
      '.nsel-row .nsel-opt { flex: 1; min-width: 0; }',
      '.nsel-status { padding: 12px; font-size: 12.5px; color: var(--dsw-alias-label-secondary); text-align: center; }',
      '.nsel-error { margin: 2px 2px 6px 2px; padding: 8px 10px; font-size: 12px; color: var(--dsw-alias-state-error-primary); border: 1px solid var(--dsw-alias-state-error-primary); border-radius: 8px; display: flex; align-items: center; gap: 8px; }',
      '.nsel-retry { flex: none; padding: 4px 10px; border: 1px solid var(--dsw-alias-border-l1); border-radius: 7px; background: transparent; color: var(--dsw-alias-label-primary); font-size: 12px; cursor: pointer; }',
      '.nsel-toast { margin: 2px 2px 4px 2px; padding: 8px 10px; font-size: 12px; color: var(--dsw-alias-state-warning-primary); border: 1px solid var(--dsw-alias-state-warning-primary); border-radius: 8px; }',
      '.nsel-why { padding: 8px 10px; font-size: 11.5px; line-height: 1.6; color: var(--dsw-alias-label-secondary); }',
      '.nsel-declare { margin: 2px auto 6px auto; padding: 7px 12px; border: 1px solid var(--dsw-alias-brand-primary); border-radius: 8px; background: transparent; color: var(--dsw-alias-brand-primary); font-size: 12.5px; font-weight: 600; cursor: pointer; }',
      '.nsel-declare:hover:not(:disabled) { background: var(--dsw-alias-bg-layer-2); }',
      '.nsel-declare:disabled { opacity: 0.55; cursor: default; }',
      '.del-act { display: inline-flex; }',
      '.del-btn { display: inline-flex; align-items: center; gap: 6px; padding: 4px 10px; border: 1px solid var(--dsw-alias-border-l1); border-radius: 8px; background: transparent; color: var(--dsw-alias-label-secondary); font-size: 12px; cursor: pointer; white-space: nowrap; }',
      '.del-btn:hover:not(:disabled) { border-color: var(--dsw-alias-state-error-primary); color: var(--dsw-alias-state-error-primary); }',
      '.del-btn:disabled { opacity: 0.5; cursor: default; }',
      '.del-btn svg { display: block; flex: none; }',
      '.del-ovl { position: fixed; inset: 0; display: flex; align-items: center; justify-content: center; background: rgba(0,0,0,0.45); z-index: 200; }',
      '.del-modal { width: 340px; max-width: calc(100vw - 48px); border: 1px solid var(--dsw-alias-border-l1); border-radius: 14px; background: var(--dsw-alias-bg-overlay); box-shadow: 0 20px 48px rgba(0,0,0,0.35); padding: 18px; }',
      '.del-mtitle { display: flex; align-items: center; gap: 9px; font-size: 14px; font-weight: 650; color: var(--dsw-alias-label-primary); margin-bottom: 10px; }',
      '.del-micon { flex: none; width: 30px; height: 30px; border-radius: 9px; display: flex; align-items: center; justify-content: center; background: var(--dsw-alias-state-error-secondary); color: var(--dsw-alias-state-error-primary); }',
      '.del-mbody { font-size: 12.5px; line-height: 1.55; color: var(--dsw-alias-label-secondary); margin-bottom: 6px; }',
      '.del-mbody strong { color: var(--dsw-alias-label-primary); font-weight: 600; }',
      '.del-mname { font-size: 12.5px; color: var(--dsw-alias-label-primary); background: var(--dsw-alias-bg-layer-2); border-radius: 8px; padding: 7px 10px; margin: 8px 0 14px 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }',
      '.del-mrow { display: flex; gap: 8px; justify-content: flex-end; }',
      '.del-cancel { padding: 7px 14px; border: 1px solid var(--dsw-alias-border-l1); border-radius: 9px; background: transparent; color: var(--dsw-alias-label-primary); font-size: 12.5px; cursor: pointer; }',
      '.del-cancel:hover { background: var(--dsw-alias-bg-layer-2); }',
      '.del-confirm { padding: 7px 14px; border: 0; border-radius: 9px; background: var(--dsw-alias-state-error-primary); color: #fff; font-size: 12.5px; font-weight: 600; cursor: pointer; }',
      '.del-confirm:hover:not(:disabled) { filter: brightness(1.08); }',
      '.del-confirm:disabled { opacity: 0.6; cursor: default; }',
      '.del-merr { margin-top: 10px; font-size: 12px; color: var(--dsw-alias-state-error-primary); }',
    ].join('\n');

    function norm(s) {
      return String(s === undefined || s === null ? '' : s).toLowerCase();
    }
    function keyOf(provider, model) {
      return provider + '/' + model;
    }
    function errStr(e) {
      return String((e && e.message ? e.message : e) === undefined ? e : e && e.message ? e.message : e);
    }
    function scoreEntry(provider, model, name, q) {
      const m = norm(model);
      const p = norm(provider);
      const n = norm(name);
      if (!q) return { keep: true, rank: 0, idx: 0, len: n.length };
      let idx = m.indexOf(q);
      let rank = -1;
      if (m.indexOf(q) === 0) rank = 0;
      else if (p.indexOf(q) === 0) rank = 1;
      else if (idx >= 0) rank = 2;
      else if (p.indexOf(q) >= 0) {
        rank = 3;
        idx = p.indexOf(q);
      } else if (n.indexOf(q) >= 0) {
        rank = 4;
        idx = n.indexOf(q);
      } else return { keep: false, rank: 99, idx: 9999, len: n.length };
      return { keep: true, rank: rank, idx: idx, len: n.length };
    }
    function safeSnap(store) {
      try {
        const s = store.getSnapshot();
        return {
          ok: true,
          snap: {
            current: s.current || null,
            groups: s.groups instanceof Array ? s.groups : [],
            failures: s.failures instanceof Array ? s.failures : [],
            status: s.status || 'idle',
            error: s.error || null,
          },
        };
      } catch (e) {
        return { ok: false, error: errStr(e) };
      }
    }

    const FAV_KEY = 'golive.model.favs.v1';
    function favsLoad() {
      const m = {};
      if (!LS) return m;
      try {
        const raw = LS.getItem(FAV_KEY);
        const arr = raw ? JSON.parse(raw) : [];
        if (arr instanceof Array) {
          for (let i = 0; i < arr.length; i++) {
            if (typeof arr[i] === 'string' && arr[i]) m[arr[i]] = true;
          }
        }
      } catch (e) {}
      return m;
    }
    function favsSet(map, provider, model, on) {
      const next = {};
      const keys = Object.keys(map);
      for (let i = 0; i < keys.length; i++) next[keys[i]] = true;
      const k = provider + '/' + model;
      if (on) next[k] = true;
      else delete next[k];
      if (LS) {
        try {
          LS.setItem(FAV_KEY, JSON.stringify(Object.keys(next)));
        } catch (e) {}
      }
      return next;
    }

    const inject = ['slots', 'sessions', 'modelDirectories', 'connection'];

    function apply(ctx) {
      const slots = ctx.get ? ctx.get('slots') : ctx.slots;
      if (slots === undefined) return;

      let styleEl = null;
      if (DOC && DOC.head && typeof DOC.createElement === 'function') {
        try {
          styleEl = DOC.createElement('style');
          styleEl.setAttribute('data-golive', 'ui');
          styleEl.textContent = CSS;
          DOC.head.appendChild(styleEl);
        } catch (e) {
          styleEl = null;
        }
      }
      if (ctx.effect) {
        ctx.effect(
          () => () => {
            try {
              if (styleEl && styleEl.remove) styleEl.remove();
            } catch (e) {}
          },
          'golive-ui: styles'
        );
      }

      function conn() {
        try {
          const c = ctx.connection || (ctx.get ? ctx.get('connection') : undefined);
          if (c && c.rpc && typeof c.rpc.call === 'function') return c;
        } catch (e) {}
        return undefined;
      }
      async function rpc(endpoint, payload) {
        const c = conn();
        if (!c) return { ok: false, error: 'sem canal rpc' };
        try {
          const res = await c.rpc.call('/golive', endpoint, payload || {});
          if (res && res.ok === true) return { ok: true, data: res.value };
          const err = res && res.error ? res.error.message || res.error.code || 'falha' : 'falha';
          return { ok: false, error: String(err) };
        } catch (e) {
          return { ok: false, error: errStr(e) };
        }
      }

      function ownerProps(sessionId) {
        try {
          const models = ctx.get ? ctx.get('modelDirectories') : ctx.modelDirectories;
          const sessions = ctx.get ? ctx.get('sessions') : ctx.sessions;
          if (models === undefined || sessions === undefined) return {};
          if (typeof models.directoryFor !== 'function') return {};
          const directory = models.directoryFor(sessionId);
          if (!directory || !directory.store) return {};
          let available = true;
          try {
            if (typeof sessions.subagentAddress === 'function') available = sessions.subagentAddress(sessionId) === void 0;
          } catch (e) {}
          return {
            available: available,
            directory: directory.store,
            load: function () {
              if (available) {
                try {
                  directory.load().catch(function () {});
                } catch (e) {}
              }
            },
            select: function (selection) {
              return available
                ? directory.select(selection).then(
                    function () {
                      return true;
                    },
                    function () {
                      return false;
                    }
                  )
                : Promise.resolve(false);
            },
          };
        } catch (e) {
          return {};
        }
      }

      function useOutside(rootRef, open, onOut) {
        React.useEffect(
          function () {
            if (!open || DOC === null) return undefined;
            function h(event) {
              const el = rootRef.current;
              if (el && el.contains && !el.contains(event.target)) onOut();
            }
            DOC.addEventListener('mousedown', h);
            return function () {
              DOC.removeEventListener('mousedown', h);
            };
          },
          [open]
        );
      }

      function favStar(provider, modelId, favMap, setFavMap) {
        const isFav = !!favMap[keyOf(provider, modelId)];
        return React.createElement(
          'button',
          {
            className: 'nsel-fav' + (isFav ? ' on' : ''),
            type: 'button',
            title: isFav ? 'Desfavoritar' : 'Favoritar',
            onClick: function (e) {
              if (e && e.stopPropagation) e.stopPropagation();
              setFavMap(favsSet(favMap, provider, modelId, !isFav));
            },
          },
          isFav ? '★' : '☆'
        );
      }

      function StoreSeat(props) {
        const store = props.store;
        const doSelect = props.doSelect;
        const doLoad = props.doLoad;
        const openState = React.useState(false);
        const open = openState[0];
        const setOpen = openState[1];
        const paneState = React.useState('root');
        const pane = paneState[0];
        const setPane = paneState[1];
        const rawState = React.useState('');
        const raw = rawState[0];
        const setRaw = rawState[1];
        const snapState = React.useState(function () {
          return safeSnap(store);
        });
        const snapRes = snapState[0];
        const setSnapRes = snapState[1];
        const favState = React.useState(function () {
          return favsLoad();
        });
        const favMap = favState[0];
        const setFavMap = favState[1];
        const toastState = React.useState(null);
        const toast = toastState[0];
        const setToast = toastState[1];
        const noteState = React.useState(null);
        const note = noteState[0];
        const setNote = noteState[1];
        const busyState = React.useState(false);
        const rpcBusy = busyState[0];
        const setRpcBusy = busyState[1];
        const lastAction = React.useRef('load');
        const rootRef = React.useRef(null);
        const cancelled = React.useRef(false);
        React.useEffect(function () {
          cancelled.current = false;
          try {
            lastAction.current = 'load';
            if (doLoad) doLoad();
          } catch (e) {}
          let unsub = null;
          try {
            setSnapRes(safeSnap(store));
            unsub = store.subscribe(function () {
              if (!cancelled.current) {
                try {
                  setSnapRes(safeSnap(store));
                } catch (e) {}
              }
            });
          } catch (e) {
            setSnapRes({ ok: false, error: errStr(e) });
          }
          return function () {
            cancelled.current = true;
            try {
              if (unsub) unsub();
            } catch (e) {}
          };
        }, []);
        useOutside(rootRef, open, function () {
          setOpen(false);
          setPane('root');
        });
        if (!snapRes.ok) {
          return React.createElement(
            'div',
            { className: 'nsel-wrap', ref: rootRef },
            React.createElement(
              'button',
              {
                className: 'nsel-trigger',
                type: 'button',
                disabled: true,
                title: 'store.getSnapshot falhou: ' + snapRes.error,
              },
              React.createElement('span', { className: 'nsel-tlabel' }, 'Modelo (erro)')
            )
          );
        }
        const state = snapRes.snap;
        const cur = state.current;
        let curModel = null;
        if (cur) {
          for (let gi = 0; gi < state.groups.length && !curModel; gi++) {
            const g = state.groups[gi];
            if (!g || g.id !== cur.provider || !(g.models instanceof Array)) continue;
            for (let mi = 0; mi < g.models.length; mi++) {
              if (g.models[mi] && g.models[mi].id === cur.model) {
                curModel = g.models[mi];
                break;
              }
            }
          }
        }
        const reasoning = curModel && curModel.reasoning && typeof curModel.reasoning === 'object' ? curModel.reasoning : undefined;
        const effectiveEffort = cur ? cur.reasoningEffort || (reasoning && reasoning.defaultEffort) || undefined : undefined;
        function effName(id) {
          if (!reasoning || !(reasoning.efforts instanceof Array)) return id;
          for (let i = 0; i < reasoning.efforts.length; i++) if (reasoning.efforts[i].id === id) return reasoning.efforts[i].name;
          return id;
        }
        const effortLabel = reasoning === undefined ? undefined : effectiveEffort === undefined ? 'Padrão' : effName(effectiveEffort);
        const modelLabel = curModel && curModel.name ? curModel.name : cur ? cur.model : 'Escolher modelo';
        const busy = state.status === 'selecting';
        function show() {
          setPane('root');
          setOpen(true);
          setToast(null);
          setNote(null);
          setRaw('');
          try {
            lastAction.current = 'load';
            if (doLoad) doLoad();
          } catch (e) {}
        }
        function close() {
          setOpen(false);
          setPane('root');
        }
        function fail(m) {
          if (m !== null) setToast(m);
        }
        function choose(sel) {
          if (cur && cur.provider === sel.provider && cur.model === sel.model) {
            close();
            return;
          }
          lastAction.current = 'select';
          let snapErr = '';
          try {
            snapErr = String((store.getSnapshot() || {}).error || '');
          } catch (e) {
            snapErr = errStr(e);
          }
          doSelect(sel).then(
            function (okSel) {
              if (okSel) close();
              else fail(snapErr || 'Seleção recusada — tente de novo.');
            },
            function (e) {
              fail(errStr(e));
            }
          );
        }
        function chooseModel(gid, mid) {
          choose({ provider: gid, model: mid });
        }
        function chooseEffort(effort) {
          if (!cur) return;
          if (effectiveEffort === effort) {
            close();
            return;
          }
          const sel = { provider: cur.provider, model: cur.model };
          if (effort !== undefined) sel.reasoningEffort = effort;
          choose(sel);
        }
        function declareLevels() {
          if (!cur) return;
          setRpcBusy(true);
          setNote(null);
          rpc('effort.declare', { provider: cur.provider }).then(function (res) {
            setRpcBusy(false);
            if (res.ok) {
              const n = res.data && res.data.patched ? res.data.patched.length : 0;
              setNote('Declarados ' + String(n) + ' modelo(s). Recarregando…');
              try {
                lastAction.current = 'load';
                if (doLoad) doLoad();
              } catch (e) {}
            } else {
              setNote('Falhou: ' + res.error);
            }
          });
        }
        function modelRow(g, m, k) {
          const selected = cur && cur.provider === g.id && cur.model === m.id;
          const main = React.createElement(
            'button',
            {
              className: 'nsel-opt' + (selected ? ' selected' : ''),
              type: 'button',
              disabled: busy,
              title: m.name || m.id,
              onClick: function () {
                chooseModel(g.id, m.id);
              },
            },
            React.createElement(
              'span',
              { className: 'nsel-copy' },
              React.createElement('span', { className: 'nsel-name' }, m.name || m.id),
              m.description || (norm(raw).trim() ? g.name || g.id : null)
                ? React.createElement('span', { className: 'nsel-desc' }, m.description || g.name || g.id)
                : null
            ),
            React.createElement('span', { className: 'nsel-check' }, selected ? '✓' : '')
          );
          return React.createElement('div', { key: k, className: 'nsel-row' }, main, favStar(g.id, m.id, favMap, setFavMap));
        }
        function effortRow(effort, labelText, desc, active, k) {
          return React.createElement(
            'button',
            {
              key: k,
              className: 'nsel-opt' + (active ? ' selected' : ''),
              type: 'button',
              disabled: busy,
              onClick: function () {
                chooseEffort(effort);
              },
            },
            React.createElement(
              'span',
              { className: 'nsel-copy' },
              React.createElement('span', { className: 'nsel-name' }, labelText),
              desc ? React.createElement('span', { className: 'nsel-desc' }, desc) : null
            ),
            React.createElement('span', { className: 'nsel-check' }, active ? '✓' : '')
          );
        }
        const trig = React.createElement(
          'button',
          {
            className: 'nsel-trigger',
            type: 'button',
            title: 'golive estático · via store da sessão',
            'aria-haspopup': 'menu',
            'aria-expanded': open,
            onClick: function () {
              if (open) close();
              else show();
            },
          },
          React.createElement('span', { className: 'nsel-tlabel' }, modelLabel),
          effortLabel !== undefined ? React.createElement('span', { className: 'nsel-teffort' }, '· ' + effortLabel) : null,
          React.createElement('span', { className: 'nsel-chev' }, open ? '▲' : '▼')
        );
        if (!open)
          return React.createElement('div', { className: 'nsel-wrap', ref: rootRef }, trig);
        const kids = [];
        if (toast) kids.push(React.createElement('div', { key: 'toast', className: 'nsel-toast' }, toast));
        if (pane === 'root') {
          kids.push(
            React.createElement(
              'button',
              {
                key: 'c-model',
                className: 'nsel-cell',
                type: 'button',
                onClick: function () {
                  setPane('model');
                },
              },
              React.createElement('span', { className: 'nsel-celllabel' }, 'Modelo'),
              React.createElement('span', { className: 'nsel-cellvalue' }, modelLabel),
              React.createElement('span', { className: 'nsel-cellchev' }, '›')
            )
          );
          if (reasoning !== undefined)
            kids.push(
              React.createElement(
                'button',
                {
                  key: 'c-effort',
                  className: 'nsel-cell',
                  type: 'button',
                  onClick: function () {
                    setPane('effort');
                  },
                },
                React.createElement('span', { className: 'nsel-celllabel' }, 'Effort'),
                React.createElement('span', { className: 'nsel-cellvalue' }, effortLabel),
                React.createElement('span', { className: 'nsel-cellchev' }, '›')
              )
            );
        }
        if (pane === 'model') {
          kids.push(
            React.createElement(
              'button',
              {
                key: 'back',
                className: 'nsel-back',
                type: 'button',
                onClick: function () {
                  setPane('root');
                },
              },
              '‹ Modelo'
            )
          );
          kids.push(
            React.createElement(
              'div',
              { key: 'search', className: 'nsel-search' },
              React.createElement('input', {
                className: 'nsel-input',
                placeholder: 'Buscar modelos… (ex: gr)',
                value: raw,
                autoFocus: true,
                type: 'text',
                onChange: function (e) {
                  setRaw(e.target.value);
                },
                onKeyDown: function (e) {
                  if (e.key === 'Escape') {
                    e.stopPropagation();
                    setPane('root');
                  }
                },
                onClick: function (e) {
                  e.stopPropagation();
                },
              })
            )
          );
          if (state.status === 'loading')
            kids.push(React.createElement('div', { key: 'loading', className: 'nsel-status' }, 'Atualizando modelos…'));
          if (state.error !== null && lastAction.current === 'load')
            kids.push(
              React.createElement(
                'div',
                { key: 'err', className: 'nsel-error' },
                React.createElement('span', { style: { flex: 1 } }, String(state.error)),
                React.createElement(
                  'button',
                  {
                    className: 'nsel-retry',
                    type: 'button',
                    onClick: function () {
                      try {
                        lastAction.current = 'load';
                        if (doLoad) doLoad();
                      } catch (e) {}
                    },
                  },
                  'Recarregar'
                )
              )
            );
          for (let fi = 0; fi < state.failures.length; fi++) {
            (function (f) {
              kids.push(
                React.createElement(
                  'div',
                  { key: 'fail-' + f.id, className: 'nsel-error' },
                  React.createElement('span', { style: { flex: 1 } }, String(f.name) + ': ' + String(f.message)),
                  React.createElement(
                    'button',
                    {
                      className: 'nsel-retry',
                      type: 'button',
                      onClick: function () {
                        try {
                          lastAction.current = 'load';
                          if (doLoad) doLoad();
                        } catch (e) {}
                      },
                    },
                    'Recarregar'
                  )
                )
              );
            })(state.failures[fi]);
          }
          const q = norm(raw).trim();
          const listKids = [];
          if (!q) {
            const favEntries = [];
            for (let a = 0; a < state.groups.length; a++) {
              const g = state.groups[a];
              if (!g || !(g.models instanceof Array)) continue;
              for (let b = 0; b < g.models.length; b++) {
                const m = g.models[b];
                if (m && favMap[keyOf(g.id, m.id)]) favEntries.push({ group: g, model: m });
              }
            }
            if (favEntries.length > 0) {
              listKids.push(React.createElement('div', { key: 't-fav', className: 'nsel-grouptitle' }, '★ Favoritos'));
              for (let c = 0; c < favEntries.length; c++) listKids.push(modelRow(favEntries[c].group, favEntries[c].model, 'fav-' + c));
            }
            for (let d = 0; d < state.groups.length; d++) {
              (function (g) {
                if (!g || !(g.models instanceof Array)) return;
                listKids.push(React.createElement('div', { key: 't-' + g.id, className: 'nsel-grouptitle' }, g.name || g.id));
                for (let e = 0; e < g.models.length; e++) listKids.push(modelRow(g, g.models[e], 'm-' + g.id + '-' + e));
              })(state.groups[d]);
            }
            if (state.status === 'ready' && state.groups.length === 0)
              listKids.push(React.createElement('div', { key: 'empty', className: 'nsel-status' }, 'Nenhum modelo disponível.'));
          } else {
            const scored = [];
            for (let f = 0; f < state.groups.length; f++) {
              const g = state.groups[f];
              if (!g || !(g.models instanceof Array)) continue;
              for (let h = 0; h < g.models.length; h++) {
                const m = g.models[h];
                if (!m || typeof m.id !== 'string') continue;
                const s = scoreEntry(g.id, m.id, m.name || m.id, q);
                if (s.keep) scored.push({ group: g, model: m, fav: !!favMap[keyOf(g.id, m.id)], rank: s.rank, idx: s.idx, len: s.len });
              }
            }
            scored.sort(function (a, b) {
              if ((b.fav ? 1 : 0) !== (a.fav ? 1 : 0)) return (b.fav ? 1 : 0) - (a.fav ? 1 : 0);
              if (a.rank !== b.rank) return a.rank - b.rank;
              if (a.idx !== b.idx) return a.idx - b.idx;
              if (a.len !== b.len) return a.len - b.len;
              return 0;
            });
            if (scored.length === 0)
              listKids.push(React.createElement('div', { key: 'empty', className: 'nsel-status' }, 'Nenhum modelo contém "' + raw.trim() + '"'));
            for (let i = 0; i < scored.length; i++) listKids.push(modelRow(scored[i].group, scored[i].model, 's-' + i));
          }
          kids.push(React.createElement('div', { key: 'list', className: 'nsel-scroll' }, listKids));
        }
        if (pane === 'effort') {
          kids.push(
            React.createElement(
              'button',
              {
                key: 'back',
                className: 'nsel-back',
                type: 'button',
                onClick: function () {
                  setPane('root');
                },
              },
              '‹ Effort'
            )
          );
          if (reasoning === undefined) {
            kids.push(
              React.createElement(
                'div',
                { key: 'noeff', className: 'nsel-status' },
                'Este modelo não tem níveis de effort declarados.'
              )
            );
            kids.push(
              React.createElement(
                'div',
                { key: 'declwrap', style: { textAlign: 'center' } },
                React.createElement(
                  'button',
                  { className: 'nsel-declare', type: 'button', disabled: rpcBusy, onClick: declareLevels },
                  rpcBusy ? 'Declarando…' : 'Declarar minimal/low/medium/high'
                )
              )
            );
            if (note) kids.push(React.createElement('div', { key: 'note', className: 'nsel-status' }, note));
          } else {
            const effKids = [];
            const hasDefault = !(typeof reasoning.defaultEffort === 'string' && reasoning.defaultEffort);
            if (hasDefault) effKids.push(effortRow(undefined, 'Padrão', null, effectiveEffort === undefined, 'def'));
            const effs = reasoning.efforts instanceof Array ? reasoning.efforts : [];
            for (let j = 0; j < effs.length; j++) {
              (function (lv) {
                effKids.push(effortRow(lv.id, lv.name, lv.description || null, effectiveEffort === lv.id, 'lv-' + lv.id));
              })(effs[j]);
            }
            if (effKids.length === 0)
              effKids.push(React.createElement('div', { key: 'empty', className: 'nsel-status' }, 'Este modelo não tem níveis de effort.'));
            kids.push(React.createElement('div', { key: 'elist', className: 'nsel-scroll' }, effKids));
          }
        }
        const menu = React.createElement('div', { className: 'nsel-menu', role: 'menu' }, kids);
        return React.createElement('div', { className: 'nsel-wrap', ref: rootRef }, trig, menu);
      }

      function FallbackSeat() {
        const openState = React.useState(false);
        const open = openState[0];
        const setOpen = openState[1];
        const rawState = React.useState('');
        const raw = rawState[0];
        const setRaw = rawState[1];
        const modelsState = React.useState([]);
        const models = modelsState[0];
        const setModels = modelsState[1];
        const curState = React.useState(null);
        const cur = curState[0];
        const setCur = curState[1];
        const loadState = React.useState(true);
        const loading = loadState[0];
        const setLoading = loadState[1];
        const favState = React.useState(function () {
          return favsLoad();
        });
        const favMap = favState[0];
        const setFavMap = favState[1];
        const msgState = React.useState(null);
        const msg = msgState[0];
        const setMsg = msgState[1];
        const rootRef = React.useRef(null);
        const cancelled = React.useRef(false);
        React.useEffect(function () {
          cancelled.current = false;
          rpc('models.list', {}).then(function (res) {
            if (cancelled.current) return;
            setModels(res.ok && res.data && res.data.models ? res.data.models : []);
          });
          rpc('models.get', {}).then(function (res) {
            if (cancelled.current) return;
            setCur(res.ok && res.data ? res.data.current : null);
            setLoading(false);
          });
          return function () {
            cancelled.current = true;
          };
        }, []);
        useOutside(rootRef, open, function () {
          setOpen(false);
        });
        const q = norm(raw).trim();
        const scored = [];
        for (let i = 0; i < models.length; i++) {
          const en = models[i];
          const s = scoreEntry(en.provider, en.model, en.label, q);
          if (s.keep) scored.push({ en: en, fav: !!favMap[keyOf(en.provider, en.model)], rank: s.rank, idx: s.idx, len: s.len });
        }
        scored.sort(function (a, b) {
          if ((b.fav ? 1 : 0) !== (a.fav ? 1 : 0)) return (b.fav ? 1 : 0) - (a.fav ? 1 : 0);
          if (a.rank !== b.rank) return a.rank - b.rank;
          if (a.idx !== b.idx) return a.idx - b.idx;
          return 0;
        });
        function choose(en) {
          rpc('models.save', { provider: en.provider, model: en.model }).then(function (res) {
            if (!res.ok) {
              setMsg('Falhou: ' + res.error);
              return;
            }
            setCur({ provider: en.provider, model: en.model, label: en.label });
            setOpen(false);
          });
        }
        const label = cur ? cur.model : loading ? '…' : 'Modelo';
        const btn = React.createElement(
          'button',
          {
            className: 'nsel-trigger',
            type: 'button',
            title: 'golive estático · fallback (default global)',
            onClick: function () {
              setOpen(!open);
              setMsg(null);
            },
          },
          React.createElement('span', { className: 'nsel-tlabel' }, label),
          React.createElement('span', { className: 'nsel-chev' }, open ? '▲' : '▼')
        );
        if (!open) return React.createElement('div', { className: 'nsel-wrap', ref: rootRef }, btn);
        const rows = [];
        rows.push(
          React.createElement(
            'div',
            { key: 'search', className: 'nsel-search' },
            React.createElement('input', {
              className: 'nsel-input',
              placeholder: 'Buscar modelos… (ex: gr)',
              value: raw,
              autoFocus: true,
              type: 'text',
              onChange: function (e) {
                setRaw(e.target.value);
              },
              onKeyDown: function (e) {
                if (e.key === 'Escape') setOpen(false);
              },
              onClick: function (e) {
                e.stopPropagation();
              },
            })
          )
        );
        if (msg) rows.push(React.createElement('div', { key: 'msg', className: 'nsel-error' }, msg));
        if (scored.length === 0)
          rows.push(
            React.createElement('div', { key: 'empty', className: 'nsel-status' }, q ? 'Nenhum modelo contém "' + q + '"' : loading ? 'Carregando…' : 'Nenhum modelo.')
          );
        const listRows = [];
        for (let j = 0; j < scored.length; j++) {
          (function (rec, k) {
            const en = rec.en;
            const active = cur && en.provider === cur.provider && en.model === cur.model;
            const main = React.createElement(
              'button',
              {
                className: 'nsel-opt' + (active ? ' selected' : ''),
                type: 'button',
                onClick: function () {
                  choose(en);
                },
              },
              React.createElement(
                'span',
                { className: 'nsel-copy' },
                React.createElement('span', { className: 'nsel-name' }, en.model),
                React.createElement('span', { className: 'nsel-desc' }, en.provider)
              ),
              React.createElement('span', { className: 'nsel-check' }, active ? '✓' : '')
            );
            listRows.push(React.createElement('div', { key: 'r' + k, className: 'nsel-row' }, main, favStar(en.provider, en.model, favMap, setFavMap)));
          })(scored[j], j);
        }
        rows.push(React.createElement('div', { key: 'list', className: 'nsel-scroll' }, listRows));
        rows.push(React.createElement('div', { key: 'why', className: 'nsel-why' }, 'Modo compatível (default global).'));
        return React.createElement(
          'div',
          { className: 'nsel-wrap', ref: rootRef },
          btn,
          React.createElement('div', { className: 'nsel-menu', role: 'menu' }, rows)
        );
      }

      function Seat(props) {
        const avail = !(props && props.available === false);
        const store =
          props && props.directory && typeof props.directory.getSnapshot === 'function' && typeof props.directory.subscribe === 'function'
            ? props.directory
            : null;
        const doSelect = props && typeof props.select === 'function' ? props.select : null;
        const doLoad = props && typeof props.load === 'function' ? props.load : null;
        if (!avail) {
          return React.createElement(
            'div',
            { className: 'nsel-wrap' },
            React.createElement(
              'button',
              {
                className: 'nsel-trigger',
                type: 'button',
                disabled: true,
                title: 'Seleção indisponível nesta sessão (subagente?)',
              },
              React.createElement('span', { className: 'nsel-tlabel' }, 'Modelo indisponível')
            )
          );
        }
        if (store && doSelect) return React.createElement(StoreSeat, { store: store, doSelect: doSelect, doLoad: doLoad });
        return React.createElement(FallbackSeat, null);
      }

      function pickSessionId(props) {
        if (props === null || props === undefined) return null;
        const direct = ['sessionId', 'sessionid', 'id', 'conversationId', 'chatId'];
        for (let i = 0; i < direct.length; i++) {
          const v = props[direct[i]];
          if (typeof v === 'string' && v) return v;
        }
        const nested = ['session', 'conversation', 'chat', 'item', 'entry', 'value'];
        for (let j = 0; j < nested.length; j++) {
          const o = props[nested[j]];
          if (o && typeof o === 'object') {
            for (let k = 0; k < direct.length; k++) {
              const v = o[direct[k]];
              if (typeof v === 'string' && v) return v;
            }
          }
        }
        return null;
      }
      function pickSessionName(props) {
        if (props === null || props === undefined) return 'esta conversa';
        const keys = ['name', 'title', 'label'];
        for (let i = 0; i < keys.length; i++) {
          const v = props[keys[i]];
          if (typeof v === 'string' && v) return v;
        }
        const nested = ['session', 'conversation', 'chat', 'item', 'entry', 'value'];
        for (let j = 0; j < nested.length; j++) {
          const o = props[nested[j]];
          if (o && typeof o === 'object') {
            for (let k = 0; k < keys.length; k++) {
              const v = o[keys[k]];
              if (typeof v === 'string' && v) return v;
            }
          }
        }
        return 'esta conversa';
      }
      function TrashIcon(size) {
        const s = size || 14;
        return React.createElement(
          'svg',
          {
            width: s,
            height: s,
            viewBox: '0 0 16 16',
            fill: 'none',
            stroke: 'currentColor',
            strokeWidth: 1.5,
            strokeLinecap: 'round',
            strokeLinejoin: 'round',
            'aria-hidden': true,
          },
          React.createElement('path', { d: 'M2.5 4.5h11' }),
          React.createElement('path', { d: 'M6.5 4.5V3.2c0-.4.3-.7.7-.7h1.6c.4 0 .7.3.7.7v1.3' }),
          React.createElement('path', { d: 'M4 4.5l.7 8.3c0 .4.3.7.7.7h5.2c.4 0 .7-.3.7-.7l.7-8.3' }),
          React.createElement('path', { d: 'M6.8 7.2v3.6M9.2 7.2v3.6' })
        );
      }
      function DeleteAction(props) {
        const sidState = React.useState(null);
        const sid = sidState[0];
        const setSid = sidState[1];
        const confirmState = React.useState(false);
        const confirmOpen = confirmState[0];
        const setConfirmOpen = confirmState[1];
        const busyState = React.useState(false);
        const busy = busyState[0];
        const setBusy = busyState[1];
        const errState = React.useState(null);
        const err = errState[0];
        const setErr = errState[1];
        React.useEffect(
          function () {
            setSid(pickSessionId(props));
          },
          [props]
        );
        function onTrash() {
          setErr(null);
          setConfirmOpen(true);
        }
        function onCancel() {
          if (!busy) {
            setConfirmOpen(false);
            setErr(null);
          }
        }
        function onConfirm() {
          if (busy) return;
          if (sid === null) {
            setErr('Não achei o id desta conversa nas props do header.');
            return;
          }
          setBusy(true);
          setErr(null);
          rpc('chat.delete', { sessionId: sid }).then(function (res) {
            setBusy(false);
            if (res.ok) setConfirmOpen(false);
            else setErr(res.error || 'Falha ao excluir.');
          });
        }
        const btn = React.createElement(
          'button',
          { className: 'del-btn', onClick: onTrash, type: 'button', title: 'Excluir esta conversa permanentemente' },
          TrashIcon(14),
          React.createElement('span', null, 'Excluir')
        );
        if (!confirmOpen) return React.createElement('span', { className: 'del-act' }, btn);
        const name = pickSessionName(props);
        const modal = React.createElement(
          'div',
          {
            className: 'del-modal',
            onClick: function (e) {
              e.stopPropagation();
            },
          },
          React.createElement(
            'div',
            { className: 'del-mtitle' },
            React.createElement('span', { className: 'del-micon' }, TrashIcon(16)),
            React.createElement('span', null, 'Excluir conversa?')
          ),
          React.createElement(
            'div',
            { className: 'del-mbody' },
            'Isso apaga ',
            React.createElement('strong', null, 'permanentemente'),
            ' o histórico, os arquivos e o registro da sessão. Não dá para desfazer.'
          ),
          React.createElement('div', { className: 'del-mname', title: name }, name),
          React.createElement(
            'div',
            { className: 'del-mrow' },
            React.createElement(
              'button',
              { className: 'del-cancel', onClick: onCancel, disabled: busy, type: 'button' },
              'Cancelar'
            ),
            React.createElement(
              'button',
              { className: 'del-confirm', onClick: onConfirm, disabled: busy, type: 'button' },
              busy ? 'Excluindo…' : 'Excluir'
            )
          ),
          err ? React.createElement('div', { className: 'del-merr' }, err) : null
        );
        const ovl = React.createElement('div', { className: 'del-ovl', onClick: onCancel }, modal);
        return React.createElement('span', { className: 'del-act' }, btn, ovl);
      }

      slots.inject('conversation.input.model', function () {
        return slots.register(
          {
            name: 'conversation.input.model',
            priority: -1,
            inject: function (sessionId) {
              return ownerProps(sessionId);
            },
          },
          function (props) {
            return React.createElement(Seat, props);
          }
        );
      });
      slots.inject('conversation.session.header.actions', function () {
        return slots.register(
          { name: 'conversation.session.header.actions', id: 'chat-hard-delete', order: 200, label: 'Excluir conversa' },
          function (props) {
            return React.createElement(DeleteAction, props);
          }
        );
      });
    }

    module.exports.inject = inject;
    module.exports.apply = apply;
    return module.exports;
  },
});
