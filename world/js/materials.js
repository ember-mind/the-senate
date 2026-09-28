// The world's material language. One palette, a few material families, and
// one shared shader tweak: surfaces darken toward the ground (a cheap contact
// occlusion that grounds every wall, column and desk) and pick up a faint
// warm rim so silhouettes separate from the stone behind them.

import * as THREE from '../vendor/three.module.min.js';
import {
  ashlarSet,
  fluteSet,
  clothSet,
  grassSet,
  marbleSet,
  pavingSet,
  roofSet,
  stoneSet,
  woodSet,
} from './textures.js';

export const PALETTE = {
  travertine: '#e4d6b8',
  stoneShade: '#a8977a',
  terracotta: '#b65a3a',
  pompeian: '#8e2b22',
  seal: '#a3261f',
  bronze: '#b0843f',
  gold: '#d9ad5c',
  laurel: '#6d7a3c',
  cypress: '#2f3d27',
  ink: '#2a211b',
  parchment: '#f1e4c6',
  ivory: '#f4ecdc',
  lamp: '#ffb65c',
  screen: '#ffcf8a',
  night: '#1d1a17',
  // State accents. Every one of them is paired with a shape or a word.
  working: '#f0a64a',
  review: '#e9dcc0',
  settled: '#c9a45a',
  blocked: '#b3342a',
};

const groundAoChunk = `
  #include <dithering_fragment>
`;

// Injects height-based contact darkening and a warm rim into a standard
// material without replacing its shader.
function stylize(material, { ao = 0.32, aoHeight = 1.1, rim = 0.18 } = {}) {
  material.onBeforeCompile = (shader) => {
    shader.uniforms.uAo = { value: ao };
    shader.uniforms.uAoHeight = { value: aoHeight };
    shader.uniforms.uRim = { value: rim };
    shader.vertexShader = shader.vertexShader
      .replace('#include <common>', '#include <common>\nvarying float vWorldY;\nvarying vec3 vWorldNormalS;\nvarying vec3 vWorldPosS;')
      .replace(
        '#include <worldpos_vertex>',
        `#include <worldpos_vertex>
        vec4 wp = modelMatrix * vec4(transformed, 1.0);
        #ifdef USE_INSTANCING
          wp = modelMatrix * instanceMatrix * vec4(transformed, 1.0);
        #endif
        vWorldY = wp.y;
        vWorldPosS = wp.xyz;
        vWorldNormalS = normalize(mat3(modelMatrix) * objectNormal);`,
      );
    shader.fragmentShader = shader.fragmentShader
      .replace(
        '#include <common>',
        '#include <common>\nuniform float uAo;\nuniform float uAoHeight;\nuniform float uRim;\nvarying float vWorldY;\nvarying vec3 vWorldNormalS;\nvarying vec3 vWorldPosS;',
      )
      .replace(
        '#include <color_fragment>',
        `#include <color_fragment>
        float contact = smoothstep(0.0, uAoHeight, max(vWorldY, 0.0));
        diffuseColor.rgb *= mix(1.0 - uAo, 1.0, contact);`,
      )
      .replace(
        groundAoChunk.trim(),
        `vec3 viewDirS = normalize(cameraPosition - vWorldPosS);
        float rimF = pow(1.0 - max(dot(normalize(vWorldNormalS), viewDirS), 0.0), 3.0);
        gl_FragColor.rgb += uRim * rimF * vec3(1.0, 0.78, 0.52) * 0.35;
        #include <dithering_fragment>`,
      );
  };
  material.customProgramCacheKey = () => `stylize-${ao}-${aoHeight}-${rim}`;
  return material;
}

export function std(params, style) {
  return stylize(new THREE.MeshStandardMaterial(params), style);
}

// Scanned PBR sets from Poly Haven (CC0), in world/assets/tex. Colour maps
// are sRGB, normal and roughness maps linear. `repeat` is per UV unit of the
// geometry, which is already world-scaled (see box() in architecture.js).
const loader = new THREE.TextureLoader();
function pbr(id, repeat) {
  const load = (suffix, color) => {
    const t = loader.load(`assets/tex/${id}_${suffix}.jpg`);
    t.colorSpace = color ? THREE.SRGBColorSpace : THREE.NoColorSpace;
    t.wrapS = t.wrapT = THREE.RepeatWrapping;
    t.repeat.set(repeat, repeat);
    t.anisotropy = 8;
    return t;
  };
  return { map: load('diff', true), normalMap: load('nor', false), roughnessMap: load('rough', false) };
}

