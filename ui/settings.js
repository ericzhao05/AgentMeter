/* AgentMeter: settings screen
 * ==========================
 * Plain script, no bundler, no external resources. Loaded by ui/settings.html.
 *
 * Backend contract (Tauri v2, `app.withGlobalTauri: true` so window.__TAURI__ exists):
 *   invoke('get_settings')               -> Settings
 *   invoke('save_settings', { settings }) -> null
 *   invoke('get_usage')                  -> AgentUsage[]
 *   listen('usage-updated',    e => e.payload as AgentUsage[])
 *   listen('settings-changed', e => e.payload as Settings)
 *   Settings   = { tubeCount: 1..8, agents: string[] (ordered, enabled ids), demoMode: boolean, dockEdge: "left"|"right" }
 *   AgentUsage = { id, name, installed, percentLeft: number|null, resetsAt, window, active, exact, note }
 *
 * Behaviour notes for integrators:
 *   - Every change is saved immediately (no Save button), and a "Saved" pill confirms it.
 *   - settings.agents holds ALL enabled agents in order, and can be longer than tubeCount.
 *     The sidebar should show the first tubeCount of them; the rest are marked "not shown" here.
 *   - Unknown keys on the Settings object are preserved on save.
 *   - Reordering uses pointer events, not HTML5 drag-and-drop, because Tauri's window-level
 *     `dragDropEnabled` (on by default) can swallow HTML5 drag events in the webview. No config change needed.
 *   - Keyboard: focus a row's grip and press ArrowUp/ArrowDown, or use the arrow buttons.
 *   - Opened in a normal browser (no window.__TAURI__), a mock backend is used: sample agents, and settings
 *     kept in localStorage under "agentmeter.settings". Usage drifts every few seconds to exercise live updates.
 */
