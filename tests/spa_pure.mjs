#!/usr/bin/env node
// Tests for the web remote's PURE helpers.
//
// static/remote.html is ~206KB of logic and had zero tests, because it is deliberately a
// single no-build browser asset: there is no bundler to import from and most of it needs
// a DOM. That is a fine reason for the DOM-heavy parts to stay untested and a poor one
// for the pure policy functions, several of which encode real decisions — which episode a
// tap searches for, whether a note reads as stale, how a status maps to a colour.
//
// So this extracts the named function declarations by brace-matching and evaluates them in
// isolation. No bundler, no dependency, no change to how the SPA ships. It is wired into
// `cargo test` (see spa_pure_tests in server.rs) and SKIPS cleanly when node is absent, so
// one command still covers everything.
//
// Extraction is deliberately strict: a helper that stops being a top-level `function`
// declaration FAILS here rather than silently going untested.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "..", "static", "remote.html"), "utf8");

function extract(name) {
  const re = new RegExp(`(^|\\n)\\s*function ${name}\\s*\\(`);
  const m = re.exec(src);
  if (!m) throw new Error(`helper ${name}() not found as a top-level function declaration`);
  const start = src.indexOf("function " + name, m.index);
  let i = src.indexOf("{", start);
  let depth = 0;
  for (; i < src.length; i++) {
    const c = src[i];
    if (c === "{") depth++;
    else if (c === "}") {
      depth--;
      if (depth === 0) return src.slice(start, i + 1);
    }
  }
  throw new Error(`unbalanced braces extracting ${name}()`);
}

const NAMES = ["searchQueryFor", "statusTone", "noteAgeDays", "fmtRuntime", "prettyEp"];
const bodies = NAMES.map(extract).join("\n");
const api = new Function(`${bodies}\nreturn {${NAMES.join(",")}};`)();

let failed = 0;
const eq = (got, want, what) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failed++;
  console.log(`  ${ok ? "ok  " : "FAIL"} ${what}${ok ? "" : ` — got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`}`);
};

// --- searchQueryFor: the fix for the Furious poster mismatch ---------------------
// A row that taps through to search already knows the year. Without it the shared scorer
// falls back on vote count and a brand-new title loses to an older one of the same name.
eq(api.searchQueryFor({ title: "Furious", year: 2026 }), "Furious 2026", "year is appended");
eq(api.searchQueryFor({ title: "Shrinking", year: "2023–present" }), "Shrinking 2023",
   "a year RANGE contributes only its first four digits");
eq(api.searchQueryFor({ title: "The Matrix" }), "The Matrix", "no year known → bare title");
eq(api.searchQueryFor({ title: "X", year: "" }), "X", "empty year is not appended");
eq(api.searchQueryFor({ title: "X", year: "unknown" }), "X", "unparseable year is not appended");
eq(api.searchQueryFor({}), "", "a shapeless item must not throw");
eq(api.searchQueryFor(null), "", "null item must not throw");

// --- statusTone: colour carries the verdict faster than the word ------------------
eq(api.statusTone("Cancelled"), "bad", "cancelled reads as bad");
eq(api.statusTone("Ended"), "done", "ended is neutral, not bad — it finished on its terms");
eq(api.statusTone("Renewal undecided"), "warn", "undecided is genuinely unresolved");
eq(api.statusTone("Returning"), "good", "returning is good");
eq(api.statusTone("Something else"), "good", "an unknown status must not read as a warning");

// --- noteAgeDays: drives whether stale intel is labelled as stale -----------------
eq(api.noteAgeDays(null), null, "no date → no opinion");
eq(api.noteAgeDays("not-a-date"), null, "unparseable date → no opinion, never NaN");
const days = api.noteAgeDays(new Date(Date.now() - 10 * 86400000).toISOString().slice(0, 10));
eq(days >= 9 && days <= 11, true, "a ten-day-old note measures about ten days");

