/* createTube(config) — glass usage tube, extracted from the AgentMeter tube design.
   Changes from the design: demo page/control panel dropped, svg.destroy() added, exported on window. */
/* ===== Config: edit these first ===== */
const liquidColor  = '#8CFF3A';   // liquid colour at the surface; a deeper shade is derived for the bottom
const neonColor    = '#D946EF';   // cap strips, corner accents and spine strip
const fillLevel    = 0.7;         // 0–1: share of the usage allowance that is LEFT
const state        = 'working';   // 'working' (bubbling) | 'idle' (1–2 slow bubbles)
const bubbleRate   = 2.5;         // new bubbles per second while working
const glowStrength = 1;           // 0–1.5: scales every glow (neon halo, liquid light)
const DEFAULTS = { liquidColor, neonColor, fillLevel, state, bubbleRate, glowStrength, width: 280, height: 720 };

/* ===== Shared geometry (SVG user units; the viewBox is always 280 wide) ===== */
const SVG_NS = 'http://www.w3.org/2000/svg';
const OUTLINE = '#0A0A0C';
const W = 280;                    // viewBox width; its height follows the requested width:height
const GX0 = 51, GX1 = 213;        // glass left and right edges (the spine takes the space on the right)
const LIP = 115;                  // distance from a tube end to the glass-side lip of its cap
const HEADROOM = 60;              // air kept above the 100% mark; the teardrop highlight sits there
const TICKS = 10;                 // scale steps: one tick per 10%, long on the even ones

/* ===== Cap artwork =====
   One facet = [shade, points, mirrored too?]. Shades: up (lit bevel), half, front (base), down (shadow), recess.
   Points describe the TOP cap. The bottom cap flips y and swaps up/down, so light still comes from above. */
const CAP_FACETS = [
  ['half',   '28,108 40,110 38,142 32,139', 1],                           // bracket: outer half
  ['front',  '40,110 50,112 50,137 42,144 38,142', 1],                    // bracket: inner half
  ['down',   '32,139 38,134 45,134 50,137 42,144 38,142', 1],             // bracket tip (the lit top of a bottom pylon)
  ['front',  '94,8 186,8 201,30 201,38 79,38 79,30'],                     // crown: back plate with ears
  ['recess', '85,22 96,17 99,29 88,32', 1],                               // crown: ear notch
  ['half',   '64,36 216,36 216,58 64,58'],                                // collar: deck between the wings
  ['up',     '94,8 186,8 170,14 110,14'],                                 // crown: top bevel
  ['half',   '110,14 170,14 182,45 175,53 105,53 98,45'],                 // crown: front face
  ['recess', '113,40 167,40 169,47 165,51 115,51 111,47'],                // crown: socket for the neon slit
  ['down',   '66,58 214,58 238,62 238,71 189,99 91,99 42,71 42,62'],      // collar: tray interior (holds the neon strip)
  ['down',   '102,58 178,58 170,69 110,69'],                              // collar: step under the crown
  ['front',  '42,71 91,99 189,99 238,71 242,83 210,115 70,115 38,83'],    // collar: chamfered tray rim
  ['up',     '51,30 64,31 66,58 42,62', 1],                               // wing: top bevel
  ['half',   '51,30 42,62 12,70', 1],                                     // wing: outer bevel
  ['front',  '12,70 42,62 42,71 38,83 12,97', 1],                         // wing: face
  ['down',   '12,97 38,83 70,115 34,113', 1],                             // wing: underside
];
const CAP_EDGES_TOP = [           // thin bright edge highlights on the top cap's upper bevels
  [0, '94,8 186,8'], [0, '110,14 170,14'], [0, '110,14 98,45', 1], [0, '64,36 79,36', 1],
  [0, '12,70 51,30 64,31', 1], [0, '51,30 42,62 12,70', 1], [0, '42,62 66,58', 1], [0, '42,71 91,99 189,99 238,71'],
];
const CAP_EDGES_BOTTOM = [        // the edges that face up once the cap is flipped (still in top-cap coordinates)
  [0, '38,83 70,115 210,115 242,83'], [0, '12,97 34,113', 1], [0, '12,97 38,83', 1], [0, '66,58 102,58', 1],
  [0, '98,45 105,53 175,53 182,45'], [0, '32,139 38,134 45,134 50,137', 1],
];
function capMarkup(flip, H, k) {
  const pts = (str, mirror) => str.split(' ').map(p => {
    const [x, y] = p.split(',').map(Number); return `${mirror ? W - x : x},${flip ? H - y : y}`;
  }).join(' ');
  const shade = s => (flip && { up: 'down', down: 'up' }[s]) || s;
  const draw = (list, make) => list.map(([a, p, both]) => make(a, pts(p)) + (both ? make(a, pts(p, true)) : '')).join('');
  return `<g class="${flip ? 'cap-bottom' : 'cap-top'}">
    <g class="ol" fill="${OUTLINE}" stroke-width="${6 * k}">${draw(CAP_FACETS, (s, p) => `<polygon points="${p}"/>`)}</g>
    <g class="ol" stroke-width="1.6">${draw(CAP_FACETS, (s, p) => `<polygon class="f-${shade(s)}" points="${p}"/>`)}</g>
    <g class="hi" stroke-width="${(1.6 * k).toFixed(1)}">${draw(flip ? CAP_EDGES_BOTTOM : CAP_EDGES_TOP, (_, p) => `<polyline points="${p}"/>`)}</g>
  </g>`;
}

