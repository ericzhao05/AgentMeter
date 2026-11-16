/* AgentMeter sidebar: one tube per enabled agent. Data arrives from Rust via the
   'usage-updated' and 'settings-changed' events; get_usage / get_settings seed the first frame. */
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

/* Liquid colour per agent (brand-ish); unknown ids get a stable colour from the fallback palette. */
const AGENT_COLORS = {
  'claude-code': '#D97757', claude: '#D97757', codex: '#10A37F', gemini: '#4E86F7', cursor: '#E6E6E6',
  copilot: '#8957E5', windsurf: '#2EC4B6', aider: '#14B014', amp: '#F34E3F', opencode: '#FAB283',
};
const FALLBACK = ['#F5B83D', '#36B9F0', '#E85D9E', '#9BD84A', '#B38CFF'];
const UNKNOWN_COLOR = '#6b6b75';
const colorFor = id => AGENT_COLORS[id] ?? FALLBACK[[...id].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7) % FALLBACK.length];

const FPS_ACTIVE = 30, FPS_IDLE = 20;   // slow liquid reads fine at 30 fps; 60 fps doubles WKWebView repaint cost
const REFILL_JUMP = 20;         // a rise of this many points means the window reset → pour animation
const REFILL_MS = 2100;         // refill() glugs to 100% over ~2 s; then settle on the real level
const MAX_TUBES = 8;

let settings = { tubeCount: 4, agents: [], demoMode: false, dockEdge: 'right' };
let usage = [];
let usageKnown = false;      // until the first usage snapshot arrives, nothing counts as "unreadable"
let settingsKnown = false;   // don't render (or resize) until the real settings arrive
const cols = new Map();         // agent id → { el, tube, last, refillTimer }
const $ = id => document.getElementById(id);

/* Enabled agents in settings order (tubeCount is legacy and ignored). Only agents whose usage can be
   read get a tube (max MAX_TUBES); the rest are listed in the "Couldn't read usage" notice. */
const readable = a => !!a && a.installed && typeof a.percentLeft === 'number';
function partition() {
  const byId = id => usage.find(a => a.id === id);
  const shown = [], unreadable = [];
  for (const id of settings.agents) (readable(byId(id)) ? shown : unreadable).push(id);
  return { ids: shown.slice(0, MAX_TUBES), unreadable: usageKnown ? unreadable : [] };
}

/* Measured in WKWebView (2x display): the blurred glow layers are the main GPU cost and are only skipped
   at glowStrength 0, so small sidebars keep full glow and 5+ tubes drop the blur (neon cores stay). */
function glowFor(n) { return n <= 2 ? 1 : n <= 4 ? .8 : 0; }

function makeCol(id) {
  const el = document.createElement('div');
  el.className = 'col';
  el.innerHTML = '<div class="label"><span class="name"></span><span class="pct"></span><span class="reset"></span></div>';
  const tube = createTube({ width: 70, height: 280, liquidColor: colorFor(id), fillLevel: 0, state: 'idle' });
  el.prepend(tube);
  return { el, tube, last: null, refillTimer: 0 };
}

/* Time left until reset: "2h 15min" / "45min" / "<1min"; days for long (e.g. weekly) windows. */
function formatReset(iso) {
  const t = iso ? Date.parse(iso) : NaN;
  if (Number.isNaN(t)) return '';
  const m = Math.floor((t - Date.now()) / 60000);
  if (m < 1) return '<1min';
  const h = Math.floor(m / 60), d = Math.floor(h / 24);
  if (d >= 1) return `${d}d ${h % 24}h`;
  return h ? `${h}h ${m % 60}min` : `${m}min`;
}

/* "Limit resets in 2h 15min" (time kept on one line when it wraps); no reset time → empty. */
function showReset(el, iso) {
  const left = formatReset(iso);
  el.dataset.iso = left ? iso : '';
  el.textContent = left ? 'Limit resets in ' : '';
  if (left) { const t = document.createElement('span'); t.className = 't'; t.textContent = left; el.append(t); }
}