// --- fmtRuntime / prettyEp: display-only, but wrong output is user-visible ---------
eq(api.fmtRuntime(136), "2h 16m", "a feature renders as hours and minutes");
eq(api.fmtRuntime(45), "45 min", "under an hour reads as minutes");
eq(api.fmtRuntime(120), "2h", "an exact hour count omits the trailing 0m");
eq(api.fmtRuntime(59), "59 min", "just under the hour boundary");
eq(api.fmtRuntime(60), "1h", "exactly the boundary crosses to hours");
eq(api.fmtRuntime(0), "", "zero runtime renders as nothing rather than '0m'");
eq(api.prettyEp("S01E01"), "S01E01", "a well-formed marker passes through");

// --- fmtRunway: the replacement for percent-downloaded ------------------------------
// NOTE 2026-08-31: this block used to sit AFTER the process.exit() below, so not one of
// these six assertions had ever executed — a test file that reported ALL PASS while
// silently skipping its own tail. Moved above the exit. Same shape as the "a detector
// that never fires looks exactly like one that works" rule.
const RW = new Function(extract("fmtRunway") + "\nreturn fmtRunway;")();
eq(RW(null), "", "no measurement → say nothing rather than zero");
eq(RW(30), "under a minute", "the about-to-stall case says so plainly");
eq(RW(59), "under a minute", "just under the boundary");
eq(RW(420), "~7 min", "the Silo case: 7 minutes, which is what 32% actually meant");
eq(RW(3600), "~1h 0m", "an hour reads as hours");
eq(RW(4500), "~1h 15m", "over an hour keeps the minutes");

// --- iosVlcUrls: the two schemes handed to VLC-iOS, in try order --------------------
// Both are registered by VLC-iOS (its own Info.plist lists `vlc` and `vlc-x-callback`).
// The documented x-callback form leads; the undocumented short form is the fallback.
const VU = new Function(extract("iosVlcUrls") + "\nreturn iosVlcUrls;")();
const forms = VU("http://spela.home/vlc/1/open.m3u?al=eng");
eq(forms.length, 2, "two forms to try, so a failure has somewhere to go");
eq(forms[0].url.startsWith("vlc-x-callback://x-callback-url/stream?url="), true,
   "the DOCUMENTED form leads: path `stream`, parameter `url`");
eq(forms[0].url.includes("%3A%2F%2F"), true,
   "the inner url is percent-escaped, or it breaks the outer scheme");
eq(forms[0].url.includes("?al=eng"), false,
   "the inner query must be escaped too, not left to split the outer one");
eq(forms[1].url, "vlc://spela.home/vlc/1/open.m3u?al=eng",
   "the fallback is the bare form the Mac handler already uses");
eq(forms.every(f => f.label && !/vlc-x-callback|vlc:\/\//.test(f.label)), true,
   "labels are human-readable, since they are offered to Fredrik in a toast");

// --- the source walk's note: announced in the loading panel ---------------------------
// The server walks down the ranked sources when one is dead and reports each switch in
// /progress `note`. Two things have to hold for the viewer to see it: the poll must take
// the note even when no warm-up is published (between two sources there is none), and the
// Chromecast panel must draw it. Both functions need a DOM or the network in the page, so
// they run here against stubs.
function extractFrom(marker) {
  const start = src.indexOf(marker);
  if (start < 0) throw new Error(`${marker} not found`);
  let i = src.indexOf("{", start);
  let depth = 0;
  for (; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}" && --depth === 0) return src.slice(start, i + 1);
  }
  throw new Error(`unbalanced braces extracting ${marker}`);
}
const mkPoll = (answer, S, onRender) =>
  new Function("api", "S", "renderWarmPanel",
    extractFrom("async function pollWarmup") + "\nreturn pollWarmup;")(
    async () => { if (answer instanceof Error) throw answer; return answer; }, S, onRender);

