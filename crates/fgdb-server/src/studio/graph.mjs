import { PALETTE, colorIndex } from './model.mjs';
import { createLayout } from './layout.mjs';

const clip = (text, n = 25) => text.length > n ? text.slice(0, n - 1) + '…' : text;
const distance = (point, a, b) => {
  const dx = b.x - a.x, dy = b.y - a.y;
  const t = Math.max(0, Math.min(1, ((point.x - a.x) * dx + (point.y - a.y) * dy) / (dx * dx + dy * dy || 1)));
  return Math.hypot(point.x - a.x - t * dx, point.y - a.y - t * dy);
};

export class GraphCanvas {
  constructor(canvas, minimap, { onSelect, onBusy, onError }) {
    this.canvas = canvas; this.minimap = minimap;
    this.onSelect = onSelect; this.onBusy = onBusy; this.onError = onError;
    this.graph = { nodes: [], edges: [] }; this.points = new Map();
    this.view = { x: 0, y: 0, k: 1 }; this.id = 0; this.selection = null;
    this.labels = true; this.focus = false; this.hover = null; this.frame = null;
    this.mode = 'force'; this.matches = null; this.pointers = new Map();
    this.reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
    try {
      this.worker = new Worker(new URL('./layout.mjs', import.meta.url), { type: 'module' });
      this.worker.onmessage = ({ data }) => {
        if (data.id !== this.id) return;
        if (!this.reduced || data.done) {
          for (const point of data.points) this.points.set(point.id, { ...point, x: point.x * this.stretch });
          if (!this.interacted) this.fit(false);
          this.drawSoon();
        }
        if (data.done) { this.running = false; this.onBusy(false); }
      };
      this.worker.onerror = () => { this.running = false; this.onBusy(false); this.onError('Layout worker is unavailable. The graph remains interactive in its initial layout.'); };
    } catch { this.onError('Layout worker is unavailable. The graph remains interactive in its initial layout.'); }
    this.observer = new ResizeObserver(() => this.resize()); this.observer.observe(canvas);
    canvas.addEventListener('wheel', event => {
      event.preventDefault(); const p = this.local(event); this.interacted = true;
      this.zoomAt(p.x, p.y, Math.exp(-Math.max(-100, Math.min(100, event.deltaY)) * 0.008));
    }, { passive: false });
    canvas.addEventListener('pointerdown', event => this.pointerDown(event));
    canvas.addEventListener('pointermove', event => this.pointerMove(event));
    canvas.addEventListener('pointerup', event => this.pointerUp(event));
    canvas.addEventListener('pointercancel', event => { this.pointers.delete(event.pointerId); this.drag = null; this.pinch = null; });
    canvas.addEventListener('pointerleave', () => { if (!this.drag) { this.hover = null; this.drawSoon(); } });
    canvas.addEventListener('dblclick', event => {
      const p = this.local(event); this.interacted = true; this.zoomAt(p.x, p.y, 1.5);
    });
    canvas.addEventListener('keydown', event => {
      if (event.key === '+' || event.key === '=') { event.preventDefault(); this.zoom(1.2); }
      if (event.key === '-') { event.preventDefault(); this.zoom(1 / 1.2); }
      if (event.key.toLowerCase() === 'f') { event.preventDefault(); this.fit(); }
      if (event.key === 'Escape') this.select(null);
      const shift = { ArrowLeft: [35, 0], ArrowRight: [-35, 0], ArrowUp: [0, 35], ArrowDown: [0, -35] }[event.key];
      if (shift) { event.preventDefault(); this.view.x += shift[0]; this.view.y += shift[1]; this.interacted = true; this.drawSoon(); }
    });
    minimap.addEventListener('pointerdown', event => {
      if (!this.miniTransform) return;
      const r = minimap.getBoundingClientRect(), t = this.miniTransform;
      const x = ((event.clientX - r.left) * 160 / r.width - t.x) / t.k;
      const y = ((event.clientY - r.top) * 92 / r.height - t.y) / t.k;
      this.view.x = this.width / 2 - x * this.view.k; this.view.y = this.height / 2 - y * this.view.k;
      this.interacted = true; this.drawSoon();
    });
  }
  local(event) { const r = this.canvas.getBoundingClientRect(); return { x: event.clientX - r.left, y: event.clientY - r.top }; }
  world(point) { return { x: (point.x - this.view.x) / this.view.k, y: (point.y - this.view.y) / this.view.k }; }
  resize() {
    const { width, height } = this.canvas.getBoundingClientRect();
    if (!width || !height) return;
    this.width = width; this.height = height;
    this.ratio = Math.min(devicePixelRatio || 1, 2);
    this.canvas.width = Math.round(width * this.ratio); this.canvas.height = Math.round(height * this.ratio);
    if (!this.interacted) this.fit(false);
    this.drawSoon();
  }
  stop() { this.id++; this.worker?.postMessage({ stop: true }); this.running = false; this.onBusy(false); }
  setData(graph) {
    this.stop(); this.graph = graph; this.selection = null; this.hover = null; this.interacted = false;
    this.points = new Map(createLayout(graph.nodes, graph.edges).points.map(point => [point.id, point]));
    this.prepareEdges(); this.fit(false); this.arrange(this.mode); this.drawSoon();
  }
  setVisible(graph) { this.graph = graph; this.matches = graph.matches; this.prepareEdges(); this.drawSoon(); }
  prepareEdges() {
    const pairs = new Map();
    for (const edge of this.graph.edges) {
      const key = [edge.source, edge.target].sort().join(':');
      if (!pairs.has(key)) pairs.set(key, []);
      pairs.get(key).push(edge);
    }
    this.lanes = new Map();
    for (const edges of pairs.values()) edges.forEach((edge, i) => this.lanes.set(edge.id, (i - (edges.length - 1) / 2) * 24));
  }
  arrange(mode = this.mode) {
    this.mode = mode; this.stop();
    if (!this.graph.nodes.length || !this.worker) return;
    // Use the available viewport rather than squeezing a square layout into
    // a wide workbench. This changes presentation, never graph coordinates
    // returned by a query or the bounded deterministic worker's state.
    const bounds = this.canvas.getBoundingClientRect();
    this.stretch = mode === 'radial' ? 1 : Math.max(1, Math.min(2.4, bounds.width / Math.max(1, bounds.height) * 0.9));
    this.interacted = false; this.running = true; this.onBusy(true);
    this.worker.postMessage({ id: this.id, nodes: this.graph.nodes.map(({ id, group }) => ({ id, group })), edges: this.graph.edges.map(({ source, target }) => ({ source, target })), mode });
  }
  fit(interacted = true) {
    if (!this.width || !this.height) return;
    const points = this.graph.nodes.map(node => this.points.get(node.id)).filter(Boolean);
    if (!points.length) { this.view = { x: this.width / 2, y: this.height / 2, k: 1 }; this.drawSoon(); return; }
    const xs = points.map(p => p.x), ys = points.map(p => p.y);
    const left = Math.min(...xs) - 60, right = Math.max(...xs) + 60, top = Math.min(...ys) - 45, bottom = Math.max(...ys) + 60;
    const k = Math.min(1.8, Math.max(0.12, Math.min((this.width - 60) / (right - left), (this.height - 50) / (bottom - top))));
    this.view = { x: this.width / 2 - (left + right) / 2 * k, y: this.height / 2 - (top + bottom) / 2 * k, k };
    if (interacted) this.interacted = true;
    this.drawSoon();
  }
  zoom(factor) { this.interacted = true; this.zoomAt(this.width / 2, this.height / 2, factor); }
  zoomAt(x, y, factor) {
    const k = Math.max(0.1, Math.min(5, this.view.k * factor)), ratio = k / this.view.k;
    this.view = { x: x - (x - this.view.x) * ratio, y: y - (y - this.view.y) * ratio, k };
    this.drawSoon();
  }
  select(selection, center = false) {
    this.selection = selection;
    if (center && selection?.kind === 'node') {
      const point = this.points.get(selection.id);
      if (point) { this.interacted = true; this.view.x = this.width / 2 - point.x * this.view.k; this.view.y = this.height / 2 - point.y * this.view.k; }
    }
    this.onSelect(selection); this.drawSoon();
  }
  radius(node) { return 7 + Math.min(8, node.degree * 0.8); }
  geometry(edge) {
    const a = this.points.get(edge.source), b = this.points.get(edge.target);
    if (!a || !b) return null;
    if (a.id === b.id) return { loop: true, x: a.x + 17, y: a.y - 22, radius: 23 + Math.abs(this.lanes.get(edge.id) || 0) };
    const dx = b.x - a.x, dy = b.y - a.y, length = Math.max(1, Math.hypot(dx, dy));
    const lane = (this.lanes.get(edge.id) || 0) * (edge.source < edge.target ? 1 : -1);
    const control = { x: (a.x + b.x) / 2 - dy / length * lane, y: (a.y + b.y) / 2 + dx / length * lane };
    const trim = (point, toward, amount) => { const d = Math.max(1, Math.hypot(toward.x - point.x, toward.y - point.y)); return { x: point.x + (toward.x - point.x) / d * amount, y: point.y + (toward.y - point.y) / d * amount }; };
    return { a: trim(a, control, 14), b: trim(b, control, edge.directed ? 19 : 14), control };
  }
  hit(point) {
    const p = this.world(point);
    for (let i = this.graph.nodes.length - 1; i >= 0; i--) {
      const node = this.graph.nodes[i], at = this.points.get(node.id);
      if (at && Math.hypot(p.x - at.x, p.y - at.y) < this.radius(node) + 7 / this.view.k) return { kind: 'node', id: node.id };
    }
    for (const edge of this.graph.edges) {
      const g = this.geometry(edge);
      if (!g) continue;
      if (g.loop) {
        if (Math.abs(Math.hypot(p.x - g.x, p.y - g.y) - g.radius) < 6 / this.view.k) return { kind: 'edge', id: edge.id };
      } else {
        let previous = g.a;
        for (let i = 1; i <= 12; i++) {
          const t = i / 12, u = 1 - t, next = { x: u * u * g.a.x + 2 * u * t * g.control.x + t * t * g.b.x, y: u * u * g.a.y + 2 * u * t * g.control.y + t * t * g.b.y };
          if (distance(p, previous, next) < 5 / this.view.k) return { kind: 'edge', id: edge.id };
          previous = next;
        }
      }
    }
    return null;
  }
  pointerDown(event) {
    if (event.button !== 0) return;
    const p = this.local(event); this.pointers.set(event.pointerId, p); this.canvas.setPointerCapture(event.pointerId);
    if (this.pointers.size === 2) {
      const [a, b] = [...this.pointers.values()]; this.pinch = Math.hypot(a.x - b.x, a.y - b.y); this.drag = null; return;
    }
    const hit = this.hit(p); this.interacted = true;
    this.drag = { start: p, last: p, hit, moved: false };
    if (hit?.kind === 'node') this.stop();
  }
  pointerMove(event) {
    const p = this.local(event);
    if (this.pointers.has(event.pointerId)) this.pointers.set(event.pointerId, p);
    if (this.pointers.size >= 2) {
      const [a, b] = [...this.pointers.values()], d = Math.max(1, Math.hypot(a.x - b.x, a.y - b.y));
      this.zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / (this.pinch || d)); this.pinch = d; return;
    }
    if (this.drag) {
      const dx = p.x - this.drag.last.x, dy = p.y - this.drag.last.y;
      if (Math.hypot(p.x - this.drag.start.x, p.y - this.drag.start.y) > 4) this.drag.moved = true;
      if (this.drag.hit?.kind === 'node') {
        const node = this.points.get(this.drag.hit.id); if (node) { node.x += dx / this.view.k; node.y += dy / this.view.k; }
      } else { this.view.x += dx; this.view.y += dy; }
      this.drag.last = p; this.drawSoon();
    } else {
      const hit = this.hit(p);
      if (hit?.id !== this.hover?.id || hit?.kind !== this.hover?.kind) { this.hover = hit; this.drawSoon(); }
      this.canvas.style.cursor = hit ? 'pointer' : 'grab';
    }
  }
  pointerUp(event) {
    this.pointers.delete(event.pointerId);
    if (this.drag && !this.drag.moved) this.select(this.drag.hit);
    this.drag = null; this.pinch = null;
  }
  drawSoon() {
    if (this.frame !== null) return;
    this.frame = requestAnimationFrame(() => { this.frame = null; this.draw(); });
  }
  draw() {
    if (!this.width || !this.height) return;
    const ctx = this.canvas.getContext('2d'), light = document.documentElement.dataset.theme === 'light';
    const background = light ? '#f6f8f7' : '#101715';
    ctx.setTransform(this.ratio, 0, 0, this.ratio, 0, 0); ctx.fillStyle = background; ctx.fillRect(0, 0, this.width, this.height);
    ctx.fillStyle = light ? '#dce4e0' : '#24302a';
    const grid = 24;
    for (let x = ((this.view.x % grid) + grid) % grid; x < this.width; x += grid) for (let y = ((this.view.y % grid) + grid) % grid; y < this.height; y += grid) { ctx.beginPath(); ctx.arc(x, y, 0.65, 0, Math.PI * 2); ctx.fill(); }
    ctx.translate(this.view.x, this.view.y); ctx.scale(this.view.k, this.view.k);
    const focusId = this.selection?.kind === 'node' ? this.selection.id : null;
    const neighbors = new Set(focusId ? [focusId] : []);
    if (focusId) for (const edge of this.graph.edges) { if (edge.source === focusId) neighbors.add(edge.target); if (edge.target === focusId) neighbors.add(edge.source); }
    for (const edge of this.graph.edges) {
      const g = this.geometry(edge); if (!g) continue;
      const selected = this.selection?.kind === 'edge' && edge.id === this.selection.id;
      const active = selected || (this.hover?.kind === 'edge' && this.hover.id === edge.id) || (focusId && (edge.source === focusId || edge.target === focusId));
      ctx.globalAlpha = active ? 0.85 : this.focus && focusId ? 0.07 : light ? 0.37 : 0.24;
      ctx.strokeStyle = active ? '#64b89a' : light ? '#668378' : '#87ab9c'; ctx.fillStyle = ctx.strokeStyle;
      ctx.lineWidth = (selected ? 2.2 : 1) / Math.sqrt(this.view.k); ctx.beginPath();
      if (g.loop) ctx.arc(g.x, g.y, g.radius, 0.35, Math.PI * 2 + 0.1);
      else { ctx.moveTo(g.a.x, g.a.y); ctx.quadraticCurveTo(g.control.x, g.control.y, g.b.x, g.b.y); }
      ctx.stroke();
      if (edge.directed) {
        const point = g.loop ? { x: g.x + Math.cos(0.1) * g.radius, y: g.y + Math.sin(0.1) * g.radius } : g.b;
        const angle = g.loop ? 0.1 + Math.PI / 2 : Math.atan2(g.b.y - g.control.y, g.b.x - g.control.x);
        ctx.save(); ctx.translate(point.x, point.y); ctx.rotate(angle); ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(-6, -3); ctx.lineTo(-6, 3); ctx.closePath(); ctx.fill(); ctx.restore();
      }
      if ((selected || (this.hover?.kind === 'edge' && this.hover.id === edge.id)) && edge.label) {
        const p = g.loop ? { x: g.x, y: g.y - g.radius - 9 } : g.control;
        this.caption(ctx, edge.label, p.x, p.y, light, true);
      }
    }
    const captions = [], occupied = [];
    for (const node of this.graph.nodes) {
      const p = this.points.get(node.id); if (!p) continue;
      const selected = this.selection?.kind === 'node' && this.selection.id === node.id;
      const hover = this.hover?.kind === 'node' && this.hover.id === node.id;
      const match = !this.matches || this.matches.has(node.id);
      ctx.globalAlpha = (!match || (this.focus && focusId && !neighbors.has(node.id))) ? 0.18 : 1;
      const color = PALETTE[colorIndex(node.group)], radius = this.radius(node);
      if (selected || hover) {
        ctx.fillStyle = color + '18'; ctx.beginPath(); ctx.arc(p.x, p.y, radius + 10, 0, Math.PI * 2); ctx.fill();
        ctx.strokeStyle = color; ctx.lineWidth = 1.2 / this.view.k; ctx.beginPath(); ctx.arc(p.x, p.y, radius + 5, 0, Math.PI * 2); ctx.stroke();
      }
      ctx.fillStyle = color; ctx.beginPath(); ctx.arc(p.x, p.y, radius, 0, Math.PI * 2); ctx.fill();
      ctx.fillStyle = light ? '#ffffff88' : '#ffffff66'; ctx.beginPath(); ctx.arc(p.x - radius / 4, p.y - radius / 4, Math.max(1.8, radius / 4), 0, Math.PI * 2); ctx.fill();
      occupied.push({ x: p.x - radius, y: p.y - radius, w: radius * 2, h: radius * 2 });
      if (this.labels || selected || hover) captions.push({ text: selected || hover ? clip(node.title, 45) : clip(node.title, 22), x: p.x, y: p.y + radius + 15 / this.view.k, light, emphasized: selected || hover, alpha: ctx.globalAlpha, degree: node.degree });
    }
    // Keep selected labels visible and suppress overlapping background labels.
    // Zooming reveals more; the searchable accessible node list always names
    // every displayed node independently of canvas label placement.
    captions.sort((a, b) => Number(b.emphasized) - Number(a.emphasized) || b.degree - a.degree);
    for (const item of captions) {
      ctx.font = `${item.emphasized ? 600 : 400} ${11 / this.view.k}px system-ui, sans-serif`;
      const w = ctx.measureText(item.text).width + 8 / this.view.k, h = 15 / this.view.k;
      const box = { x: item.x - w / 2, y: item.y - 11 / this.view.k, w, h };
      if (!item.emphasized && occupied.some(other => box.x < other.x + other.w && box.x + box.w > other.x && box.y < other.y + other.h && box.y + box.h > other.y)) continue;
      occupied.push(box); ctx.globalAlpha = item.alpha;
      this.caption(ctx, item.text, item.x, item.y, item.light, item.emphasized);
    }
    ctx.globalAlpha = 1; this.drawMinimap(light);
  }
  caption(ctx, text, x, y, light, emphasized) {
    const unit = 1 / this.view.k;
    ctx.font = `${emphasized ? 600 : 400} ${11 * unit}px system-ui, sans-serif`; ctx.textAlign = 'center';
    const width = ctx.measureText(text).width;
    ctx.fillStyle = light ? '#f6f8f7e8' : '#101715e8'; ctx.fillRect(x - width / 2 - 4 * unit, y - 11 * unit, width + 8 * unit, 15 * unit);
    ctx.fillStyle = emphasized ? (light ? '#172c23' : '#e7f3ed') : (light ? '#51635a' : '#9daea5'); ctx.fillText(text, x, y);
  }
  drawMinimap(light) {
    const ctx = this.minimap.getContext('2d'); this.minimap.width = 320; this.minimap.height = 184; ctx.scale(2, 2);
    ctx.fillStyle = light ? '#edf2ef' : '#151e19'; ctx.fillRect(0, 0, 160, 92);
    const points = this.graph.nodes.map(node => this.points.get(node.id)).filter(Boolean); if (!points.length) return;
    const left = Math.min(...points.map(p => p.x)) - 30, right = Math.max(...points.map(p => p.x)) + 30;
    const top = Math.min(...points.map(p => p.y)) - 30, bottom = Math.max(...points.map(p => p.y)) + 30;
    const k = Math.min(145 / (right - left), 78 / (bottom - top)), x = 80 - (left + right) * k / 2, y = 46 - (top + bottom) * k / 2;
    this.miniTransform = { k, x, y };
    ctx.fillStyle = light ? '#5c997c' : '#75af92';
    for (const p of points) { ctx.beginPath(); ctx.arc(x + p.x * k, y + p.y * k, 1.6, 0, Math.PI * 2); ctx.fill(); }
    ctx.strokeStyle = light ? '#668e7980' : '#99bdaa50'; ctx.lineWidth = 1;
    ctx.strokeRect(x - this.view.x / this.view.k * k, y - this.view.y / this.view.k * k, this.width / this.view.k * k, this.height / this.view.k * k);
  }
  image() { return new Promise(resolve => this.canvas.toBlob(resolve, 'image/png')); }
  destroy() { this.stop(); this.worker?.terminate(); this.observer.disconnect(); if (this.frame !== null) cancelAnimationFrame(this.frame); }
}