// Built once; every module shares these instances.
export function buildMaterials() {
  const paving = pavingSet();

  const ashlar = ashlarSet();
  const darkAshlar = ashlarSet(22, '#6d6457', '#9a8f7e');
  const stone = stoneSet();
  const marble = marbleSet();
  const darkMarble = marbleSet(43, '#3f3934', '#8a8076', '#5a524a');
  const roof = roofSet();
  const wood = woodSet();
  const darkWood = woodSet(62, '#3a2416', '#5e3c24');
  const grass = grassSet();
  const cloth = clothSet();
  clothNormal = cloth.normalMap.clone();
  clothNormal.repeat.set(8, 8);

  const m = {
    paving: std({ ...pbr('marble_01', 1.1), roughness: 1, color: '#efe3cc' }, { ao: 0.0 }),
    ashlar: std({ ...pbr('sandstone_blocks_08', 1.6), roughness: 1, color: '#f4e6cc' }, { ao: 0.34, aoHeight: 1.6 }),
    darkAshlar: std({ ...darkAshlar, roughness: 0.85 }, { ao: 0.3, aoHeight: 1.4 }),
    stone: std({ ...stone, roughness: 0.82 }, { ao: 0.3 }),
    step: std({ ...stone, roughness: 0.88, color: '#e8dcc2' }, { ao: 0.12, aoHeight: 0.6 }),
    marble: std({ ...marble, roughness: 0.38 }, { ao: 0.22 }),
    column: std({ ...stone, normalMap: fluteSet(), normalScale: new THREE.Vector2(0.9, 0.9), roughness: 0.8 }, { ao: 0.3 }),
    columnMarble: std({ ...marble, normalMap: fluteSet(), normalScale: new THREE.Vector2(0.8, 0.8), roughness: 0.4 }, { ao: 0.22 }),
    darkMarble: std({ ...darkMarble, roughness: 0.55 }, { ao: 0.18 }),
    roof: std({ ...pbr('clay_roof_tiles', 1.4), roughness: 1 }, { ao: 0.0, rim: 0.1 }),
    wood: std({ ...wood, roughness: 0.7 }, { ao: 0.25, aoHeight: 0.8 }),
    darkWood: std({ ...pbr('dark_wooden_planks', 1.2), roughness: 1 }, { ao: 0.25, aoHeight: 0.8 }),
    grass: std({ ...grass, roughness: 1 }, { ao: 0.0 }),
    bronze: std({ color: PALETTE.bronze, metalness: 0.85, roughness: 0.38 }, { ao: 0.2 }),
    gold: std({ color: PALETTE.gold, metalness: 0.9, roughness: 0.3 }, { ao: 0.1 }),
    terracotta: std({ color: '#a8562f', roughness: 0.8 }, { ao: 0.2 }),
    iron: std({ color: '#3d3833', metalness: 0.7, roughness: 0.55 }, { ao: 0.2 }),
    parchment: std({ color: PALETTE.parchment, roughness: 0.9 }, { ao: 0.1, aoHeight: 0.3 }),
    redCloth: std({ color: PALETTE.pompeian, roughness: 0.95, normalMap: cloth.normalMap, side: THREE.DoubleSide }, { ao: 0.2 }),
    awning: std({ color: '#c9b48b', roughness: 0.95, normalMap: cloth.normalMap, side: THREE.DoubleSide }, { ao: 0.0 }),
    foliage: std({ color: '#223616', roughness: 0.9 }, { ao: 0.45, aoHeight: 4, rim: 0.12 }),
    pine: std({ color: '#344a1e', roughness: 0.9 }, { ao: 0.2, aoHeight: 8.5, rim: 0.12 }),
    trunk: std({ color: '#5a4636', roughness: 1 }, { ao: 0.3 }),
    hedge: std({ color: '#4d5a2f', roughness: 1 }, { ao: 0.35, aoHeight: 0.9, rim: 0.3 }),
    water: new THREE.MeshStandardMaterial({ color: '#5f8a8a', roughness: 0.08, metalness: 0.1, transparent: true, opacity: 0.85 }),
    dark: new THREE.MeshStandardMaterial({ color: '#1c1714', roughness: 1 }),
    cloth: cloth.normalMap,
  };
  return m;
}

// Robes: one material per color, cached.
let clothNormal = null;
const robeCache = new Map();
export function robe(color, normalMap = clothNormal) {
  if (!robeCache.has(color)) {
    robeCache.set(color, std({ color, roughness: 0.92, normalMap, normalScale: new THREE.Vector2(0.3, 0.3), side: THREE.DoubleSide }, { ao: 0.18, aoHeight: 0.6, rim: 0.4 }));
  }
  return robeCache.get(color);
}
