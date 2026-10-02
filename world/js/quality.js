// Explicit quality stays fixed. Automatic quality adjusts resolution only,
// after sustained frame pressure; scene semantics never depend on frame rate.
export const QUALITY = {
  low: { pixelRatio: 1, shadow: 1024, ao: false, bloom: false, samples: 0 },
  medium: { pixelRatio: 1.5, shadow: 2048, ao: false, bloom: true, samples: 2 },
  high: { pixelRatio: 2, shadow: 2048, ao: true, bloom: true, samples: 4 },
};

export function chooseQuality(param, cores = 4, mobile = false) {
  if (Object.hasOwn(QUALITY, param)) return param;
  if (mobile || cores <= 4) return 'low';
  return cores >= 8 ? 'high' : 'medium';
}

export class ResolutionBudget {
  constructor(maxRatio) {
    this.max = maxRatio;
    this.min = Math.min(1, maxRatio);
    this.ratio = maxRatio;
    this.slow = 0;
    this.fast = 0;
    this.cooldown = 2;
  }

  sample(dt) {
    // Tab suspension and debugger stops are not GPU pressure.
    if (!Number.isFinite(dt) || dt <= 0 || dt > 0.25) {
      this.slow = this.fast = 0;
      return null;
    }
    this.cooldown = Math.max(0, this.cooldown - dt);
    this.slow = dt > 1 / 45 ? this.slow + dt : 0;
    this.fast = dt < 1 / 57 ? this.fast + dt : 0;
    let next = this.ratio;
    if (!this.cooldown && this.slow > 2) next = Math.max(this.min, this.ratio - 0.25);
    else if (!this.cooldown && this.fast > 6) next = Math.min(this.max, this.ratio + 0.25);
    if (next === this.ratio) return null;
    this.ratio = next;
    this.slow = this.fast = 0;
    this.cooldown = 4;
    return next;
  }
}
