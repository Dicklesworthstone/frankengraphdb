// Bounded deterministic layout, off the UI thread. No continuous idle simulation.
import { hash, LIMITS } from './model.mjs';
export function createLayout(nodes, edges, mode = 'force') {
  if (nodes.length > LIMITS.nodes || edges.length > LIMITS.edges) throw new Error('Layout bounds exceeded.');
  const groups = [...new Set(nodes.map(node => node.group))].sort();
  const counts = new Map(groups.map(group => [group, nodes.filter(node => node.group === group).length]));
  const seen = new Map();
  const points = nodes.map((node, i) => {
    const group = groups.indexOf(node.group), angle = group * Math.PI * 2 / Math.max(1, groups.length) - Math.PI / 2;
    const at = seen.get(node.group) || 0; seen.set(node.group, at + 1);
    const spread = Math.sqrt((at + 0.5) / counts.get(node.group)) * 135;
    const phase = at * 2.399963229728653 + (hash(node.group) % 360) * Math.PI / 180;
    return { id: node.id, x: Math.cos(angle) * 185 + Math.cos(phase) * spread, y: Math.sin(angle) * 185 + Math.sin(phase) * spread, vx: 0, vy: 0, group };
  });
  const byId = new Map(points.map((point, index) => [point.id, index]));
  const links = edges.map(edge => [byId.get(edge.source), byId.get(edge.target)]).filter(([a, b]) => a !== undefined && b !== undefined && a !== b);
  return { points, links, groups, mode, tick: 0 };
}
export function stepLayout(state, steps = 5) {
  if (state.mode === 'radial') {
    const sorted = [...state.points].sort((a, b) => a.group - b.group || a.id.localeCompare(b.id));
    const radius = Math.max(125, sorted.length * 5);
    sorted.forEach((point, i) => { const angle = i * Math.PI * 2 / sorted.length; point.x = Math.cos(angle) * radius; point.y = Math.sin(angle) * radius; });
    state.tick = 250; return;
  }
  for (let iteration = 0; iteration < steps && state.tick < 250; iteration++, state.tick++) {
    const temperature = Math.max(0.07, 1 - state.tick / 255);
    const points = state.points;
    for (let i = 0; i < points.length; i++) {
      const a = points[i];
      for (let j = i + 1; j < points.length; j++) {
        const b = points[j];
        let dx = a.x - b.x, dy = a.y - b.y;
        if (Math.abs(dx) + Math.abs(dy) < 0.01) { dx = 0.2; dy = 0.1; }
        const squared = Math.max(100, dx * dx + dy * dy), strength = Math.min(2.8, 1200 / squared) * temperature;
        const distance = Math.sqrt(squared);
        a.vx += dx / distance * strength; a.vy += dy / distance * strength;
        b.vx -= dx / distance * strength; b.vy -= dy / distance * strength;
      }
    }
    for (const [ai, bi] of state.links) {
      const a = points[ai], b = points[bi], dx = b.x - a.x, dy = b.y - a.y;
      const distance = Math.max(1, Math.hypot(dx, dy));
      const force = (distance - 100) * 0.012 * temperature;
      a.vx += dx / distance * force; a.vy += dy / distance * force;
      b.vx -= dx / distance * force; b.vy -= dy / distance * force;
    }
    for (const point of points) {
      const angle = point.group * Math.PI * 2 / Math.max(1, state.groups.length) - Math.PI / 2;
      const strength = state.mode === 'clusters' ? 0.012 : 0.0025;
      point.vx += (Math.cos(angle) * 170 - point.x) * strength * temperature;
      point.vy += (Math.sin(angle) * 170 - point.y) * strength * temperature;
      point.vx *= 0.77; point.vy *= 0.77;
      point.x += Math.max(-9, Math.min(9, point.vx)); point.y += Math.max(-9, Math.min(9, point.vy));
    }
  }
}
if (typeof self !== 'undefined' && typeof document === 'undefined') {
  let generation = 0;
  self.onmessage = ({ data }) => {
    const ticket = ++generation;
    if (data.stop) return;
    const state = createLayout(data.nodes, data.edges, data.mode);
    const advance = () => {
      if (ticket !== generation) return;
      stepLayout(state, 10);
      self.postMessage({ id: data.id, points: state.points.map(({ id, x, y }) => ({ id, x, y })), done: state.tick >= 250 });
      if (state.tick < 250) setTimeout(advance, 12);
    };
    advance();
  };
}
