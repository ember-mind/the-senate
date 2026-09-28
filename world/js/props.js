// Authored props: workstations, the campaign board, banners, the Censor's
// table, lamps. Props that show state expose small handles (screen, lamps,
// seal, tablets) that the director toggles; none animate on their own.

import * as THREE from '../vendor/three.module.min.js';
import { box } from './architecture.js';
import { drawAquila } from './aquila.js';
import { PALETTE, std } from './materials.js';

function mesh(g, m, { cast = true, receive = true } = {}) {
  const x = new THREE.Mesh(g, m);
  x.castShadow = cast;
  x.receiveShadow = receive;
  return x;
}

// ---------------------------------------------------------------------------
// Screens: a few shared animated canvases, one per kind of activity, so ten
// desks cost the same as one.

class ScreenFeed {
  constructor(kind) {
    this.kind = kind;
    this.canvas = document.createElement('canvas');
    this.canvas.width = 256;
    this.canvas.height = 160;
    this.ctx = this.canvas.getContext('2d');
    this.texture = new THREE.CanvasTexture(this.canvas);
    this.texture.colorSpace = THREE.SRGBColorSpace;
    this.lines = [];
    this.tick = 0;
    this.seed = 7;
    for (let i = 0; i < 14; i++) this.lines.push(this.newLine());
    this.draw();
  }

  rand() {
    this.seed = (this.seed * 16807) % 2147483647;
    return this.seed / 2147483647;
  }

  newLine() {
    const indent = Math.floor(this.rand() * 4) * 12;
    const segs = [];
    let x = indent;
    const n = 1 + Math.floor(this.rand() * 4);
    for (let i = 0; i < n; i++) {
      const w = 10 + this.rand() * 50;
      segs.push([x, w, this.rand()]);
      x += w + 6;
    }
    return segs;
  }

  draw() {
    // Parchment on a writing slope: ink appears line by line as the Cohort
    // writes. Nothing glows; the desk's oil lamp is the light.
    const { ctx } = this;
    const W = 256;
    const H = 160;
    ctx.fillStyle = this.kind === 'off' ? '#cdbb96' : '#e8d9b5';
    ctx.fillRect(0, 0, W, H);
    ctx.fillStyle = 'rgba(120,90,50,0.10)';
    for (let i = 0; i < 40; i++) ctx.fillRect((i * 67) % W, (i * 41) % H, 30 + (i % 5) * 9, 1);
    ctx.strokeStyle = 'rgba(110,80,40,0.35)';
    ctx.lineWidth = 2;
    ctx.strokeRect(6, 6, W - 12, H - 12);
    const ink = '#2b1d12';
    const scribble = (x, y, w, seed) => {
      // A line of handwriting: a wavy stroke with small loops.
      ctx.beginPath();
      ctx.moveTo(x, y);
      for (let k = 0; k <= w; k += 3) {
        const r = Math.sin((k + seed * 13) * 0.9) * 2.2 + Math.sin((k + seed * 7) * 0.31) * 1.2;
        ctx.lineTo(x + k, y + r);
      }
      ctx.stroke();
    };
    ctx.strokeStyle = ink;
    ctx.lineWidth = 1.6;
    ctx.lineCap = 'round';
    if (this.kind === 'code') {
      const total = 11;
      const written = this.tick % (total * 6);
      for (let row = 0; row < total; row++) {
        const len = [190, 160, 200, 120, 180, 196, 90, 170, 186, 140, 110][row];
        const shown = Math.max(0, Math.min(len, (written - row * 6) * 32));
        if (shown > 0) scribble(18 + (row % 3 === 1 ? 14 : 0), 20 + row * 12.5, shown, row);
      }
    } else if (this.kind === 'read') {
      for (let row = 0; row < 11; row++) scribble(18, 20 + row * 12.5, row % 4 === 3 ? 110 : 200, row + 3);
      ctx.strokeStyle = 'rgba(163,38,31,0.7)';
      ctx.lineWidth = 3;
      const r = this.tick % 11;
      ctx.beginPath();
      ctx.moveTo(10, 16 + r * 12.5);
      ctx.lineTo(10, 26 + r * 12.5);
      ctx.stroke();
    } else if (this.kind === 'test') {
      for (let row = 0; row < 8; row++) {
        const done = row < this.tick % 10;
        scribble(38, 22 + row * 16, 120 + ((row * 37) % 60), row + 5);
        ctx.strokeStyle = done ? '#3d5a22' : 'rgba(43,29,18,0.25)';
        ctx.lineWidth = 2.4;
        ctx.beginPath();
        ctx.moveTo(18, 22 + row * 16);
        ctx.lineTo(23, 27 + row * 16);
        ctx.lineTo(31, 15 + row * 16);
        ctx.stroke();
        ctx.strokeStyle = ink;
        ctx.lineWidth = 1.6;
      }
    } else if (this.kind === 'halt') {
      for (let row = 0; row < 5; row++) scribble(18, 22 + row * 12.5, 180, row + 9);
      ctx.fillStyle = '#a3261f';
      ctx.beginPath();
      ctx.arc(W / 2, 118, 22, 0, Math.PI * 2);
      ctx.fill();
      ctx.fillStyle = '#f3d7b0';
      ctx.font = 'bold 28px Georgia, serif';
      ctx.textAlign = 'center';
      ctx.fillText('?', W / 2, 128);
    }
    this.texture.needsUpdate = true;
  }

