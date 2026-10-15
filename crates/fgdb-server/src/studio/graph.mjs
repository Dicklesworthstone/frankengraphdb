import { PALETTE, colorIndex } from './model.mjs';
import { createLayout, DEFAULT_PHYSICS } from './layout.mjs';
const TAU = Math.PI * 2;
const clip = (text, n = 26) => String(text).length > n ? String(text).slice(0, n - 1) + '…' : String(text);
const pointOn = (g, t) => ({ x:(1-t)**2*g.a.x+2*(1-t)*t*g.c.x+t*t*g.b.x, y:(1-t)**2*g.a.y+2*(1-t)*t*g.c.y+t*t*g.b.y });
const segmentDistance = (p,a,b) => { const dx=b.x-a.x,dy=b.y-a.y,t=Math.max(0,Math.min(1,((p.x-a.x)*dx+(p.y-a.y)*dy)/(dx*dx+dy*dy||1))); return Math.hypot(p.x-a.x-t*dx,p.y-a.y-t*dy); };

export class GraphCanvas {
  constructor(canvas, minimap, { onSelect = () => {}, onBusy = () => {}, onError = () => {}, onTick = () => {} } = {}) {
    Object.assign(this, { canvas, minimap, onSelect, onBusy, onError, onTick });
    this.graph={nodes:[],edges:[]};this.points=new Map();this.pinned=new Set();this.view={x:0,y:0,k:1};
    this.id=0;this.mode='force';this.labels=true;this.focus=false;this.selection=null;this.hover=null;
    this.frame=null;this.running=false;this.paused=false;this.interacted=false;this.matches=null;this.pointers=new Map();
    this.settings={...DEFAULT_PHYSICS};this.reduced=matchMedia('(prefers-reduced-motion: reduce)').matches;
    try {
      this.worker=new Worker(new URL('./layout.mjs',import.meta.url),{type:'module'});
      this.worker.onmessage=({data})=>{
        if(data.id!==this.id)return;
        if(data.error){this.running=false;this.onBusy(false);this.onError(data.error);this.publish({engine:'unavailable',done:true});return;}
        // A frame queued before pause cannot resume the visible simulation.
        if(this.paused)return;
        if(!this.reduced||data.done||this.drag){
          for(let i=0;i<this.simulationNodes.length;i++){
            const id=this.simulationNodes[i].id;
            if(this.pinned.has(id)||this.drag?.hit?.id===id&&this.drag.moved)continue;
            const x=data.coordinates[i*2],y=data.coordinates[i*2+1];
            if(Number.isFinite(x)&&Number.isFinite(y))this.points.set(id,{id,x,y,pinned:this.pinned.has(id)});
          }
          if(!this.interacted&&(data.ticks<=4||data.ticks%32===0||data.done))this.fit(false);
          this.drawSoon();
        }
        this.running=!data.done;this.onBusy(!data.done);this.publish(data);
      };
      this.worker.onerror=()=>{this.running=false;this.onBusy(false);this.publish({engine:'unavailable',done:true});this.onError('WebAssembly worker unavailable. Initial positions remain interactive; no force simulation is running.');};
    }catch(error){this.onError(`Cannot start layout worker: ${error.message}`);}
    this.observer=new ResizeObserver(()=>this.resize());this.observer.observe(canvas);
    canvas.addEventListener('wheel',event=>{event.preventDefault();const p=this.local(event);this.interacted=true;this.zoomAt(p.x,p.y,Math.exp(-Math.max(-100,Math.min(100,event.deltaY))*0.006));},{passive:false});
    canvas.addEventListener('pointerdown',event=>this.pointerDown(event));
    canvas.addEventListener('pointermove',event=>this.pointerMove(event));
    canvas.addEventListener('pointerup',event=>this.pointerUp(event));
    canvas.addEventListener('pointercancel',event=>this.pointerUp(event,true));
    canvas.addEventListener('pointerleave',()=>{if(!this.drag){this.hover=null;this.drawSoon();}});
    canvas.addEventListener('dblclick',event=>{const hit=this.hit(this.local(event));if(hit?.kind==='node'){this.select(hit);this.togglePin(hit.id);}else{this.interacted=true;const p=this.local(event);this.zoomAt(p.x,p.y,1.5);}});
    canvas.addEventListener('keydown',event=>{
      const shift={ArrowLeft:[35,0],ArrowRight:[-35,0],ArrowUp:[0,35],ArrowDown:[0,-35]}[event.key];
      if(shift){event.preventDefault();this.view.x+=shift[0];this.view.y+=shift[1];this.interacted=true;this.drawSoon();}
      if(['+','='].includes(event.key)){event.preventDefault();this.zoom(1.2);}
      if(event.key==='-'){event.preventDefault();this.zoom(1/1.2);}
      if(event.key.toLowerCase()==='f'){event.preventDefault();this.fit();}
      if(event.key.toLowerCase()==='p'&&this.selection?.kind==='node'){event.preventDefault();this.togglePin(this.selection.id);}
      if(event.key==='Escape')this.select(null);
    });
    minimap?.addEventListener('pointerdown',event=>{
      if(!this.miniTransform)return;const r=minimap.getBoundingClientRect(),t=this.miniTransform;
      const x=((event.clientX-r.left)*160/r.width-t.x)/t.k,y=((event.clientY-r.top)*96/r.height-t.y)/t.k;
      this.view.x=this.width/2-x*this.view.k;this.view.y=this.height/2-y*this.view.k;this.interacted=true;this.drawSoon();
    });
  }
  publish(data){this.onTick(data);this.canvas.dispatchEvent(new CustomEvent('studio:physics',{detail:data}));}
  local(e){const r=this.canvas.getBoundingClientRect();return{x:e.clientX-r.left,y:e.clientY-r.top};}
  world(p){return{x:(p.x-this.view.x)/this.view.k,y:(p.y-this.view.y)/this.view.k};}
  resize(){const{width,height}=this.canvas.getBoundingClientRect();if(!width||!height)return;this.width=width;this.height=height;this.ratio=Math.min(devicePixelRatio||1,2);this.canvas.width=Math.round(width*this.ratio);this.canvas.height=Math.round(height*this.ratio);if(!this.interacted)this.fit(false);this.drawSoon();}
  stop(){this.id++;this.worker?.postMessage({stop:true});this.running=false;this.onBusy(false);}
  pause(){this.worker?.postMessage({action:'pause',id:this.id});this.running=false;this.paused=true;this.onBusy(false);this.publish({engine:'wasm',paused:true});}
  reheat(){if(!this.worker||!this.graph.nodes.length)return;this.paused=false;this.running=true;this.onBusy(true);this.worker.postMessage({action:'reheat',id:this.id});}
  configure(settings){this.settings={...this.settings,...settings};this.paused=false;this.running=true;this.onBusy(true);this.worker?.postMessage({action:'configure',id:this.id,settings:this.settings});}
  setData(graph,{preserve=true}={}){
    const previous=preserve?[...this.points.values()].map(p=>({...p,pinned:this.pinned.has(p.id)})):[];
    if(!preserve)this.pinned.clear();
    this.stop();this.graph=graph;this.selection=null;this.hover=null;this.matches=null;
    const ids=new Set(graph.nodes.map(n=>n.id));this.pinned=new Set([...this.pinned].filter(id=>ids.has(id)));
    if(!previous.length)this.interacted=false;
    this.points=new Map(createLayout(graph.nodes,graph.edges,this.mode,previous).points.map(p=>[p.id,p]));
    this.prepareEdges();this.arrange(this.mode,true);if(!this.interacted)this.fit(false);this.drawSoon();
  }
  setVisible(graph){this.graph=graph;this.matches=graph.matches;this.prepareEdges();this.drawSoon();}
  prepareEdges(){
    this.nodeMap=new Map(this.graph.nodes.map(n=>[n.id,n]));this.lanes=new Map();const pairs=new Map();
    for(const edge of this.graph.edges){const key=[edge.source,edge.target].sort().join(':');if(!pairs.has(key))pairs.set(key,[]);pairs.get(key).push(edge);}
    for(const values of pairs.values())values.sort((a,b)=>a.id.localeCompare(b.id)).forEach((e,i)=>this.lanes.set(e.id,e.source===e.target?i*14:(i-(values.length-1)/2)*32));
  }
  arrange(mode=this.mode,keep=false){
    this.mode=mode;this.stop();if(!this.worker||!this.graph.nodes.length){this.drawSoon();return;}
    this.simulationNodes=this.graph.nodes.map(({id,group,degree})=>({id,group,degree}));
    const previous=[...this.points.values()].map(p=>({...p,pinned:this.pinned.has(p.id)}));
    if(!keep)this.interacted=false;this.running=true;this.paused=false;this.onBusy(true);
    this.worker.postMessage({id:this.id,nodes:this.simulationNodes,edges:this.graph.edges.map(({source,target})=>({source,target})),mode,previous,settings:this.settings});
  }
  fit(interacted=true){
    if(!this.width||!this.height)return;const p=this.graph.nodes.map(n=>this.points.get(n.id)).filter(Boolean);
    if(!p.length){this.view={x:this.width/2,y:this.height/2,k:1};this.drawSoon();return;}
    const l=Math.min(...p.map(p=>p.x))-65,r=Math.max(...p.map(p=>p.x))+65,t=Math.min(...p.map(p=>p.y))-60,b=Math.max(...p.map(p=>p.y))+70;
    const k=Math.max(0.1,Math.min(2,(this.width-80)/(r-l),(this.height-100)/(b-t)));
    this.view={x:this.width/2-(l+r)*k/2,y:this.height/2-(t+b)*k/2,k};if(interacted)this.interacted=true;this.drawSoon();
  }
  zoom(factor){this.interacted=true;this.zoomAt(this.width/2,this.height/2,factor);}
  zoomAt(x,y,factor){const k=Math.max(0.08,Math.min(6,this.view.k*factor)),r=k/this.view.k;this.view={x:x-(x-this.view.x)*r,y:y-(y-this.view.y)*r,k};this.drawSoon();}
  select(selection,center=false){this.selection=selection;if(center&&selection?.kind==='node'){const p=this.points.get(selection.id);if(p){this.interacted=true;this.view.x=this.width/2-p.x*this.view.k;this.view.y=this.height/2-p.y*this.view.k;}}this.onSelect(selection);this.drawSoon();}
  togglePin(id){const p=this.points.get(id);if(!p)return;this.paused=false;this.pinned.has(id)?this.pinned.delete(id):this.pinned.add(id);p.pinned=this.pinned.has(id);this.worker?.postMessage({action:'pin',id:this.id,node:id,x:p.x,y:p.y,fixed:p.pinned});this.onSelect(this.selection);this.drawSoon();}
  unpin(){for(const id of [...this.pinned])this.togglePin(id);}
  radius(node){const base=node?.analysis?8+node.analysis.normalized*13:8+Math.min(10,Math.sqrt(node?.degree||0)*2);return base*Math.max(1,0.72/this.view.k);}
  geometry(edge){
    const a=this.points.get(edge.source),b=this.points.get(edge.target);if(!a||!b)return null;
    if(a.id===b.id)return{loop:true,x:a.x+12,y:a.y-23,r:23+(this.lanes.get(edge.id)||0)};
    const dx=b.x-a.x,dy=b.y-a.y,len=Math.max(1,Math.hypot(dx,dy));const lane=(this.lanes.get(edge.id)||0)*(edge.source<edge.target?1:-1);
    const c={x:(a.x+b.x)/2-dy/len*lane,y:(a.y+b.y)/2+dx/len*lane};
    const trim=(p,t,r)=>{const d=Math.max(1,Math.hypot(t.x-p.x,t.y-p.y));return{x:p.x+(t.x-p.x)/d*r,y:p.y+(t.y-p.y)/d*r};};
    return{a:trim(a,c,this.radius(this.nodeMap.get(a.id))+3),b:trim(b,c,this.radius(this.nodeMap.get(b.id))+4),c};
  }
  hit(point){const p=this.world(point);for(let i=this.graph.nodes.length-1;i>=0;i--){const n=this.graph.nodes[i],a=this.points.get(n.id);if(a&&Math.hypot(p.x-a.x,p.y-a.y)<this.radius(n)+6/this.view.k)return{kind:'node',id:n.id};}
    for(const e of this.graph.edges){const g=this.geometry(e);if(!g)continue;if(g.loop){if(Math.abs(Math.hypot(p.x-g.x,p.y-g.y)-g.r)<5/this.view.k)return{kind:'edge',id:e.id};}else{let a=g.a;for(let i=1;i<=12;i++){const b=pointOn(g,i/12);if(segmentDistance(p,a,b)<4/this.view.k)return{kind:'edge',id:e.id};a=b;}}}return null;}
  pointerDown(event){if(event.button!==0)return;const p=this.local(event);this.pointers.set(event.pointerId,p);this.canvas.setPointerCapture(event.pointerId);if(this.pointers.size>1){if(this.drag)this.releaseDrag();const[a,b]=[...this.pointers.values()];this.pinch=Math.hypot(a.x-b.x,a.y-b.y);return;}const hit=this.hit(p);this.interacted=true;this.drag={start:p,last:p,hit,moved:false,persist:event.shiftKey};if(hit?.kind==='node'){this.paused=false;const v=this.points.get(hit.id);this.worker?.postMessage({action:'pin',id:this.id,node:hit.id,x:v.x,y:v.y,fixed:true});}}
  pointerMove(event){const p=this.local(event);if(this.pointers.has(event.pointerId))this.pointers.set(event.pointerId,p);if(this.pointers.size>1){const[a,b]=[...this.pointers.values()],d=Math.max(1,Math.hypot(a.x-b.x,a.y-b.y));this.zoomAt((a.x+b.x)/2,(a.y+b.y)/2,d/(this.pinch||d));this.pinch=d;return;}if(this.drag){const dx=p.x-this.drag.last.x,dy=p.y-this.drag.last.y;if(Math.hypot(p.x-this.drag.start.x,p.y-this.drag.start.y)>3)this.drag.moved=true;if(this.drag.hit?.kind==='node'){const v=this.points.get(this.drag.hit.id);v.x+=dx/this.view.k;v.y+=dy/this.view.k;this.worker?.postMessage({action:'pin',id:this.id,node:v.id,x:v.x,y:v.y,fixed:true});}else{this.view.x+=dx;this.view.y+=dy;}this.drag.last=p;this.drawSoon();}else{const hit=this.hit(p);if(hit?.id!==this.hover?.id||hit?.kind!==this.hover?.kind){this.hover=hit;this.drawSoon();}this.canvas.style.cursor=hit?'pointer':'grab';}}
  releaseDrag(){if(this.drag?.hit?.kind==='node'){const p=this.points.get(this.drag.hit.id);if(this.drag.persist&&this.drag.moved)this.pinned.add(p.id);this.worker?.postMessage({action:'pin',id:this.id,node:p.id,x:p.x,y:p.y,fixed:this.pinned.has(p.id)});}this.drag=null;}
  pointerUp(event,cancel=false){this.pointers.delete(event.pointerId);if(!cancel&&this.drag&&!this.drag.moved)this.select(this.drag.hit);this.releaseDrag();this.pinch=null;this.drawSoon();}
  drawSoon(){if(this.frame!==null)return;this.frame=requestAnimationFrame(()=>{this.frame=null;this.draw();});}
  draw(){
    if(!this.width||!this.height)return;const c=this.canvas.getContext('2d'),light=document.documentElement.dataset.theme==='light';
    c.setTransform(this.ratio,0,0,this.ratio,0,0);c.globalAlpha=1;c.fillStyle=light?'#f3f5fa':'#080c16';c.fillRect(0,0,this.width,this.height);
    const fog=c.createRadialGradient(this.width*.52,this.height*.42,0,this.width*.5,this.height*.5,this.width*.66);fog.addColorStop(0,light?'#e7ecfa80':'#16223e80');fog.addColorStop(1,light?'#f3f5fa00':'#080c1600');c.fillStyle=fog;c.fillRect(0,0,this.width,this.height);
    c.fillStyle=light?'#c6cede':'#222c41';const grid=28;for(let x=(this.view.x%grid+grid)%grid;x<this.width;x+=grid)for(let y=(this.view.y%grid+grid)%grid;y<this.height;y+=grid)c.fillRect(x,y,1,1);
    c.translate(this.view.x,this.view.y);c.scale(this.view.k,this.view.k);
    const focusId=this.selection?.kind==='node'?this.selection.id:null,neighbors=new Set(focusId?[focusId]:[]);
    if(focusId)for(const e of this.graph.edges){if(e.source===focusId)neighbors.add(e.target);if(e.target===focusId)neighbors.add(e.source);}
    const highlighted=this.pathNodes||new Set();
    if(this.mode==='clusters')this.drawGroups(c,light);
    for(const e of this.graph.edges){
      const g=this.geometry(e);if(!g)continue;const chosen=this.selection?.kind==='edge'&&this.selection.id===e.id;
      const active=chosen||this.hover?.id===e.id&&(this.hover?.kind==='edge')||focusId&&(e.source===focusId||e.target===focusId)||this.pathEdges?.has(e.id);
      c.globalAlpha=this.focus&&focusId&&!active?0.07:active?0.95:0.38;
      let stroke=active?'#bdcaff':light?'#8393b2':'#596d99';
      if(e.change==='added')stroke='#68e6af';if(e.change==='removed')stroke='#fb8499';if(e.change==='changed')stroke='#f5c679';
      c.strokeStyle=stroke;c.fillStyle=stroke;c.lineWidth=(active?1.8:0.85)/Math.max(0.7,this.view.k);c.setLineDash(e.change==='removed'?[5,5]:[]);
      c.beginPath();if(g.loop)c.arc(g.x,g.y,g.r,0.2,TAU-0.45);else{c.moveTo(g.a.x,g.a.y);c.quadraticCurveTo(g.c.x,g.c.y,g.b.x,g.b.y);}c.stroke();c.setLineDash([]);
      if(e.directed){const p=g.loop?{x:g.x+Math.cos(0.2)*g.r,y:g.y+Math.sin(0.2)*g.r}:g.b;const a=g.loop?Math.PI/2+.2:Math.atan2(g.b.y-g.c.y,g.b.x-g.c.x);c.save();c.translate(p.x,p.y);c.rotate(a);c.beginPath();c.moveTo(0,0);c.lineTo(-5.5,-2.8);c.lineTo(-5.5,2.8);c.closePath();c.fill();c.restore();}
      if((chosen||this.hover?.kind==='edge'&&this.hover.id===e.id)&&e.label){const p=g.loop?{x:g.x,y:g.y-g.r}:pointOn(g,.5);this.caption(c,clip(e.label,25),p.x,p.y-5/this.view.k,light,true,stroke);}
    }
    const captions=[],occupied=[];
    for(const n of this.graph.nodes){
      const p=this.points.get(n.id);if(!p)continue;const selected=this.selection?.kind==='node'&&this.selection.id===n.id,hover=this.hover?.kind==='node'&&this.hover.id===n.id;
      const match=!this.matches||this.matches.has(n.id),connected=neighbors.has(n.id),onPath=highlighted.has(n.id);
      c.globalAlpha=!match||this.focus&&focusId&&!connected?0.16:1;
      const color=n.change==='added'?'#68e6af':n.change==='removed'?'#fb8499':n.change==='changed'?'#f5c679':PALETTE[colorIndex(n.group)],r=this.radius(n);
      if(selected||hover||connected||onPath){c.fillStyle=color+'12';c.beginPath();c.arc(p.x,p.y,r+11,0,TAU);c.fill();c.strokeStyle=color+(selected?'bb':'44');c.lineWidth=1/this.view.k;c.beginPath();c.arc(p.x,p.y,r+6,0,TAU);c.stroke();}
      if(n.degree>4||selected){const glow=c.createRadialGradient(p.x,p.y,0,p.x,p.y,r*2.7);glow.addColorStop(0,color+'28');glow.addColorStop(1,color+'00');c.fillStyle=glow;c.fillRect(p.x-r*3,p.y-r*3,r*6,r*6);}
      c.fillStyle=light?'#ffffff':'#10172a';c.strokeStyle=color;c.lineWidth=selected?2.5:1.5;c.setLineDash(n.change==='removed'?[3,2]:[]);c.beginPath();c.arc(p.x,p.y,r,0,TAU);c.fill();c.stroke();c.setLineDash([]);
      c.fillStyle=color+'28';c.beginPath();c.arc(p.x,p.y,r-2,0,TAU);c.fill();
      c.strokeStyle=color;c.lineWidth=1.3/Math.max(.8,this.view.k);c.beginPath();
      if(/agent|person/i.test(n.group)){c.arc(p.x,p.y-3,2.6,0,TAU);c.moveTo(p.x-4,p.y+4);c.quadraticCurveTo(p.x,p.y,p.x+4,p.y+4);}
      else if(/source|paper|document/i.test(n.group)){c.rect(p.x-3,p.y-4.5,6,9);c.moveTo(p.x-1.7,p.y-1.5);c.lineTo(p.x+1.7,p.y-1.5);c.moveTo(p.x-1.7,p.y+1.5);c.lineTo(p.x+1.7,p.y+1.5);}
      else{c.moveTo(p.x,p.y-4.5);c.lineTo(p.x+4.5,p.y);c.lineTo(p.x,p.y+4.5);c.lineTo(p.x-4.5,p.y);c.closePath();}c.stroke();
      if(this.pinned.has(n.id)){c.fillStyle=color;c.beginPath();c.arc(p.x+r*.75,p.y-r*.8,3.4,0,TAU);c.fill();c.strokeStyle=light?'#fff':'#080c16';c.lineWidth=1.5;c.stroke();}
      const sx=p.x*this.view.k+this.view.x,sy=p.y*this.view.k+this.view.y;
      occupied.push({x:sx-r*this.view.k,y:sy-r*this.view.k,w:2*r*this.view.k,h:2*r*this.view.k});
      if(this.labels||selected||hover)captions.push({n,p,r,selected,hover,connected,alpha:c.globalAlpha,color,score:(selected?1e6:0)+(hover?1e5:0)+(connected?1000:0)+n.degree});
    }
    captions.sort((a,b)=>b.score-a.score);
    for(const v of captions){const emphasis=v.selected||v.hover,txt=clip(v.n.title,emphasis?42:24);const font=emphasis?12:11;c.font=`${emphasis?600:450} ${font/this.view.k}px system-ui`;
      const w=c.measureText(txt).width*this.view.k+12,x=v.p.x*this.view.k+this.view.x-w/2,y=(v.p.y+v.r)*this.view.k+this.view.y+7,box={x,y,w,h:18};
      if(!emphasis&&occupied.some(b=>x<b.x+b.w&&x+w>b.x&&y<b.y+b.h&&y+18>b.y))continue;
      occupied.push(box);c.globalAlpha=v.alpha;this.caption(c,txt,v.p.x,v.p.y+v.r+20/this.view.k,light,emphasis,v.color);
    }
    c.globalAlpha=1;this.drawMinimap(light);
  }
  drawGroups(c,light){const groups=new Map();for(const n of this.graph.nodes){const p=this.points.get(n.id);if(!p)continue;if(!groups.has(n.group))groups.set(n.group,[]);groups.get(n.group).push(p);}
    for(const[label,p]of groups){if(p.length<2)continue;const x=p.reduce((s,p)=>s+p.x,0)/p.length,y=p.reduce((s,p)=>s+p.y,0)/p.length,r=Math.max(45,...p.map(p=>Math.hypot(x-p.x,y-p.y)))+22,color=PALETTE[colorIndex(label)];
      c.strokeStyle=color+'20';c.fillStyle=color+'05';c.lineWidth=1/this.view.k;c.setLineDash([4/this.view.k,6/this.view.k]);c.beginPath();c.arc(x,y,r,0,TAU);c.fill();c.stroke();c.setLineDash([]);c.font=`500 ${10/this.view.k}px system-ui`;c.fillStyle=color+'99';c.textAlign='center';c.fillText(`${label.toUpperCase()} · ${p.length}`,x,y-r-10/this.view.k);
    }}
  caption(c,text,x,y,light,emphasis,color){const k=1/this.view.k;c.font=`${emphasis?600:450} ${(emphasis?12:11)*k}px system-ui`;c.textAlign='center';const w=c.measureText(text).width;c.fillStyle=light?'#f7f9ffed':'#0b1120ed';c.beginPath();c.roundRect(x-w/2-5*k,y-12*k,w+10*k,18*k,4*k);c.fill();if(emphasis){c.strokeStyle=color+'66';c.lineWidth=k;c.stroke();}c.fillStyle=emphasis?(light?'#233048':'#f0f4ff'):(light?'#47556d':'#9aa9c3');c.fillText(text,x,y);}
  drawMinimap(light){if(!this.minimap)return;const c=this.minimap.getContext('2d');this.minimap.width=320;this.minimap.height=192;c.scale(2,2);c.fillStyle=light?'#edf1f9':'#0d1422';c.fillRect(0,0,160,96);const p=this.graph.nodes.map(n=>this.points.get(n.id)).filter(Boolean);if(!p.length)return;
    const l=Math.min(...p.map(p=>p.x))-35,r=Math.max(...p.map(p=>p.x))+35,t=Math.min(...p.map(p=>p.y))-35,b=Math.max(...p.map(p=>p.y))+35,k=Math.min(140/(r-l),75/(b-t)),x=80-(l+r)*k/2,y=48-(t+b)*k/2;this.miniTransform={k,x,y};
    for(const n of this.graph.nodes){const p=this.points.get(n.id);if(!p)continue;c.fillStyle=PALETTE[colorIndex(n.group)];c.globalAlpha=.7;c.beginPath();c.arc(x+p.x*k,y+p.y*k,1.6,0,TAU);c.fill();}c.globalAlpha=1;c.strokeStyle=light?'#7893c8':'#708ad299';c.fillStyle='#8ba8f40a';const vx=x-this.view.x/this.view.k*k,vy=y-this.view.y/this.view.k*k,w=this.width/this.view.k*k,h=this.height/this.view.k*k;c.fillRect(vx,vy,w,h);c.strokeRect(vx,vy,w,h);
  }
  image(){return new Promise(resolve=>this.canvas.toBlob(resolve,'image/png'));}
  destroy(){this.stop();this.worker?.terminate();this.observer.disconnect();if(this.frame!==null)cancelAnimationFrame(this.frame);}
}
