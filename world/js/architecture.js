// Static architecture of the campus: the Forum, the Legion Hall (a peristyle
// workshop), the Censors' exedra, the Curia and the Tabularium in the
// distance. Geometry is merged per material into a handful of draw calls.

import * as THREE from '../vendor/three.module.min.js';
import { mergeGeometries } from '../vendor/three.module.min.js';
import { rng } from './textures.js';
import { GLTFLoader } from '../vendor/three.module.min.js';
import { standardMaterial } from './battle.js';

// Authored trees from Blender (world/assets-src/trees.py); procedural
// fallback when the file does not load.
let TREES = null;
export async function loadTrees(url = 'assets/trees.glb') {
  try {
    const gltf = await new GLTFLoader().loadAsync(url);
    const t = {};
    gltf.scene.traverse((o) => {
      if (o.isMesh) t[o.name] = o.geometry;
    });
    TREES = t;
  } catch (error) {
    console.warn('[senate] trees unavailable, using procedural trees', error);
  }
}

// World-scaled UVs for boxes: `scale` metres per texture repeat.
export function box(w, h, d, scale = 4) {
  const g = new THREE.BoxGeometry(w, h, d);
  const uv = g.attributes.uv;
  const dims = [
    [d, h],
    [d, h],
    [w, d],
    [w, d],
    [w, h],
    [w, h],
  ];
  for (let f = 0; f < 6; f++) {
    for (let v = 0; v < 4; v++) {
      const i = f * 4 + v;
      uv.setXY(i, (uv.getX(i) * dims[f][0]) / scale, (uv.getY(i) * dims[f][1]) / scale);
    }
  }
  return g;
}

// Collects transformed geometries per material and merges them at the end.
export class Batch {
  constructor() {
    this.parts = new Map();
  }

  add(geometry, material, { x = 0, y = 0, z = 0, ry = 0, rx = 0, rz = 0, sx = 1, sy = 1, sz = 1 } = {}) {
    const m = new THREE.Matrix4().compose(
      new THREE.Vector3(x, y, z),
      new THREE.Quaternion().setFromEuler(new THREE.Euler(rx, ry, rz, 'YXZ')),
      new THREE.Vector3(sx, sy, sz),
    );
    const g = geometry.index ? geometry.toNonIndexed() : geometry.clone();
    g.applyMatrix4(m);
    for (const key of Object.keys(g.attributes)) {
      if (!['position', 'normal', 'uv'].includes(key)) g.deleteAttribute(key);
    }
    if (!g.attributes.uv) {
      g.setAttribute('uv', new THREE.BufferAttribute(new Float32Array((g.attributes.position.count) * 2), 2));
    }
    if (!this.parts.has(material)) this.parts.set(material, []);
    this.parts.get(material).push(g);
    return this;
  }

  // Adds a local batch transformed into this one.
  flush(parent, { castShadow = true, receiveShadow = true, name = 'batch' } = {}) {
    const meshes = [];
    for (const [material, geometries] of this.parts) {
      const merged = mergeGeometries(geometries, false);
      geometries.forEach(g => g.dispose());
      const mesh = new THREE.Mesh(merged, material);
      mesh.castShadow = castShadow;
      mesh.receiveShadow = receiveShadow;
      mesh.name = name;
      mesh.matrixAutoUpdate = false;
      mesh.updateMatrix();
      parent.add(mesh);
      meshes.push(mesh);
    }
    this.parts.clear();
    return meshes;
  }
}

// Tuscan column: base, shaft with entasis, echinus and abacus.
export function columnGeometry(h = 4.2, r = 0.3) {
  const p = [];
  const V = (x, y) => p.push(new THREE.Vector2(x, y));
  V(0, 0);
  V(r * 1.45, 0);
  V(r * 1.45, 0.12);
  V(r * 1.25, 0.16);
  V(r * 1.3, 0.24);
  V(r * 1.1, 0.3);
  const shaftTop = h - 0.42;
  for (let i = 0; i <= 8; i++) {
    const t = i / 8;
    const entasis = Math.sin(t * Math.PI) * 0.04 * r;
    V(r * (1 - 0.14 * t) + entasis, 0.3 + (shaftTop - 0.3) * t);
  }
  V(r * 0.95, shaftTop + 0.04);
  V(r * 0.95, shaftTop + 0.08);
  V(r * 1.3, shaftTop + 0.24);
  V(0, shaftTop + 0.24);
  const lathe = new THREE.LatheGeometry(p, 18);
  const abacus = box(r * 3, 0.18, r * 3, 1);
  abacus.translate(0, h - 0.09, 0);
  return mergeGeometries([lathe.toNonIndexed(), abacus.toNonIndexed()]);
}

// Wall panel with a round-headed opening, extruded.
export function archWall(w, h, depth, openingW, openingH) {
  const s = new THREE.Shape();
  s.moveTo(-w / 2, 0);
  s.lineTo(w / 2, 0);
  s.lineTo(w / 2, h);
  s.lineTo(-w / 2, h);
  s.lineTo(-w / 2, 0);
  const r = openingW / 2;
  const hole = new THREE.Path();
  hole.moveTo(-r, 0);
  hole.lineTo(-r, openingH - r);
  hole.absarc(0, openingH - r, r, Math.PI, 0, true);
  hole.lineTo(r, 0);
  hole.lineTo(-r, 0);
  s.holes.push(hole);
  const g = new THREE.ExtrudeGeometry(s, { depth, bevelEnabled: false, curveSegments: 12 });
  g.translate(0, 0, -depth / 2);
  const uv = g.attributes.uv;
  for (let i = 0; i < uv.count; i++) uv.setXY(i, uv.getX(i) / 4, uv.getY(i) / 4);
  return g;
}

// A sloped slab of roof tiles between two heights, as a thin box.
function roofSlab(batch, mat, { x, z, w, run, rise, ry, yLow }) {
  const len = Math.hypot(run, rise);
  const g = box(w, 0.18, len, 3);
  const angle = Math.atan2(rise, run);
  batch.add(g, mat, { x, y: yLow + rise / 2 + 0.09, z, ry, rx: angle });
}

