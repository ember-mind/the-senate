// Stylized figures. One rig for everybody: hips with two-segment legs,
// a torso, a head with eyes and a nose (so facing reads at a distance),
// two-segment arms on pivots. Roles differ by garment, color and props —
// never by color alone: the Consul wears an ankle-length toga with a red
// border and holds a scroll, the Censor a dark robe and ivory stole, Cohort
// members a belted knee-length work tunic, and You a travelling cloak.

import * as THREE from '../vendor/three.module.min.js';
import { GLTFLoader } from '../vendor/three.module.min.js';
import { robe, std } from './materials.js';
import { bake } from './architecture.js';
import { kayMotion, kayReady, makeKayFigure } from './kay.js';

const skinTones = ['#c99a78', '#b07e5c', '#d8ab86', '#9c6a4a', '#c08a66'];
const cache = new Map();
function flat(color, rough = 0.75, rim = 0.4) {
  const key = `${color}-${rough}-${rim}`;
  if (!cache.has(key)) cache.set(key, std({ color, roughness: rough }, { ao: 0, rim }));
  return cache.get(key);
}

// Lathe profiles must run bottom to top or the faces point inward and the
// garment disappears from outside; accept either order.
function lathe(points, segs = 18, start = 0, length = Math.PI * 2) {
  const pts = points[0][1] > points.at(-1)[1] ? [...points].reverse() : points;
  return new THREE.LatheGeometry(pts.map(([r, y]) => new THREE.Vector2(r, y)), segs, start, length);
}

function mesh(geometry, material) {
  const m = new THREE.Mesh(geometry, material);
  m.castShadow = true;
  m.receiveShadow = true;
  return m;
}

const G = {
  // Garments hang from the waist (y = 0) down.
  toga: lathe([[0.2, 0], [0.23, -0.2], [0.27, -0.55], [0.31, -0.86], [0.32, -0.9], [0.0, -0.9]]),
  tunic: lathe([[0.2, 0], [0.23, -0.15], [0.27, -0.38], [0.29, -0.46], [0.0, -0.46]]),
  hemToga: lathe([[0.322, -0.84], [0.326, -0.9], [0.31, -0.905]], 22),
  hemTunic: lathe([[0.292, -0.41], [0.296, -0.46], [0.28, -0.465]], 22),
  torso: lathe([[0.2, 0], [0.22, 0.12], [0.24, 0.3], [0.23, 0.4], [0.16, 0.5], [0.07, 0.54], [0.0, 0.545]]),
  belt: new THREE.TorusGeometry(0.205, 0.018, 6, 22),
  head: new THREE.SphereGeometry(0.13, 18, 14),
  hair: new THREE.SphereGeometry(0.138, 18, 10, 0, Math.PI * 2, 0, Math.PI * 0.52),
  nose: new THREE.ConeGeometry(0.022, 0.055, 5),
  eye: new THREE.SphereGeometry(0.014, 6, 5),
  neck: new THREE.CylinderGeometry(0.052, 0.062, 0.12, 8),
  shoulder: new THREE.SphereGeometry(0.075, 10, 8),
  sleeve: new THREE.CylinderGeometry(0.075, 0.068, 0.18, 10),
  upperArm: new THREE.CapsuleGeometry(0.052, 0.2, 4, 8),
  foreArm: new THREE.CapsuleGeometry(0.047, 0.2, 4, 8),
  hand: new THREE.SphereGeometry(0.055, 8, 6),
  thigh: new THREE.CapsuleGeometry(0.075, 0.3, 4, 8),
  shin: new THREE.CapsuleGeometry(0.058, 0.3, 4, 8),
  foot: new THREE.BoxGeometry(0.1, 0.06, 0.22),
  drape: new THREE.TorusGeometry(0.22, 0.06, 7, 20, Math.PI * 1.1),
  drapeBorder: new THREE.TorusGeometry(0.245, 0.02, 5, 20, Math.PI * 1.1),
  stole: new THREE.TorusGeometry(0.2, 0.035, 5, 16, Math.PI),
  scroll: new THREE.CylinderGeometry(0.035, 0.035, 0.3, 8),
  // Open at the front: three-quarters of a lathe.
  cloak: lathe([[0.27, 0], [0.3, -0.25], [0.34, -0.62], [0.35, -0.66]], 18, Math.PI * 0.2, Math.PI * 1.6),
  lap: new THREE.BoxGeometry(0.44, 0.16, 0.48),
  togaFall: lathe([[0.26, 0], [0.28, -0.3], [0.3, -0.44], [0.0, -0.44]]),
};
G.head.scale(1, 1.1, 1.04);
G.upperArm.translate(0, -0.13, 0);
G.foreArm.translate(0, -0.12, 0);
G.thigh.translate(0, -0.2, 0);
G.shin.translate(0, -0.2, 0);
G.foot.translate(0, -0.03, 0.05);
G.sleeve.translate(0, -0.06, 0);

