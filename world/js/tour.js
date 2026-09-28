// A scripted camera for making the product film: `?demo=story&tour=1`.
// It follows the story fixture's schedule (a state change every 12 s), so each
// shot frames the place where the next change happens. Capture only; the
// normal views never use it.

import * as THREE from '../vendor/three.module.min.js';

const V = (x, y, z) => new THREE.Vector3(x, y, z);
// [seconds, camera position, look-at]
const KEYS = [
  [0, V(46, 50, 64), V(0, 0, -6)],
  [7, V(4, 34, 44), V(-16, 0, -2)],
  [12, V(-12, 13, 17), V(-25, 1, 1)],
  [18, V(-21, 5.2, 5.5), V(-31, 1.1, -3)],
  [23.5, V(-17, 7, 7), V(-27, 1.2, 1)],
  [25.8, V(2, 20, 18), V(14, 0.5, -1)],
  [29, V(15, 6.5, 7), V(26, 1, 0)],
  [34, V(17.5, 4.2, 3.2), V(26.2, 1.2, 0)],
  [38, V(-12, 11, 11), V(-24, 1, 3)],
  [43, V(-20.5, 3.4, 8.2), V(-24.4, 1.5, 5.4)],
  [44.5, V(-19, 9, 12), V(-20, 1.2, 0)],
  [47, V(-8, 10, 10), V(-2, 1.2, -5)],
  [49.5, V(9, 6, 5), V(2.5, 1.4, -7)],
  [55, V(3, 3, -1.4), V(-2.6, 1.7, -6)],
  [60, V(12, 16, 16), V(0, 1, -5)],
  [66, V(46, 50, 64), V(0, 0, -6)],
];

const pos = new THREE.CatmullRomCurve3(KEYS.map((k) => k[1]), false, 'centripetal');
const look = new THREE.CatmullRomCurve3(KEYS.map((k) => k[2]), false, 'centripetal');

// Seconds → curve parameter: linear between keys, eased inside each segment
// so the camera settles near every key without stopping dead.
function param(t) {
  const last = KEYS.length - 1;
  if (t <= 0) return 0;
  if (t >= KEYS[last][0]) return 1;
  let i = 0;
  while (t > KEYS[i + 1][0]) i += 1;
  const f = (t - KEYS[i][0]) / (KEYS[i + 1][0] - KEYS[i][0]);
  const e = f * f * (3 - 2 * f);
  const s = 0.55 * e + 0.45 * f;
  return (i + s) / last;
}

export const TOUR_SECONDS = KEYS.at(-1)[0];

export function placeCamera(camera, seconds) {
  const u = param(seconds);
  camera.position.copy(pos.getPoint(u));
  const target = look.getPoint(u);
  camera.lookAt(target);
  return camera.position.distanceTo(target);
}