function steps(batch, mat, { x, z, w, d, count, rise, ry = 0 }) {
  // Steps rising toward -z in local space.
  for (let i = 0; i < count; i++) {
    const g = box(w, rise * (i + 1), d / count, 2);
    const lz = d / 2 - (d / count) * (i + 0.5);
    const c = Math.cos(ry);
    const s = Math.sin(ry);
    batch.add(g, mat, { x: x + lz * s, y: (rise * (i + 1)) / 2, z: z + lz * c, ry });
  }
}

// Gable roof with pediments over a rectangle centred at (x, z), ridge along
// local z when ry = 0.
function gableRoof(batch, M, { x, z, w, d, y, pitch = 0.32, overhang = 0.4, ry = 0 }) {
  const half = w / 2 + overhang;
  const rise = half * pitch;
  const c = Math.cos(ry);
  const s = Math.sin(ry);
  const at = (lx, lz) => ({ x: x + lx * c + lz * s, z: z - lx * s + lz * c });
  // Slopes, rotated about their long axis.
  const len = Math.hypot(half, rise);
  const ang = Math.atan2(rise, half);
  for (const side of [-1, 1]) {
    const p = at((side * half) / 2, 0);
    const g = box(len, 0.2, d + overhang * 2, 3);
    batch.add(g, M.roof, { x: p.x, y: y + rise / 2 + 0.12, z: p.z, ry, rz: side * -ang });
  }
  // Pediments (triangular gables) at both ends.
  const tri = new THREE.Shape();
  tri.moveTo(-half + 0.2, 0);
  tri.lineTo(half - 0.2, 0);
  tri.lineTo(0, rise - 0.05);
  tri.lineTo(-half + 0.2, 0);
  const tg = new THREE.ExtrudeGeometry(tri, { depth: 0.5, bevelEnabled: false });
  const tuv = tg.attributes.uv;
  for (let i = 0; i < tuv.count; i++) tuv.setXY(i, tuv.getX(i) / 4, tuv.getY(i) / 4);
  for (const end of [-1, 1]) {
    const p = at(0, end * (d / 2 + overhang - 0.3));
    batch.add(tg, M.stone, { x: p.x, y, z: p.z - 0.25 * c, ry });
  }
  // Cornice band.
  batch.add(box(w + overhang * 2, 0.35, d + overhang * 2, 2), M.stone, { x, y: y - 0.1, z, ry });
}

function colonnade(batch, mat, col, from, to, count) {
  for (let i = 0; i < count; i++) {
    const t = count === 1 ? 0.5 : i / (count - 1);
    batch.add(col, mat, { x: from[0] + (to[0] - from[0]) * t, z: from[1] + (to[1] - from[1]) * t });
  }
}

// ---------------------------------------------------------------------------

export const LAYOUT = {
  forum: { x: 0, z: 0, w: 30, d: 24 },
  legion: { x: -29, z: 0, w: 24, d: 20 },
  censors: { x: 25.5, z: 0 },
  curia: { x: 0, z: -24 },
  tabularium: { x: 27, z: -35 },
  board: { x: 0, z: -8.4 },
  spawn: { x: 0, z: 14 },
  // The battlefield beyond the west gate, where Cohorts fight their Orders.
  field: { x: -72, z: 0, fortX: -90 },
};

// The field outside the west gate is levelled ground; everything that
// stands on it reads its height from here.
export function fieldHeight(x, z) {
  void x;
  void z;
  return 0.02;
}
// How much the rolling meadow is flattened at a point: 1 on the battlefield
// and the road out to it, easing to 0 around them.
function levelled(x, z) {
  const inField = Math.max(0, 1 - Math.max(0, Math.abs(z) - 22) / 10) * Math.max(0, 1 - Math.max(0, -100 - x) / 12);
  return x < -44 ? inField : 1;
}

