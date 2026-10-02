import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import * as THREE from '../world/vendor/three.module.min.js';
import { BattleEffects, battleBeat, BATTLE_PERIOD, standardMaterial } from '../world/js/battle.js';
import { mergeCharacterSkin, kayMotion } from '../world/js/kay.js';
import { chooseQuality, ResolutionBudget } from '../world/js/quality.js';

assert.equal(chooseQuality('high', 2, true), 'high', 'explicit quality wins');
assert.equal(chooseQuality('toString', 2), 'low', 'prototype keys are not quality tiers');
assert.equal(chooseQuality(null, 6), 'medium');
assert.equal(chooseQuality(null, 12), 'high');
assert.equal(chooseQuality(null, 12, true), 'low');
const budget = new ResolutionBudget(2);
for (let i = 0; i < 10; i++) budget.sample(1 / 30);
assert.equal(budget.ratio, 2, 'short spikes do not resize render targets');
for (let i = 0; i < 100; i++) budget.sample(1 / 30);
assert.equal(budget.ratio, 1.75, 'sustained pressure lowers resolution');
for (let i = 0; i < 1000; i++) budget.sample(1 / 30);
assert.equal(budget.ratio, 1, 'resolution has a lower bound');
for (let i = 0; i < 2500; i++) budget.sample(1 / 60);
assert.equal(budget.ratio, 2, 'headroom restores detail');
budget.sample(30);
assert.equal(budget.slow, 0, 'tab suspension is not pressure');
assert.equal(budget.fast, 0);
const smallDisplay = new ResolutionBudget(0.8);
for (let i = 0; i < 1000; i++) smallDisplay.sample(1 / 30);
assert.equal(smallDisplay.ratio, 0.8);
assert.equal(battleBeat(BATTLE_PERIOD * 4 + 0.1).cycle, 4);
assert.ok(battleBeat(BATTLE_PERIOD * 0.28).advance > 0.99);
assert.ok(battleBeat(BATTLE_PERIOD * 0.72).answer > 0.99);

// Exercise real Three geometry, buffers and resource disposal without a GPU.
const scene = new THREE.Scene();
const effects = new BattleEffects(scene, 'low');
const geometry = effects.dustPool.mesh.geometry;
const matrices = effects.dustPool.mesh.instanceMatrix;
const source = new THREE.Vector3(-75, 0.02, 0);
for (let i = 0; i < 10000; i++) effects.impact(source);
assert.equal(scene.children.length, 2, 'effects have exactly two draw objects');
assert.equal(effects.dustPool.live, 48);
assert.equal(effects.sparkPool.live, 24);
effects.update(0.1);
assert.equal(effects.dustPool.mesh.geometry, geometry);
assert.equal(effects.dustPool.mesh.instanceMatrix, matrices, 'emission never reallocates buffers');
assert.deepEqual(source.toArray(), [-75, 0.02, 0], 'emission preserves caller position');
effects.update(2);
assert.equal(effects.dustPool.live, 0);
assert.equal(effects.sparkPool.mesh.visible, false);
effects.dust(source);
effects.update(0.1);
assert.equal(effects.dustPool.live, 1, 'expired slots are reusable');
let disposed = 0;
geometry.addEventListener('dispose', () => disposed++);
effects.dispose();
assert.equal(disposed, 1);
assert.equal(scene.children.length, 0);

// Skin fusion must retain joints/weights and reject incompatible transforms
// or directly animated body parts. The animation rig stays independently cloned.
function skinFixture(offset = 0) {
  const root = new THREE.Group();
  const bone = new THREE.Bone();
  root.add(bone);
  const skeleton = new THREE.Skeleton([bone]);
  const material = new THREE.MeshStandardMaterial();
  for (let i = 0; i < 2; i++) {
    const g = new THREE.BoxGeometry();
    const count = g.attributes.position.count;
    g.setAttribute('skinIndex', new THREE.Uint16BufferAttribute(new Uint16Array(count * 4), 4));
    const weights = new Float32Array(count * 4);
    for (let k = 0; k < count; k++) weights[k * 4] = 1;
    g.setAttribute('skinWeight', new THREE.Float32BufferAttribute(weights, 4));
    const part = new THREE.SkinnedMesh(g, material);
    part.name = `part${i}`;
    root.add(part);
    part.bind(skeleton, new THREE.Matrix4());
    part.position.x = i * offset;
  }
  return root;
}
const body = mergeCharacterSkin(skinFixture());
const merged = body.children.filter(o => o.isSkinnedMesh);
assert.equal(merged.length, 1);
assert.equal(merged[0].geometry.attributes.skinWeight.count, 48);
assert.equal(merged[0].skeleton.bones[0], body.children[0]);
assert.equal(mergeCharacterSkin(skinFixture(1)).children.filter(o => o.isSkinnedMesh).length, 2);
const clip = new THREE.AnimationClip('body movement', 1, [new THREE.VectorKeyframeTrack('part0.position', [0, 1], [0, 0, 0, 1, 0, 0])]);
assert.equal(mergeCharacterSkin(skinFixture(), [clip]).children.filter(o => o.isSkinnedMesh).length, 2);
const clone = THREE.SkeletonUtils.clone(body);
const cloneSkin = clone.children.find(o => o.isSkinnedMesh);
assert.notEqual(cloneSkin.skeleton.bones[0], merged[0].skeleton.bones[0]);
assert.equal(cloneSkin.geometry, merged[0].geometry, 'character clones share immutable geometry');

