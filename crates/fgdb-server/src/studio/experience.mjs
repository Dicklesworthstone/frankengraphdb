// Result exploration only. Native parsing, snapshots and authorization stay on the server.
import { graphQuery, extractGraph, LIMITS } from './model.mjs';
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
  {id:'components',title:'Find connected components',family:'Prism',description:'Native graph analysis, not a layout-derived cluster guess.',query:()=> 'CALL fnx.connected_components()\nYIELD vertex,component'},
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
export function compareResults(before,after){
  const a=extractGraph(before),b=extractGraph(after),as=signatures(before),bs=signatures(after);
  const counts={nodes:{added:0,removed:0,changed:0},edges:{added:0,removed:0,changed:0}};
  const merge=(old,current,kind,prefix)=>{
    const prior=new Map(old.map(v=>[v.id,v])),now=new Set(current.map(v=>v.id));
    const output=current.map(value=>{const previous=prior.get(value.id);let change=null;
      if(!previous)change='added';else if((as.get(prefix+value.id)??'')!==(bs.get(prefix+value.id)??'')||kind==='edges'&&(previous.source!==value.source||previous.target!==value.target||previous.directed!==value.directed))change='changed';
      if(change)counts[kind][change]++;return {...value,change};});
    for(const value of old)if(!now.has(value.id)){counts[kind].removed++;output.push({...value,change:'removed'});}return output;
  };
  const allNodes=merge(a.nodes,b.nodes,'nodes','n:'),allEdges=merge(a.edges,b.edges,'edges','e:');
  const nodes=allNodes.slice(0,LIMITS.nodes),ids=new Set(nodes.map(n=>n.id));
  const edges=allEdges.filter(e=>ids.has(e.source)&&ids.has(e.target)).slice(0,LIMITS.edges);
  const warnings=[...a.warnings,...b.warnings];
  if(nodes.length!==allNodes.length||edges.length!==allEdges.length)warnings.push('Comparison canvas is capped; counts describe the two mapped samples, not the full database.');
  return {nodes,edges,warnings,counts};
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

// Overlay native PageRank/score rows on a separately retained topology result.
// Sequence equality is necessary, not a signed certificate or proof of completeness.
export function scoreOverlay(topology,result){
  if(topology.seq===undefined||result.seq===undefined||sequence(topology.seq)!==sequence(result.seq))throw new Error('An analytics overlay requires the same exact snapshot as the pinned topology.');
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