  step() {
    this.tick += 1;
    if (this.kind === 'code') {
      this.lines.shift();
      this.lines.push(this.newLine());
    }
    this.draw();
  }
}

export const FEEDS = {
  code: new ScreenFeed('code'),
  read: new ScreenFeed('read'),
  test: new ScreenFeed('test'),
  halt: new ScreenFeed('halt'),
  off: new ScreenFeed('off'),
};

let feedClock = 0;
export function stepFeeds(dt) {
  feedClock += dt;
  if (feedClock < 0.22) return;
  feedClock = 0;
  for (const feed of Object.values(FEEDS)) if (feed.kind !== 'off') feed.step();
}

const screenMats = new Map();
function screenMaterial(kind) {
  if (!screenMats.has(kind)) {
    const tex = FEEDS[kind].texture;
    screenMats.set(
      kind,
      new THREE.MeshStandardMaterial({ map: tex, roughness: 0.92 }),
    );
  }
  return screenMats.get(kind);
}

// ---------------------------------------------------------------------------

// One Cohort workstation: desk, stool, a bronze-framed tablet-terminal, two
// reading tablets, the verification apparatus (a frame of lamps that light
// in sequence while the repository's checks run), and a red wax seal that
// stands up when the Order needs a person.
export function makeWorkstation(M) {
  const g = new THREE.Group();
  g.name = 'workstation';
  // Desk: thick top on two stone trestles.
  const top = mesh(box(2.3, 0.12, 1.05, 1.5), M.wood);
  top.position.y = 0.86;
  g.add(top);
  for (const x of [-0.9, 0.9]) {
    const leg = mesh(box(0.18, 0.8, 0.85, 1), M.stone);
    leg.position.set(x, 0.4, 0);
    g.add(leg);
  }
  // Stool behind the desk (worker sits at +z, facing -z).
  const stool = new THREE.Group();
  const seatTop = mesh(box(0.5, 0.07, 0.42, 1), M.wood);
  seatTop.position.y = 0.4;
  stool.add(seatTop);
  for (const [x, z] of [[-0.2, -0.16], [0.2, -0.16], [-0.2, 0.16], [0.2, 0.16]]) {
    const leg = mesh(box(0.05, 0.38, 0.05, 1), M.darkWood);
    leg.position.set(x, 0.19, z);
    stool.add(leg);
  }
  stool.position.set(0, 0, 0.72);
  g.add(stool);

  // Writing slope: a wooden board tilted toward the worker, holding the
  // sheet the Cohort writes on. Inkwell and quill beside it.
  const slope = mesh(box(0.86, 0.04, 0.6, 1), M.darkWood);
  slope.position.set(0, 1.0, -0.12);
  slope.rotation.x = 0.32;
  g.add(slope);
  const screen = mesh(new THREE.PlaneGeometry(0.74, 0.5), screenMaterial('off'), { cast: false });
  screen.position.set(0, 1.024, -0.11);
  screen.rotation.x = -Math.PI / 2 + 0.32;
  g.add(screen);
  const inkwell = mesh(new THREE.CylinderGeometry(0.045, 0.055, 0.08, 10), M.bronze);
  inkwell.position.set(0.58, 0.96, 0.05);
  g.add(inkwell);
  const quill = mesh(new THREE.ConeGeometry(0.018, 0.34, 5), M.parchment);
  quill.position.set(0.6, 1.1, 0.04);
  quill.rotation.set(0.2, 0, -0.35);
  g.add(quill);
  // Oil lamp: lit only while a Cohort holds the desk.
  const lampBody = mesh(new THREE.SphereGeometry(0.08, 10, 8), M.bronze);
  lampBody.scale.set(1.4, 0.55, 1);
  lampBody.position.set(-0.72, 0.95, -0.28);
  g.add(lampBody);
  const deskFlame = flameMesh(0.45, 2.6);
  deskFlame.position.set(-0.83, 0.99, -0.28);
  deskFlame.visible = false;
  // Its light only counts at dusk; by day the sun wins anyway.
  const deskLight = new THREE.PointLight('#ffb35c', 2.2, 3.2, 1.6);
  deskLight.position.y = 0.12;
  deskFlame.add(deskLight);
  g.add(deskFlame);

  // Reading tablets: wax tablets in wooden frames.
  const tablets = new THREE.Group();
  for (const [x, r] of [[-0.72, 0.25], [0.7, -0.2]]) {
    const t = mesh(box(0.42, 0.03, 0.3, 1), M.parchment);
    t.position.set(x, 0.935, 0.05);
    t.rotation.y = r;
    const rim = mesh(box(0.46, 0.025, 0.34, 1), M.darkWood);
    rim.position.set(x, 0.925, 0.05);
    rim.rotation.y = r;
    tablets.add(t, rim);
  }
  tablets.visible = false;
  g.add(tablets);

  // Clutter: a stack of wax tablets and a bundle of scrolls tied with cord.
  for (let i = 0; i < 3; i++) {
    const tab = mesh(box(0.3, 0.035, 0.22, 1), i === 1 ? M.wood : M.darkWood);
    tab.position.set(0.72, 0.945 + i * 0.036, -0.32);
    tab.rotation.y = 0.2 + i * 0.12;
    g.add(tab);
  }
  for (let i = 0; i < 3; i++) {
    const sc = mesh(new THREE.CylinderGeometry(0.035, 0.035, 0.38, 8), M.parchment);
    sc.rotation.set(0, 0.3, Math.PI / 2);
    sc.position.set(-0.55 + i * 0.03, 0.96 + (i === 2 ? 0.05 : 0), -0.38 + i * 0.07 - (i === 2 ? 0.035 : 0));
    g.add(sc);
  }
  // The Order's own tablet (what gets sealed and carried to the Censor).
  const orderTablet = mesh(box(0.36, 0.05, 0.26, 1), M.parchment);
  orderTablet.position.set(-0.35, 0.95, 0.12);
  g.add(orderTablet);

  // Verification apparatus: a small bronze arch on the desk corner with
  // five lamps that light in sequence while the repository's checks run.
  const app = new THREE.Group();
  app.position.set(0.85, 0.92, -0.25);
  const arch = mesh(new THREE.TorusGeometry(0.2, 0.025, 6, 16, Math.PI), M.bronze);
  arch.position.y = 0.02;
  app.add(arch);
  const lamps = [];
  for (let i = 0; i < 5; i++) {
    const a = Math.PI * (0.1 + (0.8 * i) / 4);
    const lm = new THREE.MeshStandardMaterial({ color: '#4b3a26', emissive: new THREE.Color(PALETTE.lamp), emissiveIntensity: 0, roughness: 0.3 });
    const l = mesh(new THREE.SphereGeometry(0.035, 8, 6), lm, { cast: false });
    l.position.set(Math.cos(a) * 0.2, 0.02 + Math.sin(a) * 0.2, 0);
    app.add(l);
    lamps.push(l);
  }
  g.add(app);

  // Needs-you seal: a red wax disc standing on the desk front.
  const seal = new THREE.Group();
  const disc = mesh(new THREE.CylinderGeometry(0.2, 0.2, 0.06, 20), std({ color: PALETTE.seal, roughness: 0.5 }));
  disc.scale.set(0.45, 0.6, 0.45);
  disc.position.set(0.55, 0.93, 0.28);
  seal.add(disc);
  seal.visible = false;
  g.add(seal);

  // The Cohort's standard (vexillum): a square cloth on a crossbar, high
  // enough to find from the overview. It stands only while a real Cohort
  // holds the desk; it turns seal-red with a "!" when the Order needs you.
  const standard = new THREE.Group();
  const staff = mesh(new THREE.CylinderGeometry(0.035, 0.035, 4.2, 6), M.darkWood);
  staff.position.set(-1.3, 2.1, 0.55);
  const crossbar = mesh(new THREE.CylinderGeometry(0.03, 0.03, 1.1, 6), M.bronze);
  crossbar.rotation.x = Math.PI / 2;
  crossbar.position.set(-1.3, 4.05, 0.55);
  const flagCanvas = document.createElement('canvas');
  flagCanvas.width = 128;
  flagCanvas.height = 128;
  const flagTex = new THREE.CanvasTexture(flagCanvas);
  flagTex.colorSpace = THREE.SRGBColorSpace;
  const flag = mesh(new THREE.PlaneGeometry(1.0, 1.0, 4, 4), new THREE.MeshStandardMaterial({ map: flagTex, roughness: 0.9, side: THREE.DoubleSide }), { receive: false });
  flag.position.set(-1.3, 3.5, 0.55);
  flag.rotation.y = Math.PI / 2;
  const tip = mesh(new THREE.ConeGeometry(0.06, 0.2, 6), M.gold);
  tip.position.set(-1.3, 4.3, 0.55);
  standard.add(staff, crossbar, flag, tip);
  standard.visible = false;
  g.add(standard);
  function drawFlag(numeral, halt) {
    const c = flagCanvas.getContext('2d');
    c.fillStyle = halt ? PALETTE.seal : '#e9dcbf';
    c.fillRect(0, 0, 128, 128);
    c.strokeStyle = halt ? '#f3d7b0' : PALETTE.pompeian;
    c.lineWidth = 6;
    c.strokeRect(8, 8, 112, 112);
    c.fillStyle = halt ? '#fff1dc' : PALETTE.pompeian;
    c.textAlign = 'center';
    c.font = '600 54px Georgia, serif';
    c.fillText(halt ? `${numeral}!` : numeral, 64, 84);
    flagTex.needsUpdate = true;
  }

  // Plaque on the desk front: short identity, also shown as a label.
  const plaque = mesh(box(0.9, 0.2, 0.03, 1), M.bronze);
  plaque.position.set(0, 0.72, -0.54);
  g.add(plaque);

  return {
    group: g,
    screen,
    tablets,
    orderTablet,
    lamps,
    seal,
    standard,
    flag,
    setStandard(numeral, halt) {
      if (!numeral) {
        standard.visible = false;
        return;
      }
      standard.visible = true;
      drawFlag(numeral, halt);
    },
    seatPosition: new THREE.Vector3(0, 0.02, 0.72),
    deskFlame,
    setScreen(kind) {
      screen.material = screenMaterial(kind);
    },
  };
}