function updateCol(id, c, a, glow) {
  const known = !!a && a.installed && typeof a.percentLeft === 'number';
  const pct = known ? Math.max(0, Math.min(100, a.percentLeft)) : null;
  const level = known ? pct / 100 : 0;
  const state = a?.active ? 'working' : 'idle';

  c.el.classList.toggle('unknown', !known);
  c.el.classList.toggle('missing', !a || !a.installed);
  c.el.classList.toggle('low', known && pct < 15);
  c.el.style.setProperty('--accent', known ? colorFor(id) : UNKNOWN_COLOR);

  const liquidColor = known ? colorFor(id) : UNKNOWN_COLOR;
  if (known && c.last !== null && pct - c.last >= REFILL_JUMP) {
    clearTimeout(c.refillTimer);
    c.tube.setConfig({ state, liquidColor, glowStrength: glow });
    c.tube.refill();
    c.refillTimer = setTimeout(() => { c.refillTimer = 0; c.tube.setConfig({ fillLevel: c.last === null ? 0 : c.last / 100 }); }, REFILL_MS);
  } else if (!c.refillTimer || !known) {
    c.tube.setConfig({ fillLevel: level, state, liquidColor, glowStrength: glow });
  } else {
    c.tube.setConfig({ state, liquidColor, glowStrength: glow });   // refill still pouring; its timer sets the level
  }
  if (c.refillTimer && !known) { clearTimeout(c.refillTimer); c.refillTimer = 0; }
  c.last = pct;

  const q = s => c.el.querySelector(s);
  q('.name').textContent = a?.name ?? id;
  q('.pct').textContent = known ? `${a.exact ? '' : '~'}${Math.round(pct)}%`
    : !a ? 'Not detected' : !a.installed ? 'Not installed' : 'Usage unavailable';
  showReset(q('.reset'), known ? a.resetsAt : null);
  const tip = [a?.name ?? id,
    known ? `${a.exact ? '' : 'about '}${pct.toFixed(0)}% of the ${a.window ?? 'current'} limit left` : 'usage unknown',
    a?.resetsAt ? `resets ${new Date(a.resetsAt).toLocaleString()}` : '', a?.note ?? ''].filter(Boolean);
  c.el.title = tip.join('\n');
}

/* "Couldn't read usage: Gemini, Cursor" — in the header when tubes are shown, as the whole content
   when none are; each agent's reason in the tooltip. */
function renderNotice(unreadable, noTubes) {
  const names = unreadable.map(id => usage.find(a => a.id === id)?.name ?? id);
  const tip = unreadable.map(id => {
    const a = usage.find(x => x.id === id);
    const why = !a ? 'not detected' : !a.installed ? 'not installed' : (a.note || 'usage unavailable');
    return `${a?.name ?? id}: ${why}`;
  }).join('\n');
  const text = names.length ? `Couldn't read usage: ${names.join(', ')}` : '';
  const notice = $('notice');
  notice.hidden = !text || noTubes;
  notice.textContent = text;
  notice.title = tip;
  return { text, tip };
}

function render() {
  if (!settingsKnown) return;
  const { ids, unreadable } = partition();
  const box = $('tubes'), glow = glowFor(ids.length);
  for (const [id, c] of cols) if (!ids.includes(id)) { clearTimeout(c.refillTimer); c.tube.destroy(); c.el.remove(); cols.delete(id); }
  ids.forEach((id, i) => {
    let c = cols.get(id);
    if (!c) { c = makeCol(id); cols.set(id, c); }
    if (box.children[i] !== c.el) box.insertBefore(c.el, box.children[i] ?? null);
    updateCol(id, c, usage.find(a => a.id === id), glow);
  });
  window.tubeLoop.maxFps = ids.some(id => usage.find(a => a.id === id)?.active) ? FPS_ACTIVE : FPS_IDLE;
  const n = renderNotice(unreadable, ids.length === 0);
  // Nothing to draw: either nothing is enabled, or nothing enabled is readable (then just the notice).
  const empty = $('empty');
  const allUnreadable = settings.agents.length > 0 && n.text;
  empty.hidden = ids.length > 0 || (settings.agents.length > 0 && !n.text);
  $('empty-text').textContent = allUnreadable ? n.text : 'No agents selected —';
  $('empty-settings').hidden = !!allUnreadable;
  empty.title = allUnreadable ? n.tip : '';
  $('demo').hidden = !settings.demoMode;
  invoke('resize_sidebar', { tubes: Math.max(1, ids.length) }).catch(console.error);
}

