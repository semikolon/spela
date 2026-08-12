#!/usr/bin/env python3
"""Report which titles in add-watchlist-batch.py you already know about.

Cross-checks the batch list against the two USER-LOCAL stores on the spela host:

  ~/.config/spela/taste_profile.md   the "watched & loved" favorites (free text)
  ~/.config/spela/watchlist.json     the to-watch list itself

Neither lives in this repo, so run this on the spela host (or point the paths at
a local copy). Matching is normalized (case, leading article, punctuation) and
substring-based against the profile prose, so it OVER-reports rather than
under-reports — treat hits as "worth a look", not gospel.

    python3 bin/watchlist-overlap.py
    python3 bin/watchlist-overlap.py --profile /path/to/taste_profile.md
"""

import argparse
import json
import os
import re
import sys

# The batch script's filename has dashes, so load it by path rather than import.
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "batch",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "add-watchlist-batch.py"),
)
batch = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(batch)

HOME = os.path.expanduser("~")


def norm(s):
    """Lowercase, drop a leading article, strip punctuation, squeeze spaces."""
    s = s.lower()
    s = re.sub(r"^(the|a|an|la|le|el)\s+", "", s)
    s = re.sub(r"[^a-z0-9 ]+", " ", s)
    return re.sub(r"\s+", " ", s).strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", default=f"{HOME}/.config/spela/taste_profile.md")
    ap.add_argument("--watchlist", default=f"{HOME}/.config/spela/watchlist.json")
    args = ap.parse_args()

    try:
        profile = norm(open(args.profile, encoding="utf-8").read())
    except OSError as e:
        print(f"no taste profile at {args.profile}: {e}", file=sys.stderr)
        profile = ""

    listed = set()
    try:
        wl = json.load(open(args.watchlist, encoding="utf-8"))
        for key in ("movies", "series"):
            for e in wl.get(key, []):
                if e.get("title"):
                    listed.add(norm(e["title"]))
    except OSError:
        pass

    in_profile, on_list, fresh = [], [], []
    for kind, titles in (("movie", batch.MOVIES), ("series", batch.SERIES)):
        for t in titles:
            n = norm(t)
            # Word-boundary, not bare substring: "her"/"pi"/"ran" would otherwise
            # match inside "there"/"pirates"/"grand". Short titles are still
            # ambiguous against prose, so they're flagged rather than trusted.
            if profile and re.search(rf"\b{re.escape(n)}\b", profile):
                in_profile.append((kind, t + ("   [short title — verify]" if len(n) <= 5 else "")))
            elif n in listed:
                on_list.append((kind, t))
            else:
                fresh.append((kind, t))

    for label, rows in (
        ("ALREADY IN taste_profile.md (watched & loved)", in_profile),
        ("ALREADY ON watchlist.json", on_list),
        ("NEW", fresh),
    ):
        print(f"\n=== {label} — {len(rows)} ===")
        for kind, t in rows:
            print(f"  {kind:6} {t}")

    print(
        f"\nprofile hits {len(in_profile)} | already listed {len(on_list)} | "
        f"new {len(fresh)} | total {len(batch.MOVIES) + len(batch.SERIES)}"
    )


if __name__ == "__main__":
    # Piping into `head` closes stdout early; don't traceback on it.
    import signal

    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    main()