// The campaign board: a bronze-framed tablet on a plinth. It shows only what
// a glance needs — title, settled count, the current Orders — and is redrawn
// from the projection whenever it changes.
export function makeCampaignBoard(M) {
  const g = new THREE.Group();
  const plinth = mesh(box(6.2, 1.1, 1.2, 2), M.marble);
  plinth.position.y = 0.55 + 0.36;
  g.add(plinth);
  const frame = mesh(box(5.6, 3.4, 0.3, 2), M.bronze);
  frame.position.y = 1.46 + 1.7;
  g.add(frame);
  const canvas = document.createElement('canvas');
  canvas.width = 1024;
  canvas.height = 600;
  const tex = new THREE.CanvasTexture(canvas);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.anisotropy = 8;
  const face = mesh(new THREE.PlaneGeometry(5.2, 3.05), new THREE.MeshStandardMaterial({ map: tex, roughness: 0.9, color: '#d8cbb0' }), { cast: false });
  face.position.set(0, 3.16, 0.16);
  g.add(face);
  // Crest: small aquila casting on top.
  const crest = mesh(box(1.4, 0.3, 0.4, 1), M.bronze);
  crest.position.set(0, 5.0, 0);
  g.add(crest);

  function draw(state) {
    const ctx = canvas.getContext('2d');
    const W = canvas.width;
    const H = canvas.height;
    ctx.fillStyle = '#efe2c2';
    ctx.fillRect(0, 0, W, H);
    ctx.fillStyle = 'rgba(120,90,50,0.07)';
    for (let i = 0; i < 60; i++) ctx.fillRect((i * 97) % W, (i * 61) % H, 140, 2);
    ctx.strokeStyle = '#8a6a3a';
    ctx.lineWidth = 6;
    ctx.strokeRect(18, 18, W - 36, H - 36);
    ctx.fillStyle = '#2a211b';
    ctx.textBaseline = 'alphabetic';
    const c = state.campaign;
    if (!c) {
      ctx.textAlign = 'center';
      ctx.font = '600 64px Georgia, "Times New Roman", serif';
      ctx.fillText('NO CAMPAIGN', W / 2, H / 2);
      ctx.font = 'italic 34px Georgia, serif';
      ctx.fillText('The Senate is at rest.', W / 2, H / 2 + 56);
      tex.needsUpdate = true;
      return;
    }
    ctx.textAlign = 'left';
    ctx.font = '600 30px Georgia, serif';
    ctx.fillStyle = '#8e2b22';
    ctx.fillText('CAMPAIGN', 60, 82);
    ctx.fillStyle = '#2a211b';
    // Shrink a long title to fit the board before squeezing it.
    const title = c.title.toUpperCase().slice(0, 40);
    let size = 64;
    ctx.font = `600 ${size}px Georgia, serif`;
    while (size > 40 && ctx.measureText(title).width > W - 120) {
      size -= 2;
      ctx.font = `600 ${size}px Georgia, serif`;
    }
    ctx.fillText(title, 60, 150, W - 120);
    ctx.font = '36px Georgia, serif';
    ctx.fillText(`${c.settled} of ${c.total} Orders settled`, 60, 206);
    // Progress bar made of tablets.
    const n = Math.max(c.total, 1);
    const bw = Math.min(64, (W - 140) / n - 8);
    for (let i = 0; i < n; i++) {
      ctx.fillStyle = i < c.settled ? '#b0843f' : 'rgba(42,33,27,0.14)';
      ctx.fillRect(60 + i * (bw + 8), 232, bw, 22);
    }
    ctx.font = '30px Georgia, serif';
    let y = 310;
    const orders = state.orders.filter((o) => o.state !== 'settled' && o.state !== 'cancelled').slice(0, 5);
    for (const o of orders) {
      const glyph = { working: '●', in_review: '◆', blocked: '!', delivered: '✓', failed: '✕', ready: '○', planned: '○' }[o.state] || '○';
      ctx.fillStyle = o.state === 'blocked' || o.state === 'failed' ? '#a3261f' : '#2a211b';
      ctx.fillText(`${glyph}  ${o.title.slice(0, 34)}`, 70, y);
      ctx.fillStyle = '#6b5a45';
      ctx.textAlign = 'right';
      ctx.fillText(STATE_WORD[o.state] || o.state, W - 70, y);
      ctx.textAlign = 'left';
      y += 50;
    }
    if (c.needs_you > 0) {
      ctx.fillStyle = '#a3261f';
      ctx.font = '600 32px Georgia, serif';
      ctx.fillText(`${c.needs_you} ${c.needs_you === 1 ? 'matter needs' : 'matters need'} your judgment`, 60, H - 50);
    }
    tex.needsUpdate = true;
  }
  return { group: g, draw, face };
}

