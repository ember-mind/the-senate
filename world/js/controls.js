// Two ways of looking. Overview: an elevated diorama camera you orbit, pan
// and zoom; everything is reachable by clicking. Walk: a small third-person
// "You" moved with WASD, camera following, E to interact with what is near.

import * as THREE from '../vendor/three.module.min.js';
import { LAYOUT } from './architecture.js';
import { MOTIONS, makeFigure, stopWalking } from './figures.js';

// Simple static colliders: axis-aligned boxes and circles in the XZ plane.
function colliders() {
  const boxes = [];
  const circles = [];
  const B = (x, z, w, d) => boxes.push({ x0: x - w / 2, x1: x + w / 2, z0: z - d / 2, z1: z + d / 2 });
  const L = LAYOUT.legion;
  const hw = L.w / 2;
  const hd = L.d / 2;
  B(L.x, L.z - hd, L.w, 0.8);
  B(L.x, L.z + hd, L.w, 0.7);
  // West wall, open in the middle: the back gate to the battlefield.
  const back = (L.d - 4.2) / 2;
  B(L.x - hw, L.z - (2.1 + back / 2), 0.8, back);
  B(L.x - hw, L.z + (2.1 + back / 2), 0.8, back);
  const side = (L.d - 4.2) / 2;
  B(L.x + hw, L.z - (2.1 + side / 2), 0.7, side);
  B(L.x + hw, L.z + (2.1 + side / 2), 0.7, side);
  // Desks (rotated: long side along z) and shelves.
  const cx = L.x + 1.2;
  const cz = L.z + 1.2;
  for (const dx of [-3.4, 3.4]) for (const dz of [-4.2, 0, 4.2]) B(cx + dx - 0.1, cz + dz, 1.1, 2.4);
  B(L.x, L.z - hd + 0.8, L.w - 2, 0.8);
  B(L.x - hw + 0.8, L.z - 6.8, 0.8, 3);
  B(L.x - hw + 0.8, L.z + 5.4, 0.8, 3);
  // The enemy fort's rampart and palisade.
  B(LAYOUT.field.fortX, LAYOUT.field.z, 4.6, 39);
  // Curia podium, Tabularium.
  B(LAYOUT.curia.x, LAYOUT.curia.z, 15.2, 12.2);
  B(LAYOUT.tabularium.x, LAYOUT.tabularium.z, 26, 10);
  // Board, rack, banner columns.
  B(LAYOUT.board.x, LAYOUT.board.z, 6.4, 1.4);
  circles.push({ x: LAYOUT.board.x + 5.6, z: LAYOUT.board.z - 0.6, r: 0.6 });
  circles.push({ x: LAYOUT.board.x - 5.6, z: LAYOUT.board.z - 0.6, r: 0.6 });
  circles.push({ x: LAYOUT.board.x + 5.2, z: LAYOUT.board.z + 0.8, r: 1.1 });
  // Censors' table and exedra wall (approximated by circles along the arc).
  const C = LAYOUT.censors;
  B(C.x + 2.5, C.z, 1.4, 4);
  for (let i = 0; i <= 16; i++) {
    const a = (i / 16) * Math.PI;
    circles.push({ x: C.x + Math.sin(a) * 7, z: C.z + Math.cos(a) * 7, r: 0.8 });
  }
  circles.push({ x: C.x - 1.1, z: C.z - 3.4, r: 0.5 }, { x: C.x - 1.1, z: C.z + 3.4, r: 0.5 });
  // Planters.
  for (const [x, z] of [[-13.5, -10.2], [-13.5, 10.2], [13.5, -10.2], [13.5, 10.2], [-6.5, 10.6], [6.5, 10.6]]) B(x, z, 1.5, 1.5);
  return { boxes, circles };
}

