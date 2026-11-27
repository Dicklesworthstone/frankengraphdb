// Actual force integration lives in the import-free, fixed-memory Wasm module.
// This worker owns only numeric display coordinates; it never receives a token.
import { hash, LIMITS } from './model.mjs';

export const DEFAULT_PHYSICS = Object.freeze({ charge: 65, distance: 115, gravity: 0.005, collision: 1, grouping: 0 });
// Build undirected hop rings over the returned topology. The selected vertex
// roots its component; elsewhere the most distinct neighbors wins, then ID.
// Sort ties by exact ID so reordering rows or parallel edges cannot rotate a ring.
function radialTargets(points, links, focusId) {
  const neighbors = points.map(() => new Set());
  for (const [a, b] of links) {
    if (a !== b) { neighbors[a].add(b); neighbors[b].add(a); }
  }
  const compareId = (a, b) => points[a].id < points[b].id ? -1 : points[a].id > points[b].id ? 1 : 0;
  const candidates = points.map((_, i) => i).sort((a, b) =>
    Number(points[b].id === focusId) - Number(points[a].id === focusId)
    || neighbors[b].size - neighbors[a].size || compareId(a, b));
  const seen = new Set(), components = [];
  let area = 0, widest = 0;
  for (const root of candidates) {
    if (seen.has(root)) continue;
    let ring = [root], radius = 0;
    const members = [];
    seen.add(root);
    while (ring.length) {
      const next = [];
      for (let j = 0; j < ring.length; j++) {
        const index = ring[j], angle = j * 2 * Math.PI / ring.length - Math.PI / 2;
        points[index].tx = Math.cos(angle) * radius;
        points[index].ty = Math.sin(angle) * radius;
        members.push(index);
        for (const neighbor of neighbors[index]) {
          if (!seen.has(neighbor)) { seen.add(neighbor); next.push(neighbor); }
        }
      }
      if (next.length) radius = Math.max(radius + 100, next.length * 48 / (2 * Math.PI));
      ring = next.sort(compareId);
    }
    const size = 2 * radius + 120;
    components.push({ members, size });
    area += size * size;
    widest = Math.max(widest, size);
  }

  // Shelf-pack disconnected components with space around their outer ring.
  // Work in display coordinates; the database contributes no stored positions.
  const width = Math.max(widest, Math.sqrt(area));
  let x = 0, y = 0, rowHeight = 0, maxAbs = 0;
  const origin = (components[0]?.size ?? 0) / 2;
  for (const { members, size } of components) {
    if (x && x + size > width) { x = 0; y += rowHeight; rowHeight = 0; }
    for (const i of members) {
      const p = points[i];
      p.tx += x + size / 2 - origin;
      p.ty += y + size / 2 - origin;
      maxAbs = Math.max(maxAbs, Math.abs(p.tx), Math.abs(p.ty));
    }
    x += size;
    rowHeight = Math.max(rowHeight, size);
  }
  // Long chains can exceed the Wasm coordinate domain. One uniform scale
  // retains hop order and component separation without clipping outer rings.
  const scale = Math.min(1, 90000 / Math.max(1, maxAbs));
  for (const p of points) {
    p.tx *= scale; p.ty *= scale;
    if (!p.pinned) { p.x = p.tx; p.y = p.ty; }
  }
}

export function createLayout(nodes, edges, mode = 'force', previous = [], focusId = null) {
  if (nodes.length > LIMITS.nodes || edges.length > LIMITS.edges) throw new Error('Layout bounds exceeded.');
  if (!['force', 'clusters', 'radial', 'flow'].includes(mode)) throw new Error('Unknown layout mode.');
  const groups = [...new Set(nodes.map(node => node.group || 'Unlabeled'))].sort();
  const cached = new Map(previous.map(point => [point.id, point]));
  // A new result may add a vertex next to an already placed neighbor. Seed that
  // vertex near the surviving neighbors rather than throwing it across the
  // canvas. This only initializes coordinates; all movement remains in Wasm.
  const anchors = new Map(nodes.map(node => [node.id, { x: 0, y: 0, count: 0 }]));
  for (const edge of edges) for (const [id, neighbor] of [[edge.source, edge.target], [edge.target, edge.source]]) {
    const old = cached.get(neighbor), anchor = anchors.get(id);
    if (anchor && old && Number.isFinite(old.x) && Number.isFinite(old.y)) {
      anchor.x += old.x; anchor.y += old.y; anchor.count++;
    }
  }
  const points = nodes.map((node, i) => {
    const group = groups.indexOf(node.group || 'Unlabeled');
    const angle = 2 * Math.PI * group / Math.max(1, groups.length) - Math.PI / 2;
    const phase = (hash(node.id) % 6283) / 1000;
    const radius = 40 + Math.sqrt((hash(node.id + ':r') % 1000) / 1000) * Math.max(120, Math.sqrt(nodes.length) * 22);
    const old = cached.get(node.id);
    const anchor = anchors.get(node.id), seeded = anchor.count > 0;
    return { id: node.id, x: old?.x ?? (seeded ? anchor.x / anchor.count + Math.cos(phase) * 35 : Math.cos(phase) * radius), y: old?.y ?? (seeded ? anchor.y / anchor.count + Math.sin(phase) * 35 : Math.sin(phase) * radius),
      group, tx: Math.cos(angle) * 230, ty: Math.sin(angle) * 230,
      mass: 1 + Math.sqrt(node.degree || 0) * 0.25, radius: 12 + Math.min(10, Math.sqrt(node.degree || 0) * 2), pinned: old?.pinned || false };
  });
  const byId = new Map(points.map((point, index) => [point.id, index]));
  const links = edges.map(edge => [byId.get(edge.source), byId.get(edge.target)]);
  if (byId.size !== nodes.length || links.some(pair => pair.includes(undefined))) throw new Error('Layout needs unique nodes and complete endpoints.');
  if (mode === 'radial') radialTargets(points, links, focusId);
  return { points, links, groups, mode };
}