// Authored parts from Blender (world/assets-src/figures.py). Each named part
// is modelled at the pivot the rig expects, so a part swaps in for its
// procedural stand-in one-for-one; if the file fails to load, the figures
// fall back to the procedural shapes above.
let PARTS = null;
export async function loadFigureParts(url = 'assets/figures.glb') {
  try {
    const gltf = await new GLTFLoader().loadAsync(url);
    const parts = {};
    gltf.scene.traverse((o) => {
      if (o.isMesh) parts[o.name] = o.geometry;
    });
    PARTS = parts;
  } catch (error) {
    console.warn('[senate] figure parts unavailable, using procedural figures', error);
  }
}
const part = (name, fallback) => (PARTS && PARTS[name]) || fallback;

const ROLES = {
  consul: { garment: 'toga', robe: '#e0d5bf', trim: '#8e2b22', drape: '#e8dfcc', hair: '#a09a90', hairPart: 'hairGrey', wreath: true, skin: skinTones[0], prop: 'scroll' },
  censor: { garment: 'toga', robe: '#2e2c34', trim: '#e9dfc9', stole: '#efe5cf', hair: '#d3cfc6', hairPart: 'hairFringe', skin: skinTones[3] },
  you: { garment: 'tunic', robe: '#35566b', trim: '#d9ad5c', cloak: '#2b4657', belt: '#6b4a2a', hair: '#3a2a1d', skin: skinTones[2] },
  cohort: { garment: 'tunic', robe: '#b98a52', trim: '#5b3a22', belt: '#4a3222', hair: '#2d2019', skin: skinTones[1] },
};

// Cohort tunics vary by desk so neighbours don't blur together; identity is
// still carried by the standard and the plaque, never by the tunic alone.
export const COHORT_TUNICS = ['#b98a52', '#6f7f8c', '#8b8a54', '#a65f45', '#7a6a8c', '#5e7d6a'];
const HAIR = ['#2d2019', '#4a3223', '#1f1a17', '#6b4a2e', '#3a2a1d', '#8a8176'];

