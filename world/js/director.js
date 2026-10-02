// The director turns a world-state projection into what the scene shows.
// It is the only place that decides what anything *means*: every figure it
// places, every lamp it lights, maps to a field of the projection. Motion
// happens only on a change of state (a Cohort arriving, a tablet carried to
// the Censor), and the first snapshot is placed without any motion, so the
// world never performs work that did not happen.

import * as THREE from '../vendor/three.module.min.js';
import { CSS2DObject } from '../vendor/three.module.min.js';
import { COHORT_TUNICS, MOTIONS, makeFigure } from './figures.js';
import { modelChip, modelOf } from './models.js';
import { LAYOUT } from './architecture.js';
import { STATE_WORD } from './props.js';
import { Cohort, Ripple } from './march.js';

const ROMAN = ['', 'I', 'II', 'III', 'IV', 'V', 'VI', 'VII', 'VIII', 'IX', 'X', 'XI', 'XII'];
export const numeral = (n) => ROMAN[n] || String(n);

// Where a desk's order state places its worker and what the desk shows.
const DESK_STATES = new Set(['working', 'in_review', 'blocked', 'failed']);

const ACTIVITY_WORD = {
  reading: 'Reading the repository',
  designing: 'Drafting the plan',
  coding: 'Writing code',
  testing: 'Running checks',
  reviewing: 'Handed to the Censor',
  deciding: 'Weighing the result',
};

