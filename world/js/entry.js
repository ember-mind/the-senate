// Keep the experimental 2D view on the same authenticated, mission-scoped
// route. It loads no models and creates no WebGL renderer.
const params = new URLSearchParams(location.search);
const map = ['map', 'game'].includes(params.get('view'));
// Retain the in-memory token across view navigation when sessionStorage is
// unavailable. api.js removes it from the address bar after each load.
const fragmentToken = new URLSearchParams(location.hash.slice(1)).get('token');
if (params.get('manual') === '1') window.__senateClock = 0;

function navigate(view) {
  const url = new URL(location.href);
  url.searchParams.set('view', view);
  url.searchParams.delete('variant');
  if (fragmentToken) url.hash = `token=${encodeURIComponent(fragmentToken)}`;
  location.assign(url);
}

document.querySelector('#map-view').addEventListener('click', () => navigate('game'));
if (map) {
  document.querySelectorAll('[data-view]').forEach(button => {
    button.addEventListener('click', () => navigate(button.dataset.view));
  });
  document.querySelector('#battle-view').addEventListener('click', () => navigate('battle'));
  await import('./map-prototype.js');
} else {
  await import('./main.js');
  if (params.get('view') === 'battle') document.querySelector('#battle-view').click();
}
