# Cast debugging recipes

Three diagnostic techniques that turned a multi-hour guessing session into a 30-minute root-cause find during the May 1, 2026 Wilderpeople incident. Reach for these when `spela` casts a stream and the receiver shows blue cast icon / `idle_reason=ERROR` / no playback.

Full incident context: [`INCIDENT_REPORT_WILDERPEOPLE_2026_05_01.md`](INCIDENT_REPORT_WILDERPEOPLE_2026_05_01.md).

---

## 1. Apple BipBop reference HLS — known-good baseline

```python
# Cast Apple's reference HLS to suspect device. If THIS plays and
# spela's URL doesn't, the issue is YOUR content/server/headers —
# NOT the receiver, the network, or rust_cast/pychromecast.
URL = "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_4x3/bipbop_4x3_variant.m3u8"
mc.play_media(URL, "application/vnd.apple.mpegurl", stream_type="BUFFERED")
```

Plays in 4-6s on every Cast Receiver including CrKey 1.56.

---

## 2. Pychromecast raw MEDIA-message capture — surface `LOAD_FAILED`

`rust_cast` and pychromecast both surface only `idle_reason=ERROR` (opaque) when DMR rejects a LOAD. The actual `LOAD_FAILED` event is a separate Cast SDK message on the `urn:x-cast:com.google.cast.media` namespace.

```python
import json, time, pychromecast
from pychromecast.controllers import BaseController

class MediaListener(BaseController):
    def __init__(self):
        super().__init__('urn:x-cast:com.google.cast.media')
    def receive_message(self, message, data):
        print(json.dumps(data, indent=2)[:1500])
        return True

ccs, browser = pychromecast.get_chromecasts(timeout=5)
target = next(cc for cc in ccs if cc.name == "Fredriks TV")
target.wait(timeout=10)
target.register_handler(MediaListener())  # MUST be before play_media

mc = target.media_controller
mc.play_media(URL, "application/vnd.apple.mpegurl", stream_type="BUFFERED")
mc.block_until_active(timeout=10)
time.sleep(12)  # let receiver finish/fail; messages stream in
target.disconnect()
pychromecast.discovery.stop_discovery(browser)
```

Look for `{"type": "LOAD_FAILED", ...}` between `MEDIA_STATUS` messages — that's the Cast-protocol-level error class hidden behind `idle_reason=ERROR`.

---

## 3. Header-diff bisect — find what changed

When a known commit was working and a recent one is broken:

```bash
# At the broken commit:
ssh darwin 'curl -sI -H "Origin: https://www.gstatic.com" \
    http://192.168.4.1:7890/hls/master.m3u8'

# Checkout known-good commit, rebuild, restart, re-run curl.
# Diff the response headers — the regression usually pops out as a
# missing or added single line. (May 1 example: missing
# `access-control-allow-origin: *` was the smoking gun.)
```

---

## Reusable Darwin diagnostic env

Pre-built pychromecast venv + Apple BipBop content + test scripts at `/tmp/cc-debug/` (~13MB, survives until reboot). Recreate via:

```bash
ssh darwin 'mkdir -p /tmp/cc-debug && cd /tmp/cc-debug && uv venv && uv pip install --python /tmp/cc-debug/.venv/bin/python pychromecast'
```

Then download Apple's BipBop HLS subset (`master.m3u8` + `gear1/prog_index.m3u8` + first 3 `fileSequence*.ts`) into `/tmp/cc-debug/apple_hls/` if you want to test serving non-spela HLS via Python `http.server`.

---

## Cast LOAD failure modes — quick triage table

| Symptom | Likely layer | Where to look |
|---------|-------------|---------------|
| `Connection refused` in ffmpeg.log | spela's loopback bind | `compute_bind_addresses`; CLAUDE.md § "Spela must dual-bind" |
| `404 Not Found` on `/torrent/N/stream/0` in ffmpeg.log | librqbit init race | `serve_torrent_stream`; CLAUDE.md § "librqbit `start()` returns before storage is ready" |
| `HLS pre-buffer timeout (60s) — casting with 0 segments` | ffmpeg crashed or never wrote a segment | Read ffmpeg.log; usual culprits are the two above |
| Blue cast icon on TV, `media_session=None` for 60s+ | Receiver never ack'd cast LOAD | Apple BipBop test (recipe 1); if BipBop plays, run pychromecast LOAD_FAILED capture (recipe 2) |
| `idle_reason=ERROR` from pychromecast, blue cast icon | Receiver returned `LOAD_FAILED` — content/header issue | Recipe 2 (LOAD_FAILED capture); recipe 3 (header-diff bisect against known-good commit) |
| `cast_health_monitor` kills stream at `stream_age=20-30s` despite ffmpeg producing segments | Cold-start IDLE protection broken | CLAUDE.md § "cold-start IDLE protection mirrors BUFFERING" |
| Master CODECS lies about actual stream profile | NVENC profile not forced | CLAUDE.md § "NVENC profile must match" — `-profile:v high -level:v 4.0` on all reencode paths |
