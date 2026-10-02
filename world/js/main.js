// Enter the Senate. Boots the world, polls the projection, and hands every
// snapshot to the director. The browser owns no Senate state: close the tab
// and nothing about the campaign changes.

import * as THREE from '../vendor/three.module.min.js';
import { CSS2DRenderer } from '../vendor/three.module.min.js';
import { fetchWorld, source } from './api.js';
import { aquilaSvg } from './aquila.js';
import { Controls } from './controls.js';
import { Director } from './director.js';
import { loadFigureParts } from './figures.js';
import { stepFeeds } from './props.js';
import { createWorld, pickQuality } from './scene.js';
import { loadTrees } from './architecture.js';
import { loadKay } from './kay.js';
import { UI } from './ui.js';
import { placeCamera } from './tour.js';
import { QUALITY, ResolutionBudget } from './quality.js';

const SCHEMA = 1;
if (new URLSearchParams(location.search).get('manual') === '1') window.__senateClock = 0;
const POLL_MS = 1000;

document.querySelector('#loading .seal').innerHTML = aquilaSvg({ fill: '#b0843f', size: 96, eye: '#efe3cb' });
document.querySelector('#brand .mark').innerHTML = aquilaSvg({ fill: '#b0843f', size: 22, eye: '#f4ead6' });

const params = new URLSearchParams(location.search);
// Capture mode: the page renders only when told to, on a virtual clock.
const manual = params.get('manual') === '1';
const tour = params.get('tour') === '1';
// The film is watched shrunk onto a web page, so its labels are drawn larger.
if (tour) document.body.classList.add('tour');
if (params.get('clean') === '1') document.body.classList.add('clean');
if (params.get('nolabels') === '1') document.body.classList.add('nolabels');
const quality = pickQuality();
const canvas = document.querySelector('#world');
await Promise.all([loadFigureParts(), loadTrees(), loadKay()]);
const world = createWorld(canvas, quality);

const labels = new CSS2DRenderer({ element: document.querySelector('#labels') });
labels.setSize(window.innerWidth, window.innerHeight);
window.addEventListener('resize', () => labels.setSize(window.innerWidth, window.innerHeight));

const ui = new UI();
const director = new Director(world, ui);
const controls = new Controls(world, canvas, {
  onModeChange: (mode) => {
    document.body.dataset.mode = mode;
    document.querySelectorAll('[data-view]').forEach((b) => b.classList.toggle('on', b.dataset.view === mode));
    document.querySelector('#battle-view').classList.remove('on');
  },
});
document.body.dataset.mode = 'overview';
const overviewPose = { ...controls.goal, target: controls.goal.target.clone() };
document.querySelectorAll('[data-view]').forEach((b) => b.addEventListener('click', () => {
  controls.setMode(b.dataset.view);
  if (b.dataset.view === 'overview') {
    controls.goal.target.copy(overviewPose.target);
    controls.goal.yaw = overviewPose.yaw;
    controls.goal.pitch = overviewPose.pitch;
    controls.goal.dist = overviewPose.dist;
  }
  document.querySelectorAll('[data-view]').forEach(view => view.classList.toggle('on', view === b));
  document.querySelector('#battle-view').classList.remove('on');
}));
document.querySelector('#battle-view').addEventListener('click', () => {
  const cohort = director.cohorts.find(c => !c.done && c.phase === 'field') || director.cohorts.find(c => !c.done);
  const pos = cohort?.slot.pos || world.field.slots[0].pos;
  controls.focus(pos, 22);
  controls.goal.yaw = 0.86;
  controls.goal.pitch = 0.34;
  document.querySelectorAll('[data-view]').forEach(b => b.classList.remove('on'));
  document.querySelector('#battle-view').classList.add('on');
});
if (params.get('view') === 'walk') controls.setMode('walk');

// Optional fixed camera for repeatable captures: ?cam=x,y,z,tx,ty,tz
const fixedCam = params.get('cam')?.split(',').map(Number);

// Click to inspect: the nearest interactive anchor under the pointer.
const raycaster = new THREE.Raycaster();
controls.onClick = (e) => {
  const ndc = new THREE.Vector2((e.clientX / window.innerWidth) * 2 - 1, -(e.clientY / window.innerHeight) * 2 + 1);
  raycaster.setFromCamera(ndc, world.camera);
  let best = null;
  for (const a of director.anchors()) {
    const p = a.pos.clone().add(new THREE.Vector3(0, 1.1, 0));
    const d = raycaster.ray.distanceToPoint(p);
    const tol = 0.9 + a.radius * 0.35;
    if (d < tol && (!best || d < best.d)) best = { a, d };
  }
  if (best) ui.interact(best.a);
};

// Walk-mode interaction: the nearest anchor within reach.
let nearest = null;
window.addEventListener('keydown', (e) => {
  if (e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLInputElement) return;
  if (e.code === 'KeyE' && nearest) ui.interact(nearest);
});

let lastState = null;
let failures = 0;
async function poll() {
  try {
    const s = await fetchWorld();
    if (s.schema !== SCHEMA) throw new Error(`Unknown world schema ${s.schema}`);
    failures = 0;
    document.body.classList.remove('offline');
    const key = JSON.stringify({ ...s, at: undefined });
    if (key !== lastState) {
      lastState = key;
      director.apply(s);
      ui.setState(s);
    }
  } catch (e) {
    failures += 1;
    if (failures > 2) document.body.classList.add('offline');
    console.warn('[senate] poll failed', e);
  }
  if (!manual) setTimeout(poll, POLL_MS);
}

