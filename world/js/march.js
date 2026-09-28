// The legions on the battlefield. Each Order with a live run has a Cohort:
// a standard-bearer and three legionaries in the Cohort's colour. When the
// Order starts between two snapshots, the Cohort musters in the Legion Hall
// and marches out of the back gates to its place facing the enemy fort;
// what it does there follows the Order (fighting while code is written,
// shields up while checks run, a red standard when the Order needs you).
// Each Cohort faces a warband that sallies from the fort when it arrives and
// fights back while the Order is at work. When the Order is delivered the
// warband flees into the fort, and the Cohort cheers and marches home. On first load a
// Cohort is simply at its place: nothing moves for work that did not happen.

import * as THREE from '../vendor/three.module.min.js';
import { MOTIONS, makeFigure } from './figures.js';

const PACE = 2.3; // marching, metres per second
const JOG = 4.2; // the double-quick outside the walls
const FADE = 0.8; // seconds to appear or vanish

// A soft round puff of dust, shared by every march.
const PUFF = new THREE.CircleGeometry(0.5, 16).rotateX(-Math.PI / 2);
let DUST = null;
function dustTexture() {
  if (DUST) return DUST;
  const c = document.createElement('canvas');
  c.width = c.height = 64;
  const g = c.getContext('2d');
  const grad = g.createRadialGradient(32, 32, 2, 32, 32, 32);
  grad.addColorStop(0, 'rgba(214,190,150,0.9)');
  grad.addColorStop(1, 'rgba(214,190,150,0)');
  g.fillStyle = grad;
  g.fillRect(0, 0, 64, 64);
  DUST = new THREE.CanvasTexture(c);
  return DUST;
}

// The standard the bearer carries: the Cohort's numeral on its colour,
// seal-red with "!" when the Order needs you.
function carriedStandard(numeral, tunic) {
  const g = new THREE.Group();
  const wood = new THREE.MeshStandardMaterial({ color: '#4a3322', roughness: 0.8 });
  const bronze = new THREE.MeshStandardMaterial({ color: '#b08440', metalness: 0.7, roughness: 0.35 });
  const staff = new THREE.Mesh(new THREE.CylinderGeometry(0.035, 0.035, 3.4, 6), wood);
  staff.position.y = 1.7;
  const bar = new THREE.Mesh(new THREE.CylinderGeometry(0.025, 0.025, 0.8, 6), bronze);
  bar.rotation.x = Math.PI / 2;
  bar.position.y = 3.25;
  const c = document.createElement('canvas');
  c.width = c.height = 128;
  const tex = new THREE.CanvasTexture(c);
  tex.colorSpace = THREE.SRGBColorSpace;
  const draw = (halt) => {
    const x = c.getContext('2d');
    // Crimson vexillum with a gold numeral and a border in the Cohort's
    // colour; seal-red with "!" when the Order needs you.
    x.fillStyle = halt ? '#c0281c' : '#7a1c16';
    x.fillRect(0, 0, 128, 128);
    x.strokeStyle = halt ? '#fff1dc' : tunic;
    x.lineWidth = 10;
    x.strokeRect(8, 8, 112, 112);
    x.fillStyle = halt ? '#fff1dc' : '#e8c26a';
    x.textAlign = 'center';
    x.font = '600 54px Georgia, serif';
    x.fillText(halt ? `${numeral}!` : numeral, 64, 84);
    tex.needsUpdate = true;
  };
  draw(false);
  const flag = new THREE.Mesh(new THREE.PlaneGeometry(0.95, 0.95), new THREE.MeshStandardMaterial({ map: tex, roughness: 0.9, side: THREE.DoubleSide }));
  // Broadside to the march, so it reads from the sides and from above.
  flag.rotation.y = Math.PI / 2;
  flag.position.y = 2.75;
  const tip = new THREE.Mesh(new THREE.ConeGeometry(0.05, 0.18, 6), bronze);
  tip.position.y = 3.5;
  g.add(staff, bar, flag, tip);
  g.traverse((o) => {
    if (o.isMesh) o.castShadow = true;
  });
  let halted = false;
  g.userData.flag = flag;
  g.userData.setHalt = (halt) => {
    if (halt === halted) return;
    halted = halt;
    draw(halt);
  };
  g.userData.dispose = () => {
    g.traverse((o) => {
      if (!o.isMesh) return;
      o.geometry.dispose();
      o.material.map?.dispose();
      o.material.dispose();
    });
  };
  return g;
}

