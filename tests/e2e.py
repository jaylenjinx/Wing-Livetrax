#!/usr/bin/env python3
"""End-to-end smoke test: fake WING + fake LiveTrax around the real bridge."""
import os, socket, struct, subprocess, sys, tempfile, time, threading, queue

def ostr(s):
    b = s.encode() + b"\0"
    return b + b"\0" * ((4 - len(b) % 4) % 4)

def msg(addr, *args):
    tags = ","
    body = b""
    for a in args:
        if isinstance(a, bool):   tags += "T" if a else "F"
        elif isinstance(a, int):  tags += "i"; body += struct.pack(">i", a)
        elif isinstance(a, float):tags += "f"; body += struct.pack(">f", a)
        else:                     tags += "s"; body += ostr(a)
    return ostr(addr) + ostr(tags) + body

def rdstr(d, i):
    end = d.index(b"\0", i)
    s = d[i:end].decode(errors="replace")
    return s, ((end + 4) // 4) * 4

def parse(d):
    addr, i = rdstr(d, 0)
    if i >= len(d): return addr, []
    tags, i = rdstr(d, i)
    args = []
    for t in tags[1:]:
        if t == "i":   args.append(struct.unpack_from(">i", d, i)[0]); i += 4
        elif t == "h": args.append(struct.unpack_from(">q", d, i)[0]); i += 8
        elif t == "f": args.append(round(struct.unpack_from(">f", d, i)[0], 4)); i += 4
        elif t == "s": s, i = rdstr(d, i); args.append(s)
        elif t in "TF": args.append(t == "T")
    return addr, args

class Peer(threading.Thread):
    """A fake endpoint: collects messages, remembers the bridge's source port."""
    def __init__(self, name, port):
        super().__init__(daemon=True)
        self.name, self.q, self.peer = name, queue.Queue(), None
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", port))
        self.log = []
    def run(self):
        while True:
            data, src = self.sock.recvfrom(65536)
            self.peer = src
            m = parse(data)
            self.log.append(m)
            self.q.put(m)
    def send(self, *a):
        for _ in range(50):
            if self.peer: break
            time.sleep(0.05)
        self.sock.sendto(msg(*a), self.peer)
    def expect(self, addr, timeout=4.0, pred=None):
        end = time.time() + timeout
        while time.time() < end:
            try: m = self.q.get(timeout=max(0.05, end - time.time()))
            except queue.Empty: break
            if m[0] == addr and (pred is None or pred(m[1])):
                return m
        return None

SESSION = """<?xml version="1.0" encoding="UTF-8"?>
<Session version="7000" name="Test" sample-rate="48000">
  <Locations>
    <Location id="1" name="Song 1" start="48000" end="48000" flags="IsMark"/>
    <!-- Ardour 7+ audio-domain positions are superclock ticks (282,240,000/s),
         not samples: 16934400000 ticks = 60 s = 2,880,000 samples at 48 kHz. -->
    <Location id="2" name="Song 2" start="a16934400000" end="a16934400000" flags="IsMark"/>
    <Location id="3" name="A range" start="0" end="9600" flags="IsRangeMarker"/>
  </Locations>
</Session>
"""

CONFIG = """
[wing]
host = "127.0.0.1"
port = 12223
local_port = 12224
channels = 8
name_address = "/ch/{{ch}}/name"
subscribe = ["/*S"]
subscribe_interval_ms = 1000
name_poll_interval_ms = 0

[livetrax]
host = "127.0.0.1"
port = 13819
local_port = 13820
session_file = "{session}"
refresh_interval_ms = 2000
add_marker_takes_name = true

[timecode]
fps = "30"

[names]
enabled = true
direction = "bidirectional"
debounce_ms = 200

[transport]
enabled = true
[[transport.buttons]]
address = "/$ctl/user/1/bu/1"
action = "toggle_play"
[[transport.buttons]]
address = "/$ctl/user/1/bu/2"
action = {{ locate_marker = "Song 1" }}
[[transport.buttons]]
address = "/$ctl/user/1/bu/3"
action = "add_marker"
[[transport.leds]]
source = "playing"
address = "/$ctl/user/1/bu/1/led"
on = 1
off = 0

[scenes]
enabled = true
direction = "bidirectional"
scene_address = "/$ctl/lib/$actidx"
recall_address = "/$ctl/lib/$action"
map = [ {{ scene = 1, marker = "Song 1" }}, {{ scene = 2, marker = "Song 2" }} ]
"""

def main():
    tmp = tempfile.mkdtemp()
    session = os.path.join(tmp, "Test.ardour")
    open(session, "w").write(SESSION)
    cfg = os.path.join(tmp, "config.toml")
    open(cfg, "w").write(CONFIG.format(session=session))

    wing = Peer("wing", 12223); wing.start()
    daw = Peer("daw", 13819); daw.start()

    binary = sys.argv[1]
    proc = subprocess.Popen([binary, "-c", cfg, "run"],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    out = []
    threading.Thread(target=lambda: [out.append(l) for l in proc.stdout], daemon=True).start()

    results = []
    def check(name, ok, detail=""):
        results.append((name, ok, detail))
        print(f"{'PASS' if ok else 'FAIL'}  {name}{('  -- ' + detail) if detail and not ok else ''}")

    # 1. Bridge announces itself to both ends.
    check("wing subscription sent", wing.expect("/*S") is not None)
    check("daw /set_surface sent", daw.expect("/set_surface") is not None)
    check("daw /strip/list requested", daw.expect("/strip/list") is not None)

    # DAW answers the strip list like Ardour does.
    daw.send("#reply", "AT", 1, "Audio 1", 1, 1, 0, 0)
    daw.send("#reply", "AT", 2, "Audio 2", 1, 1, 0, 0)
    daw.send("#reply", "AT", 5, "Talkback", 1, 1, 0, 0)
    daw.send("#reply", "end_route_list", 48000, 0)
    time.sleep(0.4)

    # "#reply" is not a legal OSC address; the bridge has to accept it anyway,
    # because that is how the DAW answers /strip/list.
    check("strip list (#reply) reaches the console",
          wing.expect("/ch/5/name", pred=lambda a: a[:1] == ["Talkback"]) is not None)

    # 2. Console name -> DAW rename.
    wing.send("/ch/1/name", "KICK")
    m = daw.expect("/strip/name", pred=lambda a: a[:2] == [1, "KICK"])
    check("channel name -> track rename", m is not None)

    # placeholder names from the DAW must not be pushed to the console
    check("placeholder name not pushed",
          wing.expect("/ch/2/name", timeout=0.8, pred=lambda a: len(a) > 0) is None)

    # 3. DAW rename -> console scribble strip.
    daw.send("/strip/name", 2, "SNARE TOP")
    m = wing.expect("/ch/2/name", pred=lambda a: a[:1] == ["SNARE TOP"])
    check("track rename -> channel name", m is not None)

    # 4. User button -> transport.
    wing.send("/$ctl/user/1/bu/1", 1.0)
    check("user button -> /toggle_roll", daw.expect("/toggle_roll") is not None)

    # 5. Transport state -> console LED.
    daw.send("/transport_play", 1)
    check("transport state -> LED", wing.expect("/$ctl/user/1/bu/1/led",
                                                pred=lambda a: a[:1] == [1]) is not None)
    daw.send("/transport_stop", 1)
    time.sleep(0.3)

    # 6. Button bound to a named marker locates using the session file.
    wing.send("/$ctl/user/1/bu/2", 1.0)
    check("button -> locate to marker", daw.expect("/locate",
          pred=lambda a: a[:1] == [48000]) is not None)
    time.sleep(1.8)  # let the scene-loop guard expire

    # 7. Scene recall -> marker locate, via the superclock-encoded position.
    wing.send("/$ctl/lib/$actidx", 2)
    check("scene recall -> locate marker", daw.expect("/locate",
          pred=lambda a: a[:1] == [2880000]) is not None)
    time.sleep(1.8)

    # 8. Marker passed in the DAW -> scene recall on the console.
    daw.send("/marker", "Song 1")
    check("marker -> scene recall", wing.expect("/$ctl/lib/$action",
          pred=lambda a: a[:1] == [1]) is not None)

    # 9. A marker dropped by the bridge is named with the DAW's own timecode.
    daw.send("/position/smpte", "01:00:05:00")
    time.sleep(0.3)
    wing.send("/$ctl/user/1/bu/3", 1.0)
    check("marker named from the DAW's timecode",
          daw.expect("/add_marker", pred=lambda a: a[:1] == ["01:00:05:00"]) is not None)

    # 10. Scene recall while rolling drops a marker instead of jumping.
    daw.send("/transport_play", 1)
    time.sleep(0.3)
    wing.send("/$ctl/lib/$actidx", 2)
    check("scene while rolling -> add marker", daw.expect("/add_marker") is not None)

    # 11. A repeated scene index (subscription refresh) must not act again.
    time.sleep(0.3)
    wing.send("/$ctl/lib/$actidx", 2)
    check("repeated scene index ignored", daw.expect("/add_marker", timeout=1.0) is None)

    proc.terminate()
    try: proc.wait(timeout=5)
    except subprocess.TimeoutExpired: proc.kill()

    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    if failed:
        print("\n--- bridge log ---")
        print("".join(out[-60:]))
    return 1 if failed else 0

sys.exit(main())