export function since(iso, now = Date.now()) {
  if (!iso) return '';
  const m = Math.max(0, Math.round((now - Date.parse(iso)) / 60000));
  if (m < 60) return `${m}m`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

function label(className, onClick) {
  const el = document.createElement('div');
  el.className = `label ${className}`;
  el.addEventListener('pointerdown', (e) => e.stopPropagation());
  if (onClick) el.addEventListener('click', onClick);
  const obj = new CSS2DObject(el);
  // A person's bubble hangs by its tail from a point just above the head,
  // so it sits over the figure instead of covering it.
  if (className.startsWith('agent')) obj.center.set(0.5, 1);
  return { el, obj };
}

// A tablet gliding along an arc from one place to another: how the world
// shows that a piece of work changed hands.
// One tablet and one seal, shared by every trip, so repeated hand-overs
// allocate nothing on the GPU.
let COURIER_PARTS = null;
function courierParts() {
  COURIER_PARTS ??= {
    tabGeo: new THREE.BoxGeometry(0.36, 0.26, 0.05),
    tabMat: new THREE.MeshStandardMaterial({ color: '#f1e4c6', emissive: '#ffcf8a', emissiveIntensity: 0.35, roughness: 0.8 }),
    waxGeo: new THREE.CylinderGeometry(0.05, 0.05, 0.02, 10),
    waxMat: new THREE.MeshStandardMaterial({ color: '#a3261f', roughness: 0.4 }),
  };
  return COURIER_PARTS;
}

class Courier {
  constructor(scene, from, to, { onArrive, duration = 3.2 } = {}) {
    const g = new THREE.Group();
    const parts = courierParts();
    const tab = new THREE.Mesh(parts.tabGeo, parts.tabMat);
    const wax = new THREE.Mesh(parts.waxGeo, parts.waxMat);
    wax.rotation.x = Math.PI / 2;
    wax.position.z = 0.035;
    g.add(tab, wax);
    tab.castShadow = true;
    scene.add(g);
    this.g = g;
    this.from = from.clone();
    this.to = to.clone();
    this.t = 0;
    this.duration = duration;
    this.onArrive = onArrive;
    this.scene = scene;
    this.done = false;
  }

  step(dt) {
    this.t += dt / this.duration;
    const t = Math.min(this.t, 1);
    const e = t * t * (3 - 2 * t);
    const p = this.from.clone().lerp(this.to, e);
    p.y += Math.sin(Math.PI * e) * (2.5 + this.from.distanceTo(this.to) * 0.08);
    this.g.position.copy(p);
    this.g.rotation.y += dt * 1.5;
    if (t >= 1 && !this.done) {
      this.done = true;
      this.scene.remove(this.g);
      this.onArrive?.();
    }
  }
}

export class Director {
  constructor(world, ui) {
    this.w = world;
    this.ui = ui;
    this.state = null;
    this.slots = new Map(); // order id → desk index
    this.workers = new Map(); // order id → worker record
    this.couriers = [];
    this.cohorts = [];
    this.sending = 0; // seconds left of the Consul's sending gesture
    this.first = true;
    this.clock = 0;

    const { scene } = world;
    // Consul: stands before the campaign board.
    this.consul = makeFigure('consul');
    this.consulHome = new THREE.Vector3(LAYOUT.board.x - 2.6, 0.36, LAYOUT.board.z + 2.8);
    this.consul.position.copy(this.consulHome);
    this.consul.rotation.y = Math.PI * 0.85;
    scene.add(this.consul);
    const cl = label('agent consul', () => ui.openConsul());
    cl.obj.position.set(0, 2.05, 0);
    this.consul.add(cl.obj);
    this.consulLabel = cl.el;

    // Censor: seated at the long table in the exedra.
    this.censor = makeFigure('censor', { pose: 'sit' });
    this.censor.position.copy(world.censors.seat).add(new THREE.Vector3(-0.3, 0.02, 0));
    this.censor.rotation.y = -Math.PI / 2;
    scene.add(this.censor);
    const cs = label('agent censor', () => ui.openCensor());
    cs.obj.position.set(0, 1.55, 0);
    this.censor.add(cs.obj);
    this.censorLabel = cs.el;

    // Institution plaques: what each building is, in plain words.
    const places = [
      ['LEGION HALL', 'Implementation', new THREE.Vector3(LAYOUT.legion.x + 12.4, 5.8, LAYOUT.legion.z)],
      ['CENSORS', 'Independent review', new THREE.Vector3(LAYOUT.censors.x - 1.1, 6.4, LAYOUT.censors.z)],
      ['CURIA', 'Senate chamber · closed', new THREE.Vector3(LAYOUT.curia.x, 11.8, LAYOUT.curia.z + 5)],
      ['TABULARIUM', 'Campaign record', new THREE.Vector3(LAYOUT.tabularium.x, 13.5, LAYOUT.tabularium.z + 4)],
    ];
    this.legionSub = places[0][1];
    this.placeLabels = places.map(([name, sub, at]) => {
      const p = label('place');
      p.el.innerHTML = `<b>${name}</b><span>${sub}</span>`;
      p.obj.position.copy(at);
      scene.add(p.obj);
      return p;
    });

    // Board label (clickable, opens the campaign).
    const bl = label('agent board', () => ui.openCampaign());
    bl.obj.position.set(LAYOUT.board.x + 4.2, 6.6, LAYOUT.board.z + 0.4);
    scene.add(bl.obj);
    this.boardLabel = bl.el;
    this.boardLabel.innerHTML = '<b>CAMPAIGN</b>';
  }

  // Positions for targeting and interaction.
  anchors() {
    const list = [
      { kind: 'consul', pos: this.consul.position, radius: 2.4, prompt: 'Talk to the Consul' },
      { kind: 'censor', pos: this.censor.position, radius: 3.6, prompt: 'Inspect the review' },
      { kind: 'board', pos: new THREE.Vector3(LAYOUT.board.x, 0, LAYOUT.board.z + 1.2), radius: 3.2, prompt: 'View the campaign' },
    ];
    for (const [id, rec] of this.workers) {
      if (!rec.cohort.done) list.push({ kind: 'order', id, pos: rec.cohort.position, radius: 3.2, prompt: `Inspect Order · ${rec.title}` });
    }
    return list;
  }

  apply(state) {
    const prev = this.state;
    this.state = state;
    this.w.board.draw(state);
    this.applyConsul(state);
    this.applyOrders(state, prev);
    this.applyCensor(state, prev);
    this.applyRack(state, prev);
    this.first = false;
  }

  applyConsul(state) {
    const c = state.consul || { state: 'quiet' };
    const word = { quiet: 'Engineering lead', conferring: 'Conferring…', awaiting_you: 'Awaiting you' }[c.state] || '';
    this.consulLabel.innerHTML = `<b>CONSUL</b><span class="s">${word}</span>`;
    this.consulLabel.classList.toggle('needs', c.state === 'awaiting_you');
    this.consulMood = c.state;
  }

  deskFor(id) {
    if (this.slots.has(id)) return this.slots.get(id);
    const used = new Set(this.slots.values());
    // Fill desks diagonally so neighbouring Cohorts never share a sightline.
    for (const i of [0, 5, 3, 2, 4, 1]) {
      if (!used.has(i)) {
        this.slots.set(id, i);
        return i;
      }
    }
    return -1;
  }

  applyOrders(state, prev) {
    const live = new Set();
    let waiting = 0;
    for (const o of state.orders) {
      if (!DESK_STATES.has(o.state)) continue;
      live.add(o.id);
      const slot = this.deskFor(o.id);
      if (slot < 0) {
        waiting += 1;
        continue;
      }
      const desk = this.w.desks[slot];
      let rec = this.workers.get(o.id);
      if (!rec) {
        rec = this.spawnWorker(o, desk, slot);
        this.workers.set(o.id, rec);
      }
      rec.title = o.title;
      rec.order = o;
      this.dressDesk(desk, o, rec);
      const was = prev?.orders.find((x) => x.id === o.id);
      if (!this.first && was && was.state !== 'in_review' && o.state === 'in_review') {
        // Work handed over: a messenger's tablet flies from the field back
        // to the Censors.
        desk.orderTablet.visible = false;
        this.couriers.push(
          new Courier(this.w.scene, rec.cohort.position.clone().add(new THREE.Vector3(0, 1.6, 0)), this.w.censorTable.group.position.clone().add(new THREE.Vector3(0.1, 1.0, 0)), {
            onArrive: () => this.applyCensor(this.state, null),
            duration: 5,
          }),
        );
      }
    }
    // Six desks: any further live Orders wait for one, and the plaque says so.
    const sub = waiting ? `${this.legionSub} · ${waiting} waiting for a desk` : this.legionSub;
    this.placeLabels[0].el.innerHTML = `<b>LEGION HALL</b><span>${sub}</span>`;
    // Orders that left their desks: delivered, settled, cancelled or gone.
    for (const [id, rec] of this.workers) {
      if (live.has(id)) continue;
      this.releaseWorker(id, rec, state.orders.find((o) => o.id === id));
    }
  }

  spawnWorker(o, desk, slot) {
    const tunic = modelOf(o.worker)?.color || COHORT_TUNICS[(o.cohort - 1 + COHORT_TUNICS.length) % COHORT_TUNICS.length];
    const l = label('agent cohort', () => this.ui.openOrder(o.id));
    // Motion only for a real start: the legion is sent when its Order starts
    // working between two snapshots. On first load, or when the Order shows
    // up already past the work (in review), the Cohort is simply in place.
    const arriving = !this.first && o.state !== 'in_review';
    const muster = this.w.field.muster;
    const cohort = new Cohort(this.w.scene, {
      slot: this.w.field.slots[slot],
      path: [muster, new THREE.Vector3(-39.6, 0.3, 0), new THREE.Vector3(-46.8, 0.03, 0), new THREE.Vector3(-58, 0.03, 0)],
      numeral: numeral(o.cohort),
      tunic,
      variant: o.cohort,
      label: l.obj,
      arriving,
      fortGate: new THREE.Vector3(LAYOUT.field.fortX + 3, 0.02, 0),
      effects: this.w.battleEffects,
      time: this.w.uniforms.uTime,
      quality: this.w.quality,
      camera: this.w.camera,
      fullMotion: this.w.fullMotion,
    });
    this.cohorts.push(cohort);
    if (arriving) {
      // The Consul gives the word.
      this.couriers.push(new Ripple(this.w.scene, new THREE.Vector3(0, 0.365, 0)));
      this.sending = 2.6;
    }
    return { cohort, tunic, label: l.el, desk, slot, title: o.title, order: o };
  }

  releaseWorker(id, rec, now) {
    const desk = rec.desk;
    this.workers.delete(id);
    this.slots.delete(id);
    desk.setScreen('off');
    desk.deskFlame.visible = false;
    desk.tablets.visible = false;
    desk.seal.visible = false;
    desk.setStandard(null);
    desk.orderTablet.visible = true;
    desk.lamps.forEach((l) => {
      l.material.emissiveIntensity = 0;
    });
    // CSS2D elements outlive their object unless removed by hand.
    rec.label.remove();
    if (this.first) {
      rec.cohort.dispose();
      return;
    }
    // Delivered or settled: a cheer, then home. Anything else: home.
    rec.cohort.recall(now?.state === 'delivered' || now?.state === 'settled');
  }

  dressDesk(desk, o, rec) {
    const model = modelChip(modelOf(o.worker), escapeHtml);
    const act = o.state === 'blocked' ? 'Needs you' : o.state === 'failed' ? 'Failed' : o.state === 'in_review' ? 'With the Censor' : ACTIVITY_WORD[o.activity] || STATE_WORD[o.state];
    const dur = o.since && o.state === 'working' ? since(o.since, Date.parse(this.state.at) || Date.now()) : '';
    rec.label.innerHTML = `<b>COHORT ${numeral(o.cohort)}${model ? ` · ${model}` : ''}</b><span class="t">${escapeHtml(o.title)}</span><span class="s">${act}</span>${dur ? `<span class="d">${dur}</span>` : ''}`;
    rec.label.classList.toggle('needs', o.state === 'blocked' || o.state === 'failed');
    rec.label.classList.toggle('quiet', o.state === 'in_review');
    let screen = 'off';
    let motion = 'wait';
    if (o.state === 'blocked' || o.state === 'failed') {
      screen = 'halt';
      motion = 'halt';
    } else if (o.state === 'in_review') {
      screen = 'off';
      motion = 'wait';
    } else if (o.activity === 'coding') {
      screen = 'code';
      motion = 'code';
    } else if (o.activity === 'testing') {
      screen = 'test';
      motion = 'watch';
    } else if (o.activity === 'reading' || o.activity === 'designing' || o.activity === 'deciding') {
      screen = 'read';
      motion = 'read';
    }
    desk.setScreen(screen);
    desk.deskFlame.visible = true;
    desk.tablets.visible = motion === 'read';
    desk.seal.visible = o.state === 'blocked' || o.state === 'failed';
    // The standard is with the Cohort on the field; the desk keeps the record.
    desk.setStandard(null);
    desk.orderTablet.visible = o.state !== 'in_review';
    rec.cohort.setOrder(o);
    rec.motion = motion;
    rec.testing = o.activity === 'testing' && o.state === 'working';
  }

  applyCensor(state, prev) {
    const c = state.censor || { state: 'idle' };
    const o = state.orders.find((x) => x.id === c.order);
    const reviewing = c.state === 'reviewing' && o;
    const inFlight = this.couriers.some((k) => !k.done);
    const t = this.w.censorTable;
    t.submitted.visible = !!reviewing && !inFlight;
    t.wax.visible = false;
    this.w.censorGlow.intensity = reviewing ? 6 : 0;
    t.flames.forEach((f) => {
      f.visible = !!reviewing;
    });
    this.censorMotion = reviewing ? 'examine' : 'still';
    const reviewer = reviewing ? modelOf(c.reviewer) : null;
    this.dressCensor(reviewer?.color || null);
    const chip = modelChip(reviewer, escapeHtml);
    this.censorLabel.innerHTML = reviewing
      ? `<b>CENSOR${chip ? ` · ${chip}` : ''}</b><span class="t">Independent review</span><span class="s">Reviewing · ${escapeHtml(o.title)}</span>`
      : '<b>CENSOR</b><span class="t">Independent review</span><span class="s">Nothing to review</span>';
    this.censorLabel.classList.toggle('quiet', !reviewing);
    void prev;
  }

  // A new reviewer means a new robe: the Censor is rebuilt in that model's
  // colour where he sits, and keeps his label.
  dressCensor(color) {
    if ((this.censorColor || null) === color) return;
    this.censorColor = color;
    const old = this.censor;
    const next = makeFigure('censor', { pose: 'sit', tunic: color || undefined });
    next.position.copy(old.position);
    next.rotation.copy(old.rotation);
    old.children.filter((ch) => ch.isCSS2DObject).forEach((ch) => next.add(ch));
    this.w.scene.remove(old);
    this.w.scene.add(next);
    this.censor = next;
  }

  // Sealed tablets waiting for integration stand in the rack by the board.
  applyRack(state, prev) {
    const delivered = state.orders.filter((o) => o.state === 'delivered');
    const before = new Set((prev?.orders || []).filter((o) => o.state === 'delivered').map((o) => o.id));
    this.w.rack.slots.forEach((s, i) => {
      s.visible = i < delivered.length;
    });
    if (!this.first) {
      delivered.forEach((o, i) => {
        if (before.has(o.id)) return;
        const slot = this.w.rack.slots[i];
        if (!slot) return;
        slot.visible = false;
        const from = this.w.censorTable.group.position.clone().add(new THREE.Vector3(0.1, 1.0, 0));
        const to = new THREE.Vector3();
        slot.getWorldPosition(to);
        this.couriers.push(new Courier(this.w.scene, from, to, { onArrive: () => (slot.visible = true), duration: 4 }));
      });
    }
    const n = delivered.length;
    this.w.rack.group.userData.count = n;
  }

  update(dt, t) {
    this.clock += dt;
    const k = 1 - Math.exp(-dt * 4);
    // Consul.
    const crig = this.consul.userData.rig;
    this.sending = Math.max(0, this.sending - dt);
    if (this.sending > 0) MOTIONS.hail(crig, t, k);
    else if (this.consulMood === 'conferring') MOTIONS.read(crig, t, k);
    else if (this.consulMood === 'awaiting_you') {
      MOTIONS.stand(crig, t, k);
    } else MOTIONS.study(crig, t, k);
    // Sending a legion, the Consul turns west, toward the battlefield.
    const targetYaw = this.sending > 0 ? -Math.PI / 2 : this.consulMood === 'awaiting_you' ? 0.2 : Math.PI * 0.85;
    this.consul.rotation.y += (targetYaw - this.consul.rotation.y) * k * 0.5;

    // Censor.
    const srig = this.censor.userData.rig;
    if (this.censorMotion === 'examine') MOTIONS.examine(srig, t, k);
    else MOTIONS.stand(srig, t * 0.3, k);

    // Desks: the record of each live Order; lamps count through checks.
    for (const rec of this.workers.values()) {
      rec.desk.lamps.forEach((l, i) => {
        const on = rec.testing ? ((t * 1.6) % 6) > i + 0.2 : false;
        l.material.emissiveIntensity += ((on ? 6 : 0) - l.material.emissiveIntensity) * k * 2;
      });
    }

    // The legions: marching out, on the field, or marching home.
    this.cohorts = this.cohorts.filter((c) => {
      if (!c.done) c.update(dt, t, k);
      return !c.done;
    });

    this.couriers = this.couriers.filter((c) => {
      c.step(dt);
      return !c.done;
    });
  }
}

export function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}