export function makeFigure(role = 'cohort', { pose = 'stand', tunic, variant = 0, armed = false } = {}) {
  if (kayReady()) return makeKayFigure(role, { pose, tunic, variant, armed });
  const spec = { ...ROLES[role] };
  if (tunic) spec.robe = tunic;
  if (role === 'cohort') {
    spec.hair = HAIR[variant % HAIR.length];
    spec.skin = skinTones[(variant * 3 + 1) % skinTones.length];
  }
  const R = robe(spec.robe);
  const T = robe(spec.trim);
  const SK = flat(spec.skin, 0.65, 0.5);
  const sitting = pose === 'sit';

  const root = new THREE.Group();
  root.name = `figure-${role}`;
  const body = new THREE.Group();
  root.add(body);

  // Hips: the pivot for legs and garment. Seated figures sit on the stool.
  const hips = new THREE.Group();
  hips.position.y = sitting ? 0.5 : 0.92;
  body.add(hips);

  const legs = [];
  const legMat = spec.garment === 'toga' ? R : SK;
  for (const side of [-1, 1]) {
    const hip = new THREE.Group();
    hip.position.set(side * 0.1, 0, 0);
    hips.add(hip);
    hip.add(mesh(part('thigh', G.thigh), legMat));
    const knee = new THREE.Group();
    knee.position.y = -0.46;
    hip.add(knee);
    knee.add(mesh(part('shin', G.shin), spec.garment === 'toga' && !sitting ? R : SK));
    const foot = mesh(part('foot', G.foot), flat('#5a3e2a', 0.8, 0.1));
    foot.position.y = -0.43;
    knee.add(foot);
    legs.push({ hip, knee, foot, side });
  }

  // Garment below the waist.
  const skirt = new THREE.Group();
  hips.add(skirt);
  if (sitting) {
    const lap = mesh(G.lap, R);
    lap.position.set(0, -0.02, 0.19);
    skirt.add(lap);
    if (spec.garment === 'toga') {
      const fall = mesh(G.togaFall, R);
      fall.scale.set(0.95, 1, 0.75);
      fall.position.set(0, -0.06, 0.42);
      skirt.add(fall);
    }
  } else if (spec.garment === 'toga') {
    skirt.add(mesh(part('toga', G.toga), R));
    skirt.add(mesh(part('hemToga', G.hemToga), T));
  } else {
    skirt.add(mesh(part('tunic', G.tunic), R));
    skirt.add(mesh(part('hemTunic', G.hemTunic), T));
  }

  // Upper body.
  const chest = new THREE.Group();
  chest.position.y = hips.position.y;
  body.add(chest);
  const torso = mesh(part('torso', G.torso), R);
  if (!PARTS) torso.scale.set(1.12, 1, 1.02);
  chest.add(torso);
  if (spec.belt) {
    const belt = mesh(G.belt, flat(spec.belt, 0.6, 0.2));
    belt.rotation.x = Math.PI / 2;
    belt.position.y = 0.03;
    belt.scale.set(1.12, 1.02, 1);
    chest.add(belt);
  }
  if (spec.drape) {
    // Toga drape over the left shoulder, across the chest, with its border.
    if (PARTS) {
      chest.add(mesh(PARTS.drape, robe(spec.drape)));
      chest.add(mesh(PARTS.drapeBorder, T));
    } else {
      const d = mesh(G.drape, robe(spec.drape));
      d.rotation.set(0.25, Math.PI / 2, 0.95);
      d.position.set(0.03, 0.24, 0);
      chest.add(d);
      const border = mesh(G.drapeBorder, T);
      border.rotation.copy(d.rotation);
      border.position.copy(d.position);
      chest.add(border);
    }
  }
  if (spec.stole) {
    if (PARTS) {
      chest.add(mesh(PARTS.stole, robe(spec.stole)));
    } else {
      const s = mesh(G.stole, robe(spec.stole));
      s.rotation.set(-0.35, 0, Math.PI);
      s.position.set(0, 0.49, 0.04);
      s.scale.set(1.08, 1.5, 1);
      chest.add(s);
    }
  }
  if (spec.cloak) {
    const c = mesh(part('cloak', G.cloak), robe(spec.cloak));
    c.position.y = PARTS ? 0.48 : 0.5;
    c.rotation.y = PARTS ? 0 : Math.PI;
    chest.add(c);
    const clasp = mesh(new THREE.SphereGeometry(0.035, 8, 6), flat('#d9ad5c', 0.3, 0.2));
    clasp.position.set(0.14, 0.46, 0.17);
    chest.add(clasp);
  }

  // Head.
  const neck = mesh(part('neck', G.neck), SK);
  neck.position.y = 0.57;
  chest.add(neck);
  const head = new THREE.Group();
  head.position.y = 0.76;
  head.scale.setScalar(1.3);
  chest.add(head);
  if (PARTS) {
    head.add(mesh(PARTS.head, SK));
    head.add(new THREE.Mesh(PARTS.eyes, flat('#231a14', 0.4, 0)));
    head.add(mesh(PARTS.brows, flat(spec.hair, 0.9, 0.2)));
    head.add(mesh(PARTS[spec.hairPart || 'hair'], flat(spec.hair, 0.9, 0.3)));
    if (spec.wreath) head.add(mesh(PARTS.wreath, flat('#5f7a3a', 0.8, 0.3)));
  } else {
    head.add(mesh(G.head, SK));
    const hairMesh = mesh(G.hair, flat(spec.hair, 0.9, 0.3));
    hairMesh.rotation.x = -0.4;
    hairMesh.position.set(0, 0.018, -0.02);
    head.add(hairMesh);
    const nose = mesh(G.nose, SK);
    nose.rotation.x = Math.PI / 2;
    nose.position.set(0, -0.012, 0.135);
    head.add(nose);
    const eyeMat = flat('#2a1f18', 0.4, 0);
    for (const side of [-1, 1]) {
      const e = new THREE.Mesh(G.eye, eyeMat);
      e.position.set(side * 0.045, 0.02, 0.118);
      head.add(e);
    }
  }

  // Arms: shoulder pivot → elbow pivot.
  const arms = [];
  for (const side of [-1, 1]) {
    const shoulder = new THREE.Group();
    shoulder.position.set(side * 0.27, 0.43, 0);
    chest.add(shoulder);
    shoulder.add(mesh(part('shoulder', G.shoulder), R));
    shoulder.add(mesh(part('sleeve', G.sleeve), R));
    shoulder.add(mesh(part('upperArm', G.upperArm), spec.garment === 'toga' ? R : SK));
    const elbow = new THREE.Group();
    elbow.position.y = -0.28;
    shoulder.add(elbow);
    elbow.add(mesh(part('foreArm', G.foreArm), SK));
    const hand = mesh(part('hand', G.hand), SK);
    hand.position.y = -0.28;
    elbow.add(hand);
    arms.push({ shoulder, elbow, hand, side });
  }

  if (spec.prop === 'scroll') {
    const scroll = mesh(G.scroll, flat('#efe2c4', 0.9, 0.2));
    scroll.rotation.z = Math.PI / 2;
    scroll.position.set(0, -0.03, 0.03);
    arms[0].hand.add(scroll);
  }

  const rig = { root, body, hips, chest, head, arms, legs, pose, phase: variant * 1.7 + (role.length % 5) };
  root.userData.rig = rig;
  // One mesh per material per rigid part: every bone group bakes only its
  // own direct meshes, so the pivots keep working.
  for (const part of [skirt, head, ...arms.flatMap((a) => [a.shoulder, a.elbow]), ...legs.flatMap((l) => [l.hip, l.knee])]) bakeDirect(part);
  bakeDirect(chest);
  rest(rig);
  return root;
}

