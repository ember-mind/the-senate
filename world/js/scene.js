// Renderer, light, sky and the static campus. Lighting is a single warm
// late-afternoon sun with a fitted shadow frustum, a sky/ground hemisphere,
// a low-intensity prefiltered environment for bronze and marble, and haze
// that matches the horizon so the distance dissolves instead of ending.

import * as THREE from '../vendor/three.module.min.js';
import {
  BokehPass,
  EffectComposer,
  ShaderPass,
  GTAOPass,
  OutputPass,
  RenderPass,
  RoomEnvironment,
  UnrealBloomPass,
} from '../vendor/three.module.min.js';
import {
  LAYOUT,
  buildCensors,
  buildCuria,
  buildGround,
  buildHills,
  buildLegionHall,
  buildMosaic,
  buildTabularium,
  buildVegetation,
} from './architecture.js';
import { drawAquila } from './aquila.js';
import { PALETTE, buildMaterials } from './materials.js';
import { flameMesh, makeBanner, makeCampaignBoard, makeCensorTable, makeLamp, makeShelf, makeTabletRack, makeWorkstation } from './props.js';
import { makeCanvasTexture } from './textures.js';
import { battleSlots, buildBattlefield, bake, box, columnGeometry } from './architecture.js';

export const QUALITY = {
  low: { pixelRatio: 1, shadow: 1024, ao: false, bloom: false, antialias: false },
  medium: { pixelRatio: 1.5, shadow: 2048, ao: false, bloom: true, antialias: true },
  high: { pixelRatio: 2, shadow: 2048, ao: true, bloom: true, antialias: true },
};

export function pickQuality() {
  const param = new URLSearchParams(location.search).get('quality');
  if (param && QUALITY[param]) return param;
  const cores = navigator.hardwareConcurrency || 4;
  const mobile = /Mobi|Android/i.test(navigator.userAgent);
  if (mobile || cores <= 4) return 'low';
  return 'high';
}

function skyDome() {
  const g = new THREE.SphereGeometry(400, 32, 16);
  const m = new THREE.ShaderMaterial({
    side: THREE.BackSide,
    depthWrite: false,
    fog: false,
    uniforms: {
      top: { value: new THREE.Color('#8fb1cf') },
      horizon: { value: new THREE.Color('#f0dcbc') },
      sunDir: { value: new THREE.Vector3() },
    },
    vertexShader: `varying vec3 vDir; void main(){ vDir = normalize(position); gl_Position = projectionMatrix * modelViewMatrix * vec4(position,1.0); }`,
    fragmentShader: `uniform vec3 top; uniform vec3 horizon; uniform vec3 sunDir; varying vec3 vDir;
      void main(){
        float h = clamp(vDir.y, 0.0, 1.0);
        vec3 c = mix(horizon, top, pow(h, 0.55));
        float s = max(dot(normalize(vDir), normalize(sunDir)), 0.0);
        c += vec3(1.0, 0.8, 0.55) * (pow(s, 12.0) * 0.35 + pow(s, 400.0) * 1.5);
        gl_FragColor = vec4(c, 1.0);
        #include <colorspace_fragment>
      }`,
  });
  return new THREE.Mesh(g, m);
}

function mosaicTexture() {
  return makeCanvasTexture(1024, 1024, (ctx, W) => {
    // Dark marble ground with a tessera grain.
    ctx.fillStyle = '#3b312a';
    ctx.fillRect(0, 0, W, W);
    for (let y = 0; y < W; y += 8) {
      for (let x = 0; x < W; x += 8) {
        const v = 40 + ((x * 13 + y * 7) % 23);
        ctx.fillStyle = `rgb(${v + 14},${v + 6},${v})`;
        ctx.fillRect(x + 0.5, y + 0.5, 7, 7);
      }
    }
    ctx.strokeStyle = '#b0843f';
    ctx.lineWidth = 14;
    ctx.beginPath();
    ctx.arc(W / 2, W / 2, W / 2 - 40, 0, Math.PI * 2);
    ctx.stroke();
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.arc(W / 2, W / 2, W / 2 - 70, 0, Math.PI * 2);
    ctx.stroke();
    drawAquila(ctx, 212, 190, 600, '#c99a4b', '#3b312a');
    ctx.fillStyle = '#c99a4b';
    ctx.font = '600 52px Georgia, serif';
    ctx.textAlign = 'center';
    ctx.fillText('S · P · Q · R', W / 2, W - 150);
  });
}