// Reset countdowns tick between polls.
setInterval(() => document.querySelectorAll('.reset[data-iso]').forEach(el => { if (el.dataset.iso) showReset(el, el.dataset.iso); }), 30000);

$('settings').addEventListener('click', () => invoke('open_settings').catch(console.error));
$('empty-settings').addEventListener('click', () => invoke('open_settings').catch(console.error));
// Drag the sidebar from anywhere except its buttons.
$('panel').addEventListener('mousedown', e => {
  if (e.button === 0 && !e.target.closest('button')) {
    invoke('drag_began').catch(() => {});   // backend won't reposition the window while it is being dragged
    appWindow.startDragging().catch(() => {});
  }
});

/* Auto-hide: the backend watches the cursor and says when the sidebar is slid in (shown) and when the
   cursor is over it (hover: light background + gear). While slid out, the tube animation is paused. */
const SLIDE_MS = 220;
let pauseTimer = 0;
/* Debug builds log each slide's frame timing (rAF deltas over the slide), to check it stays smooth. */
let lastShown = null;
function measureSlide(kind) {
  const times = [];
  const t0 = performance.now();
  const tick = t => { times.push(t); if (t - t0 < SLIDE_MS + 80) requestAnimationFrame(tick); else report(); };
  const report = () => {
    const gaps = times.slice(1).map((t, i) => t - times[i]);
    const max = Math.max(...gaps), slow = gaps.filter(g => g > 20).length;
    invoke('dev_log', { msg: `slide ${kind}: ${gaps.length} frames, max gap ${max.toFixed(1)}ms, >20ms: ${slow}` }).catch(() => {});
  };
  requestAnimationFrame(tick);
}

function applySidebarState({ shown, hover, edge }) {
  const panel = $('panel');
  panel.classList.toggle('left', edge === 'left');
  if (lastShown !== null && lastShown !== shown) measureSlide(shown ? 'in' : 'out');
  lastShown = shown;
  panel.classList.toggle('shown', shown);
  panel.classList.toggle('hover', shown && hover);
  clearTimeout(pauseTimer);
  if (shown) window.tubeLoop.resume();
  else pauseTimer = setTimeout(() => window.tubeLoop.pause(), SLIDE_MS);   // after the slide-out finishes
}

// Listen first, then fetch the initial snapshots. A snapshot is applied only if no event for that
// stream arrived after its request started (otherwise the event is newer and wins).
let settingsRev = 0, usageRev = 0, visRev = 0;
const markUsage = u => { usage = u; usageKnown = true; };
(async () => {
  await listen('sidebar-state', e => { visRev++; applySidebarState(e.payload); });
  const vRev = visRev;
  invoke('get_sidebar_state').then(v => { if (visRev === vRev) applySidebarState(v); }).catch(console.error);
  await listen('usage-updated', e => { usageRev++; markUsage(e.payload); render(); });
  await listen('settings-changed', e => { settingsRev++; settingsKnown = true; settings = e.payload; render(); });
  const sRev = settingsRev, uRev = usageRev;
  invoke('get_settings').then(s => { if (settingsRev === sRev) { settingsKnown = true; settings = s; render(); } }).catch(console.error);
  invoke('get_usage').then(u => { if (usageRev === uRev && u.length) { markUsage(u); render(); } }).catch(console.error);
})();
