// Procedural textures. Everything is painted on canvases at load so the world
// ships no image files: color maps are sRGB, height-derived normal and
// roughness maps stay linear.

import * as THREE from '../vendor/three.module.min.js';

// Deterministic PRNG so the world looks the same on every load.
export function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function canvas(w, h = w) {
  const c = document.createElement('canvas');
  c.width = w;
  c.height = h;
  return c;
}

// Value noise, tiled, cheap.
function noiseField(size, cells, rand) {
  const g = [];
  for (let i = 0; i < cells * cells; i++) g.push(rand());
  const at = (x, y) => g[((y + cells) % cells) * cells + ((x + cells) % cells)];
  const out = new Float32Array(size * size);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const fx = (x / size) * cells;
      const fy = (y / size) * cells;
      const ix = Math.floor(fx);
      const iy = Math.floor(fy);
      const tx = fx - ix;
      const ty = fy - iy;
      const sx = tx * tx * (3 - 2 * tx);
      const sy = ty * ty * (3 - 2 * ty);
      const a = at(ix, iy) * (1 - sx) + at(ix + 1, iy) * sx;
      const b = at(ix, iy + 1) * (1 - sx) + at(ix + 1, iy + 1) * sx;
      out[y * size + x] = a * (1 - sy) + b * sy;
    }
  }
  return out;
}

function fbm(size, rand, octaves = [4, 8, 16, 32], weights = [0.45, 0.28, 0.17, 0.1]) {
  const out = new Float32Array(size * size);
  octaves.forEach((cells, i) => {
    const f = noiseField(size, cells, rand);
    for (let k = 0; k < out.length; k++) out[k] += f[k] * weights[i];
  });
  return out;
}

// Sobel over a height canvas → tangent-space normal map canvas.
function normalFromHeight(heightCanvas, strength = 2) {
  const w = heightCanvas.width;
  const h = heightCanvas.height;
  const src = heightCanvas.getContext('2d').getImageData(0, 0, w, h).data;
  const H = (x, y) => src[(((y + h) % h) * w + ((x + w) % w)) * 4] / 255;
  const c = canvas(w, h);
  const ctx = c.getContext('2d');
  const img = ctx.createImageData(w, h);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const dx = (H(x + 1, y) - H(x - 1, y)) * strength;
      const dy = (H(x, y + 1) - H(x, y - 1)) * strength;
      const len = Math.hypot(dx, dy, 1);
      const i = (y * w + x) * 4;
      img.data[i] = ((-dx / len) * 0.5 + 0.5) * 255;
      img.data[i + 1] = ((dy / len) * 0.5 + 0.5) * 255;
      img.data[i + 2] = ((1 / len) * 0.5 + 0.5) * 255;
      img.data[i + 3] = 255;
    }
  }
  ctx.putImageData(img, 0, 0);
  return c;
}

function colorTex(c, repeat = [1, 1]) {
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  t.wrapS = t.wrapT = THREE.RepeatWrapping;
  t.repeat.set(repeat[0], repeat[1]);
  t.anisotropy = 4;
  return t;
}

function dataTex(c, repeat = [1, 1]) {
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.NoColorSpace;
  t.wrapS = t.wrapT = THREE.RepeatWrapping;
  t.repeat.set(repeat[0], repeat[1]);
  t.anisotropy = 4;
  return t;
}

function hex(c) {
  return new THREE.Color(c);
}

// Paints tinted noise into a canvas using a two-color ramp.
function paintNoise(ctx, size, field, lo, hi) {
  const img = ctx.createImageData(size, size);
  const a = hex(lo);
  const b = hex(hi);
  for (let k = 0; k < field.length; k++) {
    const t = Math.min(1, Math.max(0, field[k]));
    img.data[k * 4] = (a.r + (b.r - a.r) * t) * 255;
    img.data[k * 4 + 1] = (a.g + (b.g - a.g) * t) * 255;
    img.data[k * 4 + 2] = (a.b + (b.b - a.b) * t) * 255;
    img.data[k * 4 + 3] = 255;
  }
  ctx.putImageData(img, 0, 0);
}

