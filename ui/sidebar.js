/* AgentMeter sidebar: one tube per chosen agent. Data arrives from Rust via the
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

let settings = { tubeCount: 4, agents: [], demoMode: false, dockEdge: 'right' };
let usage = [];
let settingsKnown = false;   // don't render (or resize) until the real settings arrive
const cols = new Map();         // agent id → { el, tube, last, refillTimer }
const $ = id => document.getElementById(id);

/* Exactly the enabled agents, in order, up to tubeCount. First-run defaults are chosen by the backend;
   in demo mode the same ids are matched to mock data (an id without a mock entry shows as unknown). */
function chosenIds() {
  return settings.agents.slice(0, settings.tubeCount);
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

function formatReset(iso) {
  if (!iso) return '';
  const ms = Date.parse(iso) - Date.now();
  if (!(ms > 0)) return '↻ now';
  const m = Math.round(ms / 60000), h = Math.floor(m / 60), d = Math.floor(h / 24);
  if (d >= 1) return `↻ ${d}d ${h % 24}h`;
  return h ? `↻ ${h}h ${String(m % 60).padStart(2, '0')}m` : `↻ ${Math.max(1, m)}m`;
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
  q('.pct').textContent = known ? `${a.exact ? '' : '~'}${Math.round(pct)}%` : '?';
  q('.reset').textContent = known ? formatReset(a.resetsAt) : (!a ? 'not detected' : !a.installed ? 'not installed' : '');
  q('.reset').dataset.iso = known ? (a.resetsAt ?? '') : '';
  const tip = [a?.name ?? id,
    known ? `${a.exact ? '' : 'about '}${pct.toFixed(0)}% of the ${a.window ?? 'current'} limit left` : 'usage unknown',
    a?.resetsAt ? `resets ${new Date(a.resetsAt).toLocaleString()}` : '', a?.note ?? ''].filter(Boolean);
  c.el.title = tip.join('\n');
}

function render() {
  if (!settingsKnown) return;
  const ids = chosenIds();
  const box = $('tubes'), glow = glowFor(ids.length);
  for (const [id, c] of cols) if (!ids.includes(id)) { clearTimeout(c.refillTimer); c.tube.destroy(); c.el.remove(); cols.delete(id); }
  ids.forEach((id, i) => {
    let c = cols.get(id);
    if (!c) { c = makeCol(id); cols.set(id, c); }
    if (box.children[i] !== c.el) box.insertBefore(c.el, box.children[i] ?? null);
    updateCol(id, c, usage.find(a => a.id === id), glow);
  });
  window.tubeLoop.maxFps = ids.some(id => usage.find(a => a.id === id)?.active) ? FPS_ACTIVE : FPS_IDLE;
  $('empty').hidden = ids.length > 0;
  $('demo').hidden = !settings.demoMode;
  invoke('resize_sidebar', { tubes: Math.max(1, ids.length) }).catch(console.error);
}

// Reset countdowns tick between polls.
setInterval(() => document.querySelectorAll('.reset[data-iso]').forEach(el => { if (el.dataset.iso) el.textContent = formatReset(el.dataset.iso); }), 30000);

$('settings').addEventListener('click', () => invoke('open_settings').catch(console.error));
$('empty-settings').addEventListener('click', () => invoke('open_settings').catch(console.error));
$('hide').addEventListener('click', () => appWindow.hide());
// Drag the sidebar from anywhere except its buttons.
$('panel').addEventListener('mousedown', e => {
  if (e.button === 0 && !e.target.closest('button')) {
    invoke('drag_began').catch(() => {});   // backend won't reposition the window while it is being dragged
    appWindow.startDragging().catch(() => {});
  }
});

// Listen first, then fetch the initial snapshots. A snapshot is applied only if no event for that
// stream arrived after its request started (otherwise the event is newer and wins).
let settingsRev = 0, usageRev = 0;
(async () => {
  await listen('usage-updated', e => { usageRev++; usage = e.payload; render(); });
  await listen('settings-changed', e => { settingsRev++; settingsKnown = true; settings = e.payload; render(); });
  const sRev = settingsRev, uRev = usageRev;
  invoke('get_settings').then(s => { if (settingsRev === sRev) { settingsKnown = true; settings = s; render(); } }).catch(console.error);
  invoke('get_usage').then(u => { if (usageRev === uRev) { usage = u; render(); } }).catch(console.error);
})();
