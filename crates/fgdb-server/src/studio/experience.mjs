// Result exploration only. Native parsing, snapshots and authorization stay on the server.
import { graphQuery, extractGraph, LIMITS, PALETTE } from './model.mjs';
export function capabilityOrigin(origin){
  try{const url=new URL(origin);return url.protocol==='https:'||(url.protocol==='http:'&&['localhost','127.0.0.1','[::1]'].includes(url.hostname));}catch{return false;}
}
export function sequence(value) {
  const text=String(value).trim();
  if(!/^(0|[1-9]\d{0,19})$/.test(text)||BigInt(text)>(1n<<64n)-1n)throw new Error('Snapshot must be an exact unsigned 64-bit sequence.');
  return text;
}
export function temporalQuery(value, options={}) {
  const seq=sequence(value), text=graphQuery(options), split=text.indexOf('\nRETURN');
  // This is our own generated template, never a rewrite of an arbitrary query.
  return text.slice(0,split)+`\nFOR SYSTEM_TIME AS OF SEQ ${seq}`+text.slice(split);
}
export const RECIPES=Object.freeze([
  {id:'edges',title:'Explore relationships',family:'GQL',description:'Directed topology with returned labels and properties.',query:()=>graphQuery()},
  {id:'nodes',title:'Inspect vertices',family:'GQL',description:'Include isolated vertices; inspect exact identities.',query:()=>graphQuery({kind:'nodes'})},
  {id:'temporal',title:'Travel to a snapshot',family:'Chronicle',description:'Generate a native FOR SYSTEM_TIME query at an exact sequence.',query:()=>temporalQuery('1')},
  {id:'rank',title:'Find influential vertices',family:'Prism',description:'Run the native PageRank procedure. Server policy and projection admission apply.',query:()=> 'CALL fnx.pagerank(0.85,1000,1e-12,true)\nYIELD score AS rank,vertex AS id'},
  {id:'components',title:'Find connected components',family:'Prism',description:'Native graph analysis, not a layout-derived cluster guess.',query:()=> 'CALL fnx.connected_components(direction => \'undirected\')\nYIELD vertex, component\nRETURN vertex, component'},
  {id:'retrieve',title:'Retrieve a graph neighborhood',family:'Beacon',description:'Demand-driven graph retrieval with native candidate and work admission. Replace $source with a known ID.',query:()=> "CALL hybrid.search(seeds => [$source], max_hops => 2,\n     graph_access => 'demand', candidates => 50, k => 20)\nYIELD node, graph_hops, graph_rank\nRETURN node, graph_hops, graph_rank",parameters:'{"source":1}'}
]);
function canonical(value){if(Array.isArray(value))return '['+value.map(canonical).join(',')+']';if(value&&typeof value==='object')return '{'+Object.keys(value).sort().map(key=>JSON.stringify(key)+':'+canonical(value[key])).join(',')+'}';return JSON.stringify(value);}
function signatures(result){
  const index=new Map(result.columns.map((name,i)=>[name,i])), seen=new Map();
  const add=(key,value)=>{if(!seen.has(key))seen.set(key,new Set());seen.get(key).add(canonical(value));};
  for(const row of result.rows){const get=name=>row[index.get(name)];
    for(const prefix of ['node','source','target'])if(get(prefix)?.type==='vertex')add('n:'+get(prefix).value,[get(prefix+'_labels')??null,get(prefix+'_properties')??null]);
    if(get('relationship')?.type==='edge')add('e:'+get('relationship').value,[get('source')??null,get('target')??null,get('relationship_type')??null,get('relationship_properties')??null]);
  }
  return new Map([...seen].map(([key,values])=>[key,[...values].sort().join('\n')]));
}
// Track every distinct typed observation for the selected element. Joining rows
// can return the same identity several times or with different projections.
function propertyEvidence(result, kind, id) {
  const columns = new Map(result.columns.map((name, i) => [name, i]));
  const aliases = kind === 'edge' ? ['relationship'] : ['node', 'source', 'target'];
  const fields = new Map();
  let maps = 0;
  for (const row of result.rows) {
    for (const alias of aliases) {
      const entity = row[columns.get(alias)];
      if (entity?.type !== (kind === 'edge' ? 'edge' : 'vertex') || entity.value !== id) continue;
      const properties = row[columns.get(alias + '_properties')];
      if (properties?.type !== 'map') continue;
      maps++;
      for (const [name, cell] of Object.entries(properties.value)) {
        if (!fields.has(name)) fields.set(name, { present: 0, values: new Map() });
        const field = fields.get(name);
        field.present++;
        field.values.set(canonical(cell), cell);
      }
    }
  }
  return { maps, fields };
}