// Large paving slabs laid in running courses, the Forum floor.
export function pavingSet(seed = 11) {
  const size = 1024;
  const rand = rng(seed);
  const col = canvas(size);
  const hgt = canvas(size);
  const cx = col.getContext('2d');
  const hx = hgt.getContext('2d');
  paintNoise(cx, size, fbm(size, rand), '#b9a584', '#e3d4b4');
  hx.fillStyle = '#c8c8c8';
  hx.fillRect(0, 0, size, size);
  const rows = 4;
  const rh = size / rows;
  for (let r = 0; r < rows; r++) {
    let x = r % 2 ? -rh * 0.6 : 0;
    while (x < size) {
      const w = rh * (1.1 + rand() * 0.9);
      // Per-slab tint so the floor never reads as a repeat.
      const tint = 0.84 + rand() * 0.28;
      cx.fillStyle = `rgba(${tint > 1 ? 255 : 90},${tint > 1 ? 240 : 70},${tint > 1 ? 210 : 40},${Math.abs(tint - 1) * 0.9})`;
      cx.fillRect(x + 2, r * rh + 2, w - 4, rh - 4);
      // Joint.
      for (const [c2, lw, col2] of [
        [hx, 5, '#3a3a3a'],
        [cx, 3, 'rgba(92,74,52,0.55)'],
      ]) {
        c2.strokeStyle = col2;
        c2.lineWidth = lw;
        c2.strokeRect(x, r * rh, w, rh);
      }
      // Wear on the slab face.
      for (let k = 0; k < 3; k++) {
        cx.fillStyle = `rgba(80,62,40,${0.04 + rand() * 0.05})`;
        cx.beginPath();
        cx.ellipse(x + rand() * w, r * rh + rand() * rh, 8 + rand() * 30, 4 + rand() * 16, rand() * 3, 0, 7);
        cx.fill();
      }
      x += w;
    }
  }
  // Handle wraparound joints for the offset rows.
  const rough = canvas(size);
  const rx = rough.getContext('2d');
  paintNoise(rx, size, fbm(size, rand, [8, 32], [0.6, 0.4]), '#c4c4c4', '#f4f4f4');
  rx.globalCompositeOperation = 'multiply';
  return {
    map: colorTex(col),
    normalMap: dataTex(normalFromHeight(hgt, 3)),
    roughnessMap: dataTex(rough),
  };
}

// Ashlar courses for walls: travertine with pits.
export function ashlarSet(seed = 21, lo = '#c9b690', hi = '#ecdfc2', courses = 6) {
  const size = 512;
  const rand = rng(seed);
  const col = canvas(size);
  const hgt = canvas(size);
  const cx = col.getContext('2d');
  const hx = hgt.getContext('2d');
  paintNoise(cx, size, fbm(size, rand), lo, hi);
  hx.fillStyle = '#d0d0d0';
  hx.fillRect(0, 0, size, size);
  const ch = size / courses;
  for (let r = 0; r < courses; r++) {
    let x = r % 2 ? -ch : 0;
    while (x < size) {
      const w = ch * (1.6 + rand() * 1.4);
      cx.fillStyle = `rgba(255,248,230,${rand() * 0.14})`;
      cx.fillRect(x, r * ch, w, ch);
      cx.fillStyle = `rgba(90,70,45,${rand() * 0.1})`;
      cx.fillRect(x, r * ch, w, ch);
      hx.strokeStyle = '#555';
      hx.lineWidth = 4;
      hx.strokeRect(x, r * ch, w, ch);
      cx.strokeStyle = 'rgba(110,88,60,0.45)';
      cx.lineWidth = 2;
      cx.strokeRect(x, r * ch, w, ch);
      x += w;
    }
  }
  // Travertine pits.
  for (let k = 0; k < 700; k++) {
    const x = rand() * size;
    const y = rand() * size;
    const rr = 0.6 + rand() * 2.2;
    cx.fillStyle = 'rgba(120,98,66,0.5)';
    cx.beginPath();
    cx.ellipse(x, y, rr * 2.2, rr * 0.7, 0, 0, 7);
    cx.fill();
    hx.fillStyle = '#909090';
    hx.beginPath();
    hx.ellipse(x, y, rr * 2.2, rr * 0.7, 0, 0, 7);
    hx.fill();
  }
  return {
    map: colorTex(col),
    normalMap: dataTex(normalFromHeight(hgt, 2.4)),
  };
}

// Plain stone (columns, trims): fine grain, no joints.
export function stoneSet(seed = 31, lo = '#d6c6a4', hi = '#f1e7d0') {
  const size = 256;
  const rand = rng(seed);
  const col = canvas(size);
  paintNoise(col.getContext('2d'), size, fbm(size, rand, [4, 16, 64], [0.5, 0.3, 0.2]), lo, hi);
  const hgt = canvas(size);
  paintNoise(hgt.getContext('2d'), size, fbm(size, rand, [16, 64], [0.5, 0.5]), '#808080', '#b0b0b0');
  return { map: colorTex(col), normalMap: dataTex(normalFromHeight(hgt, 1.2)) };
}

// Marble with soft veins (Curia, Censors).
export function marbleSet(seed = 41, base = '#e9e2d4', vein = '#9d9383', hi = '#f7f3ea') {
  const size = 512;
  const rand = rng(seed);
  const col = canvas(size);
  const cx = col.getContext('2d');
  paintNoise(cx, size, fbm(size, rand, [2, 4, 8], [0.5, 0.3, 0.2]), base, hi);
  cx.strokeStyle = vein;
  for (let v = 0; v < 9; v++) {
    cx.globalAlpha = 0.12 + rand() * 0.2;
    cx.lineWidth = 0.6 + rand() * 1.6;
    cx.beginPath();
    let x = rand() * size;
    let y = 0;
    cx.moveTo(x, y);
    while (y < size) {
      x += (rand() - 0.5) * 30;
      y += 8 + rand() * 18;
      cx.lineTo(x, y);
    }
    cx.stroke();
  }
  cx.globalAlpha = 1;
  return { map: colorTex(col) };
}

