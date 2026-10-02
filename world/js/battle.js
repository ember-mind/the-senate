// Shared battle timing and bounded, instanced effects. Combat remains a
// projection of an Order: no damage, victory or progress is invented here.
import * as THREE from '../vendor/three.module.min.js';
import { rng } from './textures.js';

export const BATTLE_PERIOD = 1.65;

export function battleBeat(t, phase = 0) {
  const clock = Math.max(0, t + phase) / BATTLE_PERIOD;
  const beat = clock % 1;
  const pulse = (at, width) => Math.max(0, 1 - Math.abs(beat - at) / width);
  return { cycle: Math.floor(clock), beat, advance: pulse(0.28, 0.2), answer: pulse(0.72, 0.2) };
}

// Cloth moves on the GPU, with its top hem pinned to the crossbar.
export function standardMaterial(params, time, phase = 0) {
  const material = new THREE.MeshStandardMaterial({ roughness: 0.92, side: THREE.DoubleSide, ...params });
  material.onBeforeCompile = shader => {
    shader.uniforms.uBannerTime = time;
    shader.uniforms.uBannerPhase = { value: phase };
    shader.vertexShader = shader.vertexShader
      .replace('#include <common>', '#include <common>\nuniform float uBannerTime; uniform float uBannerPhase;')
      .replace('#include <begin_vertex>', `#include <begin_vertex>
        float hem = pow(1.0 - uv.y, 1.5);
        transformed.z += hem * (sin(uv.x * 8.0 + uBannerTime * 3.1 + uBannerPhase) * 0.09
          + sin(uv.y * 5.0 - uBannerTime * 2.2 + uBannerPhase) * 0.04);`);
  };
  material.customProgramCacheKey = () => 'battle-cloth-v1';
  return material;
}

class ParticlePool {
  constructor(scene, capacity, sparks) {
    this.capacity = capacity;
    this.sparks = sparks;
    this.live = 0;
    this.particles = Array.from({ length: capacity }, () => ({
      position: new THREE.Vector3(), velocity: new THREE.Vector3(), age: 0, life: 1, size: 1,
    }));
    this.alpha = new THREE.InstancedBufferAttribute(new Float32Array(capacity), 1);
    const geometry = new THREE.PlaneGeometry(1, 1);
    geometry.setAttribute('aOpacity', this.alpha);
    const material = new THREE.ShaderMaterial({
      transparent: true, depthWrite: false, toneMapped: false,
      blending: sparks ? THREE.AdditiveBlending : THREE.NormalBlending,
      uniforms: { uColor: { value: new THREE.Color(sparks ? '#ffc570' : '#b69b76') } },
      vertexShader: `attribute float aOpacity; varying vec2 vUv; varying float vAlpha;
        void main(){
          vUv = uv; vAlpha = aOpacity;
          vec4 p = modelViewMatrix * instanceMatrix * vec4(0.0, 0.0, 0.0, 1.0);
          p.xy += position.xy * vec2(length(instanceMatrix[0].xyz), length(instanceMatrix[1].xyz));
          gl_Position = projectionMatrix * p;
        }`,
      fragmentShader: `uniform vec3 uColor; varying vec2 vUv; varying float vAlpha;
        void main(){
          vec2 p = vUv * 2.0 - 1.0;
          float edge = max(0.0, 1.0 - dot(p,p));
          float alpha = edge * edge * vAlpha;
          if (alpha < 0.005) discard;
          gl_FragColor = vec4(uColor, alpha);
          #include <colorspace_fragment>
        }`,
    });
    this.mesh = new THREE.InstancedMesh(geometry, material, capacity);
    this.mesh.name = sparks ? 'battle-sparks' : 'battle-dust';
    this.mesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage);
    this.alpha.setUsage(THREE.DynamicDrawUsage);
    this.mesh.count = 0;
    this.mesh.frustumCulled = false;
    this.mesh.renderOrder = sparks ? 3 : 2;
    this.matrix = new THREE.Matrix4();
    scene.add(this.mesh);
  }

  emit(position, random, strength = 1) {
    // Full pool drops new particles rather than allocating more GPU resources.
    if (this.live >= this.capacity) return;
    const p = this.particles[this.live++];
    p.position.copy(position);
    p.position.y += this.sparks ? 1.05 : 0.08;
    p.velocity.set((random() - 0.5) * (this.sparks ? 3 : 0.8), this.sparks ? 1 + random() * 1.7 : 0.25 + random() * 0.3, (random() - 0.5) * 0.8);
    p.age = 0;
    p.life = this.sparks ? 0.22 + random() * 0.15 : 0.9 + random() * 0.5;
    p.size = (this.sparks ? 0.055 + random() * 0.05 : 0.45 + random() * 0.4) * strength;
  }

  update(dt) {
    let i = 0;
    while (i < this.live) {
      const p = this.particles[i];
      p.age += dt;
      if (p.age >= p.life) {
        this.live--;
        this.particles[i] = this.particles[this.live];
        this.particles[this.live] = p;
        continue;
      }
      p.position.addScaledVector(p.velocity, dt);
      if (this.sparks) p.velocity.y -= dt * 6;
      const u = p.age / p.life;
      const size = p.size * (this.sparks ? 1 : 1 + u * 2.5);
      this.matrix.makeScale(size, size * (this.sparks ? 2.4 : 0.75), size).setPosition(p.position);
      this.mesh.setMatrixAt(i, this.matrix);
      this.alpha.setX(i, (this.sparks ? 1 : 0.25) * Math.sin(Math.PI * u) * (1 - u));
      i++;
    }
    this.mesh.count = this.live;
    this.mesh.visible = this.live > 0;
    if (this.live) {
      this.mesh.instanceMatrix.needsUpdate = true;
      this.alpha.needsUpdate = true;
    }
  }

  dispose() {
    this.mesh.removeFromParent();
    this.mesh.geometry.dispose();
    this.mesh.material.dispose();
    this.mesh.dispose();
  }
}

export class BattleEffects {
  constructor(scene, quality = 'high') {
    this.random = rng(1207);
    this.dustPool = new ParticlePool(scene, quality === 'low' ? 48 : 144, false);
    this.sparkPool = new ParticlePool(scene, quality === 'low' ? 24 : 72, true);
  }

  dust(position, strength = 1) { this.dustPool.emit(position, this.random, strength); }

  impact(position, shield = false) {
    this.dust(position, shield ? 0.65 : 1);
    for (let i = 0; i < (shield ? 3 : 5); i++) this.sparkPool.emit(position, this.random);
  }

  update(dt) { this.dustPool.update(dt); this.sparkPool.update(dt); }
  dispose() { this.dustPool.dispose(); this.sparkPool.dispose(); }
}
