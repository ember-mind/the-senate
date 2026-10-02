// THROWAWAY 2D GAME PROTOTYPE. Painted terrain and animated sprite armies,
// sharing the committed World projection. Presentation changes no Run state.
import { fetchWorld, source } from './api.js';
import { aquilaSvg } from './aquila.js';
import { escapeHtml as esc, numeral } from './director.js';
import { modelOf } from './models.js';
import { STATE_WORD } from './props.js';
import { UI } from './ui.js';

const params = new URLSearchParams(location.search);
const manual = params.get('manual') === '1';
const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
const host = document.querySelector('#map-preview');
const ui = new UI();
const WIDTH = 1800;
const live = order => ['working', 'in_review', 'blocked', 'failed'].includes(order.state);
const needs = order => ['blocked', 'failed', 'delivered'].includes(order.state);
const activity = { reading: 'Reading the repository', designing: 'Drafting the plan', coding: 'Writing code', testing: 'Running checks', reviewing: 'Independent review', deciding: 'Weighing the result' };
const word = order => order.state === 'working' ? activity[order.activity] || 'Working' : STATE_WORD[order.state] || order.state;
const tint = order => needs(order) ? '#dc6b4f' : order.state === 'in_review' ? '#b99ade' : order.state === 'settled' ? '#b2ca91' : modelOf(order.worker)?.color || '#d6af69';
let state = null, selected = null, hovered = null, time = 0, failures = 0, polling = false;
let raf = null, previous = performance.now();
let screen = { w: window.innerWidth, h: window.innerHeight };
let camera = { x: 900, y: 505, zoom: 1 };
const keys = new Set(), units = new Map();
let anchors = [], particles = [], drag = null, spriteCount = 0;
document.body.dataset.mode = 'game';
document.body.dataset.source = source;
if (params.get('clean') === '1') document.body.classList.add('clean');
document.querySelector('#map-view').classList.add('on');
document.querySelectorAll('[data-view]').forEach(button => button.classList.remove('on'));
document.querySelector('#brand .mark').innerHTML = aquilaSvg({ fill: '#d4b275', size: 24, eye: '#1e2d27' });
document.querySelector('#loading .seal').innerHTML = aquilaSvg({ fill: '#b0843f', size: 96, eye: '#efe3cb' });
host.hidden = false;
host.innerHTML = `<canvas id="game-world" aria-label="Painted Roman campus with live Cohorts"></canvas>
  <div id="game-hover" hidden></div>
  <div id="game-tools"><button data-game="campus">Campus</button><button data-game="field">Battlefield</button><button data-game="orders">Orders</button><button data-game="fit" aria-label="Reset view">⌖</button><button data-game="out" aria-label="Zoom out">−</button><button data-game="in" aria-label="Zoom in">+</button></div>
  <div id="game-selection" hidden></div>
  <div id="game-dock"><div class="dock-heading"><span>COHORTS</span><span id="game-count"></span></div><div id="game-roster" role="group" aria-label="Inspect Cohorts"></div></div>
  <div id="game-minimap"><canvas width="180" height="101" aria-label="World overview; click to move camera"></canvas><span>THE SENATE · 2D</span></div>
  <div id="game-hint">Drag to pan · scroll to zoom · click a Cohort to inspect</div>`;
const canvas = host.querySelector('#game-world');
const ctx = canvas.getContext('2d');
const minimap = host.querySelector('#game-minimap canvas');
const mini = minimap.getContext('2d');
const tooltip = host.querySelector('#game-hover');

async function image(path) { const img = new Image(); img.src = path; await img.decode(); return img; }
let terrain, atlas;
try { [terrain, atlas] = await Promise.all([image('assets/game2d/campus.png'), image('assets/game2d/characters.png')]); }
catch (error) { document.querySelector('#loading .sub').textContent = 'Could not load the 2D artwork. Reload to try again.'; throw error; }
const HEIGHT = WIDTH * terrain.height / terrain.width;
const CELL = atlas.width / 4;
const CONSUL = { x: WIDTH * .735, y: HEIGHT * .525 };
const CENSOR = { x: WIDTH * .935, y: HEIGHT * .605 };
const HOME = { x: WIDTH * .665, y: HEIGHT * .855 };
const GATE = { x: WIDTH * .548, y: HEIGHT * .467 };
const point = (x, y) => ({ x, y });

