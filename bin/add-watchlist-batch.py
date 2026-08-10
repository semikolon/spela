#!/usr/bin/env python3
"""Batch-add a list of titles to spela's to-watch list.

Runs from any machine that can reach the spela server. POSTs each title to
`POST /watchlist` — spela dedups case-insensitively and writes its OWN host's
~/.config/spela/watchlist.json, so there is no client→server sync step and
re-running is safe.

    SPELA_URL=http://<spela-host>:7890 python3 bin/add-watchlist-batch.py
    python3 bin/add-watchlist-batch.py --dry-run   # print what would be added

Edit the MOVIES / SERIES lists below to change what gets added.
"""

import json
import os
import sys
import urllib.error
import urllib.request

SPELA = os.environ.get("SPELA_URL", "http://localhost:7890").rstrip("/")
DRY = "--dry-run" in sys.argv

# --- Source: a "favorite movies" thread, original post ------------------------
MOVIES = [
    # top favorites
    "Master and Commander: The Far Side of the World",
    "Lawrence of Arabia",
    "Barry Lyndon",
    "Citizen Kane",
    "Braveheart",
    "Gone with the Wind",
    "City of God",
    "El Topo",
    "The Holy Mountain",
    "Groundhog Day",
    "The Handmaiden",
    "The Color of Pomegranates",
    "Harakiri",
    "The Fall",
    # second tier ("also really like")
    "The Godfather",
    "The Godfather Part II",
    "La Grande Illusion",
    "Blue Valentine",
    "Border",
    "Princess Mononoke",
    "Aguirre, the Wrath of God",
    "Casablanca",
    "Forrest Gump",
    "Ben-Hur",
    "The Mummy",
    "The Lord of the Rings: The Fellowship of the Ring",
    "The Lord of the Rings: The Two Towers",
    "The Lord of the Rings: The Return of the King",
    "Gladiator",
    "The Adventures of Robin Hood",
    "RRR",
    "Oldboy",
    "Pi",
    "The Talented Mr. Ripley",
    "Everything Everywhere All at Once",
    "Dune",
    "Dune: Part Two",
    "The Man Who Would Be King",
    "The Count of Monte Cristo",
    "The Battle of Algiers",
    "Jurassic Park",
    "Battle Royale",
    "Brazil",
    "The Illusionist",
    "In the Mood for Love",
    "Parasite",
    "Spirited Away",
    "Ran",
    "Seven Samurai",
    "Throne of Blood",
    "Portrait of a Lady on Fire",
    "Her",
    "Captain Fantastic",
    "Bajirao Mastani",
    "True Grit",
    "O Brother, Where Art Thou?",
    "The Matrix",
    "Conan the Barbarian",
    "Hook",
    "The Good, the Bad and the Ugly",
    "Eyes Wide Shut",
    "Troy",
    "The Rules of the Game",
    "Sunset Boulevard",
    "Raiders of the Lost Ark",
    "Indiana Jones and the Temple of Doom",
    "Indiana Jones and the Last Crusade",
    "The Darjeeling Limited",
    "The Grand Budapest Hotel",
    "The Royal Tenenbaums",
    "The Bridge on the River Kwai",
    "Black Narcissus",
    "Ugetsu",
    "Baraka",
    "Apocalypto",
    "To Live",
    "The Ten Commandments",
    "The Thief of Bagdad",
    "The Mask of Zorro",
    "Cyrano de Bergerac",
    "2001: A Space Odyssey",
    "The Ox-Bow Incident",
    "Captain Blood",
    "Amelie",
    "Lagaan",
    "Crouching Tiger, Hidden Dragon",
    "The Adventures of Prince Achmed",
    "The Lion King",
    "Aladdin",
    "Star Wars",
    "The Empire Strikes Back",
    "Return of the Jedi",
    "The Last Samurai",
    "Pirates of the Caribbean: The Curse of the Black Pearl",
    "The Princess Bride",
    "Cleopatra",
    "Donnie Darko",
    "The Wizard of Oz",
    "Bloodsport",
    "Gandhi",
    "House of Flying Daggers",
    "The Addams Family",
    "300",
    "The Last of the Mohicans",
    "The Prince of Egypt",
    # --- reply 1 ---
    "Children of Paradise",
    "Samurai I: Musashi Miyamoto",
    "Bonaparte and the Revolution",
    "Children of a Lesser God",
    "The Ice Storm",
    "Eat Drink Man Woman",
    # --- reply 2 ---
    "The Best Years of Our Lives",
    "Kaos",
    "King of Hearts",
    "The Shootist",
    "The Man Who Shot Liberty Valance",
    # --- reply 3 (a ranked top 20) ---
    "Rebecca",
    "Back to the Future",
    "After Hours",
    "Vertigo",
    "Ghost World",
    "Some Like It Hot",
    "Manhattan",
    "Double Indemnity",
    "My Dinner with Andre",
    "Duck Soup",
    "The Lives of Others",
    "Out of the Past",
    "The Apartment",
    "It's a Gift",
    "West Side Story",
    "Gun Crazy",
    "Something Wild",
    "It's a Wonderful Life",
    "One Flew Over the Cuckoo's Nest",
]

# TV / miniseries / docuseries named in the post go to the `series` bucket.
SERIES = [
    "Rome",
    "Game of Thrones",
    "House of the Dragon",
    "A Knight of the Seven Kingdoms",
    "Chief of War",
    "Civilisation",
    "Blue Eye Samurai",
    "Arcane",
    "Avatar: The Last Airbender",
    "Planet Earth II",
    "The Blue Planet",
    "Prehistoric Planet",
    "Jesus of Nazareth",
    "Cosmos: A Personal Voyage",
]


def post(title, kind):
    body = json.dumps({"title": title, "type": kind}).encode()
    req = urllib.request.Request(
        f"{SPELA}/watchlist",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read().decode())


def main():
    added = skipped = failed = 0
    for kind, titles in (("movie", MOVIES), ("series", SERIES)):
        for t in titles:
            if DRY:
                print(f"[dry-run] {kind:6} {t}")
                continue
            try:
                res = post(t, kind)
            except (urllib.error.URLError, OSError) as e:
                print(f"FAIL  {kind:6} {t}: {e}")
                failed += 1
                continue
            if res.get("added"):
                print(f"added {kind:6} {t}")
                added += 1
            elif res.get("ok"):
                print(f"skip  {kind:6} {t} ({res.get('reason', 'already on list')})")
                skipped += 1
            else:
                print(f"FAIL  {kind:6} {t}: {res}")
                failed += 1
    if DRY:
        print(f"\n{len(MOVIES)} movies + {len(SERIES)} series = "
              f"{len(MOVIES) + len(SERIES)} titles")
    else:
        print(f"\nadded {added}, already present {skipped}, failed {failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
