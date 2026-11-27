import test from 'node:test';
import assert from 'node:assert/strict';
import { sequence,temporalQuery,compareResults,compareProperties,tracePath,scoreOverlay,analyticsOverlay,componentOverlay,focusComponent,RECIPES,capabilityOrigin } from '../experience.mjs';
import { atlasResult } from '../demo.mjs';
import { extractGraph,LIMITS } from '../model.mjs';
const c=(type,value)=>({type,value});
const node=(id,value='1',type='int')=>({v:1,seq:'18446744073709551615',columns:['node','node_properties'],rows:[[c('vertex',id),c('map',{balance:c(type,value)})]]});
test('temporal templates preserve the whole u64 domain and refuse injection',()=>{
 assert.equal(sequence('18446744073709551615'),'18446744073709551615');
 const query=temporalQuery('18446744073709551615',{label:'a`b'});
 assert.match(query,/MATCH \(source:`a``b`\)-\[relationship\]->\(target\)\nFOR SYSTEM_TIME AS OF SEQ 18446744073709551615\nRETURN/);
 for(const v of ['-1','1.5','1e2','18446744073709551616','0 RETURN 1','01',''])assert.throws(()=>sequence(v));
});
test('typed property differences do not collapse text, integers, decimals or null',()=>{
 const id='340282366920938463463374607431768211455';
 const a=node(id,'1','int');
 for(const type of ['text','decimal','null'])assert.equal(compareResults(a,node(id,'1',type)).counts.nodes.changed,1);
 assert.equal(compareResults(a,structuredClone(a)).counts.nodes.changed,0);
 const reordered=node(id);reordered.rows[0][1]=c('map',{other:c('int','2'),balance:c('int','1')});
 const reverse=structuredClone(reordered);reverse.rows[0][1].value={balance:c('int','1'),other:c('int','2')};
 assert.equal(compareResults(reordered,reverse).counts.nodes.changed,0);
});
test('illustrative snapshots retain identities and distinguish three change classes',()=>{
 const a=atlasResult(101),b=atlasResult(104),before=JSON.stringify(a),after=JSON.stringify(b),diff=compareResults(a,b);
 assert.deepEqual(diff.counts,{nodes:{added:3,removed:0,changed:1},edges:{added:6,removed:1,changed:0}});
 assert.equal(diff.edges.filter(e=>e.change==='removed').length,1);
 assert.equal(JSON.stringify(a),before);assert.equal(JSON.stringify(b),after);
});
test('comparison union obeys canvas caps without modifying either received result',()=>{
 const make=start=>({v:1,seq:1,columns:['node'],rows:Array.from({length:600},(_,i)=>[c('vertex',String(start+i))])});
 const a=make(0),b=make(600),d=compareResults(a,b);
 assert.equal(d.nodes.length,LIMITS.nodes);assert.equal(a.rows.length,600);assert.equal(b.rows.length,600);
 assert.match(d.warnings.join(' '),/capped/);assert.equal(d.counts.nodes.added,600);assert.equal(d.counts.nodes.removed,600);
});
test('returned-subgraph trace obeys known directions and does not invent unseen links',()=>{
 const g={nodes:['1','2','3','4'].map(id=>({id})),edges:[{id:'a',source:'1',target:'2',directed:true},{id:'b',source:'2',target:'3',directed:true},{id:'loop',source:'2',target:'2',directed:true}]};
 assert.deepEqual(tracePath(g,'1','3'),{nodes:['1','2','3'],edges:['a','b']});assert.equal(tracePath(g,'3','1'),null);assert.equal(tracePath(g,'1','4'),null);
 assert.deepEqual(tracePath(g,'2','2'),{nodes:['2'],edges:[]});assert.equal(tracePath(g,'1','999'),null);
 assert.deepEqual(tracePath(g,'3','1',{directed:false}).edges,['b','a']);
});
test('native score overlay requires exact matching snapshots and never invents topology',()=>{
 const id='340282366920938463463374607431768211455',topology=node(id);
 const scores={v:1,seq:topology.seq,columns:['vertex','score'],rows:[[c('vertex',id),c('float','0.25')],[c('vertex','2'),c('float','0.75')]]};
 const graph=scoreOverlay(topology,scores);assert.equal(graph.nodes.length,1);assert.equal(graph.edges.length,0);assert.equal(graph.nodes[0].analysis.text,'0.25');assert.deepEqual(graph.analytics,{field:'score',matched:1,unmatched:1});
 assert.throws(()=>scoreOverlay(topology,{...scores,seq:'18446744073709551614'}),/same exact snapshot/);
 for(const bad of ['',null,'NaN','Infinity','0x10']){const next=structuredClone(scores);next.rows[0][1].value=bad;assert.throws(()=>scoreOverlay(topology,next),/finite float/);}
});
test('score overlay normalization stays finite at extreme binary64 boundaries',()=>{
 const topology={v:1,seq:0,columns:['node'],rows:[[c('vertex','1')],[c('vertex','2')]]};
 const scores={v:1,seq:0,columns:['id','rank'],rows:[[c('vertex','1'),c('float','-1.7976931348623157e308')],[c('vertex','2'),c('float','1.7976931348623157e308')]]};
 assert.deepEqual(scoreOverlay(topology,scores).nodes.map(n=>n.analysis.normalized),[0,1]);
 scores.rows.push([c('vertex','1'),c('float','0')]);assert.throws(()=>scoreOverlay(topology,scores),/conflicting/);
});
test('query lenses are explicit read templates and sample generation is bounded',()=>{
 assert.equal(RECIPES.length,6);for(const r of RECIPES)assert.ok(r.query().length<2000);
 assert.match(RECIPES.find(r=>r.id==='rank').query(),/CALL fnx.pagerank/);
 assert.match(RECIPES.find(r=>r.id==='retrieve').query(),/graph_access => 'demand'/);
 assert.throws(()=>atlasResult(105));for(const cut of [101,102,103,104])assert.ok(extractGraph(atlasResult(cut)).nodes.length<=48);
});