export function buildGround(scene, M) {
  const b = new Batch();
  // Meadow.
  // Meadow: gently rolling beyond the campus, with patches of darker and
  // drier grass so the ground never reads as one flat colour.
  const meadow = new THREE.PlaneGeometry(320, 320, 96, 96);
  meadow.rotateX(-Math.PI / 2);
  const uv = meadow.attributes.uv;
  for (let i = 0; i < uv.count; i++) uv.setXY(i, uv.getX(i) * 40, uv.getY(i) * 40);
  const mp = meadow.attributes.position;
  const colors = new Float32Array(mp.count * 3);
  for (let i = 0; i < mp.count; i++) {
    const x = mp.getX(i);
    const z = mp.getZ(i);
    const out = Math.max(0, Math.max(Math.abs(x + 3) - 50, z > 0 ? z - 36 : -z - 52));
    mp.setY(i, (1 - levelled(x, z)) * Math.min(1, out / 30) * (Math.sin(x * 0.05) * 2.2 + Math.cos(z * 0.043) * 1.8 + 1.5));
    const n = 0.5 + 0.25 * Math.sin(x * 0.13 + Math.cos(z * 0.09) * 2) + 0.25 * Math.sin(z * 0.21 - x * 0.07);
    const dry = 0.82 + n * 0.3;
    colors.set([dry * 1.02, dry, dry * 0.9], i * 3);
  }
  meadow.setAttribute('color', new THREE.BufferAttribute(colors, 3));
  meadow.computeVertexNormals();
  M.grass.vertexColors = true;
  const ground = new THREE.Mesh(meadow, M.grass);
  ground.position.y = -0.02;
  ground.receiveShadow = true;
  scene.add(ground);

  // A low boundary wall around the campus, with openings on the approaches.
  const wall = new Batch();
  const W = { x0: -46, x1: 40, z0: -50, z1: 34 };
  const run = (x0, z0, x1, z1) => {
    const len = Math.hypot(x1 - x0, z1 - z0);
    wall.add(box(len, 1.0, 0.6, 3), M.ashlar, { x: (x0 + x1) / 2, y: 0.5, z: (z0 + z1) / 2, ry: -Math.atan2(z1 - z0, x1 - x0) });
    wall.add(box(len + 0.2, 0.14, 0.75, 2), M.stone, { x: (x0 + x1) / 2, y: 1.07, z: (z0 + z1) / 2, ry: -Math.atan2(z1 - z0, x1 - x0) });
  };
  run(W.x0, W.z1, -3.5, W.z1);
  run(3.5, W.z1, W.x1, W.z1);
  run(W.x0, W.z0, W.x1, W.z0);
  // West side: the gate the legions march out of.
  run(W.x0, W.z0, W.x0, -3.2);
  run(W.x0, 3.2, W.x0, W.z1);
  run(W.x1, W.z0, W.x1, W.z1);
  wall.flush(scene, { name: 'boundary' });

  // Scattered rocks and shrubs outside the paved areas.
  const rand = rng(404);
  const rock = new THREE.IcosahedronGeometry(1, 1);
  const rocks = new THREE.InstancedMesh(rock, M.stone, 70);
  const shrubs = new THREE.InstancedMesh(new THREE.IcosahedronGeometry(1, 2), M.hedge, 90);
  const mx = new THREE.Matrix4();
  let r = 0;
  let sh = 0;
  const clear = (x, z) => (Math.abs(x) < 17 && Math.abs(z) < 14) || (x < -15 && x > -43 && Math.abs(z) < 12) || (x > 16 && x < 34 && Math.abs(z) < 9) || (Math.abs(x) < 9 && z < -14 && z > -32) || (x > 12 && x < 42 && z < -26 && z > -42) || (Math.abs(x) < 4 && z > 10) || (x < -44 && Math.abs(z) < 24);
  while (r < 70 || sh < 90) {
    const x = -60 + rand() * 115;
    const z = -60 + rand() * 105;
    if (clear(x, z)) continue;
    const q = new THREE.Quaternion().setFromEuler(new THREE.Euler(rand(), rand() * 6, rand()));
    if (r < 70 && rand() < 0.45) {
      const s = 0.2 + rand() * 0.55;
      mx.compose(new THREE.Vector3(x, s * 0.25, z), q, new THREE.Vector3(s * 1.3, s * 0.7, s));
      rocks.setMatrixAt(r++, mx);
    } else if (sh < 90) {
      const s = 0.35 + rand() * 0.6;
      mx.compose(new THREE.Vector3(x, s * 0.45, z), new THREE.Quaternion(), new THREE.Vector3(s * 1.3, s, s * 1.2));
      shrubs.setMatrixAt(sh++, mx);
    }
  }
  rocks.castShadow = shrubs.castShadow = true;
  rocks.receiveShadow = shrubs.receiveShadow = true;
  scene.add(rocks, shrubs);

  // Forum platform with steps on all sides.
  const F = LAYOUT.forum;
  const pb = new Batch();
  const top = box(F.w, 0.36, F.d, 3.4);
  pb.add(top, M.paving, { y: 0.18 });
  pb.add(box(F.w + 1.4, 0.18, F.d + 1.4, 2), M.step, { y: 0.09 });
  // Paths to each institution.
  const path = (x, z, w, d) => pb.add(box(w, 0.12, d, 3.4), M.paving, { x, y: 0.06, z });
  path(-16, 0, 3, 5); // to Legion Hall
  path(18, 0, 6.5, 5); // to Censors
  path(0, -14.5, 6, 6); // to Curia
  path(0, 16, 5, 10); // south approach
  path(13, -19, 3, 20); // toward the Tabularium
  pb.flush(scene, { name: 'forum-floor' });
}

// The Aquila mosaic disc set into the Forum floor.
export function buildMosaic(scene, texture) {
  const g = new THREE.CircleGeometry(3.4, 64);
  g.rotateX(-Math.PI / 2);
  const mat = new THREE.MeshStandardMaterial({ map: texture, roughness: 0.55, metalness: 0.15 });
  const mesh = new THREE.Mesh(g, mat);
  mesh.position.set(0, 0.365, 0);
  mesh.receiveShadow = true;
  scene.add(mesh);
  const ring = new THREE.Mesh(new THREE.RingGeometry(3.4, 3.7, 64).rotateX(-Math.PI / 2), new THREE.MeshStandardMaterial({ color: '#8f6a35', metalness: 0.8, roughness: 0.35 }));
  ring.position.y = 0.366;
  ring.receiveShadow = true;
  scene.add(ring);
}

