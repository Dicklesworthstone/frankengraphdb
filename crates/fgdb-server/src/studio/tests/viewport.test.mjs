import test from 'node:test';
import assert from 'node:assert/strict';
import { GraphCanvas } from '../graph.mjs';

test('fit and anchored zoom cover the complete extent of long radial layouts', () => {
  const canvas = Object.create(GraphCanvas.prototype);
  const nodes = Array.from({ length:750 }, (_, i) => ({ id:String(i) }));
  Object.assign(canvas, {
    width:800, height:500, graph:{ nodes },
    points:new Map(nodes.map((n,i) => [n.id,{ x:0, y:-100*i }])),
    drawSoon() {}
  });
  canvas.fit();
  const fit = { ...canvas.view };
  assert.ok(fit.k > 0 && fit.k < 0.08);
  for (const p of canvas.points.values()) {
    assert.ok(p.x*fit.k+fit.x >= 0 && p.x*fit.k+fit.x <= canvas.width);
    assert.ok(p.y*fit.k+fit.y >= 0 && p.y*fit.k+fit.y <= canvas.height);
  }
  const cursor = { x:320, y:120 }, anchor = canvas.world(cursor);
  canvas.zoomAt(cursor.x,cursor.y,1.2);
  assert.equal(canvas.view.k,fit.k*1.2);
  assert.ok(Math.abs(canvas.world(cursor).y-anchor.y) < 1e-8);
  canvas.zoomAt(cursor.x,cursor.y,0.01);
  assert.equal(canvas.view.k,fit.k);
  canvas.zoomAt(cursor.x,cursor.y,NaN);
  assert.equal(canvas.view.k,fit.k);
});
