// Explicit sample fixture, never passed off as a query or a live database.
const cell = (type, value) => value === undefined ? { type } : { type, value };
const text = value => cell('text', value);
const props = values => cell('map', Object.fromEntries(Object.entries(values).map(([key, value]) => [key, text(String(value))])));
const names = {
  Person: ['Ada Chen', 'Marcus Lee', 'Sofia Patel', 'James Okafor', 'Elena Rossi', 'Noah Park', 'Maya Singh', 'Leo Martin'],
  Project: ['Atlas', 'Open Research', 'Graph memory', 'Orion', 'Discovery engine', 'Common ground'],
  Paper: ['Attention is all you need', 'Graph representation learning', 'Retrieval-augmented generation', 'The knowledge graph', 'A survey of graph networks', 'Language models as agents', 'Learning to retrieve', 'Reasoning over connections', 'Structured memory', 'Temporal graph learning'],
  Concept: ['Knowledge graphs', 'Machine learning', 'Information retrieval', 'Agent memory', 'Graph algorithms', 'Embeddings', 'Natural language', 'Distributed systems'],
};
export function demoResult() {
  const groups = {}, nodes = [];
  let serial = 0;
  for (const [label, titles] of Object.entries(names)) {
    groups[label] = titles.map((name, i) => {
      const node = { id: String(++serial), label, name, properties: { name, description: label === 'Concept' ? 'A connected idea in the research workspace.' : `Sample ${label.toLowerCase()} in the research workspace.`, workspace: 'Research atlas', ...(label === 'Person' ? { role: ['Research scientist', 'Systems engineer', 'Knowledge architect'][i % 3] } : {}) } };
      nodes.push(node); return node;
    });
  }
  const relations = [];
  const link = (a, b, type) => relations.push({ a, b, type, id: String(1000 + relations.length) });
  groups.Person.forEach((n, i) => { link(n, groups.Project[i % 6], 'CONTRIBUTES_TO'); link(n, groups.Paper[i % 10], 'AUTHORED'); link(n, groups.Concept[i % 8], 'WORKS_ON'); });
  groups.Paper.forEach((n, i) => { link(n, groups.Concept[i % 8], 'EXPLORES'); link(n, groups.Concept[(i + 3) % 8], 'EXPLORES'); });
  groups.Project.forEach((n, i) => { link(n, groups.Concept[i % 8], 'APPLIES'); link(n, groups.Paper[(i + 2) % 10], 'REFERENCES'); });
  link(groups.Concept[0], groups.Concept[4], 'RELATED_TO');
  link(groups.Concept[0], groups.Concept[3], 'RELATED_TO');
  link(groups.Concept[0], groups.Concept[5], 'RELATED_TO');
  const columns = ['source', 'relationship', 'target', 'source_labels', 'source_properties', 'relationship_type', 'relationship_properties', 'target_labels', 'target_properties'];
  return { v: 1, columns, rows: relations.map(({ a, b, type, id }) => [
    cell('vertex', a.id), cell('edge', id), cell('vertex', b.id), cell('list', [text(a.label)]), props(a.properties), text(type), props({ source: 'Studio sample' }), cell('list', [text(b.label)]), props(b.properties),
  ]), seq: 0 };
}

// Four explicitly illustrative Chronicle cuts. No query is evaluated here.
export function atlasResult(cut=104){
  if(![101,102,103,104].includes(cut))throw new Error('Unknown sample snapshot.');
  const nodes=[],edges=[];let eid=5000;
  const add=(id,name,label,extra={})=>{const value={id:String(id),name,label,properties:{name,provenance:'Illustrative Studio fixture',...extra}};nodes.push(value);return value;};
  const link=(a,b,type,born=101,retired=Infinity)=>{const id=String(eid++);if(cut>=born&&cut<retired)edges.push({a,b,type,id});};
  const subjects=['GraphRAG','Agent memory','Hybrid retrieval','Data provenance','Temporal reasoning'];
  const details=[['Entity resolution','Knowledge fusion','Grounded answers','Relation extraction','Graph embeddings','Context assembly'],['Episodic memory','Working memory','Long-term context','Memory consolidation','Shared knowledge','Agent collaboration'],['Vector similarity','Lexical search','Reciprocal rank fusion','Neighborhood search','Semantic retrieval','Evidence ranking'],['Source attribution','Trust boundaries','Evidence chains','Reproducible results','Capability scopes','Verified sources'],['Immutable snapshots','Temporal joins','Change detection','Historical context','Versioned knowledge','Causal history']];
  const hubs=subjects.map((name,i)=>add(100+i,name,'Concept',{description:'A connected research theme in the sample atlas.',confidence:cut>=103&&i===0?'0.97':'0.91'}));
  const agents=['Atlas researcher','Memory curator','Retrieval agent','Evidence auditor','History analyst'].map((name,i)=>add(200+i,name,'Agent',{role:'Research agent',scope:'Sample workspace'}));
  for(let i=0;i<5;i++){
    const source=add(300+i,['Graph retrieval survey','Agent memory review','Hybrid search notes','Provenance handbook','Temporal graph study'][i],'Source',{origin:'Sample research library',license:'Illustrative data'});
    link(agents[i],hubs[i],'INVESTIGATES');link(source,hubs[i],'SUPPORTS');link(agents[i],source,'CITES');
    details[i].forEach((name,j)=>{const node=add(1000+i*10+j,name,j%3===0?'Evidence':'Concept',{description:`Evidence and ideas connected to ${subjects[i]}.`,confidence:j%2?'0.88':'0.95'});link(hubs[i],node,'RELATED_TO');if(j<3)link(source,node,'SUPPORTS');if(j===1)link(agents[i],node,'CURATES',101,i===1?104:Infinity);});
    link(hubs[i],hubs[(i+1)%5],'INFORMS');
  }
  const changes=[{cut:102,id:400,name:'Retrieval evaluation',hub:2},{cut:103,id:401,name:'Evidence synthesis',hub:0},{cut:104,id:402,name:'Snapshot comparison',hub:4}];
  for(const change of changes){const value={id:String(change.id),name:change.name,label:'Evidence',properties:{name:change.name,provenance:'Illustrative Studio fixture',introduced_at:String(change.cut)}};if(cut>=change.cut)nodes.push(value);link(hubs[change.hub],value,'DERIVED_FROM',change.cut);link(value,hubs[(change.hub+1)%5],'INFORMS',change.cut);}
  const columns=['source','relationship','target','source_labels','source_properties','relationship_type','relationship_properties','target_labels','target_properties'];
  return {v:1,seq:cut,columns,rows:edges.map(({a,b,type,id})=>[cell('vertex',a.id),cell('edge',id),cell('vertex',b.id),cell('list',[text(a.label)]),props(a.properties),text(type),props({source:'Illustrative Studio fixture'}),cell('list',[text(b.label)]),props(b.properties)])};
}