// Legion Hall: a workshop yard. A roofed portico with solid walls runs along
// the north and west sides; the south and east sides, facing the Forum, are
// an open colonnade on a low wall, so the Cohorts at work can be seen from
// outside and from above.
export function buildLegionHall(scene, M) {
  const L = LAYOUT.legion;
  const b = new Batch();
  const col = columnGeometry(3.6, 0.24);
  const hw = L.w / 2;
  const hd = L.d / 2;
  const wallH = 5.0;
  const walk = 3.2;
  const y0 = 0.3;
  // Floor.
  b.add(box(L.w, 0.3, L.d, 3.4), M.paving, { x: L.x, y: 0.15, z: L.z });
  b.add(box(L.w + 1, 0.15, L.d + 1, 2), M.step, { x: L.x, y: 0.075, z: L.z });
  // Solid walls, north and west; the west wall has the back gate the
  // Cohorts march out of, toward the battlefield.
  b.add(box(L.w, wallH, 0.6, 4), M.ashlar, { x: L.x, y: wallH / 2, z: L.z - hd });
  const backGate = 4.2;
  const backSide = (L.d - backGate) / 2;
  for (const sgn of [-1, 1]) b.add(box(0.6, wallH, backSide, 4), M.ashlar, { x: L.x - hw, y: wallH / 2, z: L.z + sgn * (backGate / 2 + backSide / 2) });
  b.add(box(0.9, 1.2, backGate + 0.8, 2), M.stone, { x: L.x - hw, y: wallH - 0.6, z: L.z });
  b.add(box(L.w + 0.4, 0.4, 0.8, 2), M.stone, { x: L.x, y: wallH + 0.2, z: L.z - hd });
  b.add(box(0.8, 0.4, L.d + 0.4, 2), M.stone, { x: L.x - hw, y: wallH + 0.2, z: L.z });
  // Low walls with colonnades, south and east; the east side has the gate.
  const lowH = 1.0;
  b.add(box(L.w, lowH, 0.5, 3), M.ashlar, { x: L.x, y: lowH / 2 + y0, z: L.z + hd });
  const gate = 4.2;
  const side = (L.d - gate) / 2;
  b.add(box(0.5, lowH, side, 3), M.ashlar, { x: L.x + hw, y: lowH / 2 + y0, z: L.z - (gate / 2 + side / 2) });
  b.add(box(0.5, lowH, side, 3), M.ashlar, { x: L.x + hw, y: lowH / 2 + y0, z: L.z + (gate / 2 + side / 2) });
  for (let i = 0; i <= 8; i++) b.add(col, M.column, { x: L.x - hw + 0.3 + ((L.w - 0.6) * i) / 8, y: y0 + lowH, z: L.z + hd });
  for (let i = 0; i <= 6; i++) {
    const z = L.z - hd + 0.3 + ((L.d - 0.6) * i) / 6;
    if (Math.abs(z - L.z) < gate / 2) continue;
    b.add(col, M.column, { x: L.x + hw, y: y0 + lowH, z });
  }
  // Gate posts and lintel.
  for (const dz of [-gate / 2, gate / 2]) b.add(box(0.8, wallH, 0.8, 2), M.ashlar, { x: L.x + hw, y: wallH / 2, z: L.z + dz });
  b.add(box(0.9, 0.9, gate + 0.8, 2), M.stone, { x: L.x + hw, y: wallH - 0.45, z: L.z });
  // Timber beams over the open colonnades (a pergola: shade without a roof).
  const beamY = y0 + lowH + 3.6 + 0.1;
  b.add(box(L.w, 0.3, 0.35, 2), M.darkWood, { x: L.x, y: beamY, z: L.z + hd });
  b.add(box(0.35, 0.3, L.d, 2), M.darkWood, { x: L.x + hw, y: beamY, z: L.z });
  // Inner colonnade of the roofed portico (north and west).
  const ix = hw - walk;
  const iz = hd - walk;
  for (let i = 0; i <= 5; i++) b.add(col, M.column, { x: L.x - ix + ((hw + ix) * i) / 5 - 0.0, y: y0 + lowH * 0 + 0.4, z: L.z - iz, sy: 1.1 });
  for (let i = 1; i <= 4; i++) b.add(col, M.column, { x: L.x - ix, y: y0 + 0.4, z: L.z - iz + ((hd + iz) * i) / 4.6, sy: 1.1 });
  const archY = y0 + 0.4 + 3.96 + 0.2;
  b.add(box(hw + ix + 0.6, 0.45, 0.6, 2), M.stone, { x: L.x + (hw - ix) / 2 - 0.3 + 0.3, y: archY, z: L.z - iz });
  b.add(box(0.6, 0.45, hd + iz + 0.3, 2), M.stone, { x: L.x - ix, y: archY, z: L.z + (hd - iz) / 2 });
  b.flush(scene, { name: 'legion-hall' });

  // Lean-to roofs over the north and west walks.
  const r = new Batch();
  const low = archY + 0.2;
  const high = wallH + 0.35;
  const rise = high - low;
  const tilt = Math.atan2(rise, walk);
  const run = Math.hypot(walk, rise) + 0.3;
  r.add(box(L.w + 0.4, 0.16, run, 3), M.roof, { x: L.x + 0.2, y: (low + high) / 2 + 0.1, z: L.z - hd + walk / 2, rx: tilt });
  r.add(box(run, 0.16, L.d - walk + 0.2, 3), M.roof, { x: L.x - hw + walk / 2, y: (low + high) / 2 + 0.1, z: L.z + walk / 2 + 0.1, rz: -tilt });
  const roofs = r.flush(scene, { name: 'legion-roof' });

  return { roofs, entrance: new THREE.Vector3(L.x + hw, 0, L.z), courtyard: { x: L.x + 1.2, z: L.z + 1.2, hw: ix, hd: iz } };
}

