import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createLayout, createSimulation } from '../layout.mjs';
const binary = readFileSync(new URL('../wasm/force.wasm', import.meta.url));
async function engine() { return (await WebAssembly.instantiate(binary)).instance.exports; }
const nodes = ['a', 'b', 'c', 'd'].map(id => ({ id, group: 'Evidence', degree: 2 }));
const links = nodes.slice(1).map((n, i) => ({ source: nodes[i].id, target: n.id, directed: true }));
const start = nodes.map((n, i) => ({ id: n.id, x: 150 - i * 100, y: i % 2 * 50 }));
function settle(sim) { for (let i = 0; i < 200; i++) sim.step(4); return [...sim.positions()]; }

test('actual Wasm flow reverses a backwards chain using stored direction', async () => {
  const sim = createSimulation(await engine(), nodes, links, 'flow', start);
  const p = settle(sim);
  for (let i = 1; i < nodes.length; i++) assert.ok(p[i * 2] - p[(i - 1) * 2] > 50, `${p}`);
  assert.equal(sim.stats().ticks, 800);
});
test('unknown path orientation supplies no flow force', async () => {
  const undirected = links.map(e => ({ ...e, directed: false }));
  assert.deepEqual(settle(createSimulation(await engine(), nodes, undirected, 'flow', start)),
    settle(createSimulation(await engine(), nodes, undirected, 'force', start)));
});
test('cycles, coincident nodes, parallel and self edges remain finite and deterministic', async () => {
  const edges = [...links, links[0], { source: 'd', target: 'a', directed: true }, { source: 'a', target: 'a', directed: true }];
  const positions = nodes.map(n => ({ id: n.id, x: 0, y: 0 }));
  const a = settle(createSimulation(await engine(), nodes, edges, 'flow', positions));
  const b = settle(createSimulation(await engine(), nodes, edges, 'flow', positions));
  assert.deepEqual(a, b); assert.ok(a.every(Number.isFinite));
});
test('flow pins stay fixed while connected particles respond', async () => {
  const sim = createSimulation(await engine(), nodes, links, 'flow', start);
  settle(sim); const before = [...sim.positions()]; sim.pin('a', -300, 120, true);
  const after = settle(sim); assert.deepEqual(after.slice(0, 2), [-300, 120]);
  assert.ok(after.slice(2).some((n, i) => Math.abs(n - before[i + 2]) > 5));
});
test('flow exports refuse nonfinite, out-of-range and invalid directions', async () => {
  const e = await engine(); e.init(2, 1); e.edge(0, 0, 1);
  for (const v of [-1, .31, NaN, Infinity]) assert.equal(e.flow(v), 0);
  assert.equal(e.direction(-1, 1), 0); assert.equal(e.direction(1, 1), 0); assert.equal(e.direction(0, 2), 0);
  assert.equal(e.flow(.16), 1); assert.equal(e.direction(0, 1), 1);
  assert.equal(e.memory.buffer.byteLength, 4 * 1024 * 1024);
  assert.deepEqual(WebAssembly.Module.imports(new WebAssembly.Module(binary)), []);
});
test('new vertices warm-start near surviving neighbors without moving the survivors', () => {
  const previous = [{ id: 'a', x: 4000, y: -500, pinned: true }];
  const layout = createLayout(nodes, links, 'force', previous);
  assert.equal(layout.points[0].x, 4000); assert.equal(layout.points[0].y, -500);
  assert.equal(layout.points[0].pinned, true);
  assert.ok(Math.hypot(layout.points[1].x - 4000, layout.points[1].y + 500) <= 36);
  assert.ok(Math.abs(layout.points[3].x) < 500); // Not an invented multi-hop placement.
});
test('reusing one Wasm instance resets directional force on the next graph', async () => {
  const e = await engine(); settle(createSimulation(e, nodes, links, 'flow', start));
  const reused = settle(createSimulation(e, nodes, links, 'force', start));
  assert.deepEqual(reused, settle(createSimulation(await engine(), nodes, links, 'force', start)));
});