/* ===== Colour helpers (plain RGB maths, easy to port) ===== */
const hexToRgb = h => [1, 3, 5].map(i => parseInt(h.slice(i, i + 2), 16));
const rgbToHex = c => '#' + c.map(v => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, '0')).join('');
const mix = (a, b, t) => { const B = hexToRgb(b); return rgbToHex(hexToRgb(a).map((v, i) => v + (B[i] - v) * t)); };
// Deeper, more saturated shade for the bottom of the liquid: weak channels darken most (#8CFF3A → about #3FCF1A).
const deepen = h => { const c = hexToRgb(h), m = Math.max(...c, 1); return rgbToHex(c.map(v => v * (.45 + .36 * (v / m) ** 5))); };

/* ===== Animation constants and curves ===== */
const reduced = matchMedia('(prefers-reduced-motion: reduce)');
const LOW_AT = 0.15, LOW_NEON = '#FF3B30';                            // below 15% the neon turns red and pulses
const REF_BUBBLES = [[-8, 48, 15], [26, 86, 9.5], [-1, 129, 8.5], [25, 144, 6], [-30, 196, 4]];   // reference #5: [x from centre, depth, radius]
const easeInOut = p => p < .5 ? 4 * p ** 3 : 1 - (-2 * p + 2) ** 3 / 2;   // ordinary level changes
const glug = p => 1 - Math.exp(-7 * p) * Math.cos(7.5 * p);              // refill: about 5% overshoot, then settles
const WAVE_C2 = 4200, WAVE_K = 18, WAVE_DAMP = 2.6;                   // surface waves: speed², pull back to flat, damping
const FALL = 1500;                                                    // refill stream speed, units per second
const tubes = [];                                                     // every tube registers its step() here