function fit() { camera = { x: WIDTH / 2, y: HEIGHT / 2, zoom: 1 }; }
function scale() { return Math.max(screen.w / WIDTH, screen.h / HEIGHT) * camera.zoom; }
function project(p) { const s = scale(); return { x: (p.x - camera.x) * s + screen.w / 2, y: (p.y - camera.y) * s + screen.h / 2 }; }
function unproject(x, y) { const s = scale(); return { x: (x - screen.w / 2) / s + camera.x, y: (y - screen.h / 2) / s + camera.y }; }
function clampCamera() {
  const halfW = screen.w / (2 * scale()), halfH = screen.h / (2 * scale());
  camera.x = Math.max(Math.min(halfW, WIDTH / 2), Math.min(WIDTH - Math.min(halfW, WIDTH / 2), camera.x));
  camera.y = Math.max(Math.min(halfH, HEIGHT / 2), Math.min(HEIGHT - Math.min(halfH, HEIGHT / 2), camera.y));
}
function resize() {
  screen = { w: window.innerWidth, h: window.innerHeight };
  const ratio = Math.min(1.5, window.devicePixelRatio || 1);
  canvas.width = Math.round(screen.w * ratio); canvas.height = Math.round(screen.h * ratio);
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0); clampCamera(); draw();
}
function zoom(factor, x = screen.w / 2, y = screen.h / 2) {
  const before = unproject(x, y);
  camera.zoom = Math.max(1, Math.min(3.5, camera.zoom * factor));
  const after = unproject(x, y);
  camera.x += before.x - after.x; camera.y += before.y - after.y;
  clampCamera(); draw();
}
function focus(p, magnification = 1.8) { camera.x = p.x; camera.y = p.y; camera.zoom = magnification; clampCamera(); draw(); }
function slot(index) { return point(555 + (index % 2) * 218, 245 + Math.floor(index / 2) * 211); }
function pathToField(destination) { return [HOME, point(1100, 810), point(1070, 555), point(1030, 475), GATE, point(860, GATE.y), destination]; }
function pathPoint(path, progress) {
  const lengths = path.slice(1).map((p, i) => Math.hypot(p.x - path[i].x, p.y - path[i].y));
  let distance = progress * lengths.reduce((a, b) => a + b, 0);
  for (let i = 0; i < lengths.length; i++) {
    if (distance <= lengths[i]) { const f = lengths[i] ? distance / lengths[i] : 0; return point(path[i].x + (path[i + 1].x - path[i].x) * f, path[i].y + (path[i + 1].y - path[i].y) * f); }
    distance -= lengths[i];
  }
  return path.at(-1);
}

