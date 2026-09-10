#!/usr/bin/env python3
"""Drive `new-session` against a fake WING and validate the generated session."""
import os, socket, struct, subprocess, sys, tempfile, threading, time
import xml.etree.ElementTree as ET

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
    end = d.index(b"\0", i)
    return d[i:end].decode(errors="replace"), ((end + 4) // 4) * 4

def parse(d):
    addr, i = rdstr(d, 0)
    if i >= len(d): return addr, []
    tags, i = rdstr(d, i)
    args = []
    for t in tags[1:]:
        if t == "i":   args.append(struct.unpack_from(">i", d, i)[0]); i += 4
        elif t == "f": args.append(struct.unpack_from(">f", d, i)[0]); i += 4
        elif t == "s": s, i = rdstr(d, i); args.append(s)
    return addr, args

NAMES = {1: "KICK", 2: "SNARE", 3: "HAT", 4: "BASS", 5: "GTR/L", 6: "GTR/L", 7: "", 8: "VOX"}

class FakeWing(threading.Thread):
    """Answers empty-argument name queries the way the console does."""
    def __init__(self, port=12223):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", port))
    def run(self):
        while True:
            data, src = self.sock.recvfrom(65536)
            addr, args = parse(data)
            if addr.startswith("/ch/") and addr.endswith("/name") and not args:
                ch = int(addr.split("/")[2])
                if ch in NAMES and NAMES[ch]:
                    self.sock.sendto(msg(addr, NAMES[ch]), src)

TEMPLATE = """<?xml version="1.0" encoding="UTF-8"?>
<Session version="7002" name="Tmpl" sample-rate="44100" id-counter="500">
  <Config/>
  <Sources><Source id="10" name="old.wav"/></Sources>
  <Regions><Region id="11" name="old-region"/></Regions>
  <Playlists><Playlist id="12" name="Audio 1.1" orig-track-id="20"/></Playlists>
  <UnusedPlaylists/>
  <RouteGroups><RouteGroup id="13" name="grp" routes="20"/></RouteGroups>
  <Locations>
    <Location id="14" name="session" start="0" end="48000" flags="IsSessionRange"/>
    <Location id="15" name="Old mark" start="1000" flags="IsMark"/>
  </Locations>
  <Routes>
    <Route id="20" name="Audio 1" default-type="audio" strict-io="1" active="1" audio-playlist="12">
      <PresentationInfo order="0" flags="AudioTrack"/>
      <Controllable name="solo" id="21" value="0"/>
      <IO id="22" name="Audio 1" direction="Input">
        <Port type="audio" name="Audio 1/audio_in 1"><Connection other="system:capture_3"/></Port>
      </IO>
      <IO id="23" name="Audio 1" direction="Output">
        <Port type="audio" name="Audio 1/audio_out 1"/>
        <Port type="audio" name="Audio 1/audio_out 2"/>
      </IO>
      <Processor id="24" name="Amp" type="amp"><Controllable name="gaincontrol" id="25" value="1"/></Processor>
    </Route>
    <Route id="30" name="Master" default-type="audio" strict-io="1" active="1">
      <PresentationInfo order="0" flags="MasterOut"/>
      <IO id="31" name="Master" direction="Input"><Port type="audio" name="Master/audio_in 1"/></IO>
      <IO id="32" name="Master" direction="Output"><Port type="audio" name="Master/audio_out 1"/></IO>
    </Route>
  </Routes>
</Session>
"""

CONFIG = """
[wing]
host = "127.0.0.1"
port = 12223
local_port = 12225
channels = 8
name_address = "/ch/{ch}/name"
[livetrax]
host = "127.0.0.1"
port = 13819
local_port = 13821
"""

def main():
    binary = os.path.abspath(sys.argv[1])
    tmp = tempfile.mkdtemp()
    cfg = os.path.join(tmp, "config.toml"); open(cfg, "w").write(CONFIG)
    tpl = os.path.join(tmp, "Tmpl.ardour"); open(tpl, "w").write(TEMPLATE)
    dest = os.path.join(tmp, "sessions"); os.makedirs(dest)
    FakeWing().start()

    results = []
    def check(name, ok, detail=""):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'}  {name}{('  -- ' + detail) if detail and not ok else ''}")

    run = subprocess.run([binary, "-c", cfg, "new-session", "--dest", dest,
                          "--name", "Show A", "--channels", "1-8",
                          "--template", tpl, "--rate", "48000", "--wait-secs", "2"],
                         capture_output=True, text=True)
    check("new-session exits cleanly", run.returncode == 0, run.stdout + run.stderr)
    if run.returncode != 0:
        return 1

    folder = os.path.join(dest, "Show A")
    session_file = os.path.join(folder, "Show A.ardour")
    check("session file written", os.path.isfile(session_file))
    check("interchange folder created",
          os.path.isdir(os.path.join(folder, "interchange", "Show A", "audiofiles")))
    check("peaks folder created", os.path.isdir(os.path.join(folder, "peaks")))

    root = ET.parse(session_file).getroot()
    check("sample rate applied", root.get("sample-rate") == "48000", root.get("sample-rate"))
    check("session renamed", root.get("name") == "Show A")

    routes = root.find("Routes").findall("Route")
    names = [r.get("name") for r in routes]
    # channel 7 is unnamed and skipped; the duplicate GTR/L becomes "GTR-L 2"
    expected = ["Master", "KICK", "SNARE", "HAT", "BASS", "GTR-L", "GTR-L 2", "VOX"]
    check("routes match console names", names == expected, str(names))

    tracks = [r for r in routes if r.get("name") != "Master"]
    check("playlist reference dropped", all(r.get("audio-playlist") is None for r in tracks))
    check("presentation order renumbered",
          [r.find("PresentationInfo").get("order") for r in tracks] ==
          [str(i) for i in range(len(tracks))])

    kick = tracks[0]
    ios = kick.findall("IO")
    check("IO renamed", all(io.get("name") == "KICK" for io in ios))
    port_names = [p.get("name") for io in ios for p in io.findall("Port")]
    check("port names renamed", port_names == ["KICK/audio_in 1", "KICK/audio_out 1", "KICK/audio_out 2"],
          str(port_names))

    captures = []
    for t in tracks:
        for io in t.findall("IO"):
            if io.get("direction") == "Input":
                for p in io.findall("Port"):
                    for c in p.findall("Connection"):
                        captures.append(c.get("other"))
    check("inputs use sequential capture ports",
          captures == [f"system:capture_{i+1}" for i in range(len(tracks))], str(captures))

    ids = [e.get("id") for e in root.iter() if e.get("id") is not None]
    check("all ids unique", len(ids) == len(set(ids)), f"{len(ids)} ids, {len(set(ids))} unique")
    check("id-counter above max id",
          int(root.get("id-counter")) > max(int(i) for i in ids if i.isdigit()))

    check("template media cleared",
          len(root.find("Sources")) == 0 and len(root.find("Regions")) == 0
          and len(root.find("Playlists")) == 0)
    locs = root.find("Locations").findall("Location")
    check("only the session range kept",
          len(locs) == 1 and "IsSessionRange" in locs[0].get("flags"), str([l.get("name") for l in locs]))
    check("route group emptied", root.find("RouteGroups").find("RouteGroup").get("routes") == "")

    # Never clobber an existing session.
    again = subprocess.run([binary, "-c", cfg, "new-session", "--dest", dest, "--name", "Show A",
                            "--channels", "1-4", "--template", tpl, "--wait-secs", "1"],
                           capture_output=True, text=True)
    check("existing session refused",
          again.returncode != 0 and "already exists" in (again.stdout + again.stderr))

    # No template and no opt-in: refuse with guidance, leave nothing behind.
    os.environ["HOME"] = tmp  # hide any real LiveTrax templates
    none = subprocess.run([binary, "-c", cfg, "new-session", "--dest", dest, "--name", "NoTpl",
                           "--channels", "1-4", "--wait-secs", "1"],
                          capture_output=True, text=True, env={**os.environ, "HOME": tmp})
    check("missing template refused",
          none.returncode != 0 and "no template session found" in (none.stdout + none.stderr))
    check("no folder left behind", not os.path.exists(os.path.join(dest, "NoTpl")))

    # Opt-in minimal fallback still produces a parseable session.
    minimal = subprocess.run([binary, "-c", cfg, "new-session", "--dest", dest, "--name", "Minimal",
                              "--channels", "1-4", "--wait-secs", "1", "--allow-minimal"],
                             capture_output=True, text=True, env={**os.environ, "HOME": tmp})
    ok = minimal.returncode == 0 and os.path.isfile(os.path.join(dest, "Minimal", "Minimal.ardour"))
    check("minimal fallback writes a session", ok, minimal.stdout + minimal.stderr)
    if ok:
        mroot = ET.parse(os.path.join(dest, "Minimal", "Minimal.ardour")).getroot()
        check("minimal session has the tracks",
              [r.get("name") for r in mroot.find("Routes").findall("Route")] ==
              ["KICK", "SNARE", "HAT", "BASS"])
        check("minimal session warns", "warning:" in minimal.stdout)

    # ---- a Qu builds a session from a patch sheet, and nothing else -------
    sheet_path = os.path.join(tmp, "qu-inputs.csv")
    open(sheet_path, "w").write(
        "THE HOLLOWS - INPUT LIST\n"          # a title above the header is normal
        "Ch,Name,Source,Gain,48V,Track,Mic\n"
        "1,Kick,Local 1,32,,1,Beta 91\n"
        "2,Snare,Local 2,28,,2,SM57\n"
        "3,Bass DI,Local 3,18,Yes,3,DI\n"
    )
    qu = subprocess.run([binary, "-c", cfg, "build", "--sheet", sheet_path, "--desk", "qu-16",
                         "--dest", dest, "--name", "Qu Show", "--template", tpl],
                        capture_output=True, text=True)
    out = qu.stdout + qu.stderr
    check("qu build succeeds", qu.returncode == 0, out)
    qu_session = os.path.join(dest, "Qu Show", "Qu Show.ardour")
    check("qu session written", os.path.isfile(qu_session))
    if os.path.isfile(qu_session):
        names = [r.get("name") for r in ET.parse(qu_session).getroot()
                 .find("Routes").findall("Route") if r.get("name") != "Master"]
        check("tracks come from the sheet", names == ["Kick", "Snare", "Bass DI"], str(names))
    check("no console file for a qu",
          not any(f.endswith(".snap") for f in os.listdir(os.path.join(dest, "Qu Show"))))
    check("columns it could not use are reported",
          "not applied" in out and "Gain" in out and "48V" in out, out)
    # A title line above the header is not an error.
    check("the title row is stepped over", "3 channels" in out, out)

    refused = subprocess.run([binary, "-c", cfg, "build", "--sheet", sheet_path, "--desk", "qu-16",
                              "--dest", dest, "--name", "Qu Two", "--template", tpl,
                              "--base", tpl],
                             capture_output=True, text=True)
    check("a base snapshot is refused for a qu",
          refused.returncode != 0 and "nothing to do" in (refused.stdout + refused.stderr),
          refused.stdout + refused.stderr)

    unknown = subprocess.run([binary, "-c", cfg, "build", "--sheet", sheet_path, "--desk", "x32",
                              "--dest", dest, "--name", "Nope", "--template", tpl],
                             capture_output=True, text=True)
    check("an unknown desk lists the ones there are",
          unknown.returncode != 0 and "qu-16" in (unknown.stdout + unknown.stderr),
          unknown.stdout + unknown.stderr)

    # ---- a session from a Qu scene file ----------------------------------
    # A scene laid out the way the .DAT format describes: the names sit at
    # 0x9C in each 0xC0-byte channel, starting at 0x30.
    scene = bytearray(0x6520)
    scene[3] = 12
    scene[0x0C:0x0C + 8] = b"SOUNDCHK"
    for index, chan_name in {0: "Kick", 1: "Snare", 15: "Talkbck",
                             32: "Playbck", 35: "Verb"}.items():
        at = 0x30 + index * 0xC0
        scene[at + 0x9C:at + 0x9C + len(chan_name)] = chan_name.encode()
        scene[at + 0xB7] = index + 1
    scene_path = os.path.join(tmp, "SCENE012.DAT")
    open(scene_path, "wb").write(bytes(scene))

    info = subprocess.run([binary, "-c", cfg, "qu-scene", scene_path],
                          capture_output=True, text=True)
    check("qu-scene reads the file",
          info.returncode == 0 and "SOUNDCHK" in info.stdout, info.stdout + info.stderr)
    check("it names the slots, not just the channels",
          "ST1" in info.stdout and "FX1 return" in info.stdout, info.stdout)

    built = subprocess.run([binary, "-c", cfg, "new-session", "--from-scene", scene_path,
                            "--channels", "1-16", "--include-extras", "--include-unnamed",
                            "--dest", dest, "--name", "From Scene", "--template", tpl],
                           capture_output=True, text=True)
    check("a session is built from the scene", built.returncode == 0,
          built.stdout + built.stderr)
    scene_session = os.path.join(dest, "From Scene", "From Scene.ardour")
    if os.path.isfile(scene_session):
        names = [r.get("name") for r in ET.parse(scene_session).getroot()
                 .find("Routes").findall("Route") if r.get("name") != "Master"]
        check("named inputs come through", names[0] == "Kick" and names[15] == "Talkbck", str(names[:3]))
        check("unnamed inputs hold their place", names[2] == "Ch 3", str(names[:4]))
        check("the extras follow the inputs", "Playbck" in names and "Verb" in names, str(names[16:]))
    else:
        for name in ["named inputs come through", "unnamed inputs hold their place",
                     "the extras follow the inputs"]:
            check(name, False, "no session was written")

    junk = os.path.join(tmp, "not-a-scene.DAT")
    open(junk, "wb").write(bytes([0xAB]) * 0x6520)
    refused = subprocess.run([binary, "-c", cfg, "qu-scene", junk],
                             capture_output=True, text=True)
    check("a file that is not a scene is refused",
          refused.returncode != 0 and "does not read as a Qu scene" in (refused.stdout + refused.stderr),
          refused.stdout + refused.stderr)

    passed = sum(1 for r in results if r)
    print(f"\n{passed}/{len(results)} checks passed")
    return 0 if passed == len(results) else 1

sys.exit(main())