// Every material under a figure, made fadeable once.
function fadeable(fig) {
  const mats = [];
  fig.traverse((o) => {
    if (!o.isMesh || !o.material) return;
    // clone() drops the Cohort recolouring hooks; carry them over.
    const src = o.material;
    o.material = src.clone();
    o.material.onBeforeCompile = src.onBeforeCompile;
    if (Object.hasOwn(src, 'customProgramCacheKey')) o.material.customProgramCacheKey = src.customProgramCacheKey;
    mats.push(o.material);
  });
  return mats;
}
function setOpacity(mats, a) {
  for (const m of mats) {
    const see = a < 0.999;
    if (m.transparent !== see) {
      m.transparent = see;
      m.needsUpdate = true;
    }
    m.opacity = a;
    m.depthWrite = !see;
  }
}

// A polyline walked at constant speed.
function route(points) {
  const segs = [];
  let total = 0;
  for (let i = 1; i < points.length; i++) {
    const len = points[i].distanceTo(points[i - 1]);
    segs.push({ a: points[i - 1], b: points[i], len, start: total });
    total += len;
  }
  return {
    total,
    at(s) {
      const d = Math.min(Math.max(s, 0), total);
      const seg = segs.find((x) => d <= x.start + x.len) || segs[segs.length - 1];
      const u = seg.len ? (d - seg.start) / seg.len : 1;
      const p = seg.a.clone().lerp(seg.b, u);
      const dir = seg.b.clone().sub(seg.a).setY(0).normalize();
      return { p, yaw: Math.atan2(dir.x, dir.z) };
    },
  };
}

// Formations, in the unit's own frame (+z is where it faces). On the march
// the Cohort is two abreast behind its standard, narrow enough for the
// gates; on the field it deploys into a rank of three with the standard
// behind.
const MARCH = [new THREE.Vector3(0, 0, 1.2), new THREE.Vector3(-0.55, 0, 0), new THREE.Vector3(0.55, 0, 0), new THREE.Vector3(0, 0, -1.1)];
// The warband, in the Cohort's frame: a loose rank a few paces ahead.
const BAND = [new THREE.Vector3(-1.3, 0, 3.4), new THREE.Vector3(0.1, 0, 3.1), new THREE.Vector3(1.4, 0, 3.5)];
const LINE = [new THREE.Vector3(0, 0, -0.8), new THREE.Vector3(-1.15, 0, 0.9), new THREE.Vector3(0, 0, 0.9), new THREE.Vector3(1.15, 0, 0.9)];

export class Cohort {
  // slot: { pos, yaw } on the field; path: muster in the Legion Hall, its
  // back gate, the campus gate, then the road; label: the CSS2D label object.
  constructor(scene, { slot, path, numeral, tunic, variant, label, arriving, fortGate }) {
    this.scene = scene;
    this.fortGate = fortGate;
    this.variant = variant;
    this.band = null;
    this.slot = slot;
    this.done = false;
    this.puffs = [];
    this.puffClock = 0;
    this.t = 0;
    this.mode = 'atease';
    this.halt = false;

    this.bearer = makeFigure('cohort', { tunic, variant });
    this.standard = carriedStandard(numeral, tunic);
    this.standard.position.set(0.32, 0, 0.18);
    this.bearer.add(this.standard);
    this.bearer.add(label);
    this.label = label;
    label.position.set(0, 3.7, 0);
    this.members = [this.bearer, ...[1, 2, 3].map((i) => makeFigure('cohort', { tunic, variant: variant + i, armed: true }))].map((fig) => {
      const mats = fadeable(fig);
      scene.add(fig);
      return { fig, mats };
    });
    // Each soldier on its own beat.
    this.members.forEach((m, i) => {
      if (m.fig.userData.rig) m.fig.userData.rig.phase = variant * 1.7 + i * 0.83;
    });

    this.out = route([...path, slot.pos]);
    this.home = route([slot.pos, ...[...path].reverse()]);
    // Metres from the muster to the campus gate (path[2]): inside the walls
    // the Cohort marches, outside it goes at the double.
    this.gateAt = path.slice(1, 3).reduce((d, p, i) => d + p.distanceTo(path[i]), 0);
    if (arriving) {
      this.phase = 'muster';
      this.s = 0;
      this.members.forEach((m) => setOpacity(m.mats, 0));
      this.place(this.out.at(0), MARCH);
    } else {
      this.phase = 'field';
      this.place({ p: slot.pos, yaw: slot.yaw }, LINE);
      this.raiseBand(false);
    }
  }

