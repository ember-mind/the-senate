// Rigged characters from KayKit "Adventurers" (Kay Lousberg, CC0), dressed
// for Rome: hand props and hats hidden, outfits recoloured per role by a hue
// remap on the shared texture atlas. Each figure keeps the same small rig API
// the procedural figures expose, so the director drives either kind.

import * as THREE from '../vendor/three.module.min.js';
import { GLTFLoader, SkeletonUtils } from '../vendor/three.module.min.js';

const SOURCES = {
  Mage: 'assets/chars/Mage.min.glb',
  Rogue_Hooded: 'assets/chars/Rogue_Hooded.min.glb',
  Barbarian: 'assets/chars/Barbarian.min.glb',
  Rogue: 'assets/chars/Rogue.min.glb',
};
const HIDE = /^(1H_|2H_|Knife|Throwable|Spellbook|Mug|Barbarian_Round_Shield|Mage_Hat|Barbarian_Hat)/;

let KAY = null;
export async function loadKay() {
  try {
    const loader = new GLTFLoader();
    const entries = await Promise.all(Object.entries(SOURCES).map(async ([k, url]) => [k, await loader.loadAsync(url)]));
    KAY = Object.fromEntries(entries);
  } catch (error) {
    console.warn('[senate] KayKit characters unavailable, using procedural figures', error);
    KAY = null;
  }
}
export const kayReady = () => KAY !== null;

// Hue remap: pixels whose hue falls in [h0, h1] (degrees) and saturation is
// above sMin take the target colour, keeping their own shading (value).
function recolor(material, rules, key) {
  const m = material.clone();
  m.onBeforeCompile = (shader) => {
    const n = rules.length;
    shader.uniforms.uRules = { value: rules.map((r) => new THREE.Vector4(r.h0 / 360, r.h1 / 360, r.sMin ?? 0.18, 0)) };
    shader.uniforms.uTargets = { value: rules.map((r) => new THREE.Color(r.to)) };
    shader.uniforms.uLift = { value: rules.map((r) => r.lift ?? 1) };
    shader.fragmentShader = shader.fragmentShader
      .replace(
        '#include <common>',
        `#include <common>
        uniform vec4 uRules[${n}];
        uniform vec3 uTargets[${n}];
        uniform float uLift[${n}];
        vec3 rgb2hsv(vec3 c){ vec4 K = vec4(0.0, -1.0/3.0, 2.0/3.0, -1.0); vec4 p = mix(vec4(c.bg, K.wz), vec4(c.gb, K.xy), step(c.b, c.g)); vec4 q = mix(vec4(p.xyw, c.r), vec4(c.r, p.yzx), step(p.x, c.r)); float d = q.x - min(q.w, q.y); float e = 1.0e-10; return vec3(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x); }`,
      )
      .replace(
        '#include <map_fragment>',
        `#include <map_fragment>
        {
          vec3 hsv = rgb2hsv(diffuseColor.rgb);
          for (int i = 0; i < ${n}; i++) {
            bool inHue = uRules[i].x <= uRules[i].y ? (hsv.x >= uRules[i].x && hsv.x <= uRules[i].y) : (hsv.x >= uRules[i].x || hsv.x <= uRules[i].y);
            if (inHue && hsv.y >= uRules[i].z) {
              diffuseColor.rgb = uTargets[i] * clamp(hsv.z * uLift[i], 0.0, 1.4);
              break;
            }
          }
        }`,
      );
  };
  m.customProgramCacheKey = () => `kay-${key}`;
  return m;
}

// Roles: which body, which colours.
const PURPLE = { h0: 225, h1: 299 };
const GREEN = { h0: 90, h1: 175 };
const BLUE = { h0: 185, h1: 224 };
const ROLES = {
  consul: { body: 'Mage', rules: [{ ...PURPLE, to: '#f3ecdf', lift: 2.0 }, { h0: 300, h1: 359, sMin: 0.1, to: '#9a2c22', lift: 1.1 }] },
  censor: { body: 'Rogue_Hooded', rules: [{ ...GREEN, to: '#34313a', lift: 1.4 }] },
  you: { body: 'Rogue', rules: [{ ...GREEN, to: '#3f6781', lift: 1.2 }] },
  // The enemy: in furs and leather, with the pack's own axe, round shield
  // and horned helmet.
  barbarian: { body: 'Barbarian', rules: [{ ...BLUE, to: '#4a3a2e', lift: 1.1 }], show: /^(1H_Axe|Barbarian_Round_Shield|Barbarian_Hat)$/ },
};