// Frame loop. Rendering pauses while the tab is hidden, so the world costs
// nothing while agents work and nobody is watching.
const clock = new THREE.Timer();
let t = 0;
let frames = 0;
let fpsT = 0;
const stats = { fps: 0, quality, calls: 0, triangles: 0 };
const budget = new ResolutionBudget(world.renderer.getPixelRatio());
const adaptive = !manual && !Object.hasOwn(QUALITY, params.get('quality'));
let shadowAge = Infinity;
const perf = params.get('perf') === '1' ? document.body.appendChild(document.createElement('output')) : null;
if (perf) { perf.id = 'performance'; perf.setAttribute('aria-live', 'off'); }
window.__senate = { stats, world, director, controls };
function frame(fixedDt) {
  const started = performance.now();
  clock.update();
  const elapsed = typeof fixedDt === 'number' ? fixedDt : clock.getDelta();
  const dt = typeof fixedDt === 'number' ? fixedDt : Math.min(elapsed, 0.05);
  if (adaptive) {
    const ratio = budget.sample(elapsed);
    if (ratio !== null) world.setPixelRatio(ratio);
  }
  world.renderer.info.reset();
  t += dt;
  world.uniforms.uTime.value = t;
  stepFeeds(dt);
  director.update(dt, t);
  world.battleEffects.update(dt);
  controls.update(dt, t);
  let focus = null;
  if (tour) focus = placeCamera(world.camera, t);
  if (fixedCam?.length === 6) {
    world.camera.position.set(fixedCam[0], fixedCam[1], fixedCam[2]);
    world.camera.lookAt(fixedCam[3], fixedCam[4], fixedCam[5]);
    focus = Math.hypot(fixedCam[0] - fixedCam[3], fixedCam[1] - fixedCam[4], fixedCam[2] - fixedCam[5]);
  }
  if (world.bokeh) world.bokeh.uniforms.focus.value = focus ?? world.camera.position.distanceTo(controls.target);
  if (world.grade) world.grade.uniforms.uTime.value = t;
  if (world.bokeh && params.get('ap')) world.bokeh.uniforms.aperture.value = Number(params.get('ap'));
  if (controls.mode === 'walk') {
    const p = controls.you.position;
    nearest = null;
    let bestD = Infinity;
    for (const a of director.anchors()) {
      const d = Math.hypot(a.pos.x - p.x, a.pos.z - p.z);
      if (d < a.radius + 1.2 && d < bestD) {
        bestD = d;
        nearest = a;
      }
    }
    ui.setPrompt(nearest);
  } else {
    ui.setPrompt(null);
  }
  // Labels: all shown from above; near ones only when walking.
  document.body.classList.toggle('far', controls.mode === 'overview' && controls.dist > 75);
  if (controls.mode === 'walk') {
    const p = controls.you.position;
    document.querySelectorAll('#labels .label.agent').forEach((el) => {
      el.classList.toggle('dim', false);
    });
    void p;
  }
  // Render one shadow map at most, including AO/depth-of-field passes. Low
  // quality updates at 30 Hz; higher tiers preserve each animation frame.
  shadowAge += dt;
  if (quality !== 'low' || shadowAge >= 1 / 30) {
    world.renderer.shadowMap.needsUpdate = true;
    shadowAge = 0;
  }
  world.scene.updateMatrixWorld(true);
  world.composer.render(dt);
  labels.render(world.scene, world.camera);
  stats.calls = world.renderer.info.render.calls;
  stats.triangles = world.renderer.info.render.triangles;
  stats.frameMs = performance.now() - started;
  stats.pixelRatio = world.renderer.getPixelRatio();
  stats.particles = world.battleEffects.dustPool.live + world.battleEffects.sparkPool.live;
  frames += 1;
  fpsT += elapsed;
  if (fpsT > 1) {
    stats.fps = Math.round(frames / fpsT);
    frames = 0;
    fpsT = 0;
    if (perf) perf.textContent = `${stats.fps} FPS · ${stats.frameMs.toFixed(1)} ms CPU · ${stats.calls} draws · DPR ${stats.pixelRatio.toFixed(2)} · ${quality}${adaptive ? ' auto' : ''}`;
  }
  scheduled = false;
  schedule();
}
// One pending frame at most: a quick hide-and-show must not start a second
// render loop beside the first.
let scheduled = false;
function schedule() {
  if (manual || document.hidden || scheduled) return;
  scheduled = true;
  // rAF passes a timestamp in ms; frame() takes an optional step in seconds.
  requestAnimationFrame(() => frame());
}
document.addEventListener('visibilitychange', () => {
  if (!manual && !document.hidden && !scheduled) {
    clock.update();
    schedule();
  }
});

controls.snap = true;
await poll();
if (manual) {
  // advance(dt): move the virtual clock, poll once per virtual second, draw one frame.
  let lastPoll = 0;
  window.__senate.advance = async (dt) => {
    window.__senateClock += dt * 1000;
    if (window.__senateClock - lastPoll >= POLL_MS) {
      lastPoll = window.__senateClock;
      await poll();
    }
    frame(dt);
    stats.fps = 1;
  };
  frame(1 / 30);
  stats.fps = 1;
} else {
  schedule();
}
setTimeout(() => document.querySelector('#loading').classList.add('gone'), 250);
document.body.dataset.source = source;
