/* AgentMeter: settings screen
 * ==========================
 * A list of detected agents with an on/off switch each; enabled agents appear in the sidebar.
 * Plain script, no bundler, no external resources. Loaded by ui/settings.html.
 *
 * Backend contract (Tauri v2, `app.withGlobalTauri: true` so window.__TAURI__ exists):
 *   invoke('get_settings')               -> Settings
 *   invoke('save_settings', { settings }) -> null
 *   invoke('get_usage')                  -> AgentUsage[]
 *   listen('usage-updated',    e => e.payload as AgentUsage[])
 *   listen('settings-changed', e => e.payload as Settings)
 *   Settings   = { agents: string[] (ordered, enabled ids), ...other fields (tubeCount, demoMode, dockEdge) }
 *   AgentUsage = { id, name, installed, percentLeft: number|null, resetsAt, window, active, exact, note }
 *
 * Behaviour notes for integrators:
 *   - This screen only edits settings.agents. Turning an agent on appends it to the end; turning it off
 *     removes it; existing order is kept. Every other Settings field is passed through unchanged.
 *   - Not-installed agents cannot be turned on (row disabled, "Not installed" hint). If one is already
 *     enabled (or an enabled id is not detected at all) it stays switchable so it can be turned off.
 *   - Controls are inert until the first get_settings succeeds (Retry banner if it fails), so an early
 *     click can never save over the user's settings.
 *   - Every change is saved immediately (no Save button), and a "Saved" pill confirms it. Saves are
 *     single-flight, latest wins; once the queue drains the page re-reads get_settings as the truth.
 *   - Event listeners are registered before the initial reads. The payload of the latest settings-changed
 *     is authoritative: shown at once when no local save is pending, and preferred over a get_settings
 *     read it races. After saves drain, only events arriving after the final save resolved count.
 *     Known limitation: a save already in flight overwrites a concurrent external change (save_settings
 *     writes the whole object); the UI still converges to the stored state.
 *   - Opened in a normal browser (no window.__TAURI__), a mock backend is used: sample agents, and settings
 *     kept in localStorage under "agentmeter.settings". Usage drifts every few seconds to exercise live updates.
 *     Test hooks: URL params mockLoadDelay, mockSaveDelay (ms), mockFailLoad, mockFailSave (count), mockFailSaveMessage, mockStaleRead,
 *     mockNoEcho, and window.__agentmeterMock.{failNextSaves, failNextLoads, setSaveDelay, setLoadDelay,
 *     setStaleReads, setEcho, externalChange, emitUsage, patchUsage, read}.
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

  // Mock-only defaults; the real defaults live in the backend.
  const DEFAULT_SETTINGS = { tubeCount: 4, agents: ['claude-code', 'codex'], demoMode: false, dockEdge: 'right' };

  // Only `agents` is validated (unique non-empty strings); every other field is passed through untouched.
  function normalize(raw) {
    const s = raw && typeof raw === 'object' ? raw : {};
    const agents = Array.isArray(s.agents) ? [...new Set(s.agents.filter(a => typeof a === 'string' && a))] : [];
    return { ...s, agents };
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
    // Test knobs. URL params apply from page load (e.g. settings.html?mockLoadDelay=2000&mockFailLoad=1);
    // window.__agentmeterMock changes them at runtime from the console.
    const params = new URLSearchParams(location.search);
    const knobs = {
      loadDelay: +params.get('mockLoadDelay') || 60,
      saveDelay: +params.get('mockSaveDelay') || 60,
      failLoads: +params.get('mockFailLoad') || 0,   // number of get_settings calls to fail
      failSaves: +params.get('mockFailSave') || 0,   // number of save_settings calls to fail
      failSaveMessage: params.get('mockFailSaveMessage'), // if set, failed saves reject with this string
      staleReads: params.has('mockStaleRead'),        // get_settings returns the state as of the call, not the reply
      echo: !params.has('mockNoEcho'),                // emit settings-changed after each successful save
    };
    const after = (ms, fn) => new Promise((res, rej) => setTimeout(() => { try { res(fn()); } catch (e) { rej(e); } }, ms));
    window.__agentmeterMock = {
      knobs,
      failNextSaves(n = 1, message = null) { knobs.failSaves = n; knobs.failSaveMessage = message; },
      emitUsage() { emit('usage-updated', usage.map(u => ({ ...u }))); },   // fire a usage update now
      patchUsage(id, patch) { Object.assign(usage.find(u => u.id === id) || {}, patch); this.emitUsage(); },
      failNextLoads(n = 1) { knobs.failLoads = n; },
      setSaveDelay(ms) { knobs.saveDelay = ms; },
      setLoadDelay(ms) { knobs.loadDelay = ms; },
      setStaleReads(on = true) { knobs.staleReads = on; },
      setEcho(on = true) { knobs.echo = on; },
      // Simulates the tray or sidebar changing settings: persists, then emits settings-changed.
      externalChange(patch) {
        localStorage.setItem(KEY, JSON.stringify(normalize({ ...read(), ...patch })));
        emit('settings-changed', read());
      },
      read,
    };
    return {
      mock: true,
      invoke(cmd, args) {
        if (cmd === 'get_settings') {
          const atCall = read();
          return after(knobs.loadDelay, () => {
            if (knobs.failLoads > 0) { knobs.failLoads--; throw new Error('mock: get_settings failed'); }
            return knobs.staleReads ? atCall : read();
          });
        }
        if (cmd === 'get_usage') return after(60, () => usage.map(u => ({ ...u })));
        if (cmd === 'save_settings') return after(knobs.saveDelay, () => {
          if (knobs.failSaves > 0) {
            knobs.failSaves--;
            if (knobs.failSaveMessage) throw knobs.failSaveMessage;   // like a Tauri command returning Err(String)
            throw new Error('mock: save_settings failed');
          }
          localStorage.setItem(KEY, JSON.stringify(args.settings));
          if (knobs.echo) setTimeout(() => emit('settings-changed', read()), 0);
          return null;
        });
        return Promise.reject(new Error('mock: unknown command ' + cmd));
      },
      listen(evt, cb) {
        (listeners[evt] = listeners[evt] || []).push(cb);
        return Promise.resolve(() => { listeners[evt] = listeners[evt].filter(f => f !== cb); });
      },
    };
  }

  /* ===== State ===== */
  let settings = normalize({ agents: [] });   // placeholder until get_settings resolves
  let usage = [];
  let ready = false;          // controls stay inert until the first get_settings succeeds
  let saving = false;         // a save (or the post-save reconcile read) is in flight
  let pendingSave = null;     // latest unsaved snapshot; newer edits replace it ("latest wins")
  // The backend emits settings-changed with the full saved Settings after every save, so the payload of
  // the LATEST event is authoritative. eventSeq tells whether one arrived during a save or read.
  let latestEvent = null;
  let eventSeq = 0;
  let editGen = 0;            // bumped on every local edit; lets a slow Retry detect it is stale

  const $ = id => document.getElementById(id);
  const els = { status: $('status'), agents: $('agents'), empty: $('agents-empty') };

  /* ===== Saving ===== */
  let statusTimer = 0;
  function showStatus(text, isError, sticky) {
    clearTimeout(statusTimer);
    els.status.textContent = text;
    els.status.classList.toggle('error', !!isError);
    els.status.classList.add('show');
    if (!sticky) statusTimer = setTimeout(() => els.status.classList.remove('show'), isError ? 4000 : 1400);
  }

  const sameSettings = (a, b) => JSON.stringify(a) === JSON.stringify(b);

  // One get_settings read. If a settings-changed event arrives while it is in flight, that event is
  // newer than (or equal to) the read, so it wins, even when the read itself fails.
  async function readAuthoritative() {
    const seq = eventSeq;
    try {
      const s = normalize(await backend.invoke('get_settings'));
      return eventSeq !== seq ? latestEvent : s;
    } catch (err) {
      if (eventSeq !== seq) return latestEvent;
      throw err;
    }
  }

  function applySettings(next) {
    if (!sameSettings(next, settings)) { settings = next; render(); }
  }

  function commit(patch) {
    if (!ready) return;
    editGen++;
    settings = normalize({ ...settings, ...patch });
    render();
    pendingSave = JSON.parse(JSON.stringify(settings));
    pump();
  }

  // Single-flight save queue: at most one save in flight plus one pending snapshot (latest wins).
  // When it drains (success or failure), one get_settings read is the truth, or a settings-changed that
  // arrives during that read, i.e. AFTER the final save resolved. Events that arrived while our save was
  // in flight are ignored: that save may have overwritten them. If the user edited again meanwhile, that
  // edit is saved first. A failed read shows a retryable "Couldn't refresh settings" error, not "Saved".
  async function pump() {
    if (saving) return;
    saving = true;
    let failed = false, failMessage = null, fresh = null, refreshFailed = false;
    while (pendingSave) {
      const snap = pendingSave;
      pendingSave = null;
      try { await backend.invoke('save_settings', { settings: snap }); }
      catch (err) {
        failed = true;
        // The backend rejects with a user-facing string when it refuses the write (e.g. a broken settings
        // file: "fix it or use Reset Settings from the tray"); show that verbatim. Anything else is generic.
        failMessage = typeof err === 'string' && err.trim() ? err.trim() : null;
        console.error('save_settings failed', err);
      }
    }
    if (!pendingSave) {
      try { fresh = await readAuthoritative(); }
      catch (err) { refreshFailed = true; console.error('get_settings after save failed', err); }
    }
    saving = false;
    if (pendingSave) { pump(); return; }
    if (fresh) { applySettings(fresh); hideError('refresh'); }
    if (!failed) hideError('save');
    if (refreshFailed) { showError('refresh'); els.status.classList.remove('show'); }
    else showStatus(failed ? "Couldn't save" : 'Saved', failed);
    if (failMessage && !refreshFailed) showError('save', failMessage);
  }

  function onSettingsChanged(payload) {
    latestEvent = normalize(payload);
    eventSeq++;
    // Before init, or while a save is draining, init()/pump() pick this up from latestEvent.
    if (!ready || saving || pendingSave) return;
    applySettings(latestEvent);
    hideError('refresh');
    hideError('save');
  }

  /* ===== Agent list ===== */
  // Rows in detection order, then any enabled ids that are not detected (so they can be turned off).
  function rows() {
    const list = usage.map(u => ({ ...u, name: u.name || prettyName(u.id), detected: true }));
    for (const id of settings.agents)
      if (!usage.some(u => u.id === id)) list.push({ id, name: prettyName(id), installed: false, percentLeft: null, exact: false, detected: false });
    return list;
  }

  // Installed agents whose usage can't be read get no tube in the sidebar (it shows a notice instead);
  // here they get a muted "Usage unavailable" hint and stay switchable.
  const usageUnavailable = a => a.installed && a.percentLeft == null;
  function pctText(a) {
    if (!a.installed || a.percentLeft == null) return '';
    return (a.exact ? '' : '~') + Math.round(a.percentLeft) + '%';
  }
  function pctLabel(a) {
    return (a.exact ? '' : 'about ') + Math.round(a.percentLeft) + '% left';
  }

  // Rows are built once and then updated in place, so a usage update landing between mousedown and
  // mouseup cannot swallow a click, and VoiceOver keeps its position. The list is rebuilt only when
  // the set of agent ids changes.
  function agentRow(a) {
    const li = document.createElement('li');
    const key = a.id.replace(/[^a-z0-9_-]/gi, '_');
    li.innerHTML = `
      <button type="button" class="agent" role="switch" aria-labelledby="name-${key}" style="--c:${colorFor(a.id)}">
        <i class="dot" aria-hidden="true"></i>
        <span class="who">
          <span class="name" id="name-${key}"></span>
          <span class="sub"></span>
        </span>
        <span class="pct" aria-hidden="true"></span>
        <span class="switch" aria-hidden="true"></span>
        <span class="desc" id="desc-${key}" hidden></span>
      </button>`;
    li.firstElementChild.dataset.id = a.id;
    return li;
  }

  const setText = (el, text) => { if (el.textContent !== text) el.textContent = text; };
  function updateRow(b, a) {
    const on = settings.agents.includes(a.id);
    const unavailable = usageUnavailable(a);
    const note = typeof a.note === 'string' ? a.note.trim() : '';
    const sub = !a.installed ? (a.detected ? 'Not installed' : 'Not detected') : unavailable ? 'Usage unavailable' : '';
    const pct = pctText(a);
    const desc = unavailable && note ? `${sub}. ${note}` : sub || pctLabel(a);
    const title = unavailable ? note : '';
    if ((b.getAttribute('title') || '') !== title) { if (title) b.title = title; else b.removeAttribute('title'); }
    if (b.getAttribute('aria-checked') !== String(on)) b.setAttribute('aria-checked', String(on));
    const disabled = !a.installed && !on;
    if (b.disabled !== disabled) b.disabled = disabled;
    setText(b.querySelector('.name'), a.name);
    const subEl = b.querySelector('.sub');
    setText(subEl, sub);
    subEl.hidden = !sub;
    setText(b.querySelector('.pct'), pct);
    const descEl = b.querySelector('.desc');
    setText(descEl, desc);
    if (b.getAttribute('aria-describedby') !== descEl.id) b.setAttribute('aria-describedby', descEl.id);
  }

  function render() {
    const list = rows();
    const ids = list.map(a => a.id).join('\n');
    if (els.agents.dataset.ids !== ids) {
      const focusId = document.activeElement && document.activeElement.dataset ? document.activeElement.dataset.id : null;
      els.agents.replaceChildren(...list.map(agentRow));
      els.agents.dataset.ids = ids;
      if (focusId) {
        const b = [...els.agents.querySelectorAll('.agent')].find(x => x.dataset.id === focusId);
        if (b) b.focus({ preventScroll: true });
      }
    }
    const buttons = els.agents.querySelectorAll('.agent');
    list.forEach((a, i) => updateRow(buttons[i], a));
    els.empty.hidden = !ready || list.length > 0;
  }

  els.agents.addEventListener('click', e => {
    const b = e.target.closest('.agent');
    if (!b || b.disabled) return;
    const id = b.dataset.id;
    const agents = settings.agents.includes(id)
      ? settings.agents.filter(a => a !== id)                // off: remove, order of the rest kept
      : [...settings.agents, id];                            // on: append at the end
    commit({ agents });
  });

  /* ===== Startup ===== */
  const main = document.querySelector('main');
  const loadError = $('load-error');
  const loadErrorText = $('load-error-text');
  const loadRetry = $('load-retry');
  // 'load' (controls stay inert, Retry) | 'refresh' (Retry) | 'save' (backend message, Dismiss)
  let errorKind = null;
  function showError(kind, message) {
    errorKind = kind;
    loadErrorText.textContent = kind === 'save' ? message
      : kind === 'load' ? "Couldn't load settings." : "Couldn't refresh settings.";
    loadRetry.textContent = kind === 'save' ? 'Dismiss' : 'Retry';
    loadError.hidden = false;
  }
  function hideError(kind) {
    if (!kind || errorKind === kind) { loadError.hidden = true; errorKind = null; }
  }
  function setReady(on) {
    ready = on;
    main.inert = !on;                      // blocks clicks, focus and keyboard on every control
    main.setAttribute('aria-busy', String(!on));
    document.body.classList.toggle('loading', !on);
  }

  async function loadSettings() {
    hideError();
    showStatus('Loading…', false, true);
    try {
      settings = await readAuthoritative();
    } catch (err) {
      console.error('get_settings failed', err);
      els.status.classList.remove('show');
      els.status.textContent = '';
      showError('load');
      return;
    }
    setReady(true);
    els.status.classList.remove('show');
    els.status.textContent = '';
    render();
  }

  async function init() {
    if (backend.mock) document.documentElement.dataset.mock = 'true';
    setReady(false);
    render();
    // Listen first so nothing emitted during the initial reads is lost. Usage simply takes whichever
    // arrives last (event or read); a settings-changed during the settings read wins over the read.
    try {
      await Promise.all([
        backend.listen('usage-updated', e => { if (Array.isArray(e.payload)) { usage = e.payload; render(); } }),
        backend.listen('settings-changed', e => onSettingsChanged(e.payload)),
      ]);
    } catch (err) { console.error('event listen failed', err); }
    backend.invoke('get_usage').then(u => { if (Array.isArray(u)) { usage = u; render(); } })
      .catch(err => console.error('get_usage failed', err));
    await loadSettings();
  }
  // Retry after a failed refresh: one read. Its result (or error) is discarded if the user edited or a
  // settings-changed arrived meanwhile; those already brought newer state.
  async function refreshSettings() {
    hideError();
    const gen = editGen, seq = eventSeq;
    const stale = () => editGen !== gen || eventSeq !== seq || saving || pendingSave;
    try {
      const fresh = await readAuthoritative();
      if (!stale()) applySettings(fresh);
    } catch (err) {
      console.error('get_settings refresh failed', err);
      if (!stale()) showError('refresh');
    }
  }
  loadRetry.addEventListener('click', () => {
    if (errorKind === 'save') hideError('save');
    else if (ready) refreshSettings();
    else loadSettings();
  });
  init();
})();