// The Censors' exedra: a half-round chamber of dark stone opening toward the
// Forum, screened by two columns.
export function buildCensors(scene, M) {
  const C = LAYOUT.censors;
  const b = new Batch();
  const radius = 6.6;
  const wallH = 5.6;
  // Half-cylinder wall, opening to -x.
  const wall = new THREE.CylinderGeometry(radius + 0.6, radius + 0.6, wallH, 40, 1, true, 0, Math.PI);
  const inner = new THREE.CylinderGeometry(radius, radius, wallH, 40, 1, true, 0, Math.PI);
  inner.scale(1, 1, -1);
  // Mirroring flips the winding so the faces look inward, but the normals
  // still point outward; turn them to face the room or no light reaches it.
  const nn = inner.attributes.normal;
  for (let i = 0; i < nn.count; i++) nn.setXYZ(i, -nn.getX(i), -nn.getY(i), -nn.getZ(i));
  const uvs = (g, s) => {
    const uv = g.attributes.uv;
    for (let i = 0; i < uv.count; i++) uv.setXY(i, uv.getX(i) * s, uv.getY(i) * (wallH / 4));
  };
  uvs(wall, 8);
  uvs(inner, 8);
  b.add(wall, M.darkAshlar, { x: C.x, y: wallH / 2 + 0.3, z: C.z });
  b.add(inner, M.darkAshlar, { x: C.x, y: wallH / 2 + 0.3, z: C.z, ry: 0 });
  // Top cap of the wall.
  const cap = new THREE.RingGeometry(radius - 0.05, radius + 0.75, 40, 1, -Math.PI / 2, Math.PI);
  cap.rotateX(-Math.PI / 2);
  b.add(cap, M.stone, { x: C.x, y: wallH + 0.31, z: C.z, rx: 0, sz: 1 });
  // Floor: dark marble half-disc on a podium.
  const floor = new THREE.CylinderGeometry(radius + 0.7, radius + 0.7, 0.45, 40, 1, false, Math.PI, Math.PI);
  b.add(floor, M.darkMarble, { x: C.x, y: 0.225, z: C.z, ry: Math.PI });
  b.add(box(1.4, 0.45, (radius + 0.7) * 2, 2), M.darkMarble, { x: C.x - 0.7, y: 0.225, z: C.z });
  steps(b, M.step, { x: C.x - 2.2, z: C.z, w: 7, d: 1.2, count: 2, rise: 0.22, ry: -Math.PI / 2 });
  // Front screen: two columns and an architrave.
  const col = columnGeometry(4.8, 0.3);
  for (const dz of [-3.4, 3.4]) b.add(col, M.columnMarble, { x: C.x - 1.1, y: 0.45, z: C.z + dz });
  b.add(box(0.8, 0.6, radius * 2 + 1.4, 2), M.marble, { x: C.x - 1.1, y: 0.45 + 4.8 + 0.3, z: C.z });
  b.flush(scene, { name: 'censors' });
  // Opus sectile: an inlaid marble floor, ivory and dark stone with a
  // seal-red ring, centred under the Censor's table.
  const inlay = document.createElement('canvas');
  inlay.width = inlay.height = 1024;
  const g2 = inlay.getContext('2d');
  g2.fillStyle = '#2b2622';
  g2.fillRect(0, 0, 1024, 1024);
  const cx = 512;
  for (let ring = 0; ring < 7; ring++) {
    const r0 = 60 + ring * 66;
    const n = 12 + ring * 6;
    for (let k = 0; k < n; k++) {
      const a0 = (k / n) * Math.PI * 2;
      const a1 = ((k + 1) / n) * Math.PI * 2;
      g2.beginPath();
      g2.arc(cx, cx, r0 + 60, a0, a1);
      g2.arc(cx, cx, r0, a1, a0, true);
      g2.closePath();
      g2.fillStyle = (k + ring) % 2 ? '#d9cdb4' : ring === 3 ? '#7e2019' : '#4a4038';
      g2.fill();
      g2.strokeStyle = '#1a1612';
      g2.lineWidth = 3;
      g2.stroke();
    }
  }
  const tex = new THREE.CanvasTexture(inlay);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.anisotropy = 8;
  const disc = new THREE.Mesh(
    new THREE.CircleGeometry(radius + 0.2, 64, -Math.PI / 2, Math.PI).rotateX(-Math.PI / 2),
    new THREE.MeshStandardMaterial({ map: tex, roughness: 0.35, metalness: 0.05 }),
  );
  disc.geometry.attributes.uv.array.forEach((_, i, arr) => {
    // CircleGeometry UVs span the full disc; a half-disc keeps the centre.
  });
  disc.position.set(C.x, 0.452, C.z);
  disc.receiveShadow = true;
  scene.add(disc);
  return { seat: new THREE.Vector3(C.x + 3.6, 0.45, C.z), front: new THREE.Vector3(C.x - 2.5, 0.45, C.z) };
}

// The Curia: a closed, dignified building. It stays shut until a decision
// is pending (a later milestone opens it).
export function buildCuria(scene, M) {
  const C = LAYOUT.curia;
  const b = new Batch();
  const w = 15;
  const d = 12;
  const podium = 1.4;
  b.add(box(w, podium, d, 3), M.ashlar, { x: C.x, y: podium / 2, z: C.z });
  steps(b, M.step, { x: C.x, z: C.z + d / 2 + 1.2, w: 9, d: 2.4, count: 5, rise: podium / 5, ry: 0 });
  // Cella.
  const cw = w - 2;
  const cd = d - 4;
  const ch = 7.2;
  b.add(box(cw, ch, cd, 4), M.ashlar, { x: C.x, y: podium + ch / 2, z: C.z - 1.6 });
  // Portico columns.
  const col = columnGeometry(ch - 0.3, 0.34);
  for (let i = 0; i < 4; i++) b.add(col, M.columnMarble, { x: C.x - 4.5 + i * 3, y: podium, z: C.z + d / 2 - 1.2 });
  gableRoof(b, M, { x: C.x, z: C.z - 0.4, w: cw, d: d - 1.2, y: podium + ch + 0.2, pitch: 0.3, overhang: 0.6 });
  b.flush(scene, { name: 'curia' });
  // Bronze doors, closed.
  const doors = new THREE.Group();
  for (const dx of [-0.8, 0.8]) {
    const leaf = new THREE.Mesh(box(1.55, 4.4, 0.18, 2), M.bronze);
    leaf.position.set(dx, 2.2, 0);
    leaf.castShadow = true;
    doors.add(leaf);
    for (let k = 0; k < 3; k++) {
      const stud = new THREE.Mesh(box(1.2, 0.08, 0.06, 1), M.gold);
      stud.position.set(dx, 0.9 + k * 1.3, 0.1);
      doors.add(stud);
    }
  }
  doors.position.set(C.x, podium, C.z - 1.6 + cd / 2 + 0.1);
  scene.add(doors);
  return { doors, front: new THREE.Vector3(C.x, 0, C.z + d / 2 + 2.4) };
}