{
  const NOTE = "Source 1 did not deliver, trying source 2…";
  let renders = 0;
  const S = { now: { warming: true, note: null, warmup: { active: true, phase: "connecting" } } };
  await mkPoll({ active: false, note: NOTE }, S, () => renders++)();
  eq(S.now.note, NOTE, "a note arriving between two sources is taken");
  eq(S.now.warmup, null, "…and the dead source's last frame is dropped");
  eq(renders, 1, "…and the panel is redrawn once");

  await mkPoll({ active: false, note: NOTE }, S, () => renders++)();
  eq(renders, 1, "the same note again does not redraw an idle panel");

  const live = { active: true, phase: "downloading", note: NOTE };
  await mkPoll(live, S, () => renders++)();
  eq(S.now.warmup === live && renders === 2, true, "an active frame is shown, note kept");
  eq(S.now.note, NOTE, "the note survives into the next source's warm-up");

  await mkPoll({ active: false, note: null }, S, () => renders++)();
  eq(S.now.note, null, "the server clearing the note clears it here");

  const before = JSON.stringify(S.now);
  await mkPoll(new Error("offline"), S, () => renders++)();
  eq(JSON.stringify(S.now), before, "an unreachable server changes nothing");
}

{
  const NOTE = "Source 2 did not deliver, trying source 3…";
  const draw = (now) => {
    const root = { classList: { remove() {} }, innerHTML: "" };
    const stubs = {
      $: sel => (sel === "#warmpanel" ? root : null),
      S: { now },
      qualityLabel: () => "", warmBar: () => "<bar>", fmtSpeed: () => "1.2 MB/s",
      fmtRunway: () => "~7 min", esc: v => String(v), posterSrc: v => v, ic: () => "",
      api: async () => ({}), endWarming: () => {},
    };
    const names = Object.keys(stubs);
    new Function(...names, extract("renderWarmPanel") + "\nreturn renderWarmPanel;")(
      ...names.map(n => stubs[n]))();
    return root.innerHTML;
  };
  const cast = draw({ note: NOTE, warmupData: { title: "T" },
    warmup: { active: true, phase: "downloading", torrent: { percent: 3, speed_bps: 1, peers: 2 } } });
  eq(cast.includes("wpnote") && cast.includes(NOTE), true,
     "the Chromecast panel shows the note next to the progress, not instead of it");
  eq(cast.includes("Downloading"), true, "…and the progress line is still there");
  eq(draw({ note: null, warmupData: { title: "T" }, warmup: null }).includes("wpnote"), false,
     "no note, no line");
  eq(draw({ note: NOTE, warmupData: { title: "T" }, warmup: { active: true, vlc: true, pct: 10 } })
       .includes("wpnote"), false,
     "the VLC panel has its own note and does not draw this one");
}

// --- onDiskOffer: a complete copy on disk is stated on the card ----------------------
// Play best stays best picture. A copy on disk that is not the best source is never
// played in its place silently and never hidden either: the card shows it and the viewer
// picks. Until 2026-10-03 it was only visible inside the collapsed source list.
{
  const offer = new Function(extract("onDiskOffer") + "\nreturn onDiskOffer;")();
  const film = [
    { id: 1, quality: "1080p", size: "7.35 GB" },
    { id: 2, quality: "1080p", size: "5.88 GB", partial_pct: 40 },
    { id: 4, quality: "1080p", size: "1.76 GB", partial_pct: 100 },
    { id: 5, quality: "1080p", size: "1.76 GB", partial_pct: 100 },
  ];
  eq(offer(film), { id: 4, isBest: false, what: "1080p · 1.76 GB", best: "1080p · 7.35 GB" },
     "a smaller complete copy is offered by name, next to what the best source is");
  eq(offer([{ id: 1, quality: "2160p", size: "20 GB", partial_pct: 100 }, film[2]]).isBest, true,
     "when the best source is the one on disk, there is nothing to choose");
  eq(offer([film[0], film[1]]), null, "a partial download is not a copy on disk");
  eq(offer([film[0]]), null, "nothing on disk, nothing said");
  eq(offer([]), null, "an empty list must not throw");
  eq(offer(null), null, "a missing list must not throw");
  eq(offer([{ id: 1 }, { id: 2, partial_pct: 100 }]),
     { id: 2, isBest: false, what: "", best: "" },
     "missing quality and size leave the labels empty rather than 'undefined'");
}

console.log(failed === 0 ? "ALL PASS" : `${failed} FAILED`);
process.exit(failed === 0 ? 0 : 1);