// Merges a group's direct mesh children (not its sub-groups) per material.
function bakeDirect(group) {
  const holder = new THREE.Group();
  const direct = group.children.filter((c) => c.isMesh);
  if (direct.length < 2) return;
  for (const m of direct) holder.add(m);
  bake(holder);
  for (const m of [...holder.children]) group.add(m);
}

function rest(rig) {
  for (const a of rig.arms) {
    a.shoulder.rotation.set(0.05, 0, a.side * 0.1);
    a.elbow.rotation.set(-0.2, 0, 0);
  }
  for (const l of rig.legs) {
    if (rig.pose === 'sit') {
      l.hip.rotation.set(-Math.PI / 2 + 0.05, 0, l.side * 0.05);
      l.knee.rotation.set(Math.PI / 2 - 0.1, 0, 0);
    } else {
      l.hip.rotation.set(0, 0, 0);
      l.knee.rotation.set(0, 0, 0);
    }
  }
}

const lerp = (a, b, t) => a + (b - a) * t;
function ease(obj, prop, target, k) {
  obj[prop] = lerp(obj[prop], target, k);
}

function settleLegs(rig, k) {
  if (rig.pose === 'sit') return;
  for (const l of rig.legs) {
    ease(l.hip.rotation, 'x', 0, Math.min(1, k * 2));
    ease(l.knee.rotation, 'x', 0, Math.min(1, k * 2));
  }
  rig.body.position.y = lerp(rig.body.position.y, 0, k);
}