export const STATE_WORD = {
  planned: 'Planned',
  ready: 'Ready',
  working: 'Working',
  in_review: 'In review',
  blocked: 'Needs you',
  delivered: 'Awaiting integration',
  settled: 'Settled',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

// Banner cloth with the Aquila, moving in a light breeze (vertex shader).
export function makeBanner(M, uniforms) {
  const canvas = document.createElement('canvas');
  canvas.width = 256;
  canvas.height = 512;
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = PALETTE.pompeian;
  ctx.fillRect(0, 0, 256, 512);
  ctx.fillStyle = 'rgba(0,0,0,0.12)';
  for (let y = 0; y < 512; y += 4) ctx.fillRect(0, y, 256, 1);
  ctx.strokeStyle = '#d9ad5c';
  ctx.lineWidth = 6;
  ctx.strokeRect(14, 14, 228, 484);
  drawAquila(ctx, 28, 110, 200, '#d9ad5c', PALETTE.pompeian);
  ctx.fillStyle = '#d9ad5c';
  ctx.font = '600 34px Georgia, serif';
  ctx.textAlign = 'center';
  ctx.fillText('S·P·Q·R', 128, 400);
  const tex = new THREE.CanvasTexture(canvas);
  tex.colorSpace = THREE.SRGBColorSpace;
  const mat = new THREE.MeshStandardMaterial({ map: tex, roughness: 0.9, side: THREE.DoubleSide, normalMap: M.cloth });
  mat.onBeforeCompile = (shader) => {
    shader.uniforms.uTime = uniforms.uTime;
    shader.vertexShader = shader.vertexShader
      .replace('#include <common>', '#include <common>\nuniform float uTime;')
      .replace(
        '#include <begin_vertex>',
        `#include <begin_vertex>
        float hang = (0.5 - uv.y);
        float wave = sin(uv.y * 6.0 - uTime * 1.6 + position.x * 2.0) * 0.06 * (1.0 - uv.y);
        transformed.z += wave + hang * 0.02;`,
      );
  };
  const geo = new THREE.PlaneGeometry(1.4, 2.8, 6, 16);
  const banner = mesh(geo, mat, { receive: false });
  return banner;
}

// The Censor's long table, an inbox lectern at the front and two oil lamps.
export function makeCensorTable(M) {
  const g = new THREE.Group();
  const top = mesh(box(1.2, 0.12, 3.8, 1.5), M.darkWood);
  top.position.y = 0.9;
  g.add(top);
  for (const z of [-1.6, 1.6]) {
    const leg = mesh(box(1.0, 0.84, 0.14, 1), M.darkWood);
    leg.position.set(0, 0.42, z);
    g.add(leg);
  }
  const seat = mesh(box(0.6, 0.5, 0.7, 1), M.darkWood);
  seat.position.set(0.95, 0.25, 0);
  g.add(seat);
  // Plans laid out on the table.
  const sheets = [];
  for (const [z, r] of [[-1.2, 0.1], [1.15, -0.15], [0.0, 0.03]]) {
    const s = mesh(box(0.7, 0.02, 0.9, 1), [M.parchment, M.parchment, screenMaterial('read'), M.parchment, M.parchment, M.parchment]);
    s.position.set(0.05, 0.97, z);
    s.rotation.y = r;
    g.add(s);
    sheets.push(s);
  }
  // The submitted tablet, shown when a review is under way.
  const submitted = mesh(box(0.36, 0.05, 0.26, 1), M.parchment);
  submitted.position.set(0.1, 0.99, 0);
  submitted.visible = false;
  g.add(submitted);
  const wax = mesh(new THREE.CylinderGeometry(0.06, 0.06, 0.03, 12), std({ color: PALETTE.seal, roughness: 0.45 }));
  wax.position.set(0.1, 1.03, 0.08);
  wax.visible = false;
  g.add(wax);
  // Lamps.
  const flames = [];
  for (const z of [-2.2, 2.2]) {
    const stand = mesh(new THREE.CylinderGeometry(0.025, 0.06, 1.3, 8), M.bronze);
    stand.position.set(0.2, 0.65, z);
    g.add(stand);
    const bowl = mesh(new THREE.CylinderGeometry(0.2, 0.1, 0.12, 10), M.bronze);
    bowl.position.set(0.2, 1.34, z);
    g.add(bowl);
    const flame = flameMesh(0.9);
    flame.position.set(0.2, 1.4, z);
    g.add(flame);
    flames.push(flame);
  }
  return { group: g, submitted, wax, flames, sheets };
}

// A rack beside the board where sealed tablets wait for integration.
export function makeTabletRack(M) {
  const g = new THREE.Group();
  const base = mesh(box(2.4, 0.9, 0.7, 1.5), M.darkWood);
  base.position.y = 0.45 + 0.36;
  g.add(base);
  const slots = [];
  for (let i = 0; i < 5; i++) {
    const t = new THREE.Group();
    const tab = mesh(box(0.36, 0.26, 0.05, 1), M.parchment);
    const wax = mesh(new THREE.CylinderGeometry(0.05, 0.05, 0.02, 10), std({ color: PALETTE.seal, roughness: 0.45 }));
    wax.rotation.x = Math.PI / 2;
    wax.position.z = 0.035;
    t.add(tab, wax);
    t.position.set(-0.9 + i * 0.45, 1.4, 0.05);
    t.rotation.x = -0.25;
    t.visible = false;
    g.add(t);
    slots.push(t);
  }
  return { group: g, slots };
}

// A flame: teardrop lathe, bright enough to bloom but not to blow out.
export function flameMesh(scale = 1, strength = 1.9) {
  const pts = [[0, 0], [0.05, 0.02], [0.065, 0.06], [0.05, 0.11], [0.025, 0.16], [0, 0.2]].map(([r, y]) => new THREE.Vector2(r * scale, y * scale));
  return new THREE.Mesh(new THREE.LatheGeometry(pts, 10), new THREE.MeshBasicMaterial({ color: new THREE.Color('#ffa94d').multiplyScalar(strength) }));
}

// Standing oil lamp: a bronze tripod, a fluted stem with knops, a shallow
// dish with a rim, and the flame.
export function makeLamp(M) {
  const g = new THREE.Group();
  for (let i = 0; i < 3; i++) {
    const a = (i / 3) * Math.PI * 2;
    const leg = mesh(new THREE.CylinderGeometry(0.018, 0.026, 0.42, 6), M.bronze);
    leg.position.set(Math.sin(a) * 0.12, 0.17, Math.cos(a) * 0.12);
    leg.rotation.set(Math.cos(a) * 0.55, 0, -Math.sin(a) * 0.55);
    g.add(leg);
    const foot = mesh(new THREE.SphereGeometry(0.035, 8, 6), M.bronze);
    foot.position.set(Math.sin(a) * 0.23, 0.02, Math.cos(a) * 0.23);
    g.add(foot);
  }
  const stem = mesh(new THREE.CylinderGeometry(0.022, 0.03, 1.3, 8), M.bronze);
  stem.position.y = 1.0;
  g.add(stem);
  for (const y of [0.4, 1.0, 1.55]) {
    const knop = mesh(new THREE.SphereGeometry(0.05, 10, 8), M.bronze);
    knop.scale.y = 0.6;
    knop.position.y = y;
    g.add(knop);
  }
  const dish = mesh(new THREE.LatheGeometry([[0, 0], [0.16, 0.02], [0.22, 0.07], [0.235, 0.1], [0.215, 0.1], [0.2, 0.07], [0, 0.05]].map(([r, y]) => new THREE.Vector2(r, y)), 16), M.bronze);
  dish.position.y = 1.64;
  g.add(dish);
  const flame = flameMesh(1.1);
  flame.position.y = 1.72;
  g.add(flame);
  return { group: g, flame };
}

let shelfSeed = 1;

// Scroll shelf: a wooden case of pigeonholes with rolled scrolls.
export function makeShelf(M) {
  const g = new THREE.Group();
  const back = mesh(box(3.2, 2.4, 0.5, 1.5), M.darkWood);
  back.position.set(0, 1.2, -0.1);
  g.add(back);
  const scrollGeo = new THREE.CylinderGeometry(0.07, 0.07, 0.42, 10);
  scrollGeo.rotateX(Math.PI / 2);
  const scrolls = new THREE.InstancedMesh(scrollGeo, new THREE.MeshStandardMaterial({ roughness: 0.85 }), 44);
  const m = new THREE.Matrix4();
  const tints = ['#efe2c4', '#e3d1a9', '#d8c291', '#f3e9d2', '#8a4a2c', '#6b3a24', '#a3261f', '#d9c9a6'];
  let k = 0;
  let seed = shelfSeed++ * 97;
  const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
  for (let r = 0; r < 4; r++) {
    for (let c = 0; c < 11 && k < 44; c++) {
      if (rand() < 0.14) continue;
      const rr = 0.75 + rand() * 0.4;
      m.compose(
        new THREE.Vector3(-1.4 + c * 0.28 + (rand() - 0.5) * 0.05, 0.4 + r * 0.52 + (rr - 1) * 0.07, 0.18 + (rand() - 0.5) * 0.08),
        new THREE.Quaternion().setFromEuler(new THREE.Euler(0, (rand() - 0.5) * 0.3, 0)),
        new THREE.Vector3(rr, rr, 0.8 + rand() * 0.5),
      );
      scrolls.setMatrixAt(k, m);
      scrolls.setColorAt(k, new THREE.Color(tints[Math.floor(rand() * tints.length)]));
      k += 1;
    }
  }
  scrolls.count = k;
  scrolls.castShadow = true;
  g.add(scrolls);
  // Amphorae on top of the case.
  for (const x of [-1.05, 0.2, 1.1]) {
    if (rand() < 0.3) continue;
    const amph = mesh(new THREE.LatheGeometry([[0, 0], [0.05, 0.02], [0.13, 0.18], [0.14, 0.3], [0.09, 0.44], [0.045, 0.5], [0.05, 0.56], [0.03, 0.57]].map(([r, y]) => new THREE.Vector2(r, y)), 14), rand() < 0.5 ? M.terracotta : M.bronze);
    amph.position.set(x, 2.6, 0.05);
    g.add(amph);
  }
  for (let r = 0; r < 5; r++) {
    const plank = mesh(box(3.3, 0.06, 0.55, 1.5), M.wood);
    plank.position.set(0, 0.3 + r * 0.52, 0.1);
    g.add(plank);
  }
  return g;
}