// The Tabularium: an arcaded archive standing quiet in the distance.
export function buildTabularium(scene, M) {
  const T = LAYOUT.tabularium;
  const b = new Batch();
  const w = 26;
  const baseH = 5;
  b.add(box(w, baseH, 9, 4), M.ashlar, { x: T.x, y: baseH / 2, z: T.z });
  const arcade = archWall(w, 5, 0.8, 2.6, 4.2);
  // Arcade storey: a wall with a row of arches, built as panels.
  const bays = 7;
  const bay = w / bays;
  for (let i = 0; i < bays; i++) {
    const g = archWall(bay, 5, 0.9, bay * 0.62, 4.1);
    b.add(g, M.ashlar, { x: T.x - w / 2 + bay * (i + 0.5), y: baseH, z: T.z + 4.1 });
    b.add(columnGeometry(4.6, 0.22), M.stone, { x: T.x - w / 2 + bay * i, y: baseH, z: T.z + 4.7 });
  }
  arcade.dispose();
  b.add(box(w, 5, 8, 4), M.ashlar, { x: T.x, y: baseH + 2.5, z: T.z - 0.6 });
  b.add(box(w + 0.6, 0.5, 9.4, 2), M.stone, { x: T.x, y: baseH + 5.25, z: T.z });
  gableRoof(b, M, { x: T.x, z: T.z, w: 9, d: w - 1, y: baseH + 5.5, pitch: 0.28, overhang: 0.5, ry: Math.PI / 2 });
  b.flush(scene, { name: 'tabularium' });
}

// Vegetation: stone pines and cypresses frame the campus; laurel planters in
// the Forum. Instanced; none of it moves except in the wind.
export function buildVegetation(scene, M) {
  const rand = rng(99);
  const cypress = new THREE.LatheGeometry(
    [0, 0.2, 0.55, 0.62, 0.55, 0.42, 0.25, 0].map((r, i) => new THREE.Vector2(r, i * 1.05)),
    9,
  );
  const cypressTrunk = new THREE.CylinderGeometry(0.1, 0.14, 0.6, 6);
  // Umbrella-pine canopy: a lumpy, flattened cloud, faceted on purpose.
  const pineCanopy = new THREE.IcosahedronGeometry(1, 2);
  {
    const pos = pineCanopy.attributes.position;
    const v = new THREE.Vector3();
    for (let i = 0; i < pos.count; i++) {
      v.fromBufferAttribute(pos, i);
      const n = 1 + Math.sin(v.x * 5.1 + v.z * 3.7) * 0.12 + Math.sin(v.y * 7.3 + v.x * 2.1) * 0.08;
      v.multiplyScalar(n);
      if (v.y < 0) v.y *= 0.35;
      pos.setXYZ(i, v.x, v.y, v.z);
    }
    pineCanopy.scale(2.6, 0.75, 2.6);
    pineCanopy.computeVertexNormals();
  }
  const pineTrunk = new THREE.CylinderGeometry(0.16, 0.26, 7, 7);

  const cypresses = [];
  const pines = [];
  // Rows of cypresses along the approaches.
  for (let i = 0; i < 6; i++) {
    cypresses.push([-4.2, 18 + i * 4.5]);
    cypresses.push([4.2, 18 + i * 4.5]);
  }
  for (const [x, z] of [
    [-12, -22],
    [12, -22],
    [-45, -14],
    [-45, 14],
    [-18, 14],
    [-18, -14],
    [18, 14],
    [18, -15],
    [40, -12],
    [40, 12],
    [44, -24],
  ]) {
    cypresses.push([x, z]);
  }
  // Stone pines scattered beyond the buildings.
  const spots = [
    [-22, 26], [22, 26], [-50, 28], [-60, 30], [-52, -26], [-28, -34], [8, -46],
    [52, 24], [58, -6], [48, -44], [-6, 44], [30, 40], [-36, 42], [62, 34], [-68, -30], [70, -30],
  ];
  for (const s of spots) pines.push(s);

  const mtx = new THREE.Matrix4();
  const q = new THREE.Quaternion();
  if (TREES) {
    const inst = (geo, mat, list, cast = true) => {
      const m = new THREE.InstancedMesh(geo, mat, list.length);
      list.forEach((x, i) => m.setMatrixAt(i, x));
      m.castShadow = cast;
      m.receiveShadow = true;
      scene.add(m);
    };
    const cyp = cypresses.map(([x, z]) => {
      const s = 0.85 + rand() * 0.35;
      return new THREE.Matrix4().compose(new THREE.Vector3(x, 0.05, z), q.clone().setFromEuler(new THREE.Euler(0, rand() * 6.28, 0)), new THREE.Vector3(s, s * (1 + rand() * 0.25), s));
    });
    inst(TREES.cypressFoliage, M.foliage, cyp);
    inst(TREES.cypressTrunk, M.trunk, cyp);
    const pin = pines.map(([x, z]) => {
      const s = 0.85 + rand() * 0.3;
      return new THREE.Matrix4().compose(new THREE.Vector3(x, 0, z), q.clone().setFromEuler(new THREE.Euler(0, rand() * 6.28, 0)), new THREE.Vector3(s, s, s));
    });
    inst(TREES.pineCanopy, M.pine, pin);
    inst(TREES.pineTrunk, M.trunk, pin);
    inst(TREES.pineBranches, M.trunk, pin);
  } else {
  const cm = new THREE.InstancedMesh(cypress, M.foliage, cypresses.length);
  const ctm = new THREE.InstancedMesh(cypressTrunk, M.trunk, cypresses.length);
  cypresses.forEach(([x, z], i) => {
    const s = 0.9 + rand() * 0.35;
    mtx.compose(new THREE.Vector3(x, 0.4, z), new THREE.Quaternion(), new THREE.Vector3(s, s * (1.15 + rand() * 0.3), s));
    cm.setMatrixAt(i, mtx);
    mtx.compose(new THREE.Vector3(x, 0.3, z), new THREE.Quaternion(), new THREE.Vector3(1, 1, 1));
    ctm.setMatrixAt(i, mtx);
  });
  cm.castShadow = ctm.castShadow = true;
  cm.receiveShadow = true;
  scene.add(cm, ctm);

  const pm = new THREE.InstancedMesh(pineCanopy, M.pine, pines.length * 5);
  const ptm = new THREE.InstancedMesh(pineTrunk, M.trunk, pines.length);
  pines.forEach(([x, z], i) => {
    const lean = (rand() - 0.5) * 0.25;
    q.setFromEuler(new THREE.Euler(lean, 0, (rand() - 0.5) * 0.25));
    mtx.compose(new THREE.Vector3(x, 3.5, z), q, new THREE.Vector3(1, 1, 1));
    ptm.setMatrixAt(i, mtx);
    for (let k = 0; k < 5; k++) {
      const s = 0.7 + rand() * 0.45;
      const a = (k / 5) * Math.PI * 2 + rand();
      const r = k === 0 ? 0 : 1.8 + rand() * 0.8;
      mtx.compose(
        new THREE.Vector3(x + Math.cos(a) * r + lean * 3, 7.3 + rand() * 0.7 + (k === 0 ? 0.5 : 0), z + Math.sin(a) * r),
        new THREE.Quaternion().setFromEuler(new THREE.Euler(0, rand() * 6, 0)),
        new THREE.Vector3(s, s * (0.8 + rand() * 0.4), s),
      );
      pm.setMatrixAt(i * 5 + k, mtx);
    }
  });
  pm.castShadow = ptm.castShadow = true;
  scene.add(pm, ptm);

  }

  // Laurel planters along the Forum edges.
  const planter = new Batch();
  const bush = new THREE.IcosahedronGeometry(0.7, 1);
  bush.scale(1, 0.8, 1);
  for (const [x, z] of [
    [-13.5, -10.2], [-13.5, 10.2], [13.5, -10.2], [13.5, 10.2], [-6.5, 10.6], [6.5, 10.6],
  ]) {
    planter.add(box(1.4, 0.7, 1.4, 1.5), M.stone, { x, y: 0.36 + 0.35, z });
    planter.add(bush, M.hedge, { x, y: 1.35, z });
  }
  planter.flush(scene, { name: 'planters' });
}