function apply(next) {
  const first = !state;
  state = next;
  const visible = next.orders.filter(live).slice(0, 6);
  const claimed = new Set([...units.values()].filter(unit => !unit.returning && visible.some(o => o.id === unit.order.id)).map(unit => unit.slot));
  for (const order of visible) {
    const existing = units.get(order.id);
    if (existing) {
      existing.order = order;
      if (existing.returning) { existing.returning = false; existing.path = [existing.pos, existing.destination]; existing.progress = reduced ? 1 : 0; }
      continue;
    }
    let index = 0; while (claimed.has(index)) index++; claimed.add(index);
    const destination = slot(index);
    units.set(order.id, { order, slot: index, destination, pos: first || reduced ? destination : HOME, path: pathToField(destination), progress: first || reduced ? 1 : 0, returning: false });
  }
  for (const [id, unit] of units) {
    if (visible.some(o => o.id === id) || unit.returning) continue;
    unit.order = next.orders.find(o => o.id === id) || { ...unit.order, state: 'cancelled' };
    unit.path = [unit.pos, point(860, GATE.y), GATE, point(1070, 555), point(1100, 810), HOME];
    unit.progress = 0; unit.returning = true;
    if (reduced) units.delete(id);
  }
  ui.setState(next);
  if (ui.open?.kind === 'game-consul') briefing();
  if (selected && !next.orders.some(o => o.id === selected)) selected = null;
  roster();
}
function roster() {
  const orders = state?.orders || [];
  host.querySelector('#game-count').textContent = `${orders.length} Orders${orders.filter(live).length > 6 ? ' · 6 on screen' : ''}`;
  const scroll = host.querySelector('#game-roster').scrollLeft;
  host.querySelector('#game-roster').innerHTML = orders.length ? orders.map(order => `<button class="unit-portrait ${needs(order) ? 'needs' : ''} ${selected === order.id ? 'selected' : ''}" data-order="${esc(order.id)}" aria-label="${esc(`Cohort ${numeral(order.cohort)}: ${order.title}, ${word(order)}`)}" title="${esc(`${order.title} · ${word(order)}`)}"><span class="portrait-art"></span><b>${numeral(order.cohort)}</b><i style="background:${tint(order)}"></i></button>`).join('') : '<span class="empty-roster">No Orders. The campus is quiet.</span>';
  host.querySelector('#game-roster').scrollLeft = scroll;
  host.querySelectorAll('[data-order]').forEach(button => button.addEventListener('click', () => inspect({ kind: 'order', id: button.dataset.order })));
  const order = orders.find(o => o.id === selected), selection = host.querySelector('#game-selection');
  selection.hidden = !order;
  if (order) selection.innerHTML = `<small>COHORT ${numeral(order.cohort)} · ${esc(modelOf(order.worker)?.name || 'UNASSIGNED')}</small><strong>${esc(order.title)}</strong><span>${esc(word(order))}</span>`;
}
function briefing() { ui.show('game-consul', null, `<header><small>CONSUL</small><h2>Campaign briefing</h2></header><blockquote>${(state?.consul?.summary || []).map(esc).join('<br>') || 'Nothing to report.'}</blockquote>`); }
function inspect(anchor) {
  if (anchor.kind === 'order') { selected = anchor.id; ui.openOrder(anchor.id); roster(); }
  else if (anchor.kind === 'consul') briefing();
  else if (anchor.kind === 'censor') ui.openCensor();
  else ui.openCampaign();
  draw();
}