let CONTACT = null;
function contactMaterial() {
  if (CONTACT) return CONTACT;
  const c = document.createElement('canvas');
  c.width = c.height = 128;
  const g = c.getContext('2d');
  const grad = g.createRadialGradient(64, 64, 4, 64, 64, 64);
  grad.addColorStop(0, 'rgba(0,0,0,0.55)');
  grad.addColorStop(1, 'rgba(0,0,0,0)');
  g.fillStyle = grad;
  g.fillRect(0, 0, 128, 128);
  CONTACT = new THREE.MeshBasicMaterial({ map: new THREE.CanvasTexture(c), transparent: true, depthWrite: false });
  return CONTACT;
}

// A legionary's arms, built here rather than taken from the pack (which has
// axes and a round shield): a short gladius and a curved rectangular
// scutum in legion red. Sized in the rig's own units.
let ARMS = null;
function armsParts() {
  if (ARMS) return ARMS;
  const steel = new THREE.MeshStandardMaterial({ color: '#c9ccd1', metalness: 0.9, roughness: 0.3 });
  const brass = new THREE.MeshStandardMaterial({ color: '#b58a3f', metalness: 0.85, roughness: 0.35 });
  const grip = new THREE.MeshStandardMaterial({ color: '#3b2a1c', roughness: 0.8 });
  const blade = new THREE.BoxGeometry(0.07, 0.62, 0.02).translate(0, 0.42, 0);
  const guard = new THREE.BoxGeometry(0.17, 0.04, 0.06).translate(0, 0.1, 0);
  const handle = new THREE.CylinderGeometry(0.025, 0.025, 0.16, 6).translate(0, 0.02, 0);
  const shield = new THREE.CylinderGeometry(0.75, 0.75, 0.95, 16, 1, true, -0.38, 0.76).translate(0, 0, -0.72);
  const boss = new THREE.SphereGeometry(0.1, 12, 8, 0, Math.PI * 2, 0, Math.PI / 2).rotateX(Math.PI / 2).translate(0, 0, 0.04);
  ARMS = { steel, brass, grip, blade, guard, handle, shield, boss, shieldMats: new Map() };
  return ARMS;
}
function shieldMaterial(tunic) {
  const A = armsParts();
  if (!A.shieldMats.has(tunic)) A.shieldMats.set(tunic, new THREE.MeshStandardMaterial({ color: tunic, roughness: 0.7, side: THREE.DoubleSide }));
  return A.shieldMats.get(tunic);
}
// The pack's hand slots hold weapons along +y and shields facing +z.
function arm(rig) {
  const A = armsParts();
  const right = rig.model.getObjectByName('handslotr');
  const left = rig.model.getObjectByName('handslotl');
  if (right) {
    const sword = new THREE.Group();
    sword.add(new THREE.Mesh(A.blade, A.steel), new THREE.Mesh(A.guard, A.brass), new THREE.Mesh(A.handle, A.grip));
    sword.traverse((o) => (o.castShadow = true));
    right.add(sword);
  }
  if (left) {
    const scutum = new THREE.Group();
    const face = new THREE.Mesh(A.shield, shieldMaterial('#8e2219'));
    const boss = new THREE.Mesh(A.boss, A.brass);
    scutum.add(face, boss);
    // The left slot's y runs across the forearm; stand the scutum upright.
    scutum.rotation.z = Math.PI / 2;
    scutum.position.set(0, 0.02, 0.16);
    scutum.traverse((o) => (o.castShadow = true));
    left.add(scutum);
  }
}

export function makeKayFigure(role, { pose = 'stand', tunic, variant = 0, armed = false } = {}) {
  let spec = ROLES[role];
  // A role's main garment can take a model's colour; its trims stay.
  if (spec && tunic) spec = { ...spec, rules: [{ ...spec.rules[0], to: tunic }, ...spec.rules.slice(1)] };
  if (!spec) {
    tunic = tunic || '#b98a52';
    // Cohort members alternate two bodies whose cloaks both take the tunic
    // colour, so every soldier shows the model it works for. The Barbarian
    // body is left to the enemy: its grey cloak takes no colour.
    const body = variant % 2 ? 'Rogue_Hooded' : 'Rogue';
    spec = { body, rules: [{ ...GREEN, to: tunic, lift: 1.15 }] };
  }
  const src = KAY[spec.body];
  const model = SkeletonUtils.clone(src.scene);
  const key = `${role}-${spec.body}-${spec.rules.map((r) => r.to).join('')}`;
  const cache = new Map();
  model.traverse((o) => {
    if (!o.isMesh) return;
    if (HIDE.test(o.name) && !spec.show?.test(o.name)) {
      o.visible = false;
      return;
    }
    o.castShadow = true;
    o.receiveShadow = true;
    if (!cache.has(o.material)) cache.set(o.material, recolor(o.material, spec.rules, key));
    o.material = cache.get(o.material);
  });
  // Our world is ~1.75 m per person; KayKit characters stand ~2.2 units.
  model.scale.setScalar(0.78);
  const root = new THREE.Group();
  root.add(model);
  // A soft contact shadow so the figure sits on the floor, not above it.
  const blob = new THREE.Mesh(new THREE.CircleGeometry(0.42, 24).rotateX(-Math.PI / 2), contactMaterial());
  blob.position.y = 0.012;
  blob.renderOrder = 1;
  root.add(blob);
  root.name = `kay-${role}`;

  const mixer = new THREE.AnimationMixer(model);
  const clips = Object.fromEntries(src.animations.map((a) => [a.name, a]));
  const bones = {};
  model.traverse((o) => {
    if (o.isBone) bones[o.name] = o;
  });
  const rig = {
    kay: true,
    root,
    model,
    mixer,
    clips,
    bones,
    pose,
    current: null,
    lastT: null,
    phase: variant * 1.7,
    // The procedural rig's handles, kept so shared code never trips.
    legs: [],
    arms: [],
    head: bones.head,
    chest: bones.chest,
    body: model,
  };
  if (armed) arm(rig);
  root.userData.rig = rig;
  return root;
}

