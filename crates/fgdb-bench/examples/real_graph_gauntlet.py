"""Compare durable fgdb CLI and HTTP queries with live Kùzu on identical graphs.

Run `bash scripts/real_graph_gauntlet.sh run road-ny WORK --bin-dir target/release`.
WORK must be a new directory. Use --archive for an existing compressed download.
Requires Python 3.11+ and the optional Kùzu Python binding (tested with 0.11.3).
The prepare and compare commands allow independent input verification and repeats.
The standalone `scale WORK --bin-dir target/release` command repeats the
2026-10-04 seeded 900k-edge CLI probe, including its durable one-row write.
"""

import argparse
import gzip
import hashlib
import http.client
import io
import json
import os
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request
from contextlib import contextmanager
from pathlib import Path


SNAP = "https://snap.stanford.edu/data/"
# The original DIMACS download now returns 404. This snapshot retains the
# exact compressed distance graph from the original challenge manifest.
ROAD = ("https://raw.githubusercontent.com/Nigamkumarsingh/DAA_LAB09_ASSIGNMENT/"
        "eb9c12c8101d89857cbc84868fc6ffb2622bd8ec/data/")
# Compressed size and SHA-256 from the original public dataset manifest.
DATASETS = {
    "road-ny": (ROAD + "USA-road-d.NY.gr.gz", 3667907, "7b2446c7ffe6179efbc42af6812448e8cb782f12968ca2bb1d50d979343056d4"),
    "web-google": (SNAP + "web-Google.txt.gz", 21168784, "bcac0af0471d749f4a8c010bca92b61cf2868a0570741de06892fc062f265ea6"),
    "facebook": (SNAP + "facebook_combined.txt.gz", 218576, "125e84db872eeba443d270c70315c256b0af43a502fcfe51f50621166ad035d7"),
    "wiki-vote": (SNAP + "wiki-Vote.txt.gz", 290339, "7d3e53626e14b8b09fb3b396bece9d481ad606bd64ceab066349ff57d4ada7fc"),
    "ca-hepth": (SNAP + "ca-HepTh.txt.gz", 210596, "7235c7e11887f8a32dfd03a3aa0c77efdf89d41591a959c7d6e4ca11b1299bb5"),
    "email-enron": (SNAP + "email-Enron.txt.gz", 1055993, "55cfead79b1f0161786179a48796c2a119bd7026a246238b257b6be9d8b69b68"),
    "cit-hepph": (SNAP + "cit-HepPh.txt.gz", 1664504, "917e77b3344aed33fd2d849443c9512b7c528b9dc87251d4245fb3777bbe4128"),
    "epinions": (SNAP + "soc-Epinions1.txt.gz", 1630437, "69a2dab71fa5e3a0715487599fc16ca17ddc847379325a6c765bbad6e3e36938"),
    "roadnet-ca": (SNAP + "roadNet-CA.txt.gz", 17892860, "383f7b14424530a9e25c392969a9c19dace32ebcfd141f0967a1433498c2acdd"),
    "pokec": (SNAP + "soc-pokec-relationships.txt.gz", 132454730, "1a23e0ec8a4e497752125f6b3f01696fea7fcdb696fa61d1e822faf4d0d69b14"),
    "livejournal": (SNAP + "soc-LiveJournal1.txt.gz", 259619239, "d7bcd5a87b88c896c35fdb9611e804c3f4033c39b58c4c9ea3ba53c680d516d8"),
}
BINDINGS = ["--label", "N=1", "--relation", "E=1", "--property", "id=1", "--property", "w=2"]
LIMITS = ["--max-snapshot-records", "4000000000", "--max-work-units", "1000000000000",
          "--max-result-rows", "10000000", "--max-scratch-entries", "4000000000"]