test('capabilities are admitted only to HTTPS or explicit HTTP loopback origins',()=>{
 for(const origin of ['https://graph.example','http://127.0.0.1:8080','http://localhost:8000','http://[::1]:8080'])assert.equal(capabilityOrigin(origin),true);
 for(const origin of ['http://graph.example','http://127.0.0.1.evil.example','ftp://localhost','file:///studio/','null',''])assert.equal(capabilityOrigin(origin),false);
});

test('component overlays preserve native representatives, pinned topology and exact ids', () => {
  const a = '340282366920938463463374607431768211454', b = '340282366920938463463374607431768211455';
  const edge = (src, id, dst) => [c('vertex', src), c('edge', id), c('vertex', dst)];
  const topology = { v:1, seq:7, columns:['source','relationship','target'], rows:[edge(a,'100','9'), edge('9','101',b), edge(b,'102','33')] };
  const assign = (vertex, component) => [c('vertex', vertex), c('vertex', component)];
  const result = { v:1, seq:'7', columns:['vertex','component'], rows:[assign(a,a), assign('9',a), assign(b,b), assign('77',b), assign(b,b)] };
  const original = JSON.stringify([topology,result]);
  const graph = componentOverlay(topology, result);
  assert.equal(graph.nodes.length, 4);
  assert.equal(graph.edges.length, 3);
  assert.equal(graph.analytics.matched, 3);
  assert.equal(graph.analytics.unmatched, 1);
  assert.deepEqual(graph.analytics.groups.map(g => [g.id, g.count]), [[a,2], [b,1]]);
  assert.notEqual(graph.analytics.groups[0].color, graph.analytics.groups[1].color);
  assert.equal(graph.nodes.find(n => n.id === '33').component, null);
  assert.deepEqual(focusComponent(graph, a).edges.map(e => e.id), ['100']);
  assert.deepEqual(focusComponent(graph, b).nodes.map(n => n.id), [b]);
  assert.equal(focusComponent(graph, ''), graph);
  assert.equal(JSON.stringify([topology,result]), original);
  assert.throws(() => componentOverlay(topology, {...result, seq:8}), /same exact snapshot/);
  assert.throws(() => componentOverlay(topology, {...result, rows:[...result.rows, assign(a,b)]}), /conflicting/);
  assert.throws(() => componentOverlay(topology, {...result, rows:[[c('vertex',a),c('int',b)]]}), /typed vertices/);
  assert.throws(() => componentOverlay(topology, {...result, rows:[assign(a,'340282366920938463463374607431768211456')]}), /identity/);
});