  // Where a warband member stands, in world space.
  bandSpot(i) {
    const rot = new THREE.Quaternion().setFromAxisAngle(new THREE.Vector3(0, 1, 0), this.slot.yaw);
    return this.slot.pos.clone().add(BAND[i].clone().applyQuaternion(rot));
  }

  // The warband: three barbarians. Sallying, they run out of the fort's gate
  // to their places; otherwise they are simply there.
  raiseBand(sally) {
    this.band = BAND.map((_, i) => {
      const fig = makeFigure('barbarian', { variant: this.variant * 3 + i });
      const mats = fadeable(fig);
      const spot = this.bandSpot(i);
      const from = this.fortGate.clone().add(new THREE.Vector3(0, 0, (i - 1) * 0.9));
      if (sally) {
        fig.position.copy(from);
        setOpacity(mats, 0);
      } else {
        fig.position.copy(spot);
        fig.rotation.y = this.slot.yaw + Math.PI;
      }
      if (fig.userData.rig) fig.userData.rig.phase = this.variant * 2.3 + i * 1.1 + 0.5;
      this.scene.add(fig);
      return { fig, mats, run: sally ? route([from, spot]) : null, s: 0, fade: sally ? 0 : 1 };
    });
  }

  // Each barbarian runs its own route; true while any is still running.
  runBand(dt, t, k, away) {
    let running = false;
    for (const b of this.band) {
      if (!b.run) continue;
      b.s += JOG * 0.9 * dt;
      const at = b.run.at(b.s);
      b.fig.position.copy(at.p);
      b.fig.rotation.y = at.yaw;
      b.fade = away ? Math.max(0, 1 - (b.s - b.run.total + 3) / 3) : Math.min(1, b.s / 2);
      setOpacity(b.mats, b.fade);
      MOTIONS.jog(b.fig.userData.rig, t, k, 1.1);
      if (b.s >= b.run.total) {
        b.run = null;
        b.fig.rotation.y = this.slot.yaw + Math.PI;
      } else running = true;
    }
    return running;
  }

  // Delivered: the warband turns and runs for the fort, fading at its gate.
  routBand() {
    for (const [i, b] of this.band.entries()) {
      b.run = route([b.fig.position.clone(), this.fortGate.clone().add(new THREE.Vector3(0, 0, (i - 1) * 0.9))]);
      b.s = 0;
      b.fleeing = true;
    }
  }

  disposeBand() {
    for (const b of this.band || []) {
      this.scene.remove(b.fig);
      b.mats.forEach((x) => x.dispose());
    }
    this.band = null;
  }

  // Where the Cohort is, for labels, interaction and couriers.
  get position() {
    return this.bearer.position;
  }

  // What the Order is doing now: a field motion, and whether it needs you.
  setOrder(o) {
    this.halt = o.state === 'blocked' || o.state === 'failed';
    this.standard.userData.setHalt(this.halt);
    if (this.halt) this.mode = 'halt';
    else if (o.state === 'in_review') this.mode = 'atease';
    else if (o.activity === 'coding') this.mode = 'fight';
    else if (o.activity === 'testing') this.mode = 'guard';
    else if (o.activity === 'reading' || o.activity === 'designing' || o.activity === 'deciding') this.mode = 'scout';
    else this.mode = 'stand';
  }