export function createSimulation(engine, nodes, edges, mode = 'force', previous = [], settings = DEFAULT_PHYSICS) {
  const state = createLayout(nodes, edges, mode, previous, settings.focusId);
  if (!engine.init(nodes.length, edges.length)) throw new Error('Wasm capacity admission failed.');
  for (let i = 0; i < state.points.length; i++) {
    const p = state.points[i];
    if (!engine.node(i, p.x, p.y, p.mass, p.radius, p.tx, p.ty)) throw new Error('Invalid layout coordinate.');
    if (p.pinned) engine.pin(i, p.x, p.y, 1);
  }
  state.links.forEach(([a, b], i) => {
    if (!engine.edge(i, a, b) || !engine.direction(i, Number(edges[i].directed === true))) throw new Error('Invalid layout edge.');
  });
  if (!engine.seal()) throw new Error('Incomplete layout input.');
  const configure = values => {
    const p = { ...DEFAULT_PHYSICS, ...values };
    if (!engine.configure(p.charge, p.distance, p.gravity, p.collision, mode === 'clusters' ? 0.025 : mode === 'radial' ? 0.055 : 0)
      || !engine.flow(mode === 'flow' ? 0.16 : 0)) throw new Error('Physics controls are outside their bounds.');
  };
  configure(settings);
  return { state, configure,
    step(count = 4) { const energy = engine.step(count); if (energy < 0 || !Number.isFinite(energy)) throw new Error('Wasm simulation refused a step.'); return energy; },
    positions() { return new Float32Array(engine.memory.buffer, engine.positions(), nodes.length * 2).slice(); },
    pin(id, x, y, fixed) { const i = state.points.findIndex(p => p.id === id); if (!engine.pin(i, x, y, Number(fixed))) throw new Error('Invalid pinned position.'); },
    reheat() { engine.reheat(); },
    stats() { return { ticks: engine.ticks(), temperature: engine.temperature(), visits: engine.visits() }; }
  };
}

if (typeof self !== 'undefined' && typeof document === 'undefined') {
  let generation = 0, timer = null, simulation = null, activeId = null, engine = null, loading = null, paused = false, revision = 0;
  const pending = new Map();
  async function load() {
    if (engine) return engine;
    if (!loading) loading = (async () => {
      const response = await fetch(new URL('./force.wasm', import.meta.url), { credentials: 'omit', redirect: 'error', cache: 'no-cache' });
      if (!response.ok) throw new Error(`Layout module returned HTTP ${response.status}.`);
      const { instance } = await WebAssembly.instantiateStreaming(response, {});
      engine = instance.exports; return engine;
    })().catch(error => { loading = null; throw error; });
    return loading;
  }
  function halt() { clearTimeout(timer); timer = null; }
  function advance(ticket) {
    if (ticket !== generation || paused || !simulation) return;
    try {
      const started = performance.now(), energy = simulation.step(4), stats = simulation.stats();
      const done = stats.ticks >= 800 || (stats.ticks > 100 && stats.temperature < 0.025 && energy < 0.002);
      const coordinates = simulation.positions();
      self.postMessage({ id: activeId, revision, coordinates, ...stats, energy, milliseconds: performance.now() - started, done, engine: 'wasm' }, [coordinates.buffer]);
      timer = done ? null : setTimeout(() => advance(ticket), 12);
    } catch (error) { halt(); self.postMessage({ id: activeId, revision, error: error.message, done: true }); }
  }
  function apply(data) {
    revision = data.revision ?? revision;
    if (data.action === 'pause') { paused = true; return; }
    if (data.action === 'pin') simulation.pin(data.node, data.x, data.y, data.fixed);
    if (data.action === 'configure') simulation.configure(data.settings);
    if (data.action === 'reheat') simulation.reheat();
    paused = false;
  }
  self.onmessage = async ({ data }) => {
    try {
      if (data.stop) { generation++; halt(); simulation = null; pending.clear(); return; }
      if (data.action) {
        if (data.id !== activeId) return;
        if (!simulation) {
          // Preserve user intent while the module is loading. Coalesce by
          // particle/action, bound retention, and replay in revision order.
          const key = data.action === 'pin' ? `pin:${data.node}` : data.action === 'configure' ? 'configure' : 'motion';
          if (!pending.has(key) && pending.size >= LIMITS.nodes + 2) throw new Error('Too many pending physics controls.');
          pending.set(key, data); return;
        }
        apply(data); halt(); advance(generation); return;
      }
      const ticket = ++generation; halt(); paused = false; simulation = null; activeId = data.id; revision = data.revision ?? 0; pending.clear();
      const instance = await load();
      if (ticket !== generation) return;
      simulation = createSimulation(instance, data.nodes, data.edges, data.mode, data.previous, data.settings);
      for (const action of [...pending.values()].sort((a, b) => a.revision - b.revision)) apply(action);
      pending.clear();
      advance(ticket);
    } catch (error) { if (data.id === activeId) { halt(); pending.clear(); self.postMessage({ id: data.id, revision: data.revision ?? revision, error: `WebAssembly physics unavailable: ${error.message}`, done: true }); } }
  };
}