// Pose throttling must save mixer evaluations without losing elapsed animation
// time, and changes of activity must not wait for the next visual tick.
function animationFixture() {
  const ticks = [];
  const action = { reset() { return this; }, setEffectiveWeight() { return this; }, fadeIn() { return this; }, fadeOut() { return this; }, play() { return this; } };
  return { ticks, rig: {
    animationHz: 24, lastCallT: null, lastT: null, lastMotion: null, animationDebt: 0,
    current: null, pose: 'stand', phase: 0, bones: {},
    clips: { Idle: new THREE.AnimationClip('Idle', 1, []), Blocking: new THREE.AnimationClip('Blocking', 1, []) },
    mixer: { clipAction: () => action, update: dt => ticks.push(dt) },
  } };
}
for (const displayHz of [30, 60]) {
  const fixture = animationFixture();
  for (let i = 0; i <= displayHz * 2; i++) kayMotion(fixture.rig, 'stand', i / displayHz);
  assert.ok(fixture.ticks.length >= 48 && fixture.ticks.length <= 50, 'pose budget remains 24 Hz across display rates');
  assert.ok(Math.abs(fixture.ticks.reduce((a, b) => a + b, 0) - 2) < 0.00001, 'throttling preserves elapsed animation time');
  const count = fixture.ticks.length;
  kayMotion(fixture.rig, 'guard', 2.001);
  assert.equal(fixture.ticks.length, count + 1, 'a new activity poses immediately');
}

// Drive the actual Cohort state machine with lightweight figures, isolating
// projection behavior from model loading and canvas rendering.
let rigsDisposed = 0;
const makeFigure = () => {
  const fig = new THREE.Group();
  fig.add(new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshStandardMaterial()));
  fig.userData.rig = { phase: 0 };
  fig.userData.disposeRig = () => rigsDisposed++;
  return fig;
};
const motions = Object.fromEntries(['stand', 'atease', 'hail', 'fight', 'guard', 'walk', 'jog', 'cheer'].map(name => [name, rig => { rig.motion = name; }]));
const context = vm.createContext({
  console, URLSearchParams, document: { createElement: () => ({ getContext: () => ({ fillRect() {}, strokeRect() {}, fillText() {} }) }) },
});
const module = new vm.SourceTextModule(await readFile(new URL('../world/js/march.js', import.meta.url), 'utf8'), { context });
function synthetic(exports) {
  return new vm.SyntheticModule(Object.keys(exports), function() {
    for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
  }, { context });
}
await module.link(name => {
  if (name.includes('three.module')) return synthetic(THREE);
  if (name === './figures.js') return synthetic({ MOTIONS: motions, makeFigure });
  if (name === './battle.js') return synthetic({ battleBeat, standardMaterial });
  throw new Error(name);
});
await module.evaluate();
const Cohort = module.namespace.Cohort;
const impactLog = [];
function cohort(quality, arriving = false) {
  const label = new THREE.Object3D();
  label.element = { remove() {} };
  return new Cohort(scene, {
    slot: { pos: source, yaw: -Math.PI / 2 },
    path: [new THREE.Vector3(-70, 0.02, 0), new THREE.Vector3(-72, 0.02, 0), new THREE.Vector3(-73, 0.02, 0)],
    fortGate: new THREE.Vector3(-84, 0.02, 0), numeral: 'I', tunic: '#cc8844', variant: 1, label, quality, arriving,
    effects: { impact: (_, shield) => impactLog.push(shield === true), dust() {} },
  });
}
const unit = cohort('high');
assert.equal(unit.members.length, 7);
assert.equal(unit.band.length, 6);
unit.setOrder({ state: 'working', activity: 'coding' });
unit.update(1 / 30, 2, 1);
const impacts = impactLog.length;
unit.update(1 / 30, 2, 1);
assert.equal(impactLog.length, impacts, 'one impact per beat, not per frame');
for (const state of ['blocked', 'failed', 'in_review']) {
  unit.setOrder({ state, activity: 'coding' });
  const before = impactLog.length;
  for (let i = 0; i < 100; i++) unit.update(0.03, 3 + i * 0.03, 1);
  assert.equal(impactLog.length, before, `${state} must not produce battle effects`);
}
unit.setOrder({ state: 'working', activity: 'testing' });
impactLog.length = 0;
for (let i = 0; i < 100; i++) unit.update(0.03, 7 + i * 0.03, 1);
assert.ok(impactLog.length > 0);
assert.ok(impactLog.every(shield => shield), 'checks only produce defensive impacts');
unit.recall(true);
for (let i = 0; i < 300 && !unit.done; i++) unit.update(0.1, 11 + i * 0.1, 1);
assert.equal(unit.done, true, 'delivered Cohort cheers, returns and disposes');
assert.equal(rigsDisposed, 13);
assert.equal(scene.children.length, 0);
unit.dispose();
assert.equal(rigsDisposed, 13, 'disposal is idempotent');
const smallUnit = cohort('low', true);
assert.equal(smallUnit.members.length, 4);
smallUnit.recall(false);
for (let i = 0; i < 20; i++) smallUnit.update(0.1, i * 0.1, 1);
assert.equal(smallUnit.done, true, 'recall during muster fades safely');
assert.equal(scene.children.length, 0);
console.log('World rendering checks passed: quality budget, bounded particles, compatible skin fusion, combat states, defensive checks, return and disposal.');