  // The Order left the field. Delivered: a cheer, then home. Otherwise
  // straight home; still on the way out: the Cohort simply fades.
  recall(delivered) {
    // A Cohort no longer on an Order carries no label.
    this.label.removeFromParent();
    this.label.element.remove();
    if (this.phase === 'muster' || this.phase === 'out') {
      this.phase = 'vanish';
      this.t = 0;
      return;
    }
    this.phase = delivered ? 'cheer' : 'turn';
    this.t = 0;
    if (!this.band) return;
    if (delivered) this.routBand();
    else this.bandVanish = 0;
  }

  place({ p, yaw }, offsets, blend = null) {
    const rot = new THREE.Quaternion().setFromAxisAngle(new THREE.Vector3(0, 1, 0), yaw);
    this.members.forEach((m, i) => {
      const off = blend ? MARCH[i].clone().lerp(LINE[i], blend) : offsets[i];
      m.fig.position.copy(p).add(off.clone().applyQuaternion(rot));
      m.fig.rotation.y = yaw;
    });
  }

  dust(pos) {
    const mat = new THREE.MeshBasicMaterial({ map: dustTexture(), transparent: true, depthWrite: false, opacity: 0.5 });
    const s = new THREE.Mesh(PUFF, mat);
    s.position.copy(pos).add(new THREE.Vector3((Math.random() - 0.5) * 0.3, 0.04, (Math.random() - 0.5) * 0.3));
    s.scale.setScalar(0.4);
    s.userData.life = 0;
    this.scene.add(s);
    this.puffs.push(s);
  }

  stepPuffs(dt) {
    this.puffs = this.puffs.filter((s) => {
      s.userData.life += dt;
      const u = s.userData.life / 0.9;
      s.scale.setScalar(0.4 + u * 1.1);
      s.material.opacity = 0.5 * (1 - u);
      if (u < 1) return true;
      this.scene.remove(s);
      s.material.dispose();
      return false;
    });
  }

  update(dt, t, k) {
    this.t += dt;
    this.stepPuffs(dt);
    this.updateBand(dt, t, k);
    let motion = null;
    let speed = 1;
    if (this.phase === 'muster') {
      // The Cohort forms up in the hall while the Consul gives the word.
      this.members.forEach((m) => setOpacity(m.mats, Math.min(1, this.t / FADE)));
      motion = 'stand';
      if (this.t > 1.6) {
        this.phase = 'out';
        this.s = 0;
      }
    } else if (this.phase === 'out') {
      const outside = this.s > this.gateAt;
      speed = outside ? JOG : PACE;
      this.s += speed * dt;
      const at = this.out.at(this.s);
      // Deploy from column into line over the last few metres.
      const left = this.out.total - this.s;
      const blend = left < 4 ? 1 - left / 4 : 0;
      if (blend > 0) at.yaw = at.yaw + (this.slot.yaw - at.yaw) * blend;
      this.place(at, MARCH, blend);
      motion = outside ? 'jog' : 'walk';
      if (this.s >= this.out.total) {
        this.phase = 'field';
        this.place({ p: this.slot.pos, yaw: this.slot.yaw }, LINE);
        // The enemy answers: a warband sallies from the fort.
        this.raiseBand(true);
      }
    } else if (this.phase === 'field') {
      return this.fieldMotion(t, k);
    } else if (this.phase === 'cheer') {
      motion = 'cheer';
      if (this.t > 2.4) {
        this.phase = 'turn';
        this.t = 0;
      }
    } else if (this.phase === 'turn') {
      this.phase = 'home';
      this.s = 0;
    } else if (this.phase === 'home') {
      const inside = this.s > this.home.total - this.gateAt;
      speed = inside ? PACE : JOG * 0.8;
      this.s += speed * dt;
      const at = this.home.at(this.s);
      const blend = Math.max(0, 1 - this.s / 3);
      this.place(at, MARCH, blend);
      motion = inside ? 'walk' : 'jog';
      const left = this.home.total - this.s;
      if (left < 4) this.members.forEach((m) => setOpacity(m.mats, Math.max(0, left / 4)));
      if (left <= 0) this.dispose();
    } else if (this.phase === 'vanish') {
      const a = Math.max(0, 1 - this.t / FADE);
      this.members.forEach((m) => setOpacity(m.mats, a));
      motion = 'stand';
      if (a <= 0) this.dispose();
    }
    if (this.done) return;
    const pace = motion === 'jog' ? speed / 3.6 : speed / 1.6;
    for (const m of this.members) {
      const rig = m.fig.userData.rig;
      if (m.fig === this.bearer && motion === 'cheer') MOTIONS.hail(rig, t, k);
      else MOTIONS[motion](rig, t, k, pace);
    }
    if (motion === 'walk' || motion === 'jog') {
      this.puffClock += dt;
      if (this.puffClock > (motion === 'jog' ? 0.1 : 0.18)) {
        this.puffClock = 0;
        this.dust(this.members[Math.floor(Math.random() * this.members.length)].fig.position);
      }
    }
  }

