/* Studio's isolated presentation kernel. This is NOT database execution.
 * Freestanding wasm32, zero imports, no allocator, fixed four-MiB memory.
 * Counts/indices/finite inputs are checked at the export boundary. No graph
 * identities, credentials, strings or source properties cross this boundary.
 * Barnes-Hut tree depth <= 24; overflow leaves use exact bounded evaluation.
 * Build only as WebAssembly with build.sh, never link into the Rust server.
 */
#define NMAX 4096
#define EMAX 16384
#define QMAX 32768
#define EXPORT(name) __attribute__((export_name(name)))

typedef struct { float x,y,vx,vy,fx,fy,mass,radius,tx,ty; int pinned; } Node;
typedef struct { int a,b,directed; } Edge;
typedef struct { float x,y,half,mass,mx,my,radius; int start,end,child[4]; } Quad;
static Node nodes[NMAX];
static Edge edges[EMAX];
static Quad quads[QMAX];
static int order[NMAX], temporary[NMAX], n, m, qn, ticks;
static float positions[NMAX*2];
static float alpha=1, charge=850, length=90, gravity=0.006f, separation=1, grouping=0;
static float energy;
static float flow_strength;
static unsigned long long visits;
static int ready;
static float absf(float v) { return v<0?-v:v; }
static float maxf(float a,float b) { return a>b?a:b; }
static float clamp(float v,float a,float b) { return v<a?a:v>b?b:v; }
static int finite(float v) { return __builtin_isfinite(v); }
static float root(float v) { return __builtin_sqrtf(v); }

EXPORT("init") int init(int count,int links) {
    if(count<0 || count>NMAX || links<0 || links>EMAX) return 0;
    n=count; m=links; ticks=0; alpha=1; energy=0; visits=0; ready=0; flow_strength=0;
    for(int i=0;i<n;i++) { nodes[i]=(Node){0,0,0,0,0,0,1,8,0,0,0}; order[i]=i; }
    for(int i=0;i<m;i++) edges[i]=(Edge){-1,-1,0};
    return 1;
}
EXPORT("node") int node(int i,float x,float y,float mass,float radius,float tx,float ty) {
    if(i<0||i>=n||!finite(x)||!finite(y)||!finite(mass)||!finite(radius)||!finite(tx)||!finite(ty)
       ||absf(x)>100000||absf(y)>100000||mass<1||mass>100||radius<1||radius>100
       ||absf(tx)>100000||absf(ty)>100000) return 0;
    nodes[i]=(Node){x,y,0,0,0,0,mass,radius,tx,ty,0}; return 1;
}
EXPORT("edge") int edge(int i,int a,int b) {
    if(i<0||i>=m||a<0||a>=n||b<0||b>=n) return 0;
    edges[i]=(Edge){a,b,0}; return 1;
}
/* Direction is supplied only for explicit stored endpoints. A path's traversal
 * order is not evidence of storage direction. These display forces preserve
 * cycles and parallel edges; they never invent a DAG or change query topology. */
EXPORT("direction") int direction(int i,int directed) {
    if(i<0||i>=m||(directed!=0&&directed!=1)) return 0;
    edges[i].directed=directed; return 1;
}
EXPORT("flow") int flow(float strength) {
    if(!finite(strength)||strength<0||strength>0.3f) return 0;
    flow_strength=strength; alpha=1; ticks=0; return 1;
}
EXPORT("configure") int configure(float repulsion,float distance,float center,float collision,float cluster) {
    if(!finite(repulsion)||!finite(distance)||!finite(center)||!finite(collision)||!finite(cluster)
       ||repulsion<0||repulsion>5000||distance<10||distance>400||center<0||center>0.1f
       ||collision<0||collision>3||cluster<0||cluster>0.1f) return 0;
    charge=repulsion; length=distance; gravity=center; separation=collision; grouping=cluster;
    alpha=1; ticks=0; return 1;
}
EXPORT("pin") int pin(int i,float x,float y,int fixed) {
    if(i<0||i>=n||!finite(x)||!finite(y)||absf(x)>100000||absf(y)>100000) return 0;
    nodes[i].x=x; nodes[i].y=y; nodes[i].vx=0; nodes[i].vy=0; nodes[i].pinned=fixed!=0;
    alpha=maxf(alpha,0.45f); ticks=0; return 1;
}
EXPORT("reheat") void reheat(void) { alpha=1; ticks=0; }
EXPORT("seal") int seal(void) {
    for(int i=0;i<m;i++) if(edges[i].a<0||edges[i].a>=n||edges[i].b<0||edges[i].b>=n) return 0;
    ready=1; return 1;
}

