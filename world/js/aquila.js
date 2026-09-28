// The Aquila: one heraldic eagle, generated as SVG path data so the loading
// screen, the Forum mosaic and the banners draw the same bird. It appears in
// few places on purpose; it is the institution's seal, not decoration.

function pt([x, y]) {
  return `${x.toFixed(1)} ${y.toFixed(1)}`;
}

function serrated(start, tips, end, inset) {
  // A feathered edge: tips joined through notches pulled toward `inset`.
  const parts = [`M${pt(start)}`];
  let prev = start;
  for (const tip of tips) {
    const mid = [(prev[0] + tip[0]) / 2, (prev[1] + tip[1]) / 2];
    const notch = [mid[0] + (inset[0] - mid[0]) * 0.16, mid[1] + (inset[1] - mid[1]) * 0.16];
    parts.push(`L${pt(notch)}`, `L${pt(tip)}`);
    prev = tip;
  }
  parts.push(`L${pt(end)}`, 'Z');
  return parts.join(' ');
}

// A raised wing: seven long flight feathers fanning up and out from the
// wing's leading edge, their bases hidden under a shield-shaped covert.
function wing(mirror) {
  const m = ([x, y]) => (mirror ? [200 - x, y] : [x, y]);
  const parts = [];
  const n = 7;
  for (let i = 0; i < n; i++) {
    const t = i / (n - 1);
    const ang = (-98 + t * 80) * (Math.PI / 180);
    const len = 50 + Math.sin(t * Math.PI) * 22;
    const w = 12 - t * 3;
    const base = [118 + t * 34, 66 - t * 10];
    const d = [Math.cos(ang), Math.sin(ang)];
    const nrm = [-d[1], d[0]];
    const P = (a, b) => [base[0] + d[0] * a + nrm[0] * b, base[1] + d[1] * a + nrm[1] * b];
    const pts = [P(0, -w / 2), P(len * 0.8, -w * 0.45), P(len, 0), P(len * 0.8, w * 0.45), P(0, w / 2)].map(m);
    parts.push(`M${pts.map(pt).join(' L')} Z`);
  }
  const covert = [
    [104, 82],
    [110, 64],
    [128, 54],
    [152, 52],
    [160, 64],
    [146, 80],
    [120, 94],
  ].map(m);
  parts.push(`M${covert.map(pt).join(' L')} Z`);
  return parts;
}

// Each part is filled on its own, so overlaps never cancel.
export function aquilaParts() {
  const body = 'M100 66 C118 70 122 104 114 128 C110 140 90 140 86 128 C78 104 82 70 100 66 Z';
  const neck = 'M90 40 C102 38 108 54 108 72 L92 74 C88 60 86 50 90 40 Z';
  const head = 'M92 26 C102 24 108 32 106 42 C104 48 96 50 90 46 L76 46 C74 43 74 40 78 38 L70 38 C74 32 82 28 92 26 Z';
  const tail = [
    [82, 170],
    [92, 178],
    [100, 181],
    [108, 178],
    [118, 170],
  ];
  const notched = [];
  let prev = [90, 130];
  for (const p of tail) {
    notched.push([(prev[0] + p[0]) / 2 + (100 - (prev[0] + p[0]) / 2) * 0.12, (prev[1] + p[1]) / 2 - 6], p);
    prev = p;
  }
  const tailPath = `M90 130 ${notched.map((p) => `L${pt(p)}`).join(' ')} L110 130 Z`;
  const bar = 'M66 146 L134 146 L131 154 L69 154 Z';
  return [...wing(false), ...wing(true), tailPath, body, neck, head, bar];
}

export function aquilaPath() {
  return aquilaParts().join(' ');
}

// Eye punched out separately so it can take the ground color.
export const AQUILA_EYE = { x: 92, y: 35, r: 2.6 };

export function aquilaSvg({ fill = '#c99a4b', size = 120, eye = null } = {}) {
  // The ground-colored stroke engraves a line between overlapping parts.
  const stroke = eye ? ` stroke="${eye}" stroke-width="3" stroke-linejoin="round"` : '';
  const paths = aquilaParts().map((d) => `<path d="${d}"${stroke}/>`).join('');
  const e = eye ? `<circle cx="${AQUILA_EYE.x}" cy="${AQUILA_EYE.y}" r="${AQUILA_EYE.r}" fill="${eye}"/>` : '';
  return `<svg viewBox="0 0 200 200" width="${size}" height="${size}" aria-hidden="true"><g fill="${fill}">${paths}</g>${e}</svg>`;
}

// Draws the eagle into a canvas context inside a box.
export function drawAquila(ctx, x, y, size, fill, eyeFill) {
  ctx.save();
  ctx.translate(x, y);
  ctx.scale(size / 200, size / 200);
  ctx.fillStyle = fill;
  ctx.lineWidth = 3;
  ctx.lineJoin = 'round';
  ctx.strokeStyle = eyeFill || fill;
  for (const d of aquilaParts()) {
    const path = new Path2D(d);
    ctx.fill(path);
    if (eyeFill) ctx.stroke(path);
  }
  if (eyeFill) {
    ctx.fillStyle = eyeFill;
    ctx.beginPath();
    ctx.arc(AQUILA_EYE.x, AQUILA_EYE.y, AQUILA_EYE.r, 0, Math.PI * 2);
    ctx.fill();
  }
  ctx.restore();
}
