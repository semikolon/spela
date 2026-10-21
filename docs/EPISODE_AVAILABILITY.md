# Episode schedules and source availability

## Contract and evidence

The user wants new episodes to become actionable promptly, without false Play buttons,
synthetic release-hour delays, repeated manual retries, or unsolicited downloads.
A scheduled release, an indexed source, a healthy torrent, and a playable buffered file
are different facts. Never use one as proof of all the others.

An explicitly published broadcaster/platform schedule is the primary authority for
planned airtime. Actual platform availability is stronger evidence that release has
happened. Spela uses verified TVmaze episode lists, with TMDB as the metadata fallback;
it does **not** poll Apple or other streaming services directly.

[TVmaze's airtime policy](https://www.tvmaze.com/faq/15/episodes#airtime-release-time)
explicitly excludes release times for global web channels. An RFC3339 `airstamp`
therefore does not by itself establish an announced release hour. Only when `airtime`
is populated does Spela interpret the instant and convert its calendar date to
Europe/Stockholm. Otherwise it retains `airdate`. For example, a synthetic
`12:00:00+00:00` becomes 14:00 Stockholm in October daylight-saving time, but that
conversion does not make it a real platform release time.

On the announced day, search may find sources before a nominal timestamp. Eligible
sources override that timing gate. Future **calendar dates** remain gated. Source
availability here means title-filtered Torrentio results eligible for automatic play,
not a verified swarm connection or decoded media. Seed counts are claims, not measured
throughput. Do not describe a source listing as a playback health check.

## Cadence and ownership

The server owns the monitor; no browser tab or external scheduled automation is needed.
`server::run_server` runs a sequential sweep of followed shows every minute. The shared
per-episode cache enforces the actual source-lookup intervals:

| Trigger | Lookup policy |
| --- | --- |
| Background, release day | Missing/unknown sources: at most once per 15 minutes |
| Background, following two Stockholm calendar days | At most once per hour |
| Background, later | No further source lookups |
| Home / following view | Recheck tracked missing sources when at least 5 minutes old |
| Explicit episode search / Play | Reuse source results for at most 60 seconds |
| Sources already found | Stop background and Home source refreshes; explicit search can refresh |

These are minimum cache ages, not exact wall-clock appointments: the minute sweep and
network latency can delay a check. Watched-through episodes and marked-seen seasons
are skipped. The monitor only requests metadata; it never adds torrents, launches VLC,
or changes `last_search`. Up-next metadata preparation is also not download prefetch.

`SearchEngine::episode_sources` keys by IMDb ID, season, episode, and uses a per-key
async mutex to coalesce simultaneous checks. Results are cloned before caller-specific
ranking/annotation. Successful discovery remains positive evidence for Home even if a
later explicit refresh fails. Explicit search still receives that refresh's error,
rather than silently pretending stale results are fresh.

A valid empty `streams` array means no sources found. HTTP failure, malformed payload,
or JSON without a streams array means the provider could not be checked. Errors are
cached at the same trigger-specific intervals to avoid retry storms. They must never
become an empty-success result or an assertion that the episode is unreleased.

## Home, Search, and watch progress

`tv_status_sources` applies availability evidence to recent schedule entries before
choosing the latest available and next pending episode. Home derives new counts from
that result and the watch ledger. Pending rows use `source_wait` to distinguish
`awaiting_sources` from `check_failed`; Search exposes `searching.unaired`, and its
quick-play path explains a wait rather than treating it as a failed playback.

The schedule is not watch history. `following.json` is membership plus seen-season
preferences; the ledger derives watched-through. Continue hides watched identities
and aliases while retaining resume data. Availability checks must never mark an
episode watched or rewrite a viewer's place.

## Current limits and extension points

- Availability and metadata caches are **in memory**. Restart discards them; positive
  discovery is not a durable database record. The source cache evicts unreferenced
  entries when admitting new keys at its 512-entry threshold; this is not an LRU.
- Background availability reconciliation currently requires a verified TVmaze match
  and an IMDb ID. TMDB supplies fallback schedule metadata, but it does not yet drive
  the same source-monitor loop when TVmaze is unavailable/unmatched.
- Home initially checks episodes dated today or the previous two days, rather than
  crawling the whole backlog. Older entries already tracked in this process can be
  rechecked on demand. After restart/eviction, an older missing episode no longer has
  that remembered state; explicit Search still checks it.
- TVmaze schedules cache for six hours; TMDB show snapshots for five minutes. The
  source cadence does not promise discovery of newly added schedule metadata every
  fifteen minutes.
- The tracker still represents availability with latest/next episode high-water marks,
  not an arbitrary per-episode availability ledger. Batch releases with holes need
  separate design before claiming every counted episode has a verified source.
- No platform-specific “always drops at X” heuristic, alternate provider, durable
  polling journal, or notification system has been introduced.

These are implementation limits, not a reversal of the intended cadence. Improve them
when needed without turning a lightweight source monitor into a download scheduler.

## Verification and diagnosis

Read `/home` **before** manually searching the episode: otherwise Search can warm the
cache and conceal a broken independent Home check. Then read `/search` with the explicit
episode and target. Check new/coming-soon membership, `searching.unaired`, errors, and
source count together. Never use an empty gated search response as evidence that the
provider itself has no sources. Provider lookup failure must be examined separately.

Regression coverage in `src/search.rs` covers explicit airtime versus synthetic stamps,
Stockholm date boundaries, discovery overriding timing, missing/error states, cadence
and cutoff, cached errors, and positive discovery surviving a failed refresh. The SPA
pure tests cover release labels and quick-play wait handling. Deployment validation
confirmed Home independently listed the affected finale as new and Search returned
sources; that verifies availability routing, not video playback or day-long cadence.

Related: [watch-tracker design](watch_tracker_roadmap.md),
[playback readiness and handoff](PLAYBACK_READINESS.md),
[operational safeguards](../OPERATIONS.md).