export function createWorld(canvas, qualityName) {
  const Q = QUALITY[qualityName];
  // Dusk: a lighting variant for stills and the site (?dusk=1). Same world, later hour.
  const dusk = new URLSearchParams(location.search).get('dusk') === '1';
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: Q.antialias, powerPreference: 'high-performance' });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio, Q.pixelRatio));
  renderer.setSize(window.innerWidth, window.innerHeight);
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFShadowMap;
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.0;
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.info.autoReset = false;

  const scene = new THREE.Scene();
  const fogColor = new THREE.Color(dusk ? '#241d2a' : '#e6d3b3');
  scene.fog = new THREE.FogExp2(fogColor, dusk ? 0.006 : 0.0024);
  scene.background = fogColor;

  const camera = new THREE.PerspectiveCamera(38, window.innerWidth / window.innerHeight, 0.3, 900);

  // Light.
  const sunDir = dusk ? new THREE.Vector3(-0.8, 0.16, 0.45).normalize() : new THREE.Vector3(-0.55, 0.62, 0.56).normalize();
  const sun = new THREE.DirectionalLight(dusk ? '#ff8a4a' : '#ffe0b0', dusk ? 1.1 : 3.6);
  sun.position.copy(sunDir).multiplyScalar(90);
  sun.castShadow = true;
  sun.shadow.mapSize.set(Q.shadow, Q.shadow);
  const sc = sun.shadow.camera;
  sc.left = -58;
  sc.right = 58;
  sc.top = 48;
  sc.bottom = -48;
  sc.near = 20;
  sc.far = 220;
  sun.shadow.bias = -0.0004;
  sun.shadow.normalBias = 0.04;
  sun.shadow.radius = 3;
  scene.add(sun, sun.target);
  const hemi = dusk ? new THREE.HemisphereLight('#3a4d7c', '#1c1510', 0.85) : new THREE.HemisphereLight('#c3d6ea', '#7a6448', 0.6);
  scene.add(hemi);

  const pmrem = new THREE.PMREMGenerator(renderer);
  scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
  scene.environmentIntensity = 0.35;

  const sky = skyDome();
  sky.material.uniforms.sunDir.value.copy(sunDir);
  if (dusk) {
    sky.material.uniforms.top.value.set('#070c1a');
    sky.material.uniforms.horizon.value.set('#262b40');
    sky.material.uniforms.dusk = { value: 1 };
    sky.material.fragmentShader = sky.material.fragmentShader.replace('pow(h, 0.55)', 'pow(h, 0.28)');
    scene.environmentIntensity = 0.12;
  }
  scene.add(sky);

  const M = buildMaterials();
  const uniforms = { uTime: { value: 0 } };

  buildGround(scene, M);
  buildBattlefield(scene, M);
  buildMosaic(scene, mosaicTexture());
  buildHills(scene, dusk);
  if (dusk) {
    // Stars: a few hundred points high in the sky dome, too faint to compete.
    const n = 700;
    const pos = new Float32Array(n * 3);
    let seed = 11;
    const r01 = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
    for (let i = 0; i < n; i++) {
      const a = r01() * Math.PI * 2;
      const e = 0.12 + Math.pow(r01(), 0.7) * 1.35;
      pos.set([Math.cos(a) * Math.cos(e) * 360, Math.sin(e) * 360, Math.sin(a) * Math.cos(e) * 360], i * 3);
    }
    const sg = new THREE.BufferGeometry();
    sg.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    // Thin dusk clouds catching the last light low over the hills.
    const cloudTex = makeCanvasTexture(256, 128, (ctx, W, H) => {
      for (let i = 0; i < 14; i++) {
        const x = 30 + Math.random() * (W - 60);
        const y = 40 + Math.random() * 50;
        const r = 20 + Math.random() * 40;
        const g = ctx.createRadialGradient(x, y, 2, x, y, r);
        g.addColorStop(0, 'rgba(210,170,170,0.35)');
        g.addColorStop(1, 'rgba(210,170,170,0)');
        ctx.fillStyle = g;
        ctx.fillRect(0, 0, W, H);
      }
    });
    for (let i = 0; i < 9; i++) {
      const a = (i / 9) * Math.PI * 2 + 0.3;
      const cl = new THREE.Mesh(new THREE.PlaneGeometry(120, 40), new THREE.MeshBasicMaterial({ map: cloudTex, transparent: true, depthWrite: false, fog: false, opacity: 0.55 }));
      cl.position.set(Math.cos(a) * 300, 40 + (i % 3) * 14, Math.sin(a) * 300);
      cl.lookAt(0, cl.position.y, 0);
      scene.add(cl);
    }
    const stars = new THREE.Points(sg, new THREE.PointsMaterial({ color: '#dfe6ff', size: 1.3, sizeAttenuation: false, fog: false, transparent: true, opacity: 0.8 }));
    scene.add(stars);
  }
  const legion = buildLegionHall(scene, M);
  const censors = buildCensors(scene, M);
  const curia = buildCuria(scene, M);
  buildTabularium(scene, M);
  buildVegetation(scene, M);

  // Campaign board at the head of the Forum, flanked by honorific columns
  // bearing the Aquila banners.
  const board = makeCampaignBoard(M);
  board.group.position.set(LAYOUT.board.x, 0, LAYOUT.board.z);
  bake(board.group, [board.face]);
  scene.add(board.group);
  const rack = makeTabletRack(M);
  rack.group.position.set(LAYOUT.board.x + 5.2, 0, LAYOUT.board.z + 0.8);
  rack.group.rotation.y = -0.35;
  scene.add(rack.group);
  const tall = columnGeometry(7.5, 0.36);
  for (const dx of [-5.6, 5.6]) {
    const c = new THREE.Mesh(tall, M.marble);
    c.castShadow = c.receiveShadow = true;
    c.position.set(LAYOUT.board.x + dx, 0.36, LAYOUT.board.z - 0.6);
    scene.add(c);
    const banner = makeBanner(M, uniforms);
    banner.position.set(LAYOUT.board.x + dx, 5.4, LAYOUT.board.z - 0.1);
    scene.add(banner);
    const pole = new THREE.Mesh(new THREE.CylinderGeometry(0.04, 0.04, 1.8, 6), M.bronze);
    pole.rotation.z = Math.PI / 2;
    pole.position.set(LAYOUT.board.x + dx, 6.85, LAYOUT.board.z - 0.1);
    scene.add(pole);
  }

  // Legion Hall workstations: six places in two rows in the open courtyard.
  const desks = [];
  const L = legion.courtyard;
  // Two columns of three; Cohorts face west, so from the Forum side you see
  // each worker's shoulder and the glow of the tablet-terminal in front.
  const deskSpots = [];
  for (const dx of [-3.4, 3.4]) for (const dz of [-4.2, 0, 4.2]) deskSpots.push([L.x + dx, L.z + dz]);
  for (const [x, z] of deskSpots) {
    const ws = makeWorkstation(M);
    ws.group.position.set(x, 0.3, z);
    ws.group.rotation.y = Math.PI / 2 + (((x * 7 + z * 13) % 5) - 2) * 0.03;
    ws.group.updateMatrixWorld(true);
    bake(ws.group, [ws.screen, ws.tablets, ws.orderTablet, ws.seal, ws.standard, ws.deskFlame, ...ws.lamps]);
    scene.add(ws.group);
    desks.push(ws);
  }
  // Scroll shelves along the roofed north and west walks. Decoration only.
  const LL = LAYOUT.legion;
  for (let i = 0; i < 4; i++) {
    const shelf = makeShelf(M);
    shelf.position.set(LL.x - 6 + i * 4.6, 0.3, LL.z - LL.d / 2 + 0.75);
    scene.add(bake(shelf));
  }
  // West walk: either side of the back gate the Cohorts march out of.
  for (const z of [-6.8, 5.4]) {
    const shelf = makeShelf(M);
    shelf.position.set(LL.x - LL.w / 2 + 0.75, 0.3, LL.z + z);
    shelf.rotation.y = Math.PI / 2;
    scene.add(bake(shelf));
  }
  // Censors' table.
  const censorTable = makeCensorTable(M);
  censorTable.group.position.copy(censors.seat).add(new THREE.Vector3(-1.1, 0, 0));
  if (dusk) {
    for (const f of censorTable.flames) {
      const pl = new THREE.PointLight('#ffb766', 5, 7, 1.6);
      f.add(pl);
    }
  }
  bake(censorTable.group, [censorTable.submitted, censorTable.wax, ...censorTable.flames]);
  scene.add(censorTable.group);

  // Oil lamps at the Legion Hall entrance and in the Forum; lit, not
  // animated, except for the flame's soft breathing.
  const lamps = [];
  const LH = LAYOUT.legion;
  const lampSpots = [
    [LH.x + 12.9, LH.z - 2.8],
    [LH.x + 12.9, LH.z + 2.8],
    [LAYOUT.censors.x - 3.4, -5],
    [LAYOUT.censors.x - 3.4, 5],
  ];
  // At dusk the campus is lit like a working place after sunset: lamps along
  // the Forum, inside the Legion Hall's walks, and on the Censor's table.
  if (dusk && qualityName !== 'low') {
    lampSpots.push(
      [-12, -9.2], [12, -9.2], [-12, 9.2], [12, 9.2],
      [LH.x - 5, LH.z - 6.2], [LH.x + 5, LH.z - 6.2], [LH.x - 8.4, LH.z + 3],
    );
  }
  for (const [x, z] of lampSpots) {
    const l = makeLamp(M);
    if (dusk) {
      const pl = new THREE.PointLight('#ffb35c', 12, 13, 1.7);
      pl.position.y = 1.9;
      l.group.add(pl);
    }
    l.group.position.set(x, 0.3, z);
    bake(l.group, [l.flame]);
    scene.add(l.group);
    lamps.push(l);
  }
  // Seal-red hangings on the exedra wall, behind the Censor's seat: folded
  // cloth with an ivory border and a small gold Aquila.
  const hangingMaterial = new THREE.MeshStandardMaterial({
    map: makeCanvasTexture(256, 700, (ctx, W, H) => {
      ctx.fillStyle = '#8e2b22';
      ctx.fillRect(0, 0, W, H);
      ctx.fillStyle = 'rgba(0,0,0,0.12)';
      for (let y = 0; y < H; y += 3) ctx.fillRect(0, y, W, 1);
      ctx.strokeStyle = '#e9dfc9';
      ctx.lineWidth = 10;
      ctx.strokeRect(14, 14, W - 28, H - 28);
      drawAquila(ctx, 48, 170, 160, '#d9ad5c', '#8e2b22');
      ctx.fillStyle = '#e9dfc9';
      for (let x = 20; x < W - 10; x += 14) ctx.fillRect(x, H - 14, 6, 14);
    }),
    roughness: 0.9,
    side: THREE.DoubleSide,
    normalMap: M.cloth,
  });
  for (const dz of [-2.4, 2.4]) {
    const hg = new THREE.PlaneGeometry(1.3, 3.6, 16, 12);
    const hp = hg.attributes.position;
    for (let i = 0; i < hp.count; i++) {
      const x = hp.getX(i);
      const y = hp.getY(i);
      hp.setZ(i, Math.sin(x * 14) * 0.035 * (0.6 + (1.8 - y) * 0.25));
    }
    hg.computeVertexNormals();
    const hang = new THREE.Mesh(hg, hangingMaterial);
    const a = Math.asin(dz / 6.5);
    hang.position.set(LAYOUT.censors.x + Math.cos(a) * 6.45, 3.0, LAYOUT.censors.z + Math.sin(a) * 6.45);
    hang.rotation.y = -Math.PI / 2 - a;
    hang.receiveShadow = true;
    scene.add(hang);
    const rod = new THREE.Mesh(new THREE.CylinderGeometry(0.04, 0.04, 1.6, 6), M.bronze);
    rod.rotation.x = Math.PI / 2;
    rod.rotation.y = -a;
    rod.position.set(LAYOUT.censors.x + Math.cos(a) * 6.4, 4.85, LAYOUT.censors.z + Math.sin(a) * 6.4);
    scene.add(rod);
  }
  // Wall sconces in the Censor's exedra: the back wall reads as a wall at
  // dusk instead of a void.
  if (dusk) {
    for (const dz of [-4.4, 0, 4.4]) {
      const a = Math.asin(dz / 6.2);
      const x = LAYOUT.censors.x + Math.cos(a) * 6.2;
      const z = LAYOUT.censors.z + Math.sin(a) * 6.2;
      const sconce = new THREE.Mesh(new THREE.CylinderGeometry(0.14, 0.06, 0.18, 10), M.bronze);
      sconce.position.set(x, 3.3, z);
      scene.add(sconce);
      const f = flameMesh(0.7);
      f.position.set(x, 3.4, z);
      scene.add(f);
      const pl = new THREE.PointLight('#ffae60', 7, 7, 1.5);
      pl.position.set(x - Math.cos(a) * 0.4, 3.6, z - Math.sin(a) * 0.4);
      scene.add(pl);
    }
  }
  const censorGlow = new THREE.PointLight('#ffb766', 0, 9, 1.6);
  censorGlow.position.copy(censors.seat).add(new THREE.Vector3(-0.8, 2.2, 0));
  scene.add(censorGlow);

  // Post-processing.
  const composer = new EffectComposer(renderer);
  composer.addPass(new RenderPass(scene, camera));
  let gtao = null;
  if (Q.ao) {
    gtao = new GTAOPass(scene, camera, window.innerWidth, window.innerHeight);
    gtao.blendIntensity = 1.0;
    gtao.updateGtaoMaterial({ radius: 0.45, distanceExponent: 1.6, thickness: 1.4, scale: 1.2 });
    composer.addPass(gtao);
  }
  if (Q.bloom) {
    composer.addPass(new UnrealBloomPass(new THREE.Vector2(window.innerWidth, window.innerHeight), 0.45, 0.55, 2.8));
  }
  // Cinematic capture (?cine=1): shallow depth of field on the subject, a
  // vignette, a cool-shadow / warm-light grade and fine grain. For stills
  // and the film only; the live view stays sharp and plain.
  const cine = new URLSearchParams(location.search).get('cine') === '1';
  let bokeh = null;
  let grade = null;
  if (cine) {
    bokeh = new BokehPass(scene, camera, { focus: 10, aperture: 0.0009, maxblur: 0.0045 });
    composer.addPass(bokeh);
  }
  composer.addPass(new OutputPass());
  if (cine) {
    grade = new ShaderPass({
      uniforms: { tDiffuse: { value: null }, uTime: { value: 0 }, uAspect: { value: window.innerWidth / window.innerHeight } },
      vertexShader: 'varying vec2 vUv; void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }',
      fragmentShader: `uniform sampler2D tDiffuse; uniform float uTime; uniform float uAspect; varying vec2 vUv;
        float hash(vec2 p){ return fract(sin(dot(p, vec2(12.9898, 78.233)) + uTime * 7.0) * 43758.5453); }
        void main(){
          vec3 c = texture2D(tDiffuse, vUv).rgb;
          float l = dot(c, vec3(0.2126, 0.7152, 0.0722));
          vec3 shadows = vec3(0.86, 0.93, 1.12);
          vec3 lights = vec3(1.07, 1.0, 0.9);
          c *= mix(shadows, lights, smoothstep(0.08, 0.7, l));
          c = mix(vec3(l), c, 1.06);
          c = (c - 0.5) * 1.06 + 0.5;
          vec2 q = (vUv - 0.5) * vec2(uAspect, 1.0);
          c *= 1.0 - 0.55 * smoothstep(0.35, 1.05, length(q));
          c += (hash(vUv * 1000.0) - 0.5) * 0.028;
          gl_FragColor = vec4(clamp(c, 0.0, 1.0), 1.0);
        }`,
    });
    composer.addPass(grade);
  }

  function resize() {
    camera.aspect = window.innerWidth / window.innerHeight;
    camera.updateProjectionMatrix();
    renderer.setSize(window.innerWidth, window.innerHeight);
    composer.setSize(window.innerWidth, window.innerHeight);
  }
  window.addEventListener('resize', resize);

  return {
    renderer,
    scene,
    camera,
    composer,
    sun,
    uniforms,
    materials: M,
    board,
    rack,
    desks,
    legion,
    field: { slots: battleSlots(), muster: new THREE.Vector3(LL.x + 1.2, 0.3, LL.z - 0.9) },
    censors,
    censorTable,
    censorGlow,
    curia,
    lamps,
    palette: PALETTE,
    bokeh,
    grade,
    quality: qualityName,
  };
}