(() => {
  'use strict';

  /* ===== Agent colours (liquid colour in the sidebar too) ===== */
  const AGENT_COLORS = {
    'claude-code': '#D97757',
    'codex': '#10A37F',
    'gemini': '#4C8DF6',
    'cursor': '#C3C7D1',
    'copilot': '#A371F7',
    'aider': '#F5B83D',
    'opencode': '#38BDF8',
    'windsurf': '#2DD4BF',
    'amp': '#F472B6',
  };
  const FALLBACK_COLORS = ['#8CFF3A', '#F472B6', '#38BDF8', '#F5B83D', '#A371F7', '#2DD4BF'];
  function colorFor(id) {
    if (AGENT_COLORS[id]) return AGENT_COLORS[id];
    let h = 0;
    for (const ch of id) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
    return FALLBACK_COLORS[h % FALLBACK_COLORS.length];
  }
  const prettyName = id => id.split(/[-_]/).map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(' ');

  const MAX_TUBES = 8;
  const DEFAULT_SETTINGS = { tubeCount: 4, agents: ['claude-code', 'codex'], demoMode: false, dockEdge: 'right' };

  function normalize(raw) {
    const s = raw && typeof raw === 'object' ? raw : {};
    const n = Math.round(Number(s.tubeCount));
    const agents = Array.isArray(s.agents) ? [...new Set(s.agents.filter(a => typeof a === 'string' && a))] : DEFAULT_SETTINGS.agents.slice();
    return {
      ...s,
      tubeCount: Number.isFinite(n) ? Math.min(MAX_TUBES, Math.max(1, n)) : DEFAULT_SETTINGS.tubeCount,
      agents,
      demoMode: !!s.demoMode,
      dockEdge: s.dockEdge === 'left' ? 'left' : 'right',
    };
  }

  /* ===== Backend: real Tauri, or an in-file mock for standalone preview ===== */
  const backend = window.__TAURI__ && window.__TAURI__.core ? {
    mock: false,
    invoke: (cmd, args) => window.__TAURI__.core.invoke(cmd, args),
    listen: (evt, cb) => window.__TAURI__.event.listen(evt, cb),
  } : createMockBackend();

  function createMockBackend() {
    const KEY = 'agentmeter.settings';
    const listeners = {};
    const emit = (evt, payload) => (listeners[evt] || []).forEach(cb => cb({ event: evt, payload }));
    const usage = [
      { id: 'claude-code', name: 'Claude Code', installed: true, percentLeft: 62, resetsAt: new Date(Date.now() + 2.3 * 3600e3).toISOString(), window: '5h', active: true, exact: true, note: null },
      { id: 'codex', name: 'Codex', installed: true, percentLeft: 38, resetsAt: null, window: 'weekly', active: false, exact: false, note: 'Estimated from local session logs' },
      { id: 'gemini', name: 'Gemini CLI', installed: true, percentLeft: null, resetsAt: null, window: null, active: false, exact: false, note: 'No usage data available' },
      { id: 'cursor', name: 'Cursor', installed: false, percentLeft: null, resetsAt: null, window: null, active: false, exact: false, note: null },
      { id: 'opencode', name: 'OpenCode', installed: true, percentLeft: 12, resetsAt: null, window: 'daily', active: true, exact: true, note: null },
    ];
    const read = () => { try { return normalize(JSON.parse(localStorage.getItem(KEY))); } catch { return normalize(DEFAULT_SETTINGS); } };
    if (!localStorage.getItem(KEY)) localStorage.setItem(KEY, JSON.stringify(DEFAULT_SETTINGS));
    // Another tab saving stands in for the tray or sidebar changing settings.
    window.addEventListener('storage', e => { if (e.key === KEY) emit('settings-changed', read()); });
    // Live usage drift.
    setInterval(() => {
      for (const u of usage) if (u.percentLeft != null) u.percentLeft = Math.max(0, Math.min(100, u.percentLeft + (Math.random() * 6 - 4)));
      emit('usage-updated', usage.map(u => ({ ...u })));
    }, 4000);
    const delay = v => new Promise(r => setTimeout(() => r(v), 60));
    return {
      mock: true,
      invoke(cmd, args) {
        if (cmd === 'get_settings') return delay(read());
        if (cmd === 'get_usage') return delay(usage.map(u => ({ ...u })));
        if (cmd === 'save_settings') {
          localStorage.setItem(KEY, JSON.stringify(args.settings));
          setTimeout(() => emit('settings-changed', read()), 0);
          return delay(null);
        }
        return Promise.reject(new Error('mock: unknown command ' + cmd));
      },
      listen(evt, cb) {
        (listeners[evt] = listeners[evt] || []).push(cb);
        return Promise.resolve(() => { listeners[evt] = listeners[evt].filter(f => f !== cb); });
      },
    };
  }

  /* ===== State ===== */
  let settings = normalize(DEFAULT_SETTINGS);
  let usage = [];
  let pendingSaves = 0;
  let saveChain = Promise.resolve();
  let drag = null;
  let renderQueued = false;

  const $ = id => document.getElementById(id);
  const els = {
    status: $('status'), count: $('tube-count'), slots: $('slots'), dock: $('dock-edge'), demo: $('demo-mode'),
    agents: $('agents'), agentCount: $('agent-count'), empty: $('agents-empty'), announcer: $('announcer'),
  };

  const usageById = () => new Map(usage.map(u => [u.id, u]));
  function agentInfo(id, map) {
    const u = map.get(id);
    return u ? { ...u, name: u.name || prettyName(id), detected: true }
      : { id, name: prettyName(id), installed: false, percentLeft: null, exact: false, note: null, detected: false };
  }
  // Display order: enabled agents (in settings order), then the rest in detection order.
  function orderedAgents() {
    const map = usageById();
    const enabled = settings.agents.map(id => agentInfo(id, map));
    const rest = usage.filter(u => !settings.agents.includes(u.id)).map(u => agentInfo(u.id, map));
    return { enabled, rest };
  }

  /* ===== Saving ===== */
  let statusTimer = 0;
  function showStatus(text, isError) {
    clearTimeout(statusTimer);
    els.status.textContent = text;
    els.status.classList.toggle('error', !!isError);
    els.status.classList.add('show');
    statusTimer = setTimeout(() => els.status.classList.remove('show'), isError ? 4000 : 1400);
  }

  function commit(patch) {
    settings = normalize({ ...settings, ...patch });
    render();
    const snapshot = JSON.parse(JSON.stringify(settings));
    pendingSaves++;
    saveChain = saveChain
      .then(() => backend.invoke('save_settings', { settings: snapshot }))
      .then(() => { if (pendingSaves === 1) showStatus('Saved'); })
      .catch(err => {
        console.error('save_settings failed', err);
        showStatus("Couldn't save", true);
        return backend.invoke('get_settings').then(s => { settings = normalize(s); render(); }).catch(() => {});
      })
      .finally(() => { pendingSaves--; });
  }

  const announce = msg => { els.announcer.textContent = ''; requestAnimationFrame(() => { els.announcer.textContent = msg; }); };

  /* ===== Radio groups (segmented controls) with roving tabindex ===== */
  function setupRadioGroup(group, onPick) {
    group.addEventListener('click', e => {
      const b = e.target.closest('[role=radio]');
      if (b && group.contains(b)) onPick(b);
    });
    group.addEventListener('keydown', e => {
      const items = [...group.querySelectorAll('[role=radio]')];
      const i = items.indexOf(document.activeElement);
      if (i < 0) return;
      const next = { ArrowRight: i + 1, ArrowDown: i + 1, ArrowLeft: i - 1, ArrowUp: i - 1, Home: 0, End: items.length - 1 }[e.key];
      if (next === undefined) return;
      e.preventDefault();
      const b = items[Math.max(0, Math.min(items.length - 1, next))];
      b.focus();
      onPick(b);
    });
  }
  function markRadio(group, isOn) {
    for (const b of group.querySelectorAll('[role=radio]')) {
      const on = isOn(b);
      b.setAttribute('aria-checked', on);
      b.tabIndex = on ? 0 : -1;
    }
  }

  // Tube count buttons 1..8
  for (let n = 1; n <= MAX_TUBES; n++) {
    const b = document.createElement('button');
    b.type = 'button'; b.setAttribute('role', 'radio'); b.dataset.n = n; b.textContent = n;
    b.setAttribute('aria-label', n === 1 ? '1 tube' : n + ' tubes');
    els.count.append(b);
  }
  setupRadioGroup(els.count, b => { const n = +b.dataset.n; if (n !== settings.tubeCount) commit({ tubeCount: n }); });
  setupRadioGroup(els.dock, b => { if (b.dataset.edge !== settings.dockEdge) commit({ dockEdge: b.dataset.edge }); });
  els.demo.addEventListener('click', () => commit({ demoMode: !settings.demoMode }));

  /* ===== Agent list ===== */
  const ICON_GRIP = '<svg viewBox="0 0 8 14" aria-hidden="true"><g fill="currentColor"><circle cx="2" cy="2" r="1.4"/><circle cx="6" cy="2" r="1.4"/><circle cx="2" cy="7" r="1.4"/><circle cx="6" cy="7" r="1.4"/><circle cx="2" cy="12" r="1.4"/><circle cx="6" cy="12" r="1.4"/></g></svg>';
  const ICON_UP = '<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1.5 6.8 5 3.2l3.5 3.6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  const ICON_DOWN = '<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1.5 3.2 5 6.8l3.5-3.6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';

  function quality(a) {
    if (a.percentLeft == null) return { cls: 'unknown', text: 'unknown', title: 'No usage data available' };
    if (a.exact) return { cls: 'exact', text: 'exact', title: 'Reported exactly by the provider' };
    return { cls: 'estimate', text: '~estimate', title: 'Estimated from local data' };
  }
  function pctText(a) {
    if (a.percentLeft == null) return '—';
    return (a.exact ? '' : '~') + Math.round(a.percentLeft) + '%';
  }
  function rowTitle(a) {
    const bits = [];
    if (a.window) bits.push('Window: ' + a.window);
    if (a.resetsAt) { const d = new Date(a.resetsAt); if (!isNaN(d)) bits.push('Resets ' + d.toLocaleString([], { weekday: 'short', hour: 'numeric', minute: '2-digit' })); }
    if (a.note) bits.push(a.note);
    return bits.join(' · ');
  }
  const esc = s => String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

  function agentRow(a, enabled, index, enabledCount) {
    const li = document.createElement('li');
    const shown = enabled && index < settings.tubeCount;
    li.className = 'agent' + (enabled ? '' : ' off') + (enabled && !shown ? ' overflow' : '');
    li.dataset.id = a.id;
    li.style.setProperty('--c', colorFor(a.id));
    const title = rowTitle(a);
    if (title) li.title = title;
    const q = quality(a);
    const nameId = 'agent-name-' + a.id.replace(/[^a-z0-9_-]/gi, '_');
    const badges = [];
    if (shown) badges.push(`<span class="badge slot-n">Tube ${index + 1}</span>`);
    if (enabled && !shown) badges.push(`<span class="badge hidden-cap" title="Add more tubes to show this agent">not shown</span>`);
    if (!a.installed) badges.push(`<span class="badge missing">${a.detected ? 'not installed' : 'not detected'}</span>`);
    else badges.push('<span class="badge installed">installed</span>');
    badges.push(`<span class="badge ${q.cls}" title="${q.title}">${q.text}</span>`);
    const pos = `position ${index + 1} of ${enabledCount}`;
    li.innerHTML = `
      <button type="button" class="handle" data-role="grip" ${enabled ? '' : 'hidden tabindex="-1"'}
        aria-label="Reorder ${esc(a.name)}, ${pos}. Use up and down arrow keys.">${ICON_GRIP}</button>
      <input type="checkbox" class="check" data-role="check" ${enabled ? 'checked' : ''} aria-labelledby="${nameId}" aria-describedby="${nameId}-meta">
      <div class="who">
        <div class="name-line"><i class="dot" aria-hidden="true"></i><span class="name" id="${nameId}">${esc(a.name)}</span></div>
        <div class="badges" id="${nameId}-meta">${badges.join('')}</div>
      </div>
      <span class="pct${a.percentLeft == null ? ' na' : ''}" aria-label="${a.percentLeft == null ? 'Usage unknown' : pctText(a) + ' left'}">${pctText(a)}</span>
      <div class="move" ${enabled ? '' : 'hidden'}>
        <button type="button" class="icon-btn" data-role="up" ${enabled && index > 0 ? '' : 'disabled'} ${enabled ? '' : 'tabindex="-1"'} aria-label="Move ${esc(a.name)} up">${ICON_UP}</button>
        <button type="button" class="icon-btn" data-role="down" ${enabled && index < enabledCount - 1 ? '' : 'disabled'} ${enabled ? '' : 'tabindex="-1"'} aria-label="Move ${esc(a.name)} down">${ICON_DOWN}</button>
      </div>`;
    return li;
  }

  function render() {
    if (drag) { renderQueued = true; return; }
    // Remember focus so re-rendering doesn't lose the user's place.
    const active = document.activeElement;
    const focusRow = active && active.closest && active.closest('.agent');
    const focusKey = focusRow ? [focusRow.dataset.id, active.dataset.role] : null;

    markRadio(els.count, b => +b.dataset.n === settings.tubeCount);
    markRadio(els.dock, b => b.dataset.edge === settings.dockEdge);
    els.demo.setAttribute('aria-checked', settings.demoMode);

    const { enabled, rest } = orderedAgents();
    const shownCount = Math.min(enabled.length, settings.tubeCount);
    els.agentCount.textContent = `${shownCount} of ${settings.tubeCount} tube${settings.tubeCount === 1 ? '' : 's'} filled`;

    // Slot preview
    els.slots.textContent = '';
    for (let i = 0; i < settings.tubeCount; i++) {
      const a = enabled[i], s = document.createElement('span'), fill = document.createElement('i');
      s.className = 'slot' + (a && a.percentLeft == null ? ' unknown' : '');
      if (a) { s.style.setProperty('--c', colorFor(a.id)); s.style.setProperty('--lvl', (a.percentLeft ?? 0) + '%'); s.title = a.name; }
      s.append(fill); els.slots.append(s);
    }

    // List
    const frag = document.createDocumentFragment();
    enabled.forEach((a, i) => frag.append(agentRow(a, true, i, enabled.length)));
    if (rest.length) {
      const d = document.createElement('li');
      d.className = 'divider'; d.setAttribute('role', 'presentation'); d.textContent = 'Off';
      if (enabled.length) frag.append(d);
      rest.forEach((a, i) => frag.append(agentRow(a, false, i, enabled.length)));
    }
    els.agents.replaceChildren(frag);
    els.empty.hidden = enabled.length + rest.length > 0;

    if (focusKey) {
      const row = [...els.agents.children].find(r => r.dataset.id === focusKey[0]);
      let target = row && row.querySelector(`[data-role="${focusKey[1]}"]`);
      if (target && (target.disabled || target.hidden || target.tabIndex < 0)) target = row.querySelector('[data-role="check"]');
      if (target) target.focus({ preventScroll: false });
    }
  }

  function move(id, to) {
    const list = settings.agents.slice(), from = list.indexOf(id);
    to = Math.max(0, Math.min(list.length - 1, to));
    if (from < 0 || from === to) return false;
    list.splice(to, 0, list.splice(from, 1)[0]);
    commit({ agents: list });
    const a = agentInfo(id, usageById());
    announce(`${a.name} moved to position ${to + 1} of ${list.length}`);
    return true;
  }

  els.agents.addEventListener('change', e => {
    if (e.target.dataset.role !== 'check') return;
    const id = e.target.closest('.agent').dataset.id;
    const name = agentInfo(id, usageById()).name;
    if (e.target.checked) {
      commit({ agents: [...settings.agents.filter(a => a !== id), id] });
      announce(settings.agents.length > settings.tubeCount ? `${name} enabled, but not shown: all ${settings.tubeCount} tubes are filled` : `${name} enabled, tube ${settings.agents.length}`);
    } else {
      commit({ agents: settings.agents.filter(a => a !== id) });
      announce(`${name} disabled`);
    }
  });

  els.agents.addEventListener('click', e => {
    const b = e.target.closest('[data-role=up], [data-role=down]');
    if (!b || b.disabled) return;
    const id = b.closest('.agent').dataset.id, i = settings.agents.indexOf(id);
    move(id, i + (b.dataset.role === 'up' ? -1 : 1));
  });

  els.agents.addEventListener('keydown', e => {
    const grip = e.target.closest('[data-role=grip]');
    if (!grip || drag) return;
    const id = grip.closest('.agent').dataset.id, i = settings.agents.indexOf(id);
    const to = { ArrowUp: i - 1, ArrowDown: i + 1, Home: 0, End: settings.agents.length - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    move(id, to);
  });

  /* ===== Pointer drag reordering (enabled rows only) ===== */
  els.agents.addEventListener('pointerdown', e => {
    const grip = e.target.closest('[data-role=grip]');
    if (!grip || e.button !== 0 || grip.hidden) return;
    const li = grip.closest('.agent');
    const rows = [...els.agents.querySelectorAll('.agent:not(.off)')];
    const idx = rows.indexOf(li);
    if (idx < 0 || rows.length < 2) return;
    e.preventDefault();
    grip.setPointerCapture(e.pointerId);
    const rects = rows.map(r => r.getBoundingClientRect());
    drag = { grip, li, rows, idx, rects, startY: e.clientY, target: idx, step: rects[1].top - rects[0].top, pointerId: e.pointerId };
    li.classList.add('dragging');
    rows.forEach(r => { if (r !== li) r.classList.add('settling'); });
    grip.focus({ preventScroll: true });
  });

  els.agents.addEventListener('pointermove', e => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    const { li, rows, idx, rects, step } = drag;
    // Clamped to the list, with a little slack so the row can pass the first/last row's midpoint.
    const dy = Math.max(rects[0].top - rects[idx].top - 8, Math.min(rects[rows.length - 1].top - rects[idx].top + 8, e.clientY - drag.startY));
    li.style.transform = `translateY(${dy}px)`;
    const center = rects[idx].top + rects[idx].height / 2 + dy;
    let target = 0;
    rects.forEach((r, j) => { if (j !== idx && r.top + r.height / 2 < center) target++; });
    drag.target = target;
    rows.forEach((r, j) => {
      if (j === idx) return;
      const shift = idx < j && j <= target ? -step : target <= j && j < idx ? step : 0;
      r.style.transform = shift ? `translateY(${shift}px)` : '';
    });
  });

  function endDrag(e) {
    if (!drag || (e && e.pointerId !== drag.pointerId)) return;
    const { li, rows, idx, target, grip } = drag;
    const cancelled = e && e.type === 'pointercancel';
    rows.forEach(r => { r.classList.remove('settling', 'dragging'); r.style.transform = ''; });
    drag = null;
    const id = li.dataset.id;
    const moved = !cancelled && target !== idx && move(id, target);
    if (!moved && renderQueued) render();
    renderQueued = false;
    if (!moved) grip.focus({ preventScroll: true });
  }
  els.agents.addEventListener('pointerup', endDrag);
  els.agents.addEventListener('pointercancel', endDrag);
  els.agents.addEventListener('lostpointercapture', e => { if (drag) endDrag(e); });

  /* ===== Startup ===== */
  async function init() {
    if (backend.mock) document.documentElement.dataset.mock = 'true';
    render();
    try {
      const [s, u] = await Promise.all([backend.invoke('get_settings'), backend.invoke('get_usage')]);
      settings = normalize(s);
      usage = Array.isArray(u) ? u : [];
    } catch (err) {
      console.error('initial load failed', err);
      showStatus("Couldn't load settings", true);
    }
    render();
    backend.listen('usage-updated', e => { if (Array.isArray(e.payload)) { usage = e.payload; render(); } });
    backend.listen('settings-changed', e => {
      if (pendingSaves > 0) return;                       // our own save is in flight; it is the source of truth
      const next = normalize(e.payload);
      if (JSON.stringify(next) !== JSON.stringify(settings)) { settings = next; render(); }
    });
  }
  init();
})();