INPUTS = ("graph.ndjson", "vertices.csv", "edges.csv")
SCALE_SHA256 = "3f223276e113a82ab7bc1259b751af224f546c361fd6d29bf4cc8cc7894f8e19"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def normalized_graph(lines, weighted):
    """Keep original IDs and the first weight for each directed, non-loop arc."""
    adjacency = {}
    vertices = set()
    for line in lines:
        fields = line.replace(",", " ").split()
        if not fields or fields[0].startswith(("#", "%")):
            continue
        if weighted:
            if fields[0] == "p":
                vertices.update(range(1, int(fields[2]) + 1))
                continue
            if fields[0] == "c":
                continue
            tag, src, dst, weight = fields
            if tag != "a":
                raise ValueError(f"unexpected DIMACS row {line!r}")
            weight = int(weight)
        else:
            src, dst = fields
            weight = None
        src, dst = int(src), int(dst)
        if src != dst:
            vertices.update((src, dst))
            adjacency.setdefault(src, {}).setdefault(dst, weight)
    return sorted(vertices), adjacency


def oracle(vertices, adjacency):
    hub = max(vertices, key=lambda vertex: len(adjacency.get(vertex, ())))
    outgoing = adjacency.get(hub, ())
    return dict(vertices=len(vertices), edges=sum(map(len, adjacency.values())),
                first=vertices[0], hub=hub, degree=len(outgoing),
                two_hop=sum(len(adjacency.get(neighbor, ())) for neighbor in outgoing))


def write_inputs(work, vertices, adjacency, weighted):
    with (work / "vertices.csv").open("x") as nodes, (work / "edges.csv").open("x") as edges, (work / "graph.ndjson").open("x") as graph:
        print("id", file=nodes)
        print("src,dst,w" if weighted else "src,dst", file=edges)
        for vid in vertices:
            print(vid, file=nodes)
            print(f'{{"kind":"vertex","key":"v{vid}","labels":["N"],"props":{{"id":"int:{vid}"}}}}', file=graph)
        eid = 0
        for src in sorted(adjacency):
            for dst, weight in sorted(adjacency[src].items()):
                suffix = f',"props":{{"w":"int:{weight}"}}' if weighted else ""
                print(f"{src},{dst}" + (f",{weight}" if weighted else ""), file=edges)
                print(f'{{"kind":"edge","key":"e{eid}","source":"v{src}","destination":"v{dst}","relation":"E"{suffix}}}', file=graph)
                eid += 1


def prepare(work, name, archive):
    url, size, expected = DATASETS[name]
    work.mkdir()
    if archive is None:
        archive = work / url.rsplit("/", 1)[-1]
        request = urllib.request.Request(url, headers={
            "User-Agent": "frankengraphdb-gauntlet (+https://github.com/Dicklesworthstone/frankengraphdb)"
        })
        with urllib.request.urlopen(request, timeout=120) as source, archive.open("xb") as out:
            shutil.copyfileobj(source, out)
    if archive.stat().st_size != size or digest(archive) != expected:
        raise ValueError(f"{archive} differs from the pinned {name} archive")
    weighted = name == "road-ny"
    with gzip.open(archive, "rt", encoding="ascii") as source:
        vertices, adjacency = normalized_graph(source, weighted)
    write_inputs(work, vertices, adjacency, weighted)
    dataset = dict(name=name, raw_hash=expected, **oracle(vertices, adjacency),
                   normalized={f: digest(work / f) for f in INPUTS})
    (work / "dataset.json").write_text(json.dumps(dataset, indent=2) + "\n")


def read_dataset(work, name):
    dataset = json.loads((work / "dataset.json").read_text())
    if dataset["name"] != name or dataset["raw_hash"] != DATASETS[name][2]:
        raise ValueError("dataset identity differs from the pinned source")
    if dataset["normalized"] != {f: digest(work / f) for f in INPUTS}:
        raise ValueError("normalized graph files differ from preparation")
    return dataset


def queries(dataset):
    hub = dataset["hub"]
    match = f"MATCH (n:N {{id: {hub}}})"
    return {
        "point": (match + " RETURN n.id AS value", hub),
        "vertices": ("MATCH (n:N) RETURN count(*) AS value", dataset["vertices"]),
        "edges": ("MATCH ()-[:E]->() RETURN count(*) AS value", dataset["edges"]),
        "degree": (match + "-[:E]->(m) RETURN count(*) AS value", dataset["degree"]),
        "two_hop": (match + "-[:E]->(m)-[:E]->(k) RETURN count(*) AS value", dataset["two_hop"]),
    }