export function compareProperties(before, after, kind, id) {
  const a = propertyEvidence(before, kind, id), b = propertyEvidence(after, kind, id);
  const keys = new Set([...a.fields.keys(), ...b.fields.keys()]);
  const observation = (side, field) => {
    const values = field?.values ?? new Map();
    return {
      unprojected: side.maps === 0,
      absent: side.maps > (field?.present ?? 0),
      values: [...values.keys()].sort().map(key => values.get(key)),
    };
  };
  const differences = [];
  for (const name of [...keys].sort()) {
    const left = observation(a, a.fields.get(name)), right = observation(b, b.fields.get(name));
    if (canonical(left) !== canonical(right)) differences.push({ name, before: left, after: right });
  }
  return differences;
}

export function compareResults(before,after){
  const index = result => {
    const identities = { nodes: new Set(), edges: new Map() };
    const graph = extractGraph(result, identities);
    return { graph, identities, values: signatures(result) };
  };
  const a = index(before), b = index(after);
  const counts = { nodes: { added: 0, removed: 0, changed: 0 }, edges: { added: 0, removed: 0, changed: 0 } };
  function change(kind, id) {
    if (!a.identities[kind].has(id)) return 'added';
    if (!b.identities[kind].has(id)) return 'removed';
    const key = (kind === 'nodes' ? 'n:' : 'e:') + id;
    if (a.values.get(key) !== b.values.get(key)) return 'changed';
    if (kind === 'edges') {
      const old = a.identities.edges.get(id), next = b.identities.edges.get(id);
      if (old.directed !== next.directed) return 'changed';
      const same = old.source === next.source && old.target === next.target;
      const reversed = old.source === next.target && old.target === next.source;
      if (!same && (next.directed || !reversed)) return 'changed';
    }
    return null;
  }
  function compare(kind) {
    for (const id of b.identities[kind].keys()) {
      const status = change(kind, id);
      if (status) counts[kind][status]++;
    }
    for (const id of a.identities[kind].keys()) {
      if (!b.identities[kind].has(id)) counts[kind].removed++;
    }
    const current = b.graph[kind].map(item => ({ ...item, change: change(kind, item.id) }));
    for (const item of a.graph[kind]) {
      if (!b.identities[kind].has(item.id)) current.push({ ...item, change: 'removed' });
    }
    return current;
  }
  const allNodes = compare('nodes'), allEdges = compare('edges');
  const nodes = allNodes.slice(0, LIMITS.nodes), ids = new Set(nodes.map(node => node.id));
  const edges = allEdges.filter(edge => ids.has(edge.source) && ids.has(edge.target)).slice(0, LIMITS.edges);
  const warnings = [...a.graph.warnings, ...b.graph.warnings];
  if (nodes.length < b.identities.nodes.size + counts.nodes.removed ||
      edges.length < b.identities.edges.size + counts.edges.removed) {
    warnings.push('Comparison canvas is capped. Counts include mapped identities throughout both received results.');
  }
  return { nodes, edges, warnings, counts };
}
export function tracePath(graph,start,end,{directed=true}={}){
  const adjacency=new Map(graph.nodes.map(n=>[n.id,[]]));
  if(!adjacency.has(start)||!adjacency.has(end))return null;
  for(const e of graph.edges){adjacency.get(e.source)?.push([e.target,e.id]);if(!directed||!e.directed)adjacency.get(e.target)?.push([e.source,e.id]);}
  const queue=[start],previous=new Map([[start,null]]);
  for(let i=0;i<queue.length&&!previous.has(end);i++)for(const [next,edge]of adjacency.get(queue[i])||[])if(!previous.has(next)){previous.set(next,[queue[i],edge]);queue.push(next);}
  if(!previous.has(end))return null;const nodes=[end],edges=[];
  for(let at=end;at!==start;){const [prev,edge]=previous.get(at);nodes.push(prev);edges.push(edge);at=prev;}
  return {nodes:nodes.reverse(),edges:edges.reverse()};
}

// Sequence equality is necessary, not a signed certificate or proof of completeness.
function requireSameSnapshot(topology, result) {
  if (topology.seq === undefined || result.seq === undefined || sequence(topology.seq) !== sequence(result.seq)) {
    throw new Error('An analytics overlay requires the same exact snapshot as the pinned topology.');
  }
}