// Terracotta imbrex/tegula roof: ridged rows.
export function roofSet(seed = 51) {
  const size = 256;
  const rand = rng(seed);
  const col = canvas(size);
  const hgt = canvas(size);
  const cx = col.getContext('2d');
  const hx = hgt.getContext('2d');
  paintNoise(cx, size, fbm(size, rand), '#8f4028', '#c4683f');
  const cols = 8;
  const cw = size / cols;
  for (let i = 0; i < cols; i++) {
    const g = hx.createLinearGradient(i * cw, 0, (i + 1) * cw, 0);
    g.addColorStop(0, '#202020');
    g.addColorStop(0.5, '#f0f0f0');
    g.addColorStop(1, '#202020');
    hx.fillStyle = g;
    hx.fillRect(i * cw, 0, cw, size);
    const s = cx.createLinearGradient(i * cw, 0, (i + 1) * cw, 0);
    s.addColorStop(0, 'rgba(60,20,10,0.45)');
    s.addColorStop(0.5, 'rgba(255,200,160,0.12)');
    s.addColorStop(1, 'rgba(60,20,10,0.45)');
    cx.fillStyle = s;
    cx.fillRect(i * cw, 0, cw, size);
  }
  for (let r = 0; r < 4; r++) {
    cx.fillStyle = 'rgba(50,18,8,0.35)';
    cx.fillRect(0, r * (size / 4), size, 3);
    hx.fillStyle = '#000';
    hx.fillRect(0, r * (size / 4), size, 3);
  }
  return { map: colorTex(col), normalMap: dataTex(normalFromHeight(hgt, 3)) };
}

// Wood with grain along U.
export function woodSet(seed = 61, lo = '#5b3a22', hi = '#8a5a34') {
  const size = 256;
  const rand = rng(seed);
  const col = canvas(size);
  const cx = col.getContext('2d');
  cx.fillStyle = lo;
  cx.fillRect(0, 0, size, size);
  for (let y = 0; y < size; y += 1) {
    const t = 0.5 + 0.5 * Math.sin(y * 0.35 + Math.sin(y * 0.05) * 4 + rand() * 0.6);
    const c = hex(lo).lerp(hex(hi), t * 0.8 + rand() * 0.2);
    cx.fillStyle = `#${c.getHexString()}`;
    cx.fillRect(0, y, size, 1);
  }
  for (let k = 0; k < 40; k++) {
    cx.fillStyle = `rgba(30,16,6,${rand() * 0.25})`;
    cx.fillRect(rand() * size, rand() * size, 20 + rand() * 90, 1);
  }
  return { map: colorTex(col) };
}

// Grass meadow: warm, dry Mediterranean.
export function grassSet(seed = 71) {
  const size = 512;
  const rand = rng(seed);
  const col = canvas(size);
  const cx = col.getContext('2d');
  paintNoise(cx, size, fbm(size, rand, [3, 6, 24, 96], [0.4, 0.3, 0.2, 0.1]), '#6b7240', '#a4a35e');
  for (let k = 0; k < 5000; k++) {
    cx.fillStyle = rand() > 0.5 ? 'rgba(60,70,30,0.35)' : 'rgba(200,190,120,0.25)';
    cx.fillRect(rand() * size, rand() * size, 1, 2 + rand() * 3);
  }
  return { map: colorTex(col) };
}

// Woven cloth: a faint weave for robes and awnings.
export function clothSet(seed = 81) {
  const size = 128;
  const rand = rng(seed);
  const hgt = canvas(size);
  const hx = hgt.getContext('2d');
  paintNoise(hx, size, fbm(size, rand, [16, 64], [0.5, 0.5]), '#707070', '#a0a0a0');
  hx.globalAlpha = 0.35;
  for (let i = 0; i < size; i += 2) {
    hx.fillStyle = i % 4 ? '#fff' : '#000';
    hx.fillRect(0, i, size, 1);
    hx.fillRect(i, 0, 1, size);
  }
  return { normalMap: dataTex(normalFromHeight(hgt, 1)) };
}

export function makeCanvasTexture(w, h, draw) {
  const c = canvas(w, h);
  draw(c.getContext('2d'), w, h);
  const t = new THREE.CanvasTexture(c);
  t.colorSpace = THREE.SRGBColorSpace;
  t.anisotropy = 8;
  return t;
}

// Vertical fluting for column shafts: stripes along U (around the column).
export function fluteSet(flutes = 20) {
  const w = 512;
  const h = 64;
  const c = canvas(w, h);
  const x = c.getContext('2d');
  for (let i = 0; i < w; i++) {
    const t = (i / w) * flutes;
    const f = t - Math.floor(t);
    const v = Math.round(255 * (0.5 + 0.5 * Math.cos(f * Math.PI * 2)));
    x.fillStyle = `rgb(${v},${v},${v})`;
    x.fillRect(i, 0, 1, h);
  }
  const t = dataTex(normalFromHeight(c, 5));
  t.repeat.set(1, 1);
  return t;
}
