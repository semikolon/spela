#!/usr/bin/env python3
"""Bulletproof Chromecast controller for spela.
Uses catt's Python API with retry logic for flaky mDNS.
Called by spela server as a subprocess.

Usage:
  cast.py cast <device_name> <video_url> [--subtitle <srt_path>]
  cast.py seek <device_name> <seconds>
  cast.py pause <device_name>
  cast.py resume <device_name>
  cast.py stop <device_name>
  cast.py volume <device_name> <0-100>
  cast.py info <device_name>
  cast.py scan

All output is JSON for machine parsing.
"""

import sys
import json
import time
import os

CATT_VENV = os.path.expanduser("~/.local/share/pipx/venvs/catt/lib")
# Add catt's venv to path so we can import its dependencies
for d in os.listdir(CATT_VENV):
    site = os.path.join(CATT_VENV, d, "site-packages")
    if os.path.isdir(site) and site not in sys.path:
        sys.path.insert(0, site)

from catt.api import CattDevice
from catt.error import CastError

MAX_RETRIES = 5
RETRY_DELAY = 3

# Cached device IPs — bulletproof fallback when mDNS fails
KNOWN_DEVICES = {
    "Fredriks TV": "192.168.4.126",
    "Vardagsrum": "192.168.4.58",
}

def get_device(name, retries=MAX_RETRIES):
    """Get a CattDevice with retry logic + IP fallback for mDNS flakiness."""
    last_error = None
    for attempt in range(retries):
        try:
            return CattDevice(name=name)
        except CastError as e:
            last_error = e
            if attempt < retries - 1:
                time.sleep(RETRY_DELAY)

    # Fallback: try direct IP via pychromecast if we know the device
    if name in KNOWN_DEVICES:
        try:
            import pychromecast
            ip = KNOWN_DEVICES[name]
            services, browser = pychromecast.discovery.discover_chromecasts()
            pychromecast.discovery.stop_discovery(browser)
            # Find our device by IP in discovered services
            for svc in services:
                if str(svc.host) == ip:
                    return CattDevice(name=svc.friendly_name)
        except Exception:
            pass

    raise last_error or CastError("Device could not be found")

def out(data):
    print(json.dumps(data))

def cmd_cast(args):
    name = args[0]
    url = args[1]
    subtitle = None
    if "--subtitle" in args:
        subtitle = args[args.index("--subtitle") + 1]

    d = get_device(name)
    if subtitle:
        d.play_url(url, subtitles=subtitle)
    else:
        d.play_url(url)
    out({"status": "casting", "device": name, "url": url[:100], "subtitles": subtitle is not None})

def cmd_seek(args):
    name, seconds = args[0], float(args[1])
    d = get_device(name)
    d.seek(seconds)
    out({"status": "seeked", "device": name, "position": seconds})

def cmd_pause(args):
    d = get_device(args[0])
    d._cast.media_controller.pause()
    time.sleep(0.5)
    out({"status": "paused", "device": args[0]})

def cmd_resume(args):
    d = get_device(args[0])
    d._cast.media_controller.play()
    time.sleep(0.5)
    out({"status": "playing", "device": args[0]})

def cmd_stop(args):
    d = get_device(args[0])
    d.stop()
    out({"status": "stopped", "device": args[0]})

def cmd_volume(args):
    name, level = args[0], int(args[1])
    d = get_device(name)
    d._cast.set_volume(level / 100.0)
    out({"status": "volume_set", "device": name, "level": level})

def cmd_info(args):
    d = get_device(args[0])
    mc = d._cast.media_controller
    s = mc.status
    out({
        "device": args[0],
        "player_state": str(s.player_state) if s.player_state else "UNKNOWN",
        "current_time": s.current_time or 0,
        "duration": s.duration or 0,
        "volume": d._cast.status.volume_level,
        "muted": d._cast.status.volume_muted,
        "title": s.title or "",
        "content_id": s.content_id or "",
        "subtitle_tracks": [{"lang": t.get("language"), "name": t.get("name")} for t in (s.subtitle_tracks or [])],
    })

def cmd_scan(args):
    import pychromecast
    services, browser = pychromecast.discovery.discover_chromecasts()
    pychromecast.discovery.stop_discovery(browser)
    devices = []
    for s in services:
        devices.append({
            "name": s.friendly_name,
            "ip": str(s.host),
            "port": s.port,
            "model": s.model_name,
            "uuid": str(s.uuid),
        })
    out({"devices": devices})

if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        out({"error": "No command", "commands": ["cast", "seek", "pause", "resume", "stop", "volume", "info", "scan"]})
        sys.exit(1)

    cmd = args[0]
    cmd_args = args[1:]

    try:
        {"cast": cmd_cast, "seek": cmd_seek, "pause": cmd_pause, "resume": cmd_resume,
         "stop": cmd_stop, "volume": cmd_volume, "info": cmd_info, "scan": cmd_scan
        }[cmd](cmd_args)
    except CastError as e:
        out({"error": f"Chromecast error after {MAX_RETRIES} retries: {str(e)}", "device": cmd_args[0] if cmd_args else None})
        sys.exit(1)
    except Exception as e:
        out({"error": str(e)})
        sys.exit(1)
