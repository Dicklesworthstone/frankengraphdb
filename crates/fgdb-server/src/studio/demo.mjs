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