// Distant hills to close the horizon under the haze.
export function buildHills(scene, dusk = false) {
  const rand = rng(5);
  const g = new THREE.CylinderGeometry(190, 200, 30, 64, 4, true);
  const pos = g.attributes.position;
  for (let i = 0; i < pos.count; i++) {
    const y = pos.getY(i);
    if (y > 0) {
      const a = Math.atan2(pos.getZ(i), pos.getX(i));
      const h = 8 + Math.sin(a * 3) * 6 + Math.sin(a * 7 + 1) * 4 + rand() * 2;
      pos.setY(i, -15 + h + 6);
    } else {
      pos.setY(i, -2);
    }
  }
  g.computeVertexNormals();
  const mat = new THREE.MeshStandardMaterial({ color: dusk ? '#262b32' : '#8a8a62', roughness: 1, side: THREE.BackSide });
  const hills = new THREE.Mesh(g, mat);
  scene.add(hills);
}

export { colonnade, gableRoof, steps, roofSlab };

// Merges the static meshes under `group` into one mesh per material, in the
// group's local space. Meshes listed in `keep` (state handles the director
// toggles) and instanced meshes stay separate.
export function bake(group, keep = []) {
  const keepSet = new Set();
  for (const k of keep) k?.traverse?.((o) => keepSet.add(o));
  group.updateMatrixWorld(true);
  const inv = new THREE.Matrix4().copy(group.matrixWorld).invert();
  const byMat = new Map();
  const doomed = [];
  group.traverse((o) => {
    if (!o.isMesh || o.isInstancedMesh || keepSet.has(o) || Array.isArray(o.material)) return;
    const g = o.geometry.index ? o.geometry.toNonIndexed() : o.geometry.clone();
    g.applyMatrix4(new THREE.Matrix4().multiplyMatrices(inv, o.matrixWorld));
    for (const key of Object.keys(g.attributes)) if (!['position', 'normal', 'uv'].includes(key)) g.deleteAttribute(key);
    if (!g.attributes.uv) g.setAttribute('uv', new THREE.BufferAttribute(new Float32Array(g.attributes.position.count * 2), 2));
    const entry = byMat.get(o.material) || { list: [], cast: false, receive: false };
    entry.list.push(g);
    entry.cast ||= o.castShadow;
    entry.receive ||= o.receiveShadow;
    byMat.set(o.material, entry);
    doomed.push(o);
  });
  for (const o of doomed) o.parent.remove(o);
  for (const [material, { list, cast, receive }] of byMat) {
    const m = new THREE.Mesh(mergeGeometries(list, false), material);
    list.forEach(g => g.dispose());
    m.geometry.userData.baked = true;
    m.matrixAutoUpdate = false;
    m.updateMatrix();
    m.castShadow = cast;
    m.receiveShadow = receive;
    group.add(m);
  }
  return group;
}

// Where each Cohort takes the field: one long line facing the fort, so each
// Cohort's warband stands between it and the wall. Index order matches the
// Legion Hall's six desks.
export function battleSlots() {
  const x = LAYOUT.field.x - 3;
  return [-3.5, 3.5, -10.5, 10.5, -17, 17].map((z) => ({ pos: new THREE.Vector3(x, fieldHeight(x, z), z), yaw: -Math.PI / 2 }));
}