function sprite(row, frame, x, y, size = 72, flip = false) {
  ctx.save(); ctx.translate(x, y); ctx.fillStyle = 'rgba(25,31,17,.25)';
  ctx.beginPath(); ctx.ellipse(4, 1, size * .17, size * .06, -.18, 0, Math.PI * 2); ctx.fill();
  if (flip) ctx.scale(-1, 1);
  ctx.drawImage(atlas, frame * CELL, row * CELL, CELL, CELL, -size / 2, -size * .88, size, size);
  ctx.restore();
}
function ring(x, y, radius, color, selectedRing = false) {
  ctx.save(); ctx.strokeStyle = color; ctx.fillStyle = color;
  ctx.globalAlpha = selectedRing ? .7 : .24; ctx.lineWidth = selectedRing ? 2.5 : 1.5;
  ctx.beginPath(); ctx.ellipse(x, y, radius, radius * .5, 0, 0, Math.PI * 2); ctx.stroke();
  ctx.globalAlpha = .04; ctx.fill(); ctx.restore();
}
function flag(order, x, y) {
  ctx.save(); ctx.translate(x, y); ctx.strokeStyle = '#665438'; ctx.lineWidth = 3;
  ctx.beginPath(); ctx.moveTo(0, 5); ctx.lineTo(0, -56); ctx.stroke();
  const wave = reduced ? 0 : Math.sin(time * 2.5 + order.cohort) * 3;
  ctx.fillStyle = needs(order) ? '#a63729' : tint(order);
  ctx.beginPath(); ctx.moveTo(0, -53); ctx.lineTo(31, -51 + wave); ctx.lineTo(29, -29 + wave); ctx.lineTo(17, -24); ctx.lineTo(0, -30); ctx.closePath(); ctx.fill();
  ctx.strokeStyle = '#ecd59a'; ctx.lineWidth = 1; ctx.stroke();
  ctx.fillStyle = '#fff3d5'; ctx.font = 'bold 12px Georgia'; ctx.textAlign = 'center'; ctx.shadowColor = '#251b14'; ctx.shadowBlur = 2;
  ctx.fillText(needs(order) ? '!' : numeral(order.cohort), 15, -35 + wave * .5); ctx.restore();
}
function drawUnit(unit, people, banners) {
  const { order, pos } = unit, moving = unit.progress < 1;
  const fighting = !reduced && !moving && !unit.returning && order.state === 'working' && order.activity === 'coding';
  const testing = !reduced && !moving && !unit.returning && order.state === 'working' && order.activity === 'testing';
  const walking = moving && !reduced ? 1 + Math.floor(time * 9) % 2 : 0;
  const selectedUnit = selected === order.id || hovered?.id === order.id;
  ring(pos.x + 10, pos.y + 11, 65, selectedUnit ? '#ecd18d' : tint(order), selectedUnit);
  for (let i = 0; i < 6; i++) {
    const rank = i % 2, line = Math.floor(i / 2), beat = (time + i * .23 + unit.slot * .51) % 2.4;
    const forward = fighting && rank === 0 ? Math.sin(beat / 2.4 * Math.PI) * 17 : 0;
    const attack = fighting && rank === 0 && beat > .85 && beat < 1.4;
    people.push({ row: 0, frame: attack ? 3 : walking || (fighting && forward > 5 ? 1 + Math.floor(time * 7 + i) % 2 : 0), x: pos.x + rank * 27 - forward, y: pos.y + (line - 1) * 30, flip: unit.returning, size: 76 });
    if (!moving && !unit.returning) {
      const advance = fighting || testing ? Math.sin((beat + 1) / 2.4 * Math.PI) * 13 : 0;
      const enemyAttack = (fighting || testing) && rank === 1 && beat > 1.15 && beat < 1.75;
      people.push({ row: 1, frame: enemyAttack ? 3 : (fighting || testing) && advance > 5 ? 1 + Math.floor(time * 7 + i) % 2 : 0, x: pos.x - 92 + rank * 27 + advance, y: pos.y + (line - 1) * 30, size: 76 });
    }
  }
  people.push({ row: 0, frame: walking, x: pos.x + 48, y: pos.y + 4, flip: unit.returning, size: 80 });
  banners.push({ order, x: pos.x + 59, y: pos.y - 22 });
  if ((fighting || testing) && !moving) {
    // Bounded visual impacts signal activity; they never invent an outcome.
    const age = (time + unit.slot * .37) % 1.2;
    if (age < .22) {
      const x = pos.x - 31, y = pos.y - 7;
      ctx.save(); ctx.globalAlpha = 1 - age / .22; ctx.strokeStyle = testing ? '#bdded5' : '#ffe5a2'; ctx.lineWidth = 2;
      for (let i = 0; i < 5; i++) { const a = i * Math.PI * .4; ctx.beginPath(); ctx.moveTo(x + Math.cos(a) * 3, y + Math.sin(a) * 3); ctx.lineTo(x + Math.cos(a) * (7 + age * 35), y + Math.sin(a) * (7 + age * 35)); ctx.stroke(); }
      ctx.restore();
    }
  }
}
function draw() {
  if (!terrain) return;
  const s = scale();
  ctx.clearRect(0, 0, screen.w, screen.h);
  ctx.save(); ctx.translate(screen.w / 2, screen.h / 2); ctx.scale(s, s); ctx.translate(-camera.x, -camera.y);
  ctx.drawImage(terrain, 0, 0, WIDTH, HEIGHT);
  anchors = [{ kind: 'consul', pos: CONSUL, title: 'Consul', note: 'Read the campaign briefing' }, { kind: 'censor', pos: CENSOR, title: 'Censor', note: state?.censor?.state === 'reviewing' ? 'Independent review in progress' : 'Nothing to review' }, { kind: 'board', pos: point(1350, 450), title: 'Forum', note: 'Inspect all Orders' }];
  const people = [], banners = [];
  people.push({ row: 2, frame: state?.consul?.state === 'conferring' && !reduced ? 3 : 0, ...CONSUL, size: 91 });
  people.push({ row: 3, frame: state?.censor?.state === 'reviewing' ? 3 : 0, ...CENSOR, size: 86 });
  for (const unit of units.values()) { drawUnit(unit, people, banners); if (!unit.returning) anchors.push({ kind: 'order', id: unit.order.id, pos: unit.pos, title: `Cohort ${numeral(unit.order.cohort)} · ${unit.order.title}`, note: word(unit.order) }); }
  people.sort((a, b) => a.y - b.y).forEach(p => sprite(p.row, p.frame, p.x, p.y, p.size, p.flip));
  spriteCount = people.length;
  banners.forEach(b => flag(b.order, b.x, b.y));
  if (!reduced) {
    ctx.save(); ctx.strokeStyle = '#bce7e1'; ctx.lineWidth = 1;
    for (let i = 0; i < 3; i++) { const r = 6 + ((time * 5 + i * 5) % 17); ctx.globalAlpha = (1 - (r - 6) / 17) * .45; ctx.beginPath(); ctx.ellipse(1310, 482, r, r * .45, 0, 0, Math.PI * 2); ctx.stroke(); }
    ctx.restore();
  }
  for (const p of particles) { ctx.globalAlpha = (1 - p.age / p.life) * .27; ctx.fillStyle = '#d9bc87'; ctx.beginPath(); ctx.arc(p.x, p.y, p.radius + p.age * 3, 0, Math.PI * 2); ctx.fill(); }
  ctx.globalAlpha = 1; ctx.restore();
  mini.clearRect(0, 0, 180, 101); mini.drawImage(terrain, 0, 0, 180, 101);
  for (const unit of units.values()) { mini.fillStyle = needs(unit.order) ? '#ff8b67' : '#ffe0a0'; mini.fillRect(unit.pos.x / WIDTH * 180 - 1.5, unit.pos.y / HEIGHT * 101 - 1.5, 3, 3); }
  const viewW = screen.w / s, viewH = screen.h / s;
  mini.strokeStyle = '#fff0c7'; mini.lineWidth = 1; mini.strokeRect((camera.x - viewW / 2) / WIDTH * 180, (camera.y - viewH / 2) / HEIGHT * 101, viewW / WIDTH * 180, viewH / HEIGHT * 101);
  positionTooltip();
}
function positionTooltip() {
  const current = hovered && anchors.find(a => a.kind === hovered.kind && a.id === hovered.id);
  tooltip.hidden = !current;
  if (!current) return;
  const p = project(current.pos);
  tooltip.innerHTML = `<strong>${esc(current.title)}</strong><span>${esc(current.note)}</span>`;
  tooltip.style.left = `${Math.max(125, Math.min(screen.w - 125, p.x))}px`; tooltip.style.top = `${Math.max(125, p.y - 78)}px`;
}
function hit(x, y) {
  let found = null, best = Infinity;
  for (const anchor of anchors) { const p = project(anchor.pos), d = Math.hypot(x - p.x, y - p.y + 12 * scale()); if (d < Math.max(24, 62 * scale()) && d < best) { found = anchor; best = d; } }
  return found;
}
function update(dt) {
  time += dt;
  const speed = dt * 460 / camera.zoom;
  camera.x += ((keys.has('KeyD') || keys.has('ArrowRight') ? 1 : 0) - (keys.has('KeyA') || keys.has('ArrowLeft') ? 1 : 0)) * speed;
  camera.y += ((keys.has('KeyS') || keys.has('ArrowDown') ? 1 : 0) - (keys.has('KeyW') || keys.has('ArrowUp') ? 1 : 0)) * speed; clampCamera();
  for (const [id, unit] of units) {
    if (unit.progress < 1) {
      unit.progress = Math.min(1, unit.progress + dt / 5.5); unit.pos = pathPoint(unit.path, unit.progress);
      if (!reduced && particles.length < 80 && Math.floor(time * 12) !== Math.floor((time - dt) * 12)) particles.push({ x: unit.pos.x + 18, y: unit.pos.y + 10, age: 0, radius: 3, life: .8 });
    }
    if (unit.returning && unit.progress >= 1) units.delete(id);
  }
  particles = particles.filter(p => { p.age += dt; p.x += dt * 8; return p.age < p.life; });
}
function schedule() {
  if (manual || document.hidden || raf !== null) return;
  raf = requestAnimationFrame(now => { raf = null; const dt = Math.min((now - previous) / 1000, .05); previous = now; update(dt); draw(); schedule(); });
}

