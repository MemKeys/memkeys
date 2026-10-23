#!/usr/bin/env python3
"""ISI-6333 full-load SLO: p95 <= 150ms @ depth <= 3 on the traverse path,
under realistic concurrency, against live MemKeys on k8squad-test (via
port-forward to the /memory/* transport). Seeds a graph, warms up, then drives
concurrent POST /memory/graph/traverse and reports the latency distribution."""
import base64, hashlib, hmac, http.client, json, os, sys, time, threading
from concurrent.futures import ThreadPoolExecutor

HOST, PORT = "127.0.0.1", 13000
TENANT = "slo-6333"
N_FACTS = 150
WARMUP = 100
CONC = 32
TOTAL = 3000
DEPTH = 3

def b64url(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()

def mint():
    secret = open(os.path.join(os.path.dirname(__file__), ".slo-secret"), "rb").read()
    h = b64url(b'{"alg":"HS256","typ":"JWT"}')
    p = b64url(json.dumps({"company_id": TENANT, "sub": "testing-architect",
                           "run_id": "isi6333-slo", "exp": int(time.time()) + 3600},
                          separators=(",", ":")).encode())
    sig = b64url(hmac.new(secret, f"{h}.{p}".encode(), hashlib.sha256).digest())
    return f"{h}.{p}.{sig}"

TOKEN = mint()
HDR = {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}

_tl = threading.local()
def conn():
    c = getattr(_tl, "c", None)
    if c is None:
        c = _tl.c = http.client.HTTPConnection(HOST, PORT, timeout=10)
    return c

def call(method, path, body):
    data = json.dumps(body) if body is not None else None
    for attempt in (1, 2):
        c = conn()
        try:
            c.request(method, path, body=data, headers=HDR)
            r = c.getresponse()
            payload = r.read()
            return r.status, payload
        except Exception:
            try: c.close()
            except Exception: pass
            _tl.c = None
            if attempt == 2:
                raise

def seed():
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    ok = 0
    def store(i):
        env = {"source": {"kind": "gateway_flush", "emitted_at": now},
               "entity": {"project": TENANT, "issue": f"issue-{i % 10}",
                          "agent": f"agent-{i % 5}", "run": f"run-{i % 10}"},
               "fact": {"body": f"SLO seed fact #{i}: traverse load fixture for ISI-6333 depth<=3 concurrency test",
                        "subject": f"slo-seed-{i}"}}
        s, _ = call("POST", "/memory/facts", env)
        return s in (200, 201)
    with ThreadPoolExecutor(max_workers=16) as ex:
        ok = sum(ex.map(store, range(N_FACTS)))
    return ok

# Opaque start node id: tenant|entity.project|value  (kind.as_str() is dotted)
START = {"kind": "entity_project", "id": f"{TENANT}|entity.project|{TENANT}"}
TRAV_BODY = {"start": START, "edges": ["entity_fact", "produced_by", "supersedes"],
             "direction": "both", "depth": DEPTH, "deadline_ms": 500}

def one_traverse():
    t0 = time.perf_counter()
    s, payload = call("POST", "/memory/graph/traverse", TRAV_BODY)
    dt = (time.perf_counter() - t0) * 1000.0
    return s, dt, len(payload)

def run_load(total, conc, label):
    lat = []
    errors = 0
    sizes = []
    lock = threading.Lock()
    err = [0]
    def worker2(_):
        s, dt, sz = one_traverse()
        with lock:
            lat.append(dt); sizes.append(sz)
            if s != 200: err[0] += 1
    t0 = time.perf_counter()
    with ThreadPoolExecutor(max_workers=conc) as ex:
        list(ex.map(worker2, range(total)))
    wall = time.perf_counter() - t0
    lat.sort()
    def pct(p): return lat[min(len(lat)-1, int(len(lat)*p))]
    print(f"[{label}] n={len(lat)} conc={conc} errors={err[0]} "
          f"wall={wall:.2f}s rps={len(lat)/wall:.0f} nodes_payload_avg={sum(sizes)//len(sizes)}B")
    print(f"[{label}] p50={pct(0.50):.1f}ms p90={pct(0.90):.1f}ms p95={pct(0.95):.1f}ms "
          f"p99={pct(0.99):.1f}ms max={lat[-1]:.1f}ms min={lat[0]:.1f}ms")
    return pct(0.95), err[0]

if __name__ == "__main__":
    print(f"seeding {N_FACTS} facts under tenant={TENANT} ...")
    ok = seed()
    print(f"seeded {ok}/{N_FACTS} facts")
    # Graph edge projection is async (drained on the ~30s maintenance tick);
    # poll the start node until its entity_fact edges materialize.
    deadline = time.time() + 120
    nodes = 0
    while time.time() < deadline:
        s, payload = call("POST", "/memory/graph/traverse", TRAV_BODY)
        if s == 200:
            doc = json.loads(payload)
            nodes = len(doc.get("nodes", []))
            if nodes > 1:
                print(f"graph ready: status={s} nodes={nodes} edges={len(doc.get('edges', []))} "
                      f"truncated={doc.get('truncated')} reason={doc.get('reason')}")
                break
        print(f"  waiting for edge projection... status={s} nodes={nodes}")
        time.sleep(5)
    else:
        print("ERROR: start node never materialized edges within 120s", file=sys.stderr)
        sys.exit(2)
    run_load(WARMUP, 8, "warmup")
    p95, errs = run_load(TOTAL, CONC, "LOAD")
    verdict = "PASS" if (p95 <= 150.0 and errs == 0) else "FAIL"
    print(f"=== SLO depth<={DEPTH} @ conc={CONC}: p95={p95:.1f}ms (target<=150ms) errors={errs} => {verdict} ===")
    sys.exit(0 if verdict == "PASS" else 1)