// Overlay native PageRank/score rows on a separately retained topology result.
export function scoreOverlay(topology,result){
  requireSameSnapshot(topology, result);
  const identity=result.columns.findIndex(name=>['vertex','id','node'].includes(name));
  const score=result.columns.findIndex(name=>['score','rank'].includes(name));
  if(identity<0||score<0)throw new Error('Return a vertex/id/node column and a float score/rank column.');
  const graph=extractGraph(topology);extractGraph(result);
  const values=new Map();
  for(const row of result.rows){const id=row[identity],cell=row[score];if(id.type!=='vertex'||cell.type!=='float'||typeof cell.value!=='string'||!/^[-+]?(?:\d+\.?\d*|\.\d+)(?:e[-+]?\d+)?$/i.test(cell.value)||!Number.isFinite(Number(cell.value)))throw new Error('Analytics overlays need exact vertex identities and finite float scores.');
    const previous=values.get(id.value);if(previous&&previous.text!==cell.value)throw new Error('A vertex has conflicting analytics scores.');values.set(id.value,{number:Number(cell.value),text:cell.value,type:cell.type,field:result.columns[score]});}
  const scores=[...values.values()].map(v=>v.number),min=Math.min(...scores),max=Math.max(...scores);let matched=0;
  const nodes=graph.nodes.map(n=>{const value=values.get(n.id);if(!value)return{...n,analysis:null};matched++;const normalized=max===min?.5:(value.number/Math.max(1,Math.abs(min),Math.abs(max))-min/Math.max(1,Math.abs(min),Math.abs(max)))/(max/Math.max(1,Math.abs(min),Math.abs(max))-min/Math.max(1,Math.abs(min),Math.abs(max)));return{...n,analysis:{...value,normalized}};});
  return {...graph,nodes,analytics:{field:result.columns[score],matched,unmatched:values.size-matched},warnings:[...graph.warnings,`${matched} returned scores mapped to pinned topology; ${values.size-matched} scored vertices are outside that topology sample.`]};
}

/** Map the native representative vertex for each member onto an observed topology. */
export function componentOverlay(topology, result) {
  requireSameSnapshot(topology, result);
  const member = result.columns.findIndex(name => ['vertex', 'id', 'node'].includes(name));
  const representative = result.columns.indexOf('component');
  if (member < 0 || representative < 0) throw new Error('Return a vertex and component column.');
  const graph = extractGraph(topology);
  extractGraph(result); // Checks result bounds and the entire u128 identity domain.
  const memberships = new Map();
  for (const row of result.rows) {
    const vertex = row[member], component = row[representative];
    if (vertex.type !== 'vertex' || component.type !== 'vertex') {
      throw new Error('Both the member and the component representative must be typed vertices.');
    }
    if (memberships.has(vertex.value) && memberships.get(vertex.value) !== component.value) {
      throw new Error('A vertex has conflicting component assignments.');
    }
    memberships.set(vertex.value, component.value);
  }

  const counts = new Map();
  let matched = 0;
  for (const node of graph.nodes) {
    const id = memberships.get(node.id);
    if (id !== undefined) {
      counts.set(id, (counts.get(id) ?? 0) + 1);
      matched++;
    }
  }
  // Numeric ordering of canonical unsigned decimal strings, without Number coercion.
  const ids = [...counts.keys()].sort((a, b) => a.length - b.length || (a < b ? -1 : a > b ? 1 : 0));
  const groups = ids.map((id, index) => ({ id, count: counts.get(id), color: PALETTE[index % PALETTE.length] }));
  const byId = new Map(groups.map(group => [group.id, group]));
  const nodes = graph.nodes.map(node => ({ ...node, component: byId.get(memberships.get(node.id)) ?? null }));
  const unmatched = memberships.size - matched;
  const warnings = [...graph.warnings, `Assigned vertices outside the pinned topology: ${unmatched}. Pinned vertices without a returned assignment: ${nodes.length - matched}.`];
  return { ...graph, nodes, warnings, analytics: { field: 'component', matched, unmatched, groups } };
}

export function focusComponent(graph, component) {
  if (!component) return graph;
  const nodes = graph.nodes.filter(node => node.component?.id === component);
  const ids = new Set(nodes.map(node => node.id));
  const edges = graph.edges.filter(edge => ids.has(edge.source) && ids.has(edge.target));
  return { ...graph, nodes, edges };
}

export function analyticsOverlay(topology, result) {
  const score = result.columns.findIndex(name => ['score', 'rank'].includes(name));
  const hasScores = score >= 0 && result.rows.every(row => row[score]?.type === 'float');
  if (hasScores || !result.columns.includes('component')) return scoreOverlay(topology, result);
  return componentOverlay(topology, result);
}