function play(rig, name, fade = 0.35) {
  if (rig.current === name) return;
  const clip = rig.clips[name];
  if (!clip) return;
  const next = rig.mixer.clipAction(clip);
  next.reset().setEffectiveWeight(1).fadeIn(fade).play();
  if (rig.current) rig.mixer.clipAction(rig.clips[rig.current]).fadeOut(fade);
  rig.current = name;
}

// Motion names the director uses → a clip, plus a small procedural overlay
// on top for the activities the pack has no clip for (writing, a raised hand).
const CLIP = {
  stand: 'Idle',
  study: 'Unarmed_Idle',
  walk: 'Walking_A',
  code: 'Sit_Chair_Idle',
  read: 'Sit_Chair_Idle',
  watch: 'Sit_Chair_Idle',
  wait: 'Sit_Chair_Idle',
  halt: 'Sit_Chair_Idle',
  hail: 'Idle',
  guard: 'Blocking',
  jog: 'Running_A',
  atease: 'Unarmed_Idle',
  cheer: 'Cheer',
  examine: 'Sit_Chair_Idle',
};

export function kayMotion(rig, name, t, speed = 1) {
  const dt = rig.lastT === null ? 0 : Math.min(0.1, Math.max(0, t - rig.lastT));
  rig.lastT = t;
  // Fighting cycles through three sword strokes, each Cohort member on its
  // own beat so a rank never swings in unison.
  const FIGHT = ['1H_Melee_Attack_Chop', '1H_Melee_Attack_Slice_Diagonal', '1H_Melee_Attack_Stab'];
  const clip = name === 'fight' ? FIGHT[Math.floor((t + rig.phase) / 1.15) % 3] : rig.pose === 'sit' && (name === 'stand' || name === 'study') ? 'Sit_Chair_Idle' : CLIP[name] || 'Idle';
  play(rig, clip, name === 'fight' ? 0.18 : 0.35);
  // A faster walk plays the clip faster, so feet keep up with the ground.
  if (rig.current) rig.mixer.clipAction(rig.clips[rig.current]).timeScale = name === 'walk' || name === 'jog' ? speed : 1;
  rig.mixer.update(dt);
  const b = rig.bones;
  const s = Math.sin(t * 9 + rig.phase);
  // Overlays are small rotations added after the clip has posed the bones.
  if (name === 'code' && b.upperarmr && b.lowerarmr && b.head) {
    // Writing: the right arm reaches forward to the sheet and moves along
    // the line; the head bows over the desk.
    b.upperarmr.rotation.x -= 0.95;
    b.lowerarmr.rotation.x += s * 0.12;
    b.upperarmr.rotation.z += Math.sin(t * 0.7 + rig.phase) * 0.12;
    b.head.rotation.x += 0.3;
  } else if (name === 'read' && b.head) {
    b.head.rotation.x += 0.4;
    b.head.rotation.y += Math.sin(t * 0.45 + rig.phase) * 0.35;
  } else if (name === 'examine' && b.head) {
    b.head.rotation.x += 0.45;
  } else if (name === 'watch' && b.head) {
    b.head.rotation.y -= 0.5;
  } else if (name === 'hail' && b.upperarmr && b.lowerarmr) {
    // Standing, the right arm raised toward where the legion goes.
    b.upperarmr.rotation.x -= 2.5;
    b.upperarmr.rotation.z -= 0.35;
    b.lowerarmr.rotation.x -= 0.2;
  } else if (name === 'halt' && b.upperarmr && b.head) {
    // A raised hand: waiting for the user.
    b.upperarmr.rotation.x -= 2.9;
    b.upperarmr.rotation.z -= 0.2;
    b.head.rotation.x -= 0.15;
  }
}