test('additional result columns preserve compatible score and component overlays', () => {
  const topology = node('1');
  const result = { v:1, seq:topology.seq, columns:['vertex','score','component'],
    rows:[[c('vertex','1'),c('float','0.125'),c('text','unrelated')]] };
  assert.equal(analyticsOverlay(topology, result).analytics.field, 'score');
  result.rows[0][2] = c('vertex','2');
  assert.equal(analyticsOverlay(topology, result).analytics.field, 'score');
  result.rows[0][1] = c('int','12');
  assert.equal(analyticsOverlay(topology, result).analytics.field, 'component');
});

test('reordering rows across the canvas boundary cannot create snapshot changes', () => {
  const rows = Array.from({ length: LIMITS.nodes + 2 }, (_, i) =>
    [c('vertex', String(i)), c('map', { name: c('text', String(i)) })]);
  const a = { v: 1, seq: 1, columns: ['node', 'node_properties'], rows };
  const b = { ...a, seq: 2, rows: [...rows].reverse() };
  const reordered = compareResults(a, b);
  assert.deepEqual(reordered.counts.nodes, { added: 0, removed: 0, changed: 0 });
  assert.ok(reordered.nodes.every(n => n.change === null));
  // The changed value lies outside the second canvas but inside the response.
  b.rows[b.rows.length - 1] = [c('vertex', '0'), c('map', { name: c('text', 'changed') })];
  assert.deepEqual(compareResults(a, b).counts.nodes, { added: 0, removed: 0, changed: 1 });
});
test('parallel edges across the draw limit compare using their returned endpoints', () => {
  const path = (id, nodes) => [c('path', { nodes, edges: [String(id)] })];
  const rows = Array.from({ length: LIMITS.edges + 1 }, (_, i) => path(i, ['1', '2']));
  const a = { v: 1, seq: 1, columns: ['route'], rows };
  const b = { ...a, seq: 2, rows: rows.map((_, i) => path(i, ['2', '1'])).reverse() };
  assert.deepEqual(compareResults(a, b).counts.edges, { added: 0, removed: 0, changed: 0 });
  b.rows.pop();
  b.rows[0] = path(LIMITS.edges, ['2', '3']);
  b.rows.push(path(LIMITS.edges + 1, ['3', '3']));
  assert.deepEqual(compareResults(a, b).counts.edges, { added: 1, removed: 1, changed: 1 });
});

test('inspected property differences use all returned maps and preserve typed evidence', () => {
  const id = '340282366920938463463374607431768211455';
  const result = (...maps) => ({ v:1, seq:1, columns:['source','source_properties'],
    rows: maps.map(map => [c('vertex',id),c('map',map)]) });
  const a = result({ v:c('int','9007199254740993'), removed:c('null') }, { v:c('int','9007199254740993') });
  const b = result({ v:c('text','9007199254740993'), added:c('list',[c('int','1')]) });
  const changes = compareProperties(a,b,'node',id);
  assert.deepEqual(changes.map(d => d.name), ['added','removed','v']);
  assert.deepEqual(changes[0].before,{unprojected:false,absent:true,values:[]});
  assert.deepEqual(changes[1].before,{unprojected:false,absent:true,values:[c('null')]});
  assert.deepEqual(changes[2].before.values,[c('int','9007199254740993')]);
  assert.deepEqual(changes[2].after.values,[c('text','9007199254740993')]);
  assert.deepEqual(compareProperties(a,{...a,rows:[...a.rows].reverse()},'node',id),[]);
  const unprojected = {v:1,seq:2,columns:['node'],rows:[[c('vertex',id)]]};
  assert.deepEqual(compareProperties(b,unprojected,'node',id)[0].after,{unprojected:true,absent:false,values:[]});
  const mixed = {v:1,seq:1,columns:['relationship','relationship_properties','node','node_properties'],
    rows:[[c('edge',id),c('map',{x:c('int','1')}),c('vertex',id),c('map',{x:c('int','2')})]]};
  assert.deepEqual(compareProperties(mixed,unprojected,'edge',id)[0].before.values,[c('int','1')]);
});

test('structured display-name properties remain inspectable across snapshots', () => {
  const a = node('3'), b = node('3');
  a.rows[0][1] = c('map', { name:c('map',{ part:c('text','A') }), title:c('text','Library') });
  b.rows[0][1] = c('map', { name:c('list',[c('map',{ part:c('text','B') })]) });
  const diff = compareResults(a,b);
  assert.equal(extractGraph(a).nodes[0].title,'Library');
  assert.equal(diff.nodes[0].title,'Vertex 3');
  assert.equal(diff.nodes[0].change,'changed');
  assert.deepEqual(compareProperties(a,b,'node','3').map(d => d.name),['name','title']);
});
