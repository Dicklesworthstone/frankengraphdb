import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createSimulation, createLayout } from '../layout.mjs';
const bytes = await readFile(new URL('../wasm/force.wasm', import.meta.url));
const module = await WebAssembly.compile(bytes);
const fresh = () => new WebAssembly.Instance(module, {}).exports;
const node = (id, degree = 0) => ({ id: String(id), group: 'A', degree });
function sim(nodes, edges = [], previous = [], physics = {}) { const engine = fresh(); return { engine, sim: createSimulation(engine, nodes, edges, 'force', previous, physics) }; }
function distance(p) { return Math.hypot(p[0]-p[2], p[1]-p[3]); }
test('real wasm has no imports and cannot grow beyond its fixed 4 MiB', () => {
  assert.equal(WebAssembly.validate(bytes), true); assert.deepEqual(WebAssembly.Module.imports(module), []);
  const e = fresh(); assert.equal(e.memory.buffer.byteLength, 4194304); assert.throws(() => e.memory.grow(1));
});
test('all input boundaries refuse before a partial graph can execute', () => {
  const e = fresh(); assert.equal(e.init(4097, 0), 0); assert.equal(e.init(2, 1), 1);
  assert.equal(e.step(1), -1); assert.equal(e.edge(0, 0, 2), 0); assert.equal(e.seal(), 0);
  assert.equal(e.node(0, NaN, 0, 1, 8, 0, 0), 0); assert.equal(e.node(-1, 0, 0, 1, 8, 0, 0), 0);
  assert.equal(e.configure(Infinity, 100, 0, 1, 0), 0); assert.equal(e.pin(0, Infinity, 0, 1), 0);
  assert.equal(e.edge(0, 0, 1), 1); assert.equal(e.seal(), 1); assert.equal(e.step(9), -1);
});
test('electrostatic repulsion separates particles; springs attract distant neighbors', () => {
  const near = [{id:'0', x:-1,y:0},{id:'1',x:1,y:0}];
  const a = sim([node(0),node(1)], [], near, {charge:200, gravity:0, collision:0});
  a.sim.step(8); assert.ok(distance(a.sim.positions()) > 2);
  const far = [{id:'0',x:-200,y:0},{id:'1',x:200,y:0}];
  const b = sim([node(0),node(1)], [{source:'0',target:'1'}], far, {charge:0,gravity:0,collision:0});
  for(let i=0;i<20;i++)b.sim.step(8); assert.ok(distance(b.sim.positions()) < 150);
});
test('collision force operates independently of electrostatic charge', () => {
  const {sim:s} = sim([node(0),node(1)],[],[{id:'0',x:0,y:0},{id:'1',x:0,y:0}],{charge:0,gravity:0,collision:1});
  for(let i=0;i<20;i++)s.step(8); assert.ok(distance(s.positions()) >= 20);
});
test('pinned particle stays exact while its neighbors respond and release resumes motion', () => {
  const {sim:s} = sim([node(0),node(1)], [{source:'0',target:'1'}]); s.pin('0', 321,-123,true);
  for(let i=0;i<20;i++)s.step(8); assert.deepEqual([...s.positions()].slice(0,2), [321,-123]);
  s.pin('0',321,-123,false); s.step(8); assert.notEqual(s.positions()[0],321);
});
test('same graph, settings and prior positions produce identical Wasm coordinates', () => {
  const nodes = Array.from({length:80},(_,i)=>node(i,3)); const edges = nodes.slice(1).map((_,i)=>({source:String(i),target:String(i+1)}));
  const a = sim(nodes,edges), b = sim(nodes,edges);
  for(let i=0;i<100;i++){a.sim.step(8);b.sim.step(8);}
  assert.deepEqual(a.sim.positions(),b.sim.positions()); assert.equal(a.engine.ticks(),800);
  const before = a.sim.positions(); a.sim.step(8); assert.deepEqual(a.sim.positions(),before);
});
test('Barnes-Hut reduces actual visit count below an all-pairs pass on 750 spread nodes', () => {
  const nodes=Array.from({length:750},(_,i)=>node(i)); const {sim:s,engine:e}=sim(nodes);
  s.step(1); assert.ok(e.visits() < 750*750/2, String(e.visits()));
  assert.ok([...s.positions()].every(Number.isFinite));
});
test('coincident inputs, parallel links and self-loops remain finite', () => {
  const nodes=Array.from({length:200},(_,i)=>node(i));
  const edges=nodes.flatMap((v,i)=>[{source:v.id,target:v.id},{source:v.id,target:String((i+1)%200)},{source:v.id,target:String((i+1)%200)}]);
  const {sim:s}=sim(nodes,edges,nodes.map(v=>({...v,x:0,y:0})));
  for(let i=0;i<20;i++)s.step(8);assert.ok([...s.positions()].every(Number.isFinite));
});
test('initial placement preserves exact string identities and prior coordinates', () => {
  const id='340282366920938463463374607431768211455';
  const layout=createLayout([node(id)], [],'force',[{id,x:22,y:-4,pinned:true}]);
  assert.equal(layout.points[0].id,id);assert.equal(layout.points[0].x,22);assert.equal(layout.points[0].pinned,true);
  assert.throws(()=>createLayout([node(1),node(1)],[]));assert.throws(()=>createLayout([node(1)],[{source:'1',target:'2'}]));
});
test('empty graph settles, and changing controls reheats a settled graph', () => {
  const empty=sim([]);assert.equal(empty.sim.step(),0);
  const a=sim([node(1)]);for(let i=0;i<100;i++)a.sim.step(8);
  a.sim.configure({distance:200});assert.equal(a.engine.ticks(),0);assert.equal(a.engine.temperature(),1);
});