function floorHeight(x, z) {
  const F = LAYOUT.forum;
  if (Math.abs(x - F.x) < F.w / 2 && Math.abs(z - F.z) < F.d / 2) return 0.36;
  const L = LAYOUT.legion;
  if (Math.abs(x - L.x) < L.w / 2 && Math.abs(z - L.z) < L.d / 2) return 0.3;
  const C = LAYOUT.censors;
  if (Math.hypot(x - C.x, z - C.z) < 7.3 && x > C.x - 1.8) return 0.45;
  return 0.06;
}

export class Controls {
  constructor(world, dom, { onModeChange } = {}) {
    this.w = world;
    this.dom = dom;
    this.camera = world.camera;
    this.mode = 'overview';
    this.onModeChange = onModeChange;
    this.keys = new Set();
    this.col = colliders();

    // Overview orbit state.
    // Wide enough to take in the battlefield west of the walls.
    this.target = new THREE.Vector3(-27, 0, -3);
    this.yaw = 0.55;
    this.pitch = 0.72;
    this.dist = 124;
    this.goal = { target: this.target.clone(), yaw: this.yaw, pitch: this.pitch, dist: this.dist };

    // The user's avatar: "You".
    this.you = makeFigure('you');
    this.you.position.set(LAYOUT.spawn.x, 0.06, LAYOUT.spawn.z);
    this.you.rotation.y = Math.PI;
    world.scene.add(this.you);
    const ring = new THREE.Mesh(
      new THREE.RingGeometry(0.42, 0.5, 32).rotateX(-Math.PI / 2),
      new THREE.MeshBasicMaterial({ color: '#d9ad5c', transparent: true, opacity: 0.8, depthWrite: false }),
    );
    ring.position.y = 0.03;
    this.you.add(ring);
    this.walkYaw = Math.PI; // camera yaw behind the avatar
    this.walkPitch = 0.32;
    this.velocity = new THREE.Vector3();

    this.bind();
  }