@contextmanager
def kuzu_connection(work, read_only):
    import kuzu
    db = kuzu.Database(str(work / "kuzu"), buffer_pool_size=2 * 1024**3,
                       max_num_threads=2, read_only=read_only)
    try:
        conn = kuzu.Connection(db, num_threads=2)
        try:
            yield conn
        finally:
            conn.close()
    finally:
        db.close()


def load_fgdb(work, bins, bindings, rows):
    db = ["--db", work / "graph", "--key-file", work / "g.keys"]
    started = time.perf_counter()
    subprocess.run([bins / "fgdbd", "keygen", "--database", work / "g.keys"], check=True)
    subprocess.run([bins / "fgdb", "--robot", "create", *db], check=True)
    with (work / "load.ndjson").open("x") as output:
        subprocess.run([bins / "fgdb", "--robot", "load", *db, *bindings, "--input", work / "graph.ndjson",
                        "--checkpoint", work / "checkpoint", "--max-total-key-bytes", "1073741824",
                        "--max-source-rows", str(rows)],
                       check=True, stdout=output)
    return time.perf_counter() - started


def load(work, dataset, bins):
    # Refuse partial or completed prior loads, so recovery cannot silently
    # change the graph or the reported load time.
    for path in ("graph", "kuzu", "g.keys", "checkpoint", "load.ndjson"):
        if (work / path).exists():
            raise FileExistsError(work / path)
    fgdb_seconds = load_fgdb(work, bins, BINDINGS, dataset["vertices"] + dataset["edges"])
    started = time.perf_counter()
    with kuzu_connection(work, False) as conn:
        conn.execute("CREATE NODE TABLE N(id INT64 PRIMARY KEY)")
        weight = ", w INT64" if dataset["name"] == "road-ny" else ""
        conn.execute(f"CREATE REL TABLE E(FROM N TO N{weight})")
        for table, filename in [("N", "vertices.csv"), ("E", "edges.csv")]:
            path = str(work / filename).replace("\\", "\\\\").replace("'", "\\'")
            conn.execute(f"COPY {table} FROM '{path}' (HEADER=true)")
    (work / "load-times.json").write_text(json.dumps(dict(fgdb_s=fgdb_seconds, kuzu_s=time.perf_counter() - started)))


def result_digest(rows, expected):
    if rows != [[expected]] or type(rows[0][0]) is not int:
        raise ValueError(f"wrong result: expected one integer {expected}, received {rows!r}")
    return hashlib.sha256(json.dumps(rows, separators=(",", ":")).encode()).hexdigest()


def kuzu_rows(conn, query):
    result = conn.execute(query)
    if result.get_column_names() != ["value"]:
        raise ValueError("unexpected Kùzu columns")
    return list(result)


def decode_cells(cells):
    row = []
    for cell in cells:
        if cell["type"] not in ("int", "count", "uint", "wideint"):
            raise ValueError(f"unexpected scalar {cell!r}")
        value = cell["value"]
        if type(value) is not int:
            digits = value.removeprefix("-") if isinstance(value, str) else ""
            if not digits.isascii() or not digits.isdecimal():
                raise ValueError(f"unexpected integer representation {value!r}")
        row.append(int(value))
    return row