/* Build a deterministic, compact spatial tree by partitioning index ranges.
 * No recursive insertion chains, unbounded buckets or pointer allocations.
 * Coincident coordinates stop splitting and use exact pair forces below.
 */
static int build(int start,int end,float x,float y,float half,int depth) {
    int id=qn++; Quad *q=&quads[id];
    *q=(Quad){x,y,half,0,0,0,0,start,end,{-1,-1,-1,-1}};
    for(int i=start;i<end;i++) {
        Node *a=&nodes[order[i]];
        q->mass+=a->mass; q->mx+=a->x*a->mass; q->my+=a->y*a->mass;
        q->radius=maxf(q->radius,a->radius);
    }
    q->mx/=q->mass; q->my/=q->mass;
    if(end-start<=4||depth>=24||half<0.01f||qn+4>=QMAX) return id;
    int counts[4]={0,0,0,0};
    for(int i=start;i<end;i++) {
        Node *a=&nodes[order[i]]; int side=(a->x>=x)+2*(a->y>=y); counts[side]++;
    }
    int offsets[4]; offsets[0]=start;
    for(int i=1;i<4;i++) offsets[i]=offsets[i-1]+counts[i-1];
    int at[4]; for(int i=0;i<4;i++) at[i]=offsets[i];
    for(int i=start;i<end;i++) {
        Node *a=&nodes[order[i]]; int side=(a->x>=x)+2*(a->y>=y); temporary[at[side]++]=order[i];
    }
    for(int i=start;i<end;i++) order[i]=temporary[i];
    for(int i=0;i<4;i++) if(counts[i]) {
        /* Descendants may consume the remaining tree; stop before each child.
         * Fall back to an exact leaf for this entire range if capacity is low.
         */
        if(qn+4>=QMAX) { for(int j=0;j<4;j++) q->child[j]=-1; return id; }
        q->child[i]=build(offsets[i],offsets[i]+counts[i],x+(i&1?half:-half)*0.5f,
                         y+(i&2?half:-half)*0.5f,half*0.5f,depth+1);
    }
    return id;
}
static void pair_force(int i,int j) {
    if(i==j) return;
    Node *a=&nodes[i],*b=&nodes[j]; float dx=a->x-b->x,dy=a->y-b->y;
    if(absf(dx)+absf(dy)<0.0001f) {
        /* Stable anti-symmetric perturbation: no entropy or time imports. */
        unsigned lo=i<j?i:j, hi=i<j?j:i;
        unsigned h=(lo+1u)*2654435761u ^ (hi+1u)*2246822519u;
        dx=(float)((int)(h&1023u)-511)*0.001f;
        dy=(float)((int)((h>>10)&1023u)-511)*0.001f;
        if(i>j) { dx=-dx;dy=-dy; }
        if(absf(dx)+absf(dy)<0.01f) dx=i<j?0.1f:-0.1f;
    }
    float d2=dx*dx+dy*dy, d=root(d2), safe=maxf(d,0.01f);
    float f=charge*b->mass*alpha/(d2+100);
    float overlap=a->radius+b->radius+8-d;
    if(overlap>0) f+=overlap*separation*0.28f/safe;
    a->fx+=dx*f; a->fy+=dy*f;
}
static void repel(int i,int id) {
    visits++; Node *a=&nodes[i]; Quad *q=&quads[id];
    int leaf=q->child[0]<0&&q->child[1]<0&&q->child[2]<0&&q->child[3]<0;
    if(leaf) { for(int at=q->start;at<q->end;at++) { visits++; pair_force(i,order[at]); } return; }
    float dx=a->x-q->mx,dy=a->y-q->my,d2=dx*dx+dy*dy;
    int contains=absf(a->x-q->x)<=q->half&&absf(a->y-q->y)<=q->half;
    float near=q->half*2+a->radius+q->radius+8;
    if(!contains && q->half*q->half*4 < 0.64f*d2 && d2>near*near) {
        float f=charge*q->mass*alpha/(d2+100); a->fx+=dx*f; a->fy+=dy*f; return;
    }
    for(int k=0;k<4;k++) if(q->child[k]>=0) repel(i,q->child[k]);
}
static void step(void) {
    float xmin=nodes[0].x,xmax=xmin,ymin=nodes[0].y,ymax=ymin;
    for(int i=0;i<n;i++) {
        Node *a=&nodes[i]; order[i]=i; a->fx=0; a->fy=0;
        if(a->x<xmin)xmin=a->x; if(a->x>xmax)xmax=a->x;
        if(a->y<ymin)ymin=a->y; if(a->y>ymax)ymax=a->y;
    }
    qn=0; build(0,n,(xmin+xmax)*0.5f,(ymin+ymax)*0.5f,maxf(maxf(xmax-xmin,ymax-ymin)*0.51f,1),0);
    for(int i=0;i<n;i++) repel(i,0);
    for(int i=0;i<m;i++) {
        Edge e=edges[i]; if(e.a==e.b) continue;
        Node *a=&nodes[e.a],*b=&nodes[e.b]; float dx=b->x-a->x,dy=b->y-a->y;
        float d=maxf(root(dx*dx+dy*dy),0.1f);
        float f=(d-length)*0.055f*alpha/d;
        a->fx+=dx*f; a->fy+=dy*f; b->fx-=dx*f; b->fy-=dy*f;
        if(e.directed) {
            /* A soft left-to-right preference, not a hard rank constraint.
             * Equal/opposite forces also handle cycles without a special case.
             * The usual spring, repulsion and collision remain active. */
            float along=(dx-length)*flow_strength*alpha;
            a->fx+=along; b->fx-=along;
            float cross=dy*flow_strength*0.2f*alpha;
            a->fy+=cross; b->fy-=cross;
        }
    }
    energy=0;
    for(int i=0;i<n;i++) {
        Node *a=&nodes[i];
        if(a->pinned) { a->vx=0;a->vy=0;continue; }
        a->fx+=(-a->x*gravity+(a->tx-a->x)*grouping)*alpha;
        a->fy+=(-a->y*gravity+(a->ty-a->y)*grouping)*alpha;
        a->vx=clamp((a->vx+a->fx/a->mass)*0.78f,-14,14);
        a->vy=clamp((a->vy+a->fy/a->mass)*0.78f,-14,14);
        a->x=clamp(a->x+a->vx,-100000,100000); a->y=clamp(a->y+a->vy,-100000,100000);
        energy+=a->vx*a->vx+a->vy*a->vy;
    }
    energy/=n; alpha*=0.984f; ticks++;
}
EXPORT("step") float advance(int count) {
    if(!ready||count<1||count>8) return -1;
    if(!n) { alpha=0;return 0; }
    for(int i=0;i<count&&ticks<800;i++) step();
    return energy;
}
EXPORT("positions") float *output(void) {
    for(int i=0;i<n;i++) { positions[i*2]=nodes[i].x; positions[i*2+1]=nodes[i].y; }
    return positions;
}
EXPORT("temperature") float temperature(void) { return alpha; }
EXPORT("ticks") int tick_count(void) { return ticks; }
EXPORT("visits") double visit_count(void) { return (double)visits; }
EXPORT("capacity") int capacity(void) { return NMAX; }