/* ===== createTube(config) → <svg>, with svg.setConfig(patch) and svg.refill() ===== */
let tubeCount = 0;
function createTube(config = {}) {
  const cfg = { ...DEFAULTS, ...config };
  const id = 'tube' + (++tubeCount);                           // prefix for this tube's filter/gradient ids
  const H = Math.round(W * cfg.height / cfg.width);            // viewBox height: caps keep their shape, the glass stretches
  const k = Math.max(1, Math.min(2, 126 / cfg.width));         // heavier lines when the tube is drawn small
  const kn = 1 + (k - 1) * .6;                                 // same idea for neon thickness

  const gBot = H - LIP - 4;                                    // glass bottom; a dark seat shows between it and the cap
  const yEmpty = gBot - 2.5, yFull = LIP + HEADROOM;           // liquid surface y at 0% and at 100%
  const glassD = inset => {                                    // glass outline: square top (hidden by the cap), rounded bottom
    const a = GX0 + inset, b = GX1 - inset, r = 9 - inset, y = gBot - inset;
    return `M${a} ${LIP - 11}H${b}V${y - r}Q${b} ${y} ${b - r} ${y}H${a + r}Q${a} ${y} ${a} ${y - r}Z`;
  };

  /* --- defs: blur for the neon halo, cylinder gradient for the glass --- */
  const defs = `<defs>
    <filter id="${id}-blurNeon" filterUnits="userSpaceOnUse" x="-40" y="-40" width="${W + 80}" height="${H + 80}"><feGaussianBlur stdDeviation="${5 * k}"/></filter>
    <linearGradient id="${id}-glass" x1="0" x2="1" y1="0" y2="0">
      <stop offset="0" stop-color="#fff" stop-opacity=".17"/><stop offset=".08" stop-color="#fff" stop-opacity=".06"/>
      <stop offset=".5" stop-color="#fff" stop-opacity=".025"/><stop offset=".92" stop-color="#fff" stop-opacity=".06"/>
      <stop offset="1" stop-color="#fff" stop-opacity=".13"/>
    </linearGradient>
    <linearGradient id="${id}-liq" x1="0" x2="0" y1="0" y2="1">
      <stop offset="0" style="stop-color:var(--liq)"/><stop offset=".86" style="stop-color:var(--liq-deep)"/>
      <stop offset="1" style="stop-color:var(--liq-floor)"/>
    </linearGradient>
    <radialGradient id="${id}-inner" cx=".5" cy=".5" r=".6">
      <stop offset="0" style="stop-color:var(--liq)" stop-opacity=".28"/><stop offset="1" style="stop-color:var(--liq)" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="${id}-shade" x1="0" x2="1" y1="0" y2="0">
      <stop offset="0" style="stop-color:var(--liq-ring)" stop-opacity=".6"/><stop offset=".1" style="stop-color:var(--liq-ring)" stop-opacity=".22"/>
      <stop offset=".25" style="stop-color:var(--liq-ring)" stop-opacity="0"/><stop offset=".3" stop-color="#fff" stop-opacity="0"/>
      <stop offset=".37" stop-color="#fff" stop-opacity=".13"/><stop offset=".45" stop-color="#fff" stop-opacity="0"/>
      <stop offset=".72" style="stop-color:var(--liq-ring)" stop-opacity="0"/><stop offset=".9" style="stop-color:var(--liq-ring)" stop-opacity=".22"/>
      <stop offset="1" style="stop-color:var(--liq-ring)" stop-opacity=".55"/>
    </linearGradient>
    <linearGradient id="${id}-ray" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="#fff" stop-opacity=".17"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></linearGradient>
    <linearGradient id="${id}-film" x1="0" x2="0" y1="0" y2="1"><stop offset="0" style="stop-color:var(--liq)" stop-opacity=".16"/><stop offset="1" style="stop-color:var(--liq)" stop-opacity=".38"/></linearGradient>
    <clipPath id="${id}-clip"><path d="${glassD(2.5)}"/></clipPath>
    <filter id="${id}-blurAura" filterUnits="userSpaceOnUse" x="-80" y="-40" width="${W + 160}" height="${H + 80}"><feGaussianBlur stdDeviation="22"/></filter>
    <filter id="${id}-blurLight" filterUnits="userSpaceOnUse" x="0" y="0" width="${W}" height="${H}"><feGaussianBlur stdDeviation="${2.5 * k}"/></filter>
  </defs>`;

  /* --- glow, behind the tube: the liquid's faint light on the background --- */
  const glowBack = `<g class="glow glow-back"><rect class="aura" x="${GX0 - 4}" width="${GX1 - GX0 + 8}" filter="url(#${id}-blurAura)"/></g>`;

  /* --- liquid, bubbles and surface, all clipped to the inside of the glass (back to front):
         wet film · body with cylinder shading, inner glow and light shafts · surface · pour stream · bubbles · front edge --- */
  const CX = (GX0 + GX1) / 2, RX = (GX1 - GX0) / 2 - 2.5, RY = 9 * k;   // surface ellipse centre and radii
  const liquid = `<g clip-path="url(#${id}-clip)">
    <path class="film" fill="url(#${id}-film)"/>
    <g class="liquid">
      <path class="body" fill="url(#${id}-liq)"/><path class="shade" fill="url(#${id}-shade)"/><path class="inner-glow" fill="url(#${id}-inner)"/>
      <g class="rays" fill="url(#${id}-ray)"><path/><path/><path/></g>
    </g>
    <g class="liquid-surface">
      <path class="top"/><path class="back"/>
      <ellipse class="glint" cx="${(CX - RX * .32).toFixed(1)}" cy="${(-RY * .3).toFixed(1)}" rx="${(RX * .3).toFixed(1)}" ry="${(RY * .2).toFixed(1)}"/>
    </g>
    <path class="stream"/>
    <g class="bubbles"></g>
    <g class="liquid-surface liquid-surface-front">
      <path class="under" transform="translate(0 ${3 * k})" stroke-width="${(2.2 * k).toFixed(1)}"/>
      <path class="front" stroke-width="${(4 * k).toFixed(1)}"/>
    </g>
  </g>`;

  /* --- glow, on the glass: red wash for the low state. Glow, on the caps: liquid light on their inner edges --- */
  const glowGlass = `<g class="glow glow-glass"><path class="low" d="${glassD(1.5)}" fill="#FF2D2D" opacity="0"/></g>`;
  const glowCaps = `<g class="glow glow-caps" filter="url(#${id}-blurLight)">
    <path class="cap-light top" d="M72 ${LIP}H208" stroke-width="5"/>
    <path class="cap-light bottom" d="M72 ${H - LIP}H208M50 ${H - 136}V${H - 117}" stroke-width="5"/>
  </g>`;

  /* --- glass: gray-tinted clear cylinder; brighter edges make it read as round --- */
  const glass = `<g class="glass">
    <rect x="${GX0 - 3}" y="${gBot - 8}" width="${GX1 - GX0 + 6}" height="14" fill="${OUTLINE}"/>
    <path class="ol" d="${glassD(0)}" fill="url(#${id}-glass)" stroke-width="${3 * k}"/>
  </g>`;

  /* --- ticks: 10 steps on the inner left, alternating long/short; the top tick is the 100% mark --- */
  const ticks = `<g class="ticks" stroke="#fff" stroke-width="${(4 * k).toFixed(1)}" stroke-linecap="round" opacity=".7">${
    Array.from({ length: TICKS }, (_, i) => {
      const n = i + 1, y = (yEmpty - n / TICKS * (yEmpty - yFull)).toFixed(1);
      return `<line x1="73" x2="${n % 2 ? 90 : 100}" y1="${y}" y2="${y}"/>`;
    }).join('')}</g>`;

  /* --- glass highlights: cap shadow rim, bright edges, teardrop, faint streaks, bottom glints --- */
  const glassHighlight = `<g class="glass-highlight" fill="none" stroke="#fff" stroke-linecap="round">
    <rect x="${GX0}" y="${LIP}" width="${GX1 - GX0}" height="7" fill="${OUTLINE}" stroke="none" opacity=".5"/>
    <path d="M${GX0 + 2.2} 152V${H - 152}" stroke-width="${(3.2 * k).toFixed(1)}" opacity=".95"/>
    <path d="M${GX1 - 2} 152V${H - 152}" stroke-width="${(2 * k).toFixed(1)}" opacity=".55"/>
    <path d="M80 171C81 160 77 152 77 142C77 128 97 126 97 140C97 150 86 156 80 171Z" fill="#fff" stroke="none" opacity=".95"/>
    <path d="M63.5 190V${H - 190}" stroke-width="2.2" opacity=".14"/>
    <path d="M200 150V${H - 175}" stroke-width="2" opacity=".07"/>
    <path d="M68 ${gBot - 9}h13" stroke-width="4" opacity=".7"/>
    <path d="M${GX1 - 24} ${gBot - 6}Q${GX1 - 8} ${gBot - 7} ${GX1 - 7} ${gBot - 22}" stroke-width="2.6" opacity=".55"/>
  </g>`;

  /* --- spine: black bar on the right, from cap to cap (its neon strip lives in the neon group) --- */
  const spine = `<g class="spine">
    <rect class="ol f-recess" x="224" y="108" width="27" height="${H - 216}" stroke-width="${3 * k}"/>
    <rect class="f-front" x="225.5" y="108" width="5" height="${H - 216}"/>
  </g>`;

  /* --- neon: every strip is a round-capped line, drawn three times (blurred halo, colour body, hot centre) --- */
  const neonLines = [[238, 156, 238, H - 156, 8]];                                            // spine strip
  for (const f of [y => y, y => H - y]) neonLines.push(                                       // top cap, then mirrored bottom cap
    [119, f(45.5), 161, f(45.5), 6.5],                                                        // crown slit
    [99, f(88), 181, f(88), 11],                                                             // collar strip
    [49, f(66), 55, f(76), 3.6], [W - 49, f(66), W - 55, f(76), 3.6],                         // corner accents
    [48.5, f(117), 48.5, f(135), 1.8, .55], [W - 48.5, f(117), W - 48.5, f(135), 1.8, .55]);  // rim light on the brackets
  const neonLayer = (cls, grow, attrs = '') => `<g class="${cls}" ${attrs}>${neonLines.map(([x1, y1, x2, y2, w, a = 1]) =>
    `<line x1="${x1}" y1="${y1}" x2="${x2}" y2="${y2}" stroke-width="${(w * grow * kn).toFixed(1)}" opacity="${a}"/>`).join('')}</g>`;
  const neon = `<g class="neon">${neonLayer('neon-glow', 2.2, `filter="url(#${id}-blurNeon)"`)}
    <g class="neon-core">${neonLayer('body', 1)}${neonLayer('hot', .55)}</g></g>`;

  /* --- assemble (back to front) --- */
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('class', 'tube');
  svg.setAttribute('viewBox', `0 0 ${W} ${H}`);
  svg.setAttribute('width', cfg.width); svg.setAttribute('height', cfg.height);
  svg.setAttribute('role', 'img');
  svg.innerHTML = defs + glowBack + spine + glass + liquid + ticks + glowGlass + glassHighlight
    + capMarkup(false, H, k) + capMarkup(true, H, k) + glowCaps + neon;
  const q = sel => svg.querySelector(sel);
  const all = sel => [...svg.querySelectorAll(sel)];
  const bodyEls = all('.liquid > path'), frontEls = all('.liquid-surface-front path'), rays = all('.rays path');
  const topFill = q('.liquid-surface .top'), backLine = q('.liquid-surface .back'), glint = q('.glint');
  const film = q('.film'), stream = q('.stream'), aura = q('.aura');
  const lightTop = q('.cap-light.top'), lightBottom = q('.cap-light.bottom');

  /* --- colours → CSS variables on this tube --- */
  function applyColors() {
    const liq = cfg.liquidColor, deep = deepen(liq), set = (name, v) => svg.style.setProperty(name, v);
    set('--liq', liq); set('--liq-deep', deep);
    set('--liq-floor', mix(deep, liq, .55));               // slight lift at the very bottom
    set('--liq-top', mix(liq, '#ffffff', .3));             // meniscus fill and bubbles
    set('--liq-light', mix(liq, '#ffffff', .58));          // bright surface line
    set('--liq-ring', mix(deep, '#000000', .38));           // bubble outline, back of the meniscus
    const neonNow = mix(isLow ? LOW_NEON : cfg.neonColor, '#ffffff', .6 * flash);   // red when low, whitened by a flash
    set('--neon', neonNow); set('--neon-hot', mix(neonNow, '#ffffff', .72));
  }

  /* --- surface simulation: a row of liquid columns joined by springs (1-D wave equation).
         Popping bubbles, the refill stream and level changes kick it; the glass walls reflect the waves. --- */
  const M = 32;                                             // segments across the glass (M + 1 columns)
  const U = Array.from({ length: M + 1 }, (_, i) => -1 + 2 * i / M);                    // column position: −1 left wall … 1 right wall
  const ARC = U.map(u => Math.sqrt(1 - u * u));                                          // ellipse profile of the surface
  const CLIMB = U.map(u => -3.2 * k * Math.exp(-(1 - Math.abs(u)) * RX / (4.5 * k)));    // meniscus: liquid creeps up the glass
  const LINGER = U.map((_, i) => .5 + .5 * Math.sin(i * .34 + 1) * Math.sin(i * .15 + 2)); // where the wet film hangs on longest
  const wave = { h: new Float32Array(M + 1), v: new Float32Array(M + 1) };               // height offset and velocity per column
  const colAt = x => Math.max(0, Math.min(M, Math.round((x - CX + RX) / (2 * RX) * M)));
  function kick(x, amount) {                                // push the surface around x (a soft bump about five columns wide)
    const j = colAt(x);
    for (let i = Math.max(0, j - 5); i <= Math.min(M, j + 5); i++) wave.v[i] += amount * Math.exp(-((i - j) ** 2) / 9);
  }
  const slosh = amount => { for (let i = 0; i <= M; i++) wave.v[i] += amount * U[i]; };   // tip the whole surface side to side
  function stepWave(dt) {
    const { h, v } = wave, n = Math.ceil(dt / .006), sub = dt / n;                       // small sub-steps keep it stable
    for (let pass = 0; pass < n; pass++) {
      for (let i = 0; i <= M; i++) {
        const l = h[i ? i - 1 : 1], r = h[i < M ? i + 1 : M - 1];
        v[i] += (WAVE_C2 * (l + r - 2 * h[i]) - WAVE_K * h[i] - WAVE_DAMP * v[i]) * sub;
      }
      let mean = 0;
      for (let i = 0; i <= M; i++) { h[i] = Math.max(-16 * k, Math.min(16 * k, h[i] + v[i] * sub)); mean += h[i] / (M + 1); }
      for (let i = 0; i <= M; i++) h[i] -= mean;                                         // the volume stays constant
    }
  }

  /* --- level + waves → paths. Front arc = near edge of the surface, back arc = far edge; the body hangs from the front arc --- */
  let shown = cfg.fillLevel, lastS = -1, filmTop = null;   // level on screen; last drawn surface y; top of the wet film
  const surfaceY = level => yEmpty - level * (yEmpty - yFull);
  function drawLiquid(level, t) {
    const s = surfaceY(level), has = level > .004, { h } = wave, x0 = (CX - RX).toFixed(1);
    let front = '', back = '';
    for (let i = 0; i <= M; i++) {
      const x = (CX + RX * U[i]).toFixed(1);
      const far = h[i] * (1 - ARC[i]) + .5 * h[M - i] * ARC[i];                          // far edge: tied to the near edge at the walls
      front += `${i ? 'L' : 'M'}${x} ${(s + RY * ARC[i] + h[i] + CLIMB[i]).toFixed(1)}`;
      back = `L${x} ${(s - RY * ARC[i] + far + CLIMB[i]).toFixed(1)}` + back;            // right to left, so it closes the front arc
    }
    const bodyD = has ? `${front}V${yEmpty}H${x0}Z` : '';
    for (const el of bodyEls) el.setAttribute('d', bodyD);
    topFill.setAttribute('d', has ? front + back + 'Z' : '');
    backLine.setAttribute('d', has ? 'M' + back.slice(1) : '');
    for (const el of frontEls) el.setAttribute('d', has ? front : '');
    glint.setAttribute('transform', `translate(0 ${(s + h[colAt(CX - RX * .32)] * .6).toFixed(1)})`);
    glint.style.display = has ? '' : 'none';
    // Wet film: glass the liquid has just left stays coated for a moment, in tongues that run down to the surface.
    const wetSpan = filmTop === null ? 0 : s - filmTop;
    let filmD = '';
    if (wetSpan > 1) {
      const top = i => s - wetSpan * (.5 + .5 * LINGER[i]), px = i => CX + RX * U[i];
      filmD = `M${x0} ${top(0).toFixed(1)}`;
      for (let i = 1; i < M; i++)                           // curve through the midpoints so the runs have rounded heads
        filmD += `Q${px(i).toFixed(1)} ${top(i).toFixed(1)} ${((px(i) + px(i + 1)) / 2).toFixed(1)} ${((top(i) + top(i + 1)) / 2).toFixed(1)}`;
      filmD += `L${px(M).toFixed(1)} ${top(M).toFixed(1)}V${(s + RY).toFixed(1)}H${x0}Z`;
    }
    film.setAttribute('d', filmD);
    // Light shafts: the rippled surface focuses light into soft bands that sway below it.
    const len = Math.min((yEmpty - s) * .8, 190 * k);
    rays.forEach((el, i) => {
      const x = CX + RX * (-.52 + .5 * i) + 9 * Math.sin(t * .45 + i * 2.1), j = colAt(x), y = s + RY * ARC[j] + h[j];
      const w = (6 + 2.5 * Math.sin(t * .8 + i)) * k, lean = 12 * Math.sin(t * .3 + i * 1.3);
      el.setAttribute('d', has && len > 14 ? `M${(x - w).toFixed(1)} ${y.toFixed(1)}h${(2 * w).toFixed(1)}L${(x + 2.4 * w + lean).toFixed(1)} ${(y + len).toFixed(1)}h${(-4.8 * w).toFixed(1)}Z` : '');
    });
    if (Math.abs(s - lastS) > .05) {                       // glow shapes only move when the level does
      lastS = s;
      aura.setAttribute('y', s.toFixed(1)); aura.setAttribute('height', Math.max(0, yEmpty - s).toFixed(1));
    }
    const g = cfg.glowStrength, lv = Math.max(0, Math.min(1, level)), wet = Math.min(1, lv * 8);
    aura.style.opacity = (.12 * g * wet).toFixed(3);
    lightBottom.style.opacity = (.75 * g * wet).toFixed(3);          // bottom cap is lit whenever there is liquid
    lightTop.style.opacity = (.75 * g * lv ** 3).toFixed(3);         // top cap only once the liquid gets near it
  }

  /* --- bubbles: unit circle + shade arc + white glint, moved with a transform only.
         States: stuck (clinging to the glass) → rise (buoyant) → dome (resting at the surface) → pop (a ring) --- */
  const bubblesG = q('.bubbles'), bubbles = [];
  const SITES = Array.from({ length: 4 }, () => CX + (Math.random() * 2 - 1) * (RX - 16 * k));   // nucleation points on the glass floor
  function addBubble(x, y, r, extra) {
    const g = document.createElementNS(SVG_NS, 'g');
    g.innerHTML = '<circle r="1"/><path d="M.78-.3A.84.84 0 0 1-.3.78"/><ellipse cx="-.36" cy="-.4" rx=".3" ry=".19" transform="rotate(-38 -.36 -.4)"/>';
    bubblesG.append(g);
    // z = where on the surface it comes up (1 near edge … −1 far edge); pace slows idle bubbles
    bubbles.push({ g, x, y, r, v: 0, z: Math.random() * 1.5 - .5, phase: Math.random() * 6.28, state: 'rise', timer: 0, pace: 1, ...extra });
  }
  function placeBubble(b, t) {
    const big = Math.min(1, b.r / (12 * k));               // large bubbles wobble sideways and flatten; small ones go straight
    const x = b.x + (b.state === 'rise' ? Math.sin(t * (7 - 4 * big) + b.phase) * b.r * (.12 + .4 * big) : 0);
    const flat = b.state === 'dome' ? .2 : b.state === 'pop' ? .45 : big * (.07 + .07 * Math.sin(t * 9 + b.phase));
    const size = b.r * (b.state === 'pop' ? 1 + b.timer * .9 : 1);
    b.g.setAttribute('transform', `translate(${x.toFixed(1)} ${b.y.toFixed(1)}) scale(${(size * (1 + flat)).toFixed(2)} ${(size * (1 - flat)).toFixed(2)})`);
    b.g.style.opacity = (b.state === 'pop' ? 1 - b.timer : 1).toFixed(2);
  }
  function spawnBubble(slow) {                             // most bubbles stream up from the same few points on the floor
    const r = (slow ? 2.5 + Math.random() * 2.5 : Math.random() < .1 ? 9 + Math.random() * 5 : 2.2 + Math.random() ** 2 * 6) * k;
    const x = Math.random() < .75 ? SITES[Math.random() * SITES.length | 0] + (Math.random() - .5) * 5 * k : CX + (Math.random() * 2 - 1) * (RX - r - 5);
    addBubble(x, yEmpty + r, r, { pace: slow ? .4 : 1 });
  }
  function seedBubbles() {                                 // the still arrangement from reference #5 (all there is under reduced motion)
    for (const b of bubbles) b.g.remove();
    bubbles.length = 0;
    const s = surfaceY(shown);
    for (const [dx, depth, r] of REF_BUBBLES.slice(0, cfg.state === 'working' ? 5 : 1))
      if (s + (depth + r) * k < yEmpty - 2) addBubble(CX + dx, s + depth * k, r * k, { z: 1 });
    for (const b of bubbles) placeBubble(b, 0);
  }

  /* --- one animation step (dt seconds since the last one, t = clock in seconds) --- */
  const neonGlow = q('.neon-glow'), neonCore = q('.neon-core'), lowGlow = q('.glow-glass .low');
  let tween = null, pour = null, flash = 0, isLow = false, flickerEnd = 0, nextFlicker = 4 + Math.random() * 8, idleWait = 1;
  function step(dt, t) {
    const still = reduced.matches, working = cfg.state === 'working', before = surfaceY(shown);
    // 1. Level: ease toward the target, or jump straight there when motion is reduced.
    if (still) { shown = cfg.fillLevel; tween = null; }
    else if (tween) {
      tween.p = Math.min(1, tween.p + dt / tween.dur);
      shown = tween.from + (tween.to - tween.from) * tween.ease(tween.p);
      if (tween.p >= 1) tween = null;
    }
    const s = surfaceY(shown);
    // 2. Surface: a moving level churns it, a working agent keeps it faintly restless, then the waves advance.
    if (still || !dt) { wave.h.fill(0); wave.v.fill(0); }
    else {
      const speed = Math.abs(s - before) / dt;
      if (speed > 25) kick(CX + (Math.random() * 2 - 1) * RX, (Math.random() - .5) * Math.min(500, speed) * .25 * k);
      if (Math.random() < (working ? 5 : .5) * dt) kick(CX + (Math.random() * 2 - 1) * RX, (Math.random() - .5) * 30 * k);
      stepWave(dt);
    }
    // 3. Wet film: it stays where the surface was, then drains down toward it (fast at first, then a slow tail).
    if (still || filmTop === null || s <= filmTop) filmTop = s;
    else filmTop = Math.min(s, filmTop + (14 * k + (s - filmTop) * 1.4) * dt);
    // 4. Refill stream: falls from the top cap while the level rises, churns the surface, drags small bubbles under.
    let streamD = '';
    if (still) pour = null;
    if (pour) {
      pour.age += dt;
      if (pour.tail === null && !(tween && tween.ease === glug && tween.p < .28)) pour.tail = LIP;   // tap closes: the tail falls away
      if (pour.tail !== null) pour.tail += FALL * k * dt;
      const y0 = pour.tail ?? LIP, y1 = Math.min(s, LIP + pour.age * FALL * k);
      if (y0 >= s) pour = null;
      else {
        if (y1 >= s) {
          kick(pour.x, (Math.random() - .35) * 130 * k);
          if (Math.random() < 45 * dt)
            addBubble(pour.x + (Math.random() - .5) * 26 * k, s + (8 + Math.random() * 30) * k, (1.5 + Math.random() * 3.5) * k, { v: -(40 + Math.random() * 110) * k });
        }
        let left = '', right = '';
        for (let i = 0; i <= 8; i++) {                      // a column that thins as it falls, with a quick shimmer on its edges
          const y = y0 + (y1 - y0) * i / 8, w = (6.5 - 2.5 * Math.min(1, (y - LIP) / (300 * k)) + .9 * Math.sin(y * .07 / k - t * 22)) * k;
          left += `${i ? 'L' : 'M'}${(pour.x - w).toFixed(1)} ${y.toFixed(1)}`;
          right = `L${(pour.x + w).toFixed(1)} ${y.toFixed(1)}` + right;
        }
        streamD = left + right + 'Z';
      }
    }
    stream.setAttribute('d', streamD);
    drawLiquid(shown, t);
    // 5. Bubbles: a stream from the floor while working (plus a few that nucleate on the glass); one or two slow ones while idle.
    if (!still && shown > .04) {
      const depth = yEmpty - s;
      if (working) {
        if (Math.random() < cfg.bubbleRate * dt) spawnBubble(false);
        if (depth > 50 * k && Math.random() < .4 * dt && bubbles.filter(b => b.state === 'stuck').length < 6)
          addBubble(CX + (Math.random() < .5 ? -1 : 1) * (RX - (4 + Math.random() * 9) * k), s + 25 * k + Math.random() * (depth - 40 * k),
            (1.1 + Math.random() * 1.2) * k, { state: 'stuck', timer: 2 + Math.random() * 7, z: 1 });
      } else if (bubbles.length < 2 && (idleWait -= dt) <= 0) { spawnBubble(true); idleWait = 2.5 + Math.random() * 5; }
    }
    for (let i = bubbles.length - 1; i >= 0; i--) {
      const b = bubbles[i], j = colAt(b.x), top = s + RY * b.z * ARC[j] + wave.h[j] + CLIMB[j];   // where this bubble meets the surface
      if (b.state === 'stuck') { b.r += .2 * k * dt; if ((b.timer -= dt) <= 0) b.state = 'rise'; }   // grows on the glass, then lets go
      if (b.state === 'rise') {                             // buoyancy: speeds up to a terminal velocity that grows with size
        b.v += ((18 + 24 * Math.sqrt(b.r / k)) * k * b.pace - b.v) * Math.min(1, dt * 3.5);
        b.y -= b.v * dt; b.r *= 1 + .06 * dt;               // and swells a little as the pressure drops
      }
      if ((b.state === 'rise' || b.state === 'stuck') && b.y - b.r * .3 <= top) { b.state = 'dome'; b.timer = .06 + Math.random() * .45; }
      else if (b.state === 'dome') {                        // rests half out of the surface, riding the waves, then bursts
        b.y = top + b.r * .15;
        if ((b.timer -= dt) <= 0) { b.state = 'pop'; b.timer = 0; b.g.setAttribute('class', 'pop'); kick(b.x, -(40 + 6 * b.r / k) * k * (.4 + .6 * Math.max(0, b.z))); }
      } else if (b.state === 'pop') b.timer += dt / .22;    // the ring widens and fades
      if (b.state === 'pop' && b.timer >= 1) { b.g.remove(); bubbles.splice(i, 1); } else placeBubble(b, t);
    }
    // 6. Neon: slow breathing, a rare tiny flicker, a flash on refill; red and pulsing below LOW_AT.
    const low = shown < LOW_AT, flashing = flash > 0;
    flash = Math.max(0, flash - dt / .7);
    if (low !== isLow || flashing) { isLow = low; applyColors(); }
    if (!still && t > nextFlicker) { flickerEnd = t + .06 + Math.random() * .07; nextFlicker = t + 7 + Math.random() * 12; }
    const pulse = .5 + .5 * Math.sin(t * 7);
    let glowAmt = still ? 1 : low ? .35 + .65 * pulse : .86 + .14 * Math.sin(t * 1.25);
    if (t < flickerEnd) glowAmt *= .4;
    neonGlow.style.opacity = Math.min(1, cfg.glowStrength * (.72 * glowAmt + flash)).toFixed(3);
    neonCore.style.opacity = Math.min(1, .3 + .7 * glowAmt + flash).toFixed(3);
    lowGlow.setAttribute('opacity', low ? (cfg.glowStrength * (still ? .13 : .07 + .09 * pulse)).toFixed(3) : 0);
  }

  /* --- public API --- */
  const redrawIfStill = () => { if (reduced.matches) { step(0, 0); seedBubbles(); } };   // no loop is running then
  svg.setConfig = patch => {                               // change any config value; a new fillLevel eases in over 1 s
    const before = cfg.fillLevel;
    Object.assign(cfg, patch);
    if (cfg.fillLevel !== before) { tween = { from: shown, to: cfg.fillLevel, p: 0, dur: 1, ease: easeInOut }; slosh((cfg.fillLevel > before ? 8 : -8) * k); }
    applyColors(); redrawIfStill();
  };
  svg.refill = () => {                                     // limit window reset: pour in, glug up to full, slosh, flash the neon once
    cfg.fillLevel = 1; tween = { from: shown, to: 1, p: 0, dur: 2, ease: glug }; flash = 1;
    pour = { age: 0, tail: null, x: CX + 16 }; slosh(30 * k);
    redrawIfStill();
  };

  applyColors(); step(0, 0); seedBubbles();
  const entry = { step, seedBubbles };
  tubes.push(entry);
  svg.destroy = () => {                                    // stop animating and detach (AgentMeter addition)
    const i = tubes.indexOf(entry); if (i >= 0) tubes.splice(i, 1);
    svg.remove();
  };
  return svg;
}

/* ===== One shared animation loop for every tube (a single still frame under reduced motion) ===== */
let lastFrame = 0;
// AgentMeter additions: the sidebar caps the frame rate, and pauses the loop entirely while hidden.
const tubeLoop = {
  maxFps: 60, paused: false,
  pause() { this.paused = true; },
  resume() { if (!this.paused) return; this.paused = false; lastFrame = 0; if (!reduced.matches) requestAnimationFrame(frame); },
};
function frame(now) {
  if (tubeLoop.paused) return;                                         // loop stops; resume() restarts it
  if (!reduced.matches && lastFrame && now - lastFrame < 1000 / tubeLoop.maxFps - 2) { requestAnimationFrame(frame); return; }
  const dt = reduced.matches ? 0 : Math.min(.05, (now - lastFrame) / 1000);
  lastFrame = now;
  for (const tube of tubes) tube.step(dt, now / 1000);
  if (!reduced.matches) requestAnimationFrame(frame);
}
reduced.addEventListener('change', () => { tubes.forEach(tube => tube.seedBubbles()); if (!tubeLoop.paused) requestAnimationFrame(frame); });
requestAnimationFrame(frame);

window.createTube = createTube;
window.tubeLoop = tubeLoop;
