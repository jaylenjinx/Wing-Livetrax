#!/usr/bin/env python3
"""Fake WING that answers patch queries, to test the live-console patch path."""
import json, os, queue, socket, struct, subprocess, sys, tempfile, threading, time

def ostr(s):
    b = s.encode() + b"\0"
    return b + b"\0" * ((4 - len(b) % 4) % 4)

def msg(addr, *args):
    tags, body = ",", b""
    for a in args:
        if isinstance(a, int):    tags += "i"; body += struct.pack(">i", a)
        elif isinstance(a, float):tags += "f"; body += struct.pack(">f", a)
        else:                     tags += "s"; body += ostr(a)
    return ostr(addr) + ostr(tags) + body

def rdstr(d, i):
    e = d.index(b"\0", i); return d[i:e].decode(errors="replace"), ((e + 4)//4)*4

def parse(d):
    addr, i = rdstr(d, 0)
    if i >= len(d): return addr, []
    tags, i = rdstr(d, i); args = []
    for t in tags[1:]:
        if t == "i":   args.append(struct.unpack_from(">i", d, i)[0]); i += 4
        elif t == "f": args.append(struct.unpack_from(">f", d, i)[0]); i += 4
        elif t == "s": s, i = rdstr(d, i); args.append(s)
    return addr, args

# A console patched the way a real one is: channel 3 lives on local input 5, so
# USB output 4 - not output 3 - carries it.
CH_NAMES = {1: "KICK", 2: "SNARE", 3: "VOX", 4: ""}
CH_INPUT = {1: ("LCL", 1), 2: ("LCL", 2), 3: ("LCL", 5), 4: ("OFF", 1)}
USB_OUT = {1: ("LCL", 1), 2: ("LCL", 2), 3: ("LCL", 3), 4: ("LCL", 5),
           5: ("MTX", 1), 6: ("MTX", 2), 7: ("OFF", 1)}
OBJECTS = {("mtx", 1): "REC", ("bus", 1): "MON 1", ("main", 1): "PA"}
SOCKETS = {("LCL", 3): "TALKBACK"}

class FakeWing(threading.Thread):
    def __init__(self, port=12223):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", port))
        self.q = queue.Queue()
        self.peer = None
        self.answered = 0

    def run(self):
        while True:
            data, src = self.sock.recvfrom(65536)
            self.peer = src
            addr, args = parse(data)
            if args:                       # a write, not a query
                self.q.put((addr, args))
                continue
            for reply in self.answer(addr):
                self.answered += 1
                self.sock.sendto(reply, src)

    def answer(self, addr):
        p = addr.strip("/").split("/")
        if p[0] == "ch" and len(p) == 3 and p[2] == "name":
            ch = int(p[1])
            if CH_NAMES.get(ch):
                return [msg(addr, CH_NAMES[ch])]
        if p[0] == "ch" and p[2:] == ["in", "conn", "grp"]:
            ch = int(p[1])
            if ch in CH_INPUT: return [msg(addr, CH_INPUT[ch][0])]
        if p[0] == "ch" and p[2:] == ["in", "conn", "in"]:
            ch = int(p[1])
            if ch in CH_INPUT: return [msg(addr, CH_INPUT[ch][1])]
        if p[:3] == ["io", "out", "USB"] and len(p) == 5:
            n = int(p[3])
            if n in USB_OUT:
                return [msg(addr, USB_OUT[n][0] if p[4] == "grp" else USB_OUT[n][1])]
        if p[:3] == ["io", "in", "LCL"] and len(p) == 5 and p[4] == "name":
            key = ("LCL", int(p[3]))
            if key in SOCKETS: return [msg(addr, SOCKETS[key])]
        if len(p) == 3 and p[2] == "name" and (p[0], int(p[1])) in OBJECTS:
            return [msg(addr, OBJECTS[(p[0], int(p[1]))])]
        return []

    def expect(self, addr, timeout=6.0, pred=None):
        end = time.time() + timeout
        while time.time() < end:
            try: m = self.q.get(timeout=max(0.05, end - time.time()))
            except queue.Empty: break
            if m[0] == addr and (pred is None or pred(m[1])): return m
        return None

class FakeDaw(threading.Thread):
    def __init__(self, port=13819):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", port))
        self.q = queue.Queue()
    def run(self):
        while True:
            data, src = self.sock.recvfrom(65536)
            addr, args = parse(data)
            self.q.put((addr, args))
            if addr == "/strip/list":
                for i in range(1, 8):
                    self.sock.sendto(msg("#reply", "AT", i, f"Track {i}", 1, 1, 0, 0), src)
                self.sock.sendto(msg("#reply", "end_route_list", 48000, 0), src)
    def expect(self, addr, timeout=6.0, pred=None):
        end = time.time() + timeout
        while time.time() < end:
            try: m = self.q.get(timeout=max(0.05, end - time.time()))
            except queue.Empty: break
            if m[0] == addr and (pred is None or pred(m[1])): return m
        return None

CONFIG = """
[wing]
host = "127.0.0.1"
port = 12223
local_port = 12226
channels = 4
name_poll_interval_ms = 0
[livetrax]
host = "127.0.0.1"
port = 13819
local_port = 13822
[names]
enabled = true
direction = "wing-to-daw"
debounce_ms = 100
[patch]
source = "console"
output_group = "USB"
[patch.live]
settle_ms = 600
[patch.live.group_sizes]
USB = 7
"""

def serve():
    """Run the fakes and idle, so a GUI can be pointed at them."""
    wing = FakeWing(); wing.start()
    daw = FakeDaw(); daw.start()
    tmp = tempfile.mkdtemp()
    cfg = os.path.join(tmp, "config.toml"); open(cfg, "w").write(CONFIG)
    print(cfg, flush=True)
    while True:
        time.sleep(1)

def main():
    if sys.argv[1] == "--serve":
        serve()
    binary = os.path.abspath(sys.argv[1])
    tmp = tempfile.mkdtemp()
    cfg = os.path.join(tmp, "config.toml"); open(cfg, "w").write(CONFIG)
    wing = FakeWing(); wing.start()
    daw = FakeDaw(); daw.start()

    results = []
    def check(name, ok, detail=""):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'}  {name}{('  -- ' + detail) if detail and not ok else ''}")

    # 1. The CLI resolves the live patch.
    run = subprocess.run([binary, "-c", cfg, "patch", "--output", "USB", "--seconds", "3"],
                         capture_output=True, text=True)
    out = run.stdout + run.stderr
    check("patch command succeeds", run.returncode == 0, out)
    rows = {}
    for line in run.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0].isdigit() and parts[1] in ("LCL", "MTX", "off"):
            rows[int(parts[0])] = line
    check("output 1 resolves to its channel", "KICK" in rows.get(1, ""), rows.get(1, ""))
    check("channel on a shifted input resolves",
          "VOX" in rows.get(4, "") and " 3 " in rows.get(4, "").replace("LCL 5", "LCL5"),
          rows.get(4, ""))
    check("socket with no channel uses its own label", "TALKBACK" in rows.get(3, ""), rows.get(3, ""))
    check("stereo matrix legs resolve",
          "REC L" in rows.get(5, "") and "REC R" in rows.get(6, ""),
          f"{rows.get(5,'')} | {rows.get(6,'')}")
    check("unpatched output stays blank", "off" in rows.get(7, ""), rows.get(7, ""))

    # 2. The bridge maps names through the live patch.
    proc = subprocess.Popen([binary, "-c", cfg, "run"],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    log = []
    threading.Thread(target=lambda: [log.append(l) for l in proc.stdout], daemon=True).start()
    time.sleep(4)   # startup queries + settle

    check("bridge logged a live patch",
          any("patch: console USB" in l for l in log),
          "".join(log[-8:]))

    # Channel 3 sits on USB output 4, so its name must land on strip 4.
    wing.sock.sendto(msg("/ch/3/name", "LEAD VOX"), (("127.0.0.1"), 12226))
    m = daw.expect("/strip/name", pred=lambda a: a[:2] == [4, "LEAD VOX"])
    check("console name follows the patch to the right strip", m is not None)

    proc.terminate()
    try: proc.wait(timeout=5)
    except subprocess.TimeoutExpired: proc.kill()

    passed = sum(1 for r in results if r)
    print(f"\n{passed}/{len(results)} checks passed")
    if passed != len(results):
        print("--- bridge log ---"); print("".join(log[-40:]))
    return 0 if passed == len(results) else 1

sys.exit(main())