@contextmanager
def serve(work, bins, output):
    server = bins / "fgdbd"
    issuer = output / "issuer.key"
    key = work / "g.keys"
    subprocess.run([server, "keygen", "--issuer", issuer], check=True, stdout=subprocess.DEVNULL)
    token = subprocess.check_output([server, "token", "--rights", "read", "--key-file", key,
                                    "--issuer-key-file", issuer, "--max-nodes", "4000000000",
                                    "--max-work", "1000000000000", "--max-rows", "10000000"], text=True).strip()
    with (output / "server.stderr").open("x") as errors:
        proc = subprocess.Popen([server, "serve", "--listen", "127.0.0.1:0", "--http-listen", "127.0.0.1:0",
                                 "--database", f"g={work / 'graph'}", "--key-file", key, "--issuer-key-file", issuer,
                                 *BINDINGS, *LIMITS], stdout=subprocess.PIPE, stderr=errors, text=True)
        try:
            for line in proc.stdout:
                event = json.loads(line)
                if event.get("event") == "listening" and event.get("protocol") == "http":
                    yield event["addr"], {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
                    break
            else:
                raise RuntimeError(f"server did not listen, see {errors.name}")
        finally:
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
            # fgdbd emits a final stopped event before returning its exit code.
            # Keep the pipe open through shutdown and preserve those events.
            tail, _ = proc.communicate(timeout=180)
            (output / "server-shutdown.ndjson").write_text(tail)
            code = proc.returncode
            (output / "server.exit").write_text(f"{code}\n")
            if code:
                raise RuntimeError(f"fgdbd exited {code}, see {errors.name}")


def http_rows(endpoint, headers, query):
    conn = http.client.HTTPConnection(endpoint, timeout=900)
    try:
        conn.request("POST", "/v1/databases/g/query", json.dumps({"statement": query, "parameters": {}}), headers)
        response = conn.getresponse()
        payload = json.loads(response.read())
        if response.status != 200:
            raise ValueError(f"HTTP {response.status}: {payload}")
        return [decode_cells(row) for row in payload["rows"]]
    finally:
        conn.close()


def measure(work, bins, mode, query, endpoint, headers, conn):
    if mode == "fgdbd-http":
        return http_rows(endpoint, headers, query)
    if mode == "kuzu-connection":
        return kuzu_rows(conn, query)
    if mode == "kuzu-process":
        result = subprocess.check_output([sys.executable, __file__, "_kuzu", work, query], timeout=900)
        return json.loads(result)
    return cli_rows(work, bins, BINDINGS, query)


def cli_rows(work, bins, bindings, query):
    args = ["query", "--db", work / "graph", "--key-file", work / "g.keys", *bindings, *LIMITS, query]
    return robot_rows(bins, args, "rows")


def robot_rows(bins, args, kind, expected_seq=None):
    result = subprocess.run([bins / "fgdb", "--robot", *args], capture_output=True, timeout=900)
    events = [json.loads(line) for line in result.stdout.splitlines()]
    if result.returncode or not events or events[-1].get("kind") != kind or events[-1]["event"] != "result":
        raise ValueError(f"{args[0]} exited {result.returncode}: {events[-1:]} {result.stderr.decode()}")
    if expected_seq is not None and events[-1]["seq"] != expected_seq:
        raise ValueError(f"unexpected commit revision: {events[-1]}")
    return [decode_cells(event["cells"]) for event in events if event["event"] == "row"]


def commit_id(value):
    if not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", value):
        raise argparse.ArgumentTypeError("supply the full product build commit")
    return value


def compare(work, dataset, bins, rounds, product_commit):
    import kuzu
    output = Path(tempfile.mkdtemp(prefix="comparison-", dir=work))
    print(output, flush=True)
    participants = {name: digest(bins / name) for name in ("fgdb", "fgdbd")}
    revision = subprocess.run(["git", "-C", Path(__file__).resolve().parent, "rev-parse", "HEAD"],
                              capture_output=True, text=True)
    metadata = dict(dataset=dataset, host=platform.uname()._asdict(), binary_sha256=participants,
                    harness_commit=revision.stdout.strip() if revision.returncode == 0 else None,
                    supplied_product_commit=product_commit,
                    kuzu=kuzu.__version__, python=sys.version, rounds=rounds,
                    timing="Fresh client processes use the shared filesystem cache. Resident HTTP includes reconnect. Resident Kuzu reuses a connection.")
    (output / "environment.json").write_text(json.dumps(metadata, indent=2))
    timings = {}
    modes = ("fgdb-cli", "kuzu-process", "fgdbd-http", "kuzu-connection")
    with serve(work, bins, output) as (endpoint, headers), kuzu_connection(work, True) as conn, (output / "samples.ndjson").open("x") as samples:
        for rep in range(rounds):
            for workload, (query, expected) in queries(dataset).items():
                for mode in (modes if rep % 2 == 0 else reversed(modes)):
                    started = time.perf_counter_ns()
                    rows = measure(work, bins, mode, query, endpoint, headers, conn)
                    elapsed = time.perf_counter_ns() - started
                    checksum = result_digest(rows, expected)
                    timings.setdefault((workload, mode), []).append(elapsed)
                    print(json.dumps(dict(repetition=rep, workload=workload, engine=mode, wall_ns=elapsed,
                                          statement=query, answer=expected, row_sha256=checksum, loadavg=os.getloadavg())),
                          file=samples, flush=True)
    summary = []
    for (workload, mode), times in timings.items():
        median = statistics.median(times)
        summary.append(dict(workload=workload, engine=mode, samples=len(times), median_ns=median,
                            median_absolute_deviation_ns=statistics.median(abs(t - median) for t in times),
                            min_ns=min(times), max_ns=max(times)))
    (output / "scoreboard.json").write_text(json.dumps(dict(environment=metadata, measurements=summary), indent=2) + "\n")


def scale(work, bins, product_commit):
    """Preserve parallel relationships, self loops, original order and schema."""
    work.mkdir()
    vertices, edges = 45_000, 900_000
    rng = random.Random(13)
    degrees = [0] * vertices
    hub_neighbors = []
    graph = work / "graph.ndjson"
    with graph.open("x") as out:
        for vid in range(vertices):
            print(f'{{"kind":"vertex","key":"v{vid}","labels":["Person"],"props":{{"p":"int:{vid}"}}}}', file=out)
        for eid in range(edges):
            src = min(vertices - 1, int(vertices * rng.random() ** 3))
            dst = rng.randrange(vertices)
            degrees[src] += 1
            if src == 0:
                hub_neighbors.append(dst)
            print(f'{{"kind":"edge","key":"e{eid}","source":"v{src}","destination":"v{dst}","relation":"R","props":{{}}}}', file=out)
    if digest(graph) != SCALE_SHA256 or max(degrees) != degrees[0]:
        raise ValueError("scale fixture differs from the original seed-13 stream")
    # A two-edge trail may revisit a vertex, but may not traverse the same
    # relationship twice. That reuse is possible only after a first self-loop.
    two_hop = sum(degrees[v] - int(v == 0) for v in hub_neighbors)
    bindings = ["--label", "Person=1", "--property", "p=1", "--relation", "R=1"]
    load_seconds = load_fgdb(work, bins, bindings, vertices + edges)
    basis = json.loads((work / "checkpoint").read_text())["frontier"]
    steps = [
        ("point", "MATCH (n:Person {p: 1}) RETURN n.p", 1),
        ("hub", "MATCH (a:Person {p: 0})-[:R]->(b) RETURN count(b)", degrees[0]),
        ("two_hop", "MATCH (a:Person {p: 0})-[:R]->(b)-[:R]->(c) RETURN count(c)", two_hop),
        ("count", "MATCH ()-[:R]->() RETURN count(*)", edges),
        ("one_row_write", f"CREATE (n:Person {{p: {vertices}}}) RETURN n.p", vertices),
        ("reopen_write", f"MATCH (n:Person {{p: {vertices}}}) RETURN n.p", vertices),
        ("reopen_count", "MATCH (n:Person) RETURN count(*)", vertices + 1),
    ]
    with (work / "samples.ndjson").open("x") as samples:
        for workload, query, expected in steps:
            start = time.perf_counter_ns()
            if workload == "one_row_write":
                args = ["write", "--db", work / "graph", "--key-file", work / "g.keys",
                        *bindings, "--if-current", str(basis), query]
                rows = robot_rows(bins, args, "written", basis + 1)
            else:
                rows = cli_rows(work, bins, bindings, query)
            elapsed = time.perf_counter_ns() - start
            print(json.dumps(dict(workload=workload, statement=query, wall_ns=elapsed, answer=expected,
                                  row_sha256=result_digest(rows, expected), loadavg=os.getloadavg())),
                  file=samples, flush=True)
    (work / "scoreboard.json").write_text(json.dumps(dict(
        input_sha256=SCALE_SHA256, seed=13, vertices=vertices, edges=edges, load_s=load_seconds,
        samples="samples.ndjson", host=platform.uname()._asdict(), supplied_product_commit=product_commit,
        binary_sha256={f: digest(bins / f) for f in ("fgdb", "fgdbd")}), indent=2) + "\n")


def self_test():
    vertices, adj = normalized_graph(io.StringIO("p sp 4 5\na 2 1 7\na 2 1 9\na 1 2 4\na 2 3 1\na 4 4 8\n"), True)
    if vertices != [1, 2, 3, 4] or adj != {2: {1: 7, 3: 1}, 1: {2: 4}}:
        raise ValueError("DIMACS weights, isolates or deduplication changed")
    data = oracle(vertices, adj)
    if [answer for _, answer in queries(data).values()] != [2, 4, 3, 2, 1]:
        raise ValueError("independent degree or two-hop counting changed")
    vertices, adj = normalized_graph(io.StringIO("# snap\n8 8\n2 1\n1 2\n"), False)
    if vertices != [1, 2] or oracle(vertices, adj)["hub"] != 1:
        raise ValueError("SNAP loops or hub tie-break changed")
    result_digest([[3]], 3)
    for wrong in ([], [[True]], [[3.0]], [[2]], [[3], [3]]):
        try:
            result_digest(wrong, 3)
        except ValueError:
            continue
        raise ValueError(f"incorrect result accepted: {wrong}")
    print("Gauntlet normalization, answer oracle and refusal controls passed")


def main():
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    if sys.argv[1:2] == ["_kuzu"]:
        with kuzu_connection(Path(sys.argv[2]), True) as conn:
            print(json.dumps(kuzu_rows(conn, sys.argv[3])))
        return
    if sys.argv[1:2] == ["scale"]:
        parser = argparse.ArgumentParser(description=scale.__doc__)
        parser.add_argument("work_dir", type=Path)
        parser.add_argument("--bin-dir", type=Path, default=Path("target/release"))
        parser.add_argument("--product-commit", type=commit_id, help="source revision that produced the supplied binaries")
        args = parser.parse_args(sys.argv[2:])
        scale(args.work_dir.resolve(), args.bin_dir.resolve(), args.product_commit)
        return
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=["prepare", "load", "compare", "run"])
    parser.add_argument("dataset", choices=DATASETS)
    parser.add_argument("work_dir", type=Path)
    parser.add_argument("--archive", type=Path, help="already downloaded compressed archive")
    parser.add_argument("--bin-dir", type=Path, default=Path("target/release"), help="directory containing fgdb and fgdbd")
    parser.add_argument("--rounds", type=int, default=4)
    parser.add_argument("--product-commit", type=commit_id, help="source revision that produced the supplied binaries")
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    work, bins = args.work_dir.resolve(), args.bin_dir.resolve()
    if args.action != "prepare":
        try:
            import kuzu
        except ImportError:
            parser.error("install the external Kùzu Python binding for this comparison")
        for binary in (bins / "fgdb", bins / "fgdbd"):
            if not os.access(binary, os.X_OK):
                parser.error(f"missing executable {binary}")
    if args.action in ("prepare", "run"):
        prepare(work, args.dataset, args.archive)
    if args.action != "prepare":
        dataset = read_dataset(work, args.dataset)
        if args.action in ("load", "run"):
            load(work, dataset, bins)
        if args.action in ("compare", "run"):
            compare(work, dataset, bins, args.rounds, args.product_commit)


if __name__ == "__main__":
    main()