  updateBand(dt, t, k) {
    if (!this.band) return;
    if (this.bandVanish !== undefined) {
      // The Order left without victory: the warband simply withdraws.
      this.bandVanish += dt;
      const a = Math.max(0, 1 - this.bandVanish / FADE);
      this.band.forEach((b) => setOpacity(b.mats, a));
      if (a <= 0) this.disposeBand();
      return;
    }
    const fleeing = this.band.some((b) => b.fleeing);
    if (this.runBand(dt, t, k, fleeing)) {
      // Those already in place fight or wait while the rest arrive.
    } else if (fleeing) {
      this.disposeBand();
      return;
    }
    // In place: the band answers what the Cohort is doing.
    const act = { fight: 'fight', guard: 'fight', atease: 'atease' }[this.mode] || 'stand';
    for (const b of this.band) {
      if (b.run) continue;
      MOTIONS[this.phase === 'field' ? act : 'stand'](b.fig.userData.rig, t, k);
    }
  }

  // On the field: the rank does what the Order is doing.
  fieldMotion(t, k) {
    const [bearer, ...soldiers] = this.members;
    const brig = bearer.fig.userData.rig;
    this.standard.userData.flag.rotation.y = Math.PI / 2 + Math.sin(t * 1.3 + this.slot.pos.z) * 0.12;
    if (this.mode === 'halt') {
      // Waiting for you: the bearer turns to the campus with a raised hand.
      MOTIONS.hail(brig, t, k);
      bearer.fig.rotation.y = this.slot.yaw + Math.PI;
      soldiers.forEach((m) => MOTIONS.stand(m.fig.userData.rig, t, k));
      return;
    }
    bearer.fig.rotation.y = this.slot.yaw;
    if (this.mode === 'scout') MOTIONS.hail(brig, t, k);
    else MOTIONS.stand(brig, t, k);
    const act = { fight: 'fight', guard: 'guard', atease: 'atease', scout: 'stand', stand: 'stand' }[this.mode] || 'stand';
    soldiers.forEach((m) => MOTIONS[act](m.fig.userData.rig, t, k));
  }

  dispose() {
    if (this.done) return;
    this.done = true;
    this.disposeBand();
    for (const m of this.members) {
      this.scene.remove(m.fig);
      m.mats.forEach((x) => x.dispose());
    }
    this.standard.userData.dispose();
    for (const s of this.puffs) {
      this.scene.remove(s);
      s.material.dispose();
    }
    this.puffs = [];
  }
}

// The Consul's word: a gold ring spreading over the Forum's mosaic.
export class Ripple {
  constructor(scene, at) {
    this.scene = scene;
    this.t = 0;
    this.done = false;
    this.mesh = new THREE.Mesh(
      new THREE.RingGeometry(0.9, 1.05, 64).rotateX(-Math.PI / 2),
      new THREE.MeshBasicMaterial({ color: '#f3c46b', transparent: true, opacity: 0.9, depthWrite: false, blending: THREE.AdditiveBlending }),
    );
    this.mesh.position.copy(at).add(new THREE.Vector3(0, 0.03, 0));
    scene.add(this.mesh);
  }

  step(dt) {
    this.t += dt;
    const u = this.t / 1.8;
    this.mesh.scale.setScalar(1 + u * 7);
    this.mesh.material.opacity = 0.9 * (1 - u) ** 1.5;
    if (u >= 1) {
      this.done = true;
      this.scene.remove(this.mesh);
      this.mesh.geometry.dispose();
      this.mesh.material.dispose();
    }
  }
}