// Motions. `t` is seconds, `k` the easing factor for this frame. Every motion
// eases toward its pose so switching activities never snaps.
const PROC_MOTIONS = {
  // Standing quietly: breath and an occasional glance.
  stand(rig, t, k) {
    const b = Math.sin(t * 1.3 + rig.phase) * 0.012;
    rig.chest.scale.setScalar(1 + b * 0.3);
    ease(rig.head.rotation, 'y', Math.sin(t * 0.21 + rig.phase) * 0.25, k);
    ease(rig.head.rotation, 'x', 0.04, k);
    ease(rig.chest.rotation, 'x', 0, k);
    for (const a of rig.arms) {
      ease(a.shoulder.rotation, 'x', 0.05, k);
      ease(a.shoulder.rotation, 'z', a.side * 0.1, k);
      ease(a.elbow.rotation, 'x', -0.2, k);
    }
    settleLegs(rig, k);
  },
  // Consul studying the campaign board, scroll in hand.
  study(rig, t, k) {
    ease(rig.head.rotation, 'x', -0.12 + Math.sin(t * 0.4) * 0.03, k);
    ease(rig.head.rotation, 'y', Math.sin(t * 0.17 + rig.phase) * 0.18, k);
    const [l, r] = rig.arms;
    ease(l.shoulder.rotation, 'x', -0.55, k);
    ease(l.elbow.rotation, 'x', -1.2, k);
    ease(r.shoulder.rotation, 'x', -0.2, k);
    ease(r.elbow.rotation, 'x', -1.5, k);
    ease(r.shoulder.rotation, 'z', 0.35, k);
    settleLegs(rig, k);
  },
  // Coding: hands on the tablet-terminal, small alternating strokes, head
  // down toward the screen.
  code(rig, t, k) {
    // Writing: head bowed over the sheet, the right hand moving the quill
    // along a line, the left hand steadying the sheet.
    ease(rig.head.rotation, 'x', 0.42, k);
    ease(rig.head.rotation, 'y', Math.sin(t * 0.6 + rig.phase) * 0.06, k);
    ease(rig.chest.rotation, 'x', 0.18, k);
    const [l, r] = rig.arms;
    ease(l.shoulder.rotation, 'x', -0.7, k);
    ease(l.shoulder.rotation, 'z', -0.25, k);
    ease(l.elbow.rotation, 'x', -1.0, k);
    const stroke = (t * 0.7 + rig.phase) % 1;
    ease(r.shoulder.rotation, 'x', -0.8 + Math.sin(t * 13) * 0.03, 0.5);
    ease(r.shoulder.rotation, 'z', 0.12 + stroke * 0.28, 0.4);
    ease(r.elbow.rotation, 'x', -1.05 + Math.sin(t * 17 + rig.phase) * 0.05, 0.5);
  },
  // Reading: head moves between the tablets laid on the desk; one hand turns
  // a leaf from time to time.
  read(rig, t, k) {
    const look = Math.sin(t * 0.45 + rig.phase);
    ease(rig.head.rotation, 'y', look * 0.45, k);
    ease(rig.head.rotation, 'x', 0.35, k);
    ease(rig.chest.rotation, 'x', 0.1, k);
    const [l, r] = rig.arms;
    ease(l.shoulder.rotation, 'x', -0.6, k);
    ease(l.elbow.rotation, 'x', -0.7, k);
    const turn = Math.max(0, Math.sin(t * 0.9 + rig.phase)) ** 8;
    ease(r.shoulder.rotation, 'x', -0.7 - turn * 0.4, 0.3);
    ease(r.shoulder.rotation, 'z', 0.2 + turn * 0.3, 0.3);
    ease(r.elbow.rotation, 'x', -0.8, k);
  },
  // Checks running: sits back, watching the lamps of the apparatus.
  watch(rig, t, k) {
    ease(rig.head.rotation, 'y', -0.5, k);
    ease(rig.head.rotation, 'x', 0.15, k);
    ease(rig.chest.rotation, 'x', -0.08, k);
    for (const a of rig.arms) {
      ease(a.shoulder.rotation, 'x', -0.45, k);
      ease(a.elbow.rotation, 'x', -1.0, k);
      ease(a.shoulder.rotation, 'z', a.side * 0.1, k);
    }
  },
  // Handed over, or between stages: sits back, hands in the lap.
  wait(rig, t, k) {
    ease(rig.head.rotation, 'y', Math.sin(t * 0.15 + rig.phase) * 0.3, k);
    ease(rig.head.rotation, 'x', 0.05, k);
    ease(rig.chest.rotation, 'x', -0.1, k);
    for (const a of rig.arms) {
      ease(a.shoulder.rotation, 'x', -0.35, k);
      ease(a.elbow.rotation, 'x', -0.9, k);
      ease(a.shoulder.rotation, 'z', a.side * 0.08, k);
    }
  },
  // Stopped: needs a person. Turned toward the Forum, one hand raised.
  halt(rig, t, k) {
    ease(rig.head.rotation, 'y', -0.9, k);
    ease(rig.head.rotation, 'x', -0.1, k);
    ease(rig.chest.rotation, 'x', -0.05, k);
    const [l, r] = rig.arms;
    ease(l.shoulder.rotation, 'x', -0.3, k);
    ease(l.elbow.rotation, 'x', -1.1, k);
    ease(r.shoulder.rotation, 'x', -2.4, k);
    ease(r.shoulder.rotation, 'z', 0.25, k);
    ease(r.elbow.rotation, 'x', -0.4, k);
  },
  // Consul sending a legion: standing, the right arm raised.
  hail(rig, t, k) {
    const [, r] = rig.arms;
    ease(r.shoulder.rotation, 'x', -2.4, k);
    ease(r.shoulder.rotation, 'z', 0.25, k);
    ease(r.elbow.rotation, 'x', -0.2, k);
  },
  // Censor examining a submitted tablet: head bowed, one hand at the chin,
  // the other tracing down the page.
  examine(rig, t, k) {
    ease(rig.head.rotation, 'x', 0.42, k);
    ease(rig.head.rotation, 'y', Math.sin(t * 0.3) * 0.12, k);
    ease(rig.chest.rotation, 'x', 0.15, k);
    const [l, r] = rig.arms;
    ease(l.shoulder.rotation, 'x', -1.0, k);
    ease(l.shoulder.rotation, 'z', -0.4, k);
    ease(l.elbow.rotation, 'x', -2.0, k);
    const trace = (t * 0.25) % 1;
    ease(r.shoulder.rotation, 'x', -0.55 - trace * 0.25, 0.2);
    ease(r.elbow.rotation, 'x', -0.9, k);
  },
  // Walking: legs swing from the hips, knees bend on the back swing, arms
  // counter-swing.
  walk(rig, t, k, speed = 1) {
    const w = t * 7 * speed;
    const s = Math.sin(w);
    rig.legs.forEach((l, i) => {
      l.hip.rotation.x = (i ? s : -s) * 0.45;
      l.knee.rotation.x = Math.max(0, i ? -Math.cos(w) : Math.cos(w)) * 0.7;
    });
    rig.body.position.y = Math.abs(Math.cos(w)) * 0.035;
    rig.arms.forEach((a, i) => {
      ease(a.shoulder.rotation, 'x', (i ? -s : s) * 0.45, 0.5);
      ease(a.shoulder.rotation, 'z', a.side * 0.1, k);
      ease(a.elbow.rotation, 'x', -0.35, k);
    });
    ease(rig.head.rotation, 'x', 0, k);
    ease(rig.head.rotation, 'y', 0, k);
    ease(rig.chest.rotation, 'x', 0.05, k);
  },
};

// Every motion routes to the rigged characters when they are loaded.
// Field motions fall back to the nearest procedural pose when the KayKit
// characters are unavailable.
PROC_MOTIONS.fight = PROC_MOTIONS.study;
PROC_MOTIONS.guard = PROC_MOTIONS.stand;
PROC_MOTIONS.jog = (rig, t, k, speed) => PROC_MOTIONS.walk(rig, t, k, (speed ?? 1) * 1.6);
PROC_MOTIONS.atease = PROC_MOTIONS.stand;
PROC_MOTIONS.cheer = PROC_MOTIONS.hail;

export const MOTIONS = Object.fromEntries(
  Object.entries(PROC_MOTIONS).map(([name, fn]) => [name, (rig, t, k, speed) => (rig.kay ? kayMotion(rig, name, t, speed) : fn(rig, t, k, speed))]),
);

export function stopWalking(rig) {
  if (rig.kay || rig.pose !== 'stand') return;
  settleLegs(rig, 1);
}
