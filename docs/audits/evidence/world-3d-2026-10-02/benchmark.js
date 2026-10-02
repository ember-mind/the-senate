
if (params.get('audit') === '1' && manual) {
  const samples = [];
  for (let i = 0; i < 150; i++) {
    await new Promise(requestAnimationFrame);
    const start = performance.now();
    await window.__senate.advance(1 / 30);
    world.renderer.getContext().finish();
    if (i >= 30) samples.push(performance.now() - start);
  }
  samples.sort((a,b) => a-b);
  const out = document.createElement('pre');
  out.id = 'audit-results'; out.hidden = true;
  out.textContent = JSON.stringify({ ...stats, width: innerWidth, height: innerHeight,
    pixelRatio: world.renderer.getPixelRatio(), medianMs: samples[60], p95Ms: samples[114],
    memory: world.renderer.info.memory, programs: world.renderer.info.programs.length,
    phases: director.cohorts.map(c => ({phase:c.phase, members:c.members.length, enemies:c.band?.length, mode:c.mode})) });
  document.body.append(out);
}