  bind() {
    const d = this.dom;
    let drag = null;
    d.addEventListener('pointerdown', (e) => {
      drag = { x: e.clientX, y: e.clientY, button: e.button, shift: e.shiftKey, moved: 0 };
      d.setPointerCapture(e.pointerId);
    });
    d.addEventListener('pointermove', (e) => {
      if (!drag) return;
      const dx = e.clientX - drag.x;
      const dy = e.clientY - drag.y;
      drag.x = e.clientX;
      drag.y = e.clientY;
      drag.moved += Math.abs(dx) + Math.abs(dy);
      if (this.mode === 'overview') {
        if (drag.button === 2 || drag.shift) {
          const s = this.goal.dist * 0.0016;
          const right = new THREE.Vector3(Math.cos(this.goal.yaw), 0, -Math.sin(this.goal.yaw));
          const fwd = new THREE.Vector3(Math.sin(this.goal.yaw), 0, Math.cos(this.goal.yaw));
          this.goal.target.addScaledVector(right, -dx * s).addScaledVector(fwd, -dy * s);
          this.clampTarget();
        } else {
          this.goal.yaw -= dx * 0.005;
          this.goal.pitch = THREE.MathUtils.clamp(this.goal.pitch + dy * 0.004, 0.25, 1.35);
        }
      } else {
        this.walkYaw -= dx * 0.005;
        this.walkPitch = THREE.MathUtils.clamp(this.walkPitch + dy * 0.003, 0.05, 0.9);
      }
    });
    d.addEventListener('pointerup', (e) => {
      if (drag && drag.moved < 5) this.onClick?.(e);
      drag = null;
    });
    d.addEventListener('contextmenu', (e) => e.preventDefault());
    d.addEventListener(
      'wheel',
      (e) => {
        e.preventDefault();
        if (this.mode === 'overview') {
          this.goal.dist = THREE.MathUtils.clamp(this.goal.dist * (1 + Math.sign(e.deltaY) * 0.1), 14, 130);
        }
      },
      { passive: false },
    );
    window.addEventListener('keydown', (e) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
      this.keys.add(e.code);
      if (e.code === 'Tab' || e.code === 'KeyV') {
        e.preventDefault();
        this.setMode(this.mode === 'walk' ? 'overview' : 'walk');
      }
    });
    window.addEventListener('keyup', (e) => this.keys.delete(e.code));
    window.addEventListener('blur', () => this.keys.clear());
  }

  held(...codes) {
    return codes.some((c) => this.keys.has(c)) ? 1 : 0;
  }

  clampTarget() {
    this.goal.target.x = THREE.MathUtils.clamp(this.goal.target.x, -86, 45);
    this.goal.target.z = THREE.MathUtils.clamp(this.goal.target.z, -42, 30);
  }

  setMode(mode) {
    if (mode === this.mode) return;
    this.mode = mode;
    if (mode === 'walk') {
      this.walkYaw = this.you.rotation.y + Math.PI;
    }
    this.onModeChange?.(mode);
  }

  // Frame a point from the overview (used by "go to" buttons).
  focus(pos, dist = 34) {
    if (this.mode !== 'overview') this.setMode('overview');
    this.goal.target.set(pos.x, 0, pos.z);
    this.goal.dist = dist;
  }

  update(dt, t) {
    const k = 1 - Math.exp(-dt * 5);
    if (this.mode === 'overview') {
      // WASD / arrows fly the camera over the campus; Q/E turn it. Your
      // character only moves in walk mode (the Walk button or Tab).
      const f = this.held('KeyW', 'ArrowUp') - this.held('KeyS', 'ArrowDown');
      const s = this.held('KeyD', 'ArrowRight') - this.held('KeyA', 'ArrowLeft');
      const turn = this.held('KeyE') - this.held('KeyQ');
      if (f || s) {
        const speed = this.goal.dist * (this.keys.has('ShiftLeft') || this.keys.has('ShiftRight') ? 0.8 : 0.35) * dt;
        const fwd = new THREE.Vector3(-Math.sin(this.goal.yaw), 0, -Math.cos(this.goal.yaw));
        const right = new THREE.Vector3(Math.cos(this.goal.yaw), 0, -Math.sin(this.goal.yaw));
        this.goal.target.addScaledVector(fwd, f * speed).addScaledVector(right, s * speed);
        this.clampTarget();
      }
      if (turn) this.goal.yaw -= turn * 1.4 * dt;
      this.yaw += (this.goal.yaw - this.yaw) * k;
      this.pitch += (this.goal.pitch - this.pitch) * k;
      this.dist += (this.goal.dist - this.dist) * k;
      this.target.lerp(this.goal.target, k);
      const off = new THREE.Vector3(Math.sin(this.yaw) * Math.cos(this.pitch), Math.sin(this.pitch), Math.cos(this.yaw) * Math.cos(this.pitch)).multiplyScalar(this.dist);
      const want = this.target.clone().add(off);
      this.camera.position.lerp(want, this.snap ? 1 : k * 1.5);
      this.camera.lookAt(this.target);
      this.snap = false;
      stopWalking(this.you.userData.rig);
      MOTIONS.stand(this.you.userData.rig, t, k);
      return;
    }
    // Walk.
    const f = (this.keys.has('KeyW') || this.keys.has('ArrowUp') ? 1 : 0) - (this.keys.has('KeyS') || this.keys.has('ArrowDown') ? 1 : 0);
    const s = (this.keys.has('KeyD') || this.keys.has('ArrowRight') ? 1 : 0) - (this.keys.has('KeyA') || this.keys.has('ArrowLeft') ? 1 : 0);
    const run = this.keys.has('ShiftLeft') || this.keys.has('ShiftRight');
    const fwd = new THREE.Vector3(-Math.sin(this.walkYaw), 0, -Math.cos(this.walkYaw));
    const right = new THREE.Vector3(-fwd.z, 0, fwd.x);
    const want = fwd.multiplyScalar(f).add(right.multiplyScalar(s));
    const moving = want.lengthSq() > 0;
    if (moving) want.normalize().multiplyScalar(run ? 7.5 : 4.2);
    this.velocity.lerp(want, 1 - Math.exp(-dt * 10));
    const p = this.you.position;
    const proposed = p.clone().addScaledVector(this.velocity, dt);
    const attempted = proposed.clone();
    this.collide(proposed);
    // Collision resolution can move a proposed point back across a wall.
    // Clear only velocity components that point into that correction; keeping
    // them alive made each frame re-enter the wall and visibly tremble.
    const correction = proposed.clone().sub(attempted);
    if (correction.x * this.velocity.x < -1e-5) this.velocity.x = 0;
    if (correction.z * this.velocity.z < -1e-5) this.velocity.z = 0;
    p.copy(proposed);
    p.y += (floorHeight(p.x, p.z) - p.y) * Math.min(1, dt * 12);
    const rig = this.you.userData.rig;
    if (this.velocity.lengthSq() > 0.2) {
      const yaw = Math.atan2(this.velocity.x, this.velocity.z);
      let dy = yaw - this.you.rotation.y;
      dy = Math.atan2(Math.sin(dy), Math.cos(dy));
      this.you.rotation.y += dy * Math.min(1, dt * 10);
      MOTIONS.walk(rig, t, k, run ? 1.35 : 1);
    } else {
      stopWalking(rig);
      MOTIONS.stand(rig, t, k);
    }
    // Camera: behind and above, easing, pulled in if a wall is in the way.
    const head = p.clone().add(new THREE.Vector3(0, 1.7, 0));
    const back = new THREE.Vector3(Math.sin(this.walkYaw) * Math.cos(this.walkPitch), Math.sin(this.walkPitch), Math.cos(this.walkYaw) * Math.cos(this.walkPitch));
    let dist = 6.2;
    for (let d = 1.2; d <= 6.2; d += 0.5) {
      const c = head.clone().addScaledVector(back, d);
      if (this.blocked(c.x, c.z) && c.y < 6) {
        dist = Math.max(1.2, d - 0.6);
        break;
      }
    }
    const camWant = head.clone().addScaledVector(back, dist);
    this.camera.position.lerp(camWant, this.snap ? 1 : 1 - Math.exp(-dt * 8));
    this.camera.lookAt(head.clone().add(new THREE.Vector3(0, 0.2, 0)));
    this.snap = false;
  }

  blocked(x, z) {
    for (const b of this.col.boxes) if (x > b.x0 && x < b.x1 && z > b.z0 && z < b.z1) return true;
    for (const c of this.col.circles) if (Math.hypot(x - c.x, z - c.z) < c.r) return true;
    return false;
  }

  collide(p) {
    const r = 0.35;
    for (const b of this.col.boxes) {
      const cx = THREE.MathUtils.clamp(p.x, b.x0, b.x1);
      const cz = THREE.MathUtils.clamp(p.z, b.z0, b.z1);
      const dx = p.x - cx;
      const dz = p.z - cz;
      const d2 = dx * dx + dz * dz;
      if (d2 < r * r) {
        if (d2 > 1e-6) {
          const d = Math.sqrt(d2);
          p.x = cx + (dx / d) * r;
          p.z = cz + (dz / d) * r;
        } else {
          // Inside: push out on the shortest axis.
          const opts = [
            [b.x0 - r - p.x, 0],
            [b.x1 + r - p.x, 0],
            [0, b.z0 - r - p.z],
            [0, b.z1 + r - p.z],
          ].sort((a, c) => Math.abs(a[0] + a[1]) - Math.abs(c[0] + c[1]));
          p.x += opts[0][0];
          p.z += opts[0][1];
        }
      }
    }
    for (const c of this.col.circles) {
      const dx = p.x - c.x;
      const dz = p.z - c.z;
      const d = Math.hypot(dx, dz);
      if (d < c.r + r && d > 1e-6) {
        p.x = c.x + (dx / d) * (c.r + r);
        p.z = c.z + (dz / d) * (c.r + r);
      }
    }
    p.x = THREE.MathUtils.clamp(p.x, -100, 60);
    p.z = THREE.MathUtils.clamp(p.z, -50, 45);
  }
}