// A trampled earth patch that fades into the grass at its edge.
function dirtTexture() {
  const c = document.createElement('canvas');
  c.width = c.height = 512;
  const g = c.getContext('2d');
  const r = rng(77);
  const grad = g.createRadialGradient(256, 256, 80, 256, 256, 256);
  grad.addColorStop(0, 'rgba(122,98,68,0.95)');
  grad.addColorStop(0.7, 'rgba(118,96,66,0.6)');
  grad.addColorStop(1, 'rgba(118,96,66,0)');
  g.fillStyle = grad;
  g.fillRect(0, 0, 512, 512);
  for (let i = 0; i < 3600; i++) {
    const x = r() * 512;
    const y = r() * 512;
    const d = Math.hypot(x - 256, y - 256) / 256;
    if (d > 0.95) continue;
    g.fillStyle = `rgba(${70 + r() * 60},${58 + r() * 40},${40 + r() * 30},${0.25 * (1 - d)})`;
    g.fillRect(x, y, 2 + r() * 5, 1 + r() * 3);
  }
  // Parallel wagon ruts and broken foot tracks establish the siege ground.
  for (const y of [230, 247, 265, 282]) {
    g.strokeStyle = 'rgba(61,45,29,0.16)';
    g.lineWidth = 2;
    g.beginPath();
    g.moveTo(85, y); g.bezierCurveTo(180, y - 3, 320, y + 6, 426, y - 2); g.stroke();
  }
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  return t;
}

// The battlefield beyond the west gate: trampled ground, a road out from the
// gate, and the enemy's fort, an earthen rampart with a palisade and two
// wooden towers. What the Cohorts face while their Orders are at work.
export function buildBattlefield(scene, M, uniforms = { uTime: { value: 0 } }) {
  const F = LAYOUT.field;
  const dirt = new THREE.MeshStandardMaterial({ map: dirtTexture(), transparent: true, depthWrite: false, roughness: 1 });
  const patch = new THREE.Mesh(new THREE.PlaneGeometry(46, 44).rotateX(-Math.PI / 2), dirt);
  patch.position.set(F.x - 4, 0.012, F.z);
  patch.receiveShadow = true;
  scene.add(patch);
  const road = new THREE.Mesh(new THREE.PlaneGeometry(20, 5).rotateX(-Math.PI / 2), dirt);
  road.position.set(-52, 0.01, 0);
  road.scale.set(1, 1, 1);
  road.receiveShadow = true;
  scene.add(road);

  const b = new Batch();
  const earth = new THREE.MeshStandardMaterial({ color: '#6f5a40', roughness: 1 });
  const fx = F.fortX;
  // Rampart.
  b.add(box(3.4, 1.5, 38, 3), earth, { x: fx, y: 0.75, z: F.z });
  b.add(box(4.6, 0.5, 39, 3), earth, { x: fx, y: 0.25, z: F.z });
  // Palisade of sharpened stakes, with a gap for the gate.
  const stake = new THREE.CylinderGeometry(0.13, 0.15, 2.6, 6);
  const tip = new THREE.ConeGeometry(0.15, 0.4, 6);
  const r = rng(91);
  for (let z = -18.6; z <= 18.6; z += 0.36) {
    if (Math.abs(z) < 2.2) continue;
    const h = 2.4 + r() * 0.5;
    const lean = (r() - 0.5) * 0.06;
    b.add(stake, M.darkWood, { x: fx + 0.6, y: 1.5 + h / 2 - 0.1, z, sy: h / 2.6, rz: lean });
    b.add(tip, M.darkWood, { x: fx + 0.6, y: 1.5 + h + 0.1, z, rz: lean });
  }
  // Gate: two heavy leaves, shut.
  b.add(box(0.3, 3.0, 4.4, 2), M.darkWood, { x: fx + 0.7, y: 3.0, z: F.z });
  // Towers either side of the gate.
  for (const tz of [-4.2, 4.2]) {
    for (const [dx, dz] of [[-1, -1], [1, -1], [-1, 1], [1, 1]]) b.add(new THREE.CylinderGeometry(0.16, 0.18, 7, 6), M.darkWood, { x: fx + dx * 1.1, y: 3.5, z: tz + dz * 1.1 });
    b.add(box(2.8, 0.25, 2.8, 2), M.darkWood, { x: fx, y: 6.2, z: tz });
    for (const [dx, dz, w, d] of [[-1.35, 0, 0.12, 2.8], [1.35, 0, 0.12, 2.8], [0, -1.35, 2.8, 0.12], [0, 1.35, 2.8, 0.12]]) b.add(box(w, 0.9, d, 2), M.darkWood, { x: fx + dx, y: 6.75, z: tz + dz });
    b.add(new THREE.ConeGeometry(2.3, 1.4, 4), M.darkWood, { x: fx, y: 8.1, z: tz, ry: Math.PI / 4 });
    // Cross bracing and bronze gate straps read even in a wide battlefield view.
    for (const dz of [-1.08, 1.08]) {
      b.add(box(0.16, 4.8, 0.14), M.darkWood, { x: fx + 1.1, y: 3.7, z: tz + dz, rz: 0.42 });
    }
  }
  for (const y of [2, 3.1, 4]) b.add(box(0.36, 0.14, 4.4), M.bronze, { x: fx + 0.9, y, z: 0 });
  // Broken stakes and stones stay outside the marching corridor and ranks.
  const rock = new THREE.IcosahedronGeometry(0.22, 0);
  for (let i = 0; i < 34; i++) {
    const z = (i % 2 ? -1 : 1) * (20 + r() * 3);
    const x = fx + 2 + r() * 25;
    b.add(rock, earth, { x, y: 0.1, z, sy: 0.65, sx: 0.6 + r(), sz: 0.7 + r() });
    if (i % 5 === 0) b.add(stake, M.darkWood, { x: x + 0.5, y: 0.25, z, rx: 1.35, ry: r() * 3 });
  }
  b.flush(scene, { name: 'fort' });
  // The enemy's banners: dark cloth on the towers.
  for (const tz of [-4.2, 4.2]) {
    const pole = new THREE.Mesh(new THREE.CylinderGeometry(0.05, 0.05, 2.4, 6), M.darkWood);
    pole.position.set(fx, 9.6, tz);
    const cloth = new THREE.Mesh(new THREE.PlaneGeometry(1.5, 1.1, 6, 8), standardMaterial({ color: '#43333b' }, uniforms.uTime, tz));
    cloth.position.set(fx, 10.3, tz + 0.62);
    cloth.castShadow = false;
    scene.add(pole, cloth);
  }
}