host.querySelectorAll('[data-game]').forEach(button => button.addEventListener('click', () => {
  const action = button.dataset.game;
  if (action === 'fit') fit(); else if (action === 'campus') focus(point(1350, 525), 1.8); else if (action === 'field') focus(point(615, 452), 1.3); else if (action === 'orders') ui.openCampaign(); else zoom(action === 'in' ? 1.25 : .8); draw();
}));
minimap.addEventListener('pointerdown', e => {
  const r = minimap.getBoundingClientRect(); camera.x = (e.clientX - r.left) / r.width * WIDTH; camera.y = (e.clientY - r.top) / r.height * HEIGHT;
  if (camera.zoom === 1) camera.zoom = 1.8; clampCamera(); draw();
});
canvas.addEventListener('pointerdown', e => { if (e.button !== 0) return; drag = { x: e.clientX, y: e.clientY, moved: 0 }; canvas.setPointerCapture(e.pointerId); });
canvas.addEventListener('pointermove', e => {
  if (drag) { const dx = e.clientX - drag.x, dy = e.clientY - drag.y; drag.moved += Math.abs(dx) + Math.abs(dy); drag.x = e.clientX; drag.y = e.clientY; camera.x -= dx / scale(); camera.y -= dy / scale(); clampCamera(); hovered = null; }
  else hovered = hit(e.clientX, e.clientY);
  canvas.style.cursor = drag ? 'grabbing' : hovered ? 'pointer' : 'grab'; draw();
});
canvas.addEventListener('pointerup', e => { if (drag && drag.moved < 6) { const anchor = hit(e.clientX, e.clientY); if (anchor) inspect(anchor); } drag = null; });
canvas.addEventListener('pointercancel', () => { drag = null; });
canvas.addEventListener('pointerleave', () => { hovered = null; tooltip.hidden = true; });
canvas.addEventListener('wheel', e => { e.preventDefault(); zoom(e.deltaY > 0 ? .9 : 1 / .9, e.clientX, e.clientY); }, { passive: false });
window.addEventListener('keydown', e => {
  if (e.target.closest('input, textarea, [contenteditable="true"]')) return;
  if (['KeyW', 'KeyA', 'KeyS', 'KeyD', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight'].includes(e.code)) { e.preventDefault(); keys.add(e.code); }
  if (e.code === 'KeyF') { fit(); draw(); }
});
window.addEventListener('keyup', e => keys.delete(e.code));
window.addEventListener('blur', () => { keys.clear(); drag = null; });
window.addEventListener('resize', resize);
document.addEventListener('visibilitychange', () => { keys.clear(); if (document.hidden && raf !== null) { cancelAnimationFrame(raf); raf = null; } else { previous = performance.now(); schedule(); } });

async function poll() {
  if (polling) return;
  polling = true;
  try {
    const next = await fetchWorld();
    if (next.schema !== 1) throw new Error(`Unknown world schema ${next.schema}`);
    if (JSON.stringify({ ...next, at: undefined }) !== JSON.stringify({ ...state, at: undefined })) apply(next);
    failures = 0; document.body.classList.remove('offline');
  } catch (error) { if (++failures > 2) document.body.classList.add('offline'); console.warn('[senate] 2D game poll failed', error); }
  finally { polling = false; if (!manual) setTimeout(poll, 1000); }
}
resize(); await poll(); draw(); document.querySelector('#loading').classList.add('gone'); schedule();
window.__senate = { ui, get state() { return state; }, units, get camera() { return { ...camera }; }, get stats() { return { sprites: spriteCount, particles: particles.length }; }, poll, fit };
if (manual) window.__senate.advance = async dt => { window.__senateClock += dt * 1000; await poll(); update(dt); draw(); };
