#!/usr/bin/env python3
"""Scenario 2: broker failure (docker pause) under fail-closed / fail-open. Real binary, real Kafka."""
import os, re, socket, subprocess, sys, time, json, uuid, http.client

D = "/tmp/rust-proxy-qual/kreal"
BIN = "/Users/schohn/dev/rustifyProxy/TrafficCapture/captureProxy/target/release/capture-proxy"
POLICY, TOPIC = sys.argv[1], sys.argv[2]
TAG = "s2c" if POLICY == "fail-closed" else "s2o"
ORIG, PORT = (18731, 18732) if POLICY == "fail-closed" else (18741, 18742)
RUN = uuid.uuid4().hex[:8]
EV = open(f"{D}/{TAG}-events.log", "w")
OLOG = f"{D}/{TAG}-origin.log"
open(OLOG, "w").close()

def now_ms(): return int(time.time() * 1000)
def ev(msg):
    line = f"{now_ms()} {msg}"; print(line, flush=True); EV.write(line + "\n"); EV.flush()
def plog(): return open(f"{D}/{TAG}-proxy.log", errors="replace").read()
def origin_has(marker): return marker in open(OLOG).read()

def req(method, path, body=None, timeout=40):
    t0 = time.time()
    try:
        c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=timeout)
        c.request(method, path, body=body, headers={"Content-Type": "application/json"} if body else {})
        r = c.getresponse(); r.read(); c.close()
        return f"HTTP {r.status} in {time.time()-t0:.2f}s"
    except Exception as e:
        return f"ERROR {type(e).__name__}: {e} after {time.time()-t0:.2f}s"

def docker(*a): return subprocess.run(["docker", *a], capture_output=True, text=True).returncode

orig = subprocess.Popen([sys.executable, "-I", f"{D}/origin.py", str(ORIG), OLOG]); time.sleep(0.5)
env = dict(os.environ, MIGRATIONS_ROOT_LOG_LEVEL="debug")
P = subprocess.Popen([BIN, "--bind-address", "127.0.0.1", "--listen-port", str(PORT),
    "--destination-uri", f"http://127.0.0.1:{ORIG}", "--kafka-brokers", "127.0.0.1:29700",
    "--kafka-topic", TOPIC, "--heartbeat-interval-seconds", "2", "--heartbeat-expiration-interval-seconds", "6",
    "--capture-failure-policy", POLICY], env=env, stdout=open(f"{D}/{TAG}-proxy.log", "w"), stderr=subprocess.STDOUT)
end = time.time() + 60
while time.time() < end:
    try: socket.create_connection(("127.0.0.1", PORT), 0.5).close(); break
    except OSError: time.sleep(0.3)
ev(f"proxy pid={P.pid} policy={POLICY} listening run={RUN}")

res = {"policy": POLICY, "run": RUN}
for i in range(3):
    m = f"PRE-{RUN}-{i}"
    ev(f"PUT {m}: {req('PUT', f'/idx/_doc/pre{i}', json.dumps({'marker': m}))} origin_received={origin_has(m)}")
time.sleep(3)  # let the pre-failure traffic be captured and acknowledged
ev("docker pause kreal-kafka rc=%d" % docker("pause", "kreal-kafka")); t_pause = time.time()
mk = f"DURING-{RUN}"
ev(f"PUT {mk} sent")
r = req("PUT", "/idx/_doc/during", json.dumps({"marker": mk}))
ev(f"PUT {mk} result: {r}")

if POLICY == "fail-closed":
    try: rc = P.wait(60)
    except subprocess.TimeoutExpired: rc = "STILL-RUNNING"
    t_exit = time.time()
    ev(f"proxy exit={rc} {t_exit - t_pause:.2f}s after pause")
    time.sleep(2)
    ev(f"origin received {mk}: {origin_has(mk)} (proxy dead; checked 2s after exit)")
    res.update(exit=rc, exit_after_pause_s=round(t_exit - t_pause, 2), during_forwarded=origin_has(mk))
    ev("docker unpause kreal-kafka rc=%d" % docker("unpause", "kreal-kafka"))
else:
    ev(f"origin received {mk}: {origin_has(mk)}")
    g = f"GET-{RUN}"
    ev(f"GET /idx/_search?{g}: {req('GET', f'/idx/_search?q={g}')} origin_received={origin_has(g)}")
    m2 = f"POSTCOMP-{RUN}"
    ev(f"PUT {m2}: {req('PUT', '/idx/_doc/postcomp', json.dumps({'marker': m2}))} origin_received={origin_has(m2)}")
    time.sleep(5); ev(f"alive after compromise: {P.poll() is None}")
    ev("docker unpause kreal-kafka rc=%d" % docker("unpause", "kreal-kafka")); t_unpause = now_ms()
    time.sleep(15)
    m3 = f"AFTERUNPAUSE-{RUN}"
    ev(f"PUT {m3}: {req('PUT', '/idx/_doc/after', json.dumps({'marker': m3}))} origin_received={origin_has(m3)}")
    g3 = f"GETAFTER-{RUN}"
    ev(f"GET ?{g3}: {req('GET', f'/idx/_search?q={g3}')} origin_received={origin_has(g3)}")
    time.sleep(8)
    alive = P.poll() is None; ev(f"alive 23s after unpause: {alive}")
    P.terminate()
    try: rc = P.wait(30)
    except subprocess.TimeoutExpired: rc = "STILL-RUNNING"
    ev(f"proxy exit after SIGTERM={rc}")
    res.update(exit=rc, t_unpause_ms=t_unpause, alive=alive,
               during_forwarded=origin_has(mk), after_forwarded=origin_has(m3))
L = plog()
for e in ["capture_compromised", "capture_failure_terminating", "capture_shutdown_summary", "capture_gap", "pass_through"]:
    res[f"count_{e}"] = len(re.findall(e, L))
ev("RESULT " + json.dumps(res))
orig.terminate()
json.dump(res, open(f"{D}/{TAG}-result.json", "w"))
