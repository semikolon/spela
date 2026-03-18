#!/usr/bin/env node
// spela server — HTTP API for media streaming control
// Runs on Darwin. Clients (CLI, voice agents) connect via HTTP from any machine.
//
// Search: TMDB (metadata) + Torrentio (magnet links). Zero scraping.
// Stream: webtorrent-cli to Chromecast/VLC/mpv.
// State: ~/.spela/state.json (resume, history, preferences)
//
// Start: node server.js [--port 7890]
// Endpoints: /search /play /stop /status /targets /history /config /next /prev

import { createServer } from 'http'
import { readFileSync, writeFileSync, existsSync, mkdirSync, openSync } from 'fs'
import { execSync, spawn } from 'child_process'
import { homedir } from 'os'
import { fileURLToPath } from 'url'
import { dirname } from 'path'

// Load .env from script directory (TMDB_API_KEY etc)
const __dirname = dirname(fileURLToPath(import.meta.url))
try {
  const envFile = readFileSync(`${__dirname}/.env`, 'utf8')
  for (const line of envFile.split('\n')) {
    const match = line.match(/^(\w+)=['"]?([^'"]*?)['"]?\s*$/)
    if (match && !process.env[match[1]]) process.env[match[1]] = match[2]
  }
} catch {}
import { join } from 'path'

const PORT = parseInt(process.argv.find((_, i) => process.argv[i-1] === '--port') || '7890')
const STATE_DIR = join(homedir(), '.spela')
const STATE_FILE = join(STATE_DIR, 'state.json')
const PID_FILE = join(STATE_DIR, 'webtorrent.pid')
const MEDIA_DIR = join(homedir(), 'media')

const PUBLIC_TRACKERS = [
  'udp://tracker.opentrackr.org:1337/announce',
  'udp://open.stealth.si:80/announce',
  'udp://tracker.torrent.eu.org:451/announce',
  'udp://tracker.bittor.pw:1337/announce',
  'udp://explodie.org:6969/announce'
]

if (!existsSync(STATE_DIR)) mkdirSync(STATE_DIR, { recursive: true })
if (!existsSync(MEDIA_DIR)) mkdirSync(MEDIA_DIR, { recursive: true })

// --- State ---
function loadState() {
  try { return JSON.parse(readFileSync(STATE_FILE, 'utf8')) }
  catch { return { current: null, history: [], preferences: { default_target: 'chromecast', chromecast_name: null, preferred_quality: '1080p' } } }
}
function saveState(s) { writeFileSync(STATE_FILE, JSON.stringify(s, null, 2)) }

// --- TMDB ---
async function tmdbSearch(query, type = 'tv') {
  const key = process.env.TMDB_API_KEY
  if (!key) return { error: 'TMDB_API_KEY not set. Get one free at themoviedb.org' }

  const resp = await fetch(`https://api.themoviedb.org/3/search/${type}?query=${encodeURIComponent(query)}&api_key=${key}`)
  const data = await resp.json()
  if (!data.results?.length) return { error: `No ${type} found for "${query}"` }
  return data.results[0]
}

async function tmdbShowDetails(tmdbId) {
  const key = process.env.TMDB_API_KEY
  const resp = await fetch(`https://api.themoviedb.org/3/tv/${tmdbId}?api_key=${key}&append_to_response=external_ids`)
  return await resp.json()
}

async function tmdbMovieDetails(tmdbId) {
  const key = process.env.TMDB_API_KEY
  const resp = await fetch(`https://api.themoviedb.org/3/movie/${tmdbId}?api_key=${key}&append_to_response=external_ids`)
  return await resp.json()
}

// --- Torrentio ---
function buildMagnet(infoHash, name) {
  const trackers = PUBLIC_TRACKERS.map(t => `&tr=${encodeURIComponent(t)}`).join('')
  return `magnet:?xt=urn:btih:${infoHash}&dn=${encodeURIComponent(name || '')}${trackers}`
}

function parseTorrentioTitle(title) {
  // Parse emoji-encoded metadata: 👤 seeders 💾 size ⚙️ source
  const seedMatch = title.match(/👤\s*(\d+)/)
  const sizeMatch = title.match(/💾\s*([\d.]+ [A-Z]+)/)
  const sourceMatch = title.match(/⚙️\s*(\S+)/)
  return {
    seeds: seedMatch ? parseInt(seedMatch[1]) : 0,
    size: sizeMatch ? sizeMatch[1] : '',
    source: sourceMatch ? sourceMatch[1] : ''
  }
}

async function torrentioStreams(imdbId, season, episode) {
  const path = season != null
    ? `stream/series/${imdbId}:${season}:${episode}.json`
    : `stream/movie/${imdbId}.json`
  const url = `https://torrentio.strem.fun/sort=seeders/${path}`

  const resp = await fetch(url, { headers: { 'User-Agent': 'spela/0.1' } })
  if (!resp.ok) throw new Error(`Torrentio returned ${resp.status}`)
  const data = await resp.json()

  return (data.streams || []).map((s, i) => {
    const meta = parseTorrentioTitle(s.title || '')
    const quality = (s.name || '').replace('Torrentio\n', '').trim()
    return {
      id: i + 1,
      quality,
      title: s.behaviorHints?.filename || s.title?.split('\n')[0] || 'Unknown',
      seeds: meta.seeds,
      size: meta.size,
      source: meta.source,
      magnet: buildMagnet(s.infoHash, s.behaviorHints?.filename),
      info_hash: s.infoHash,
      file_index: s.fileIdx
    }
  })
}

// --- Search Orchestrator ---
async function search(query, opts = {}) {
  const type = opts.movie ? 'movie' : 'tv'

  // Step 1: TMDB lookup
  const tmdbResult = await tmdbSearch(query, type)
  if (tmdbResult.error) return { query, error: tmdbResult.error, results: [] }

  const tmdbId = tmdbResult.id
  let imdbId, showInfo

  if (type === 'tv') {
    const detail = await tmdbShowDetails(tmdbId)
    imdbId = detail.external_ids?.imdb_id
    showInfo = {
      tmdb_id: tmdbId,
      imdb_id: imdbId,
      title: detail.name,
      seasons: detail.number_of_seasons,
      status: detail.status,
      latest_episode: detail.last_episode_to_air ? {
        season: detail.last_episode_to_air.season_number,
        episode: detail.last_episode_to_air.episode_number,
        name: detail.last_episode_to_air.name,
        air_date: detail.last_episode_to_air.air_date
      } : null,
      next_episode: detail.next_episode_to_air ? {
        season: detail.next_episode_to_air.season_number,
        episode: detail.next_episode_to_air.episode_number,
        air_date: detail.next_episode_to_air.air_date
      } : null
    }

    if (!imdbId) return { query, show: showInfo, error: 'No IMDB ID found for this show', results: [] }

    // Determine which episode to search
    let season = opts.season, episode = opts.episode
    if (!season && !episode && showInfo.latest_episode) {
      season = showInfo.latest_episode.season
      episode = showInfo.latest_episode.episode
    }
    if (!season) return { query, show: showInfo, error: 'Cannot determine episode to search', results: [] }

    // Step 2: Torrentio lookup
    const results = await torrentioStreams(imdbId, season, episode)
    return {
      query,
      show: showInfo,
      searching: { season, episode },
      torrent_available: results.length > 0,
      results: results.slice(0, 8)
    }
  } else {
    // Movie
    const detail = await tmdbMovieDetails(tmdbId)
    imdbId = detail.external_ids?.imdb_id || detail.imdb_id
    showInfo = {
      tmdb_id: tmdbId,
      imdb_id: imdbId,
      title: detail.title,
      release_date: detail.release_date,
      overview: detail.overview?.slice(0, 200)
    }

    if (!imdbId) return { query, show: showInfo, error: 'No IMDB ID', results: [] }

    const results = await torrentioStreams(imdbId)
    return {
      query,
      show: showInfo,
      torrent_available: results.length > 0,
      results: results.slice(0, 8)
    }
  }
}

// --- Subtitle Fetching (Stremio OpenSubtitles v3 addon — zero auth) ---
async function fetchSubtitles(imdbId, season, episode, lang = 'eng') {
  try {
    const id = season != null ? `${imdbId}:${season}:${episode}` : imdbId
    const type = season != null ? 'series' : 'movie'
    const resp = await fetch(`https://opensubtitles-v3.strem.io/subtitles/${type}/${id}.json`)
    if (!resp.ok) return null

    const data = await resp.json()
    const subs = (data.subtitles || []).filter(s => s.lang === lang)
    if (!subs.length) return null

    // Download best subtitle to media dir
    const srtPath = join(MEDIA_DIR, `subtitle_${lang}.srt`)
    const srtResp = await fetch(subs[0].url)
    if (!srtResp.ok) return null

    const srtText = await srtResp.text()
    writeFileSync(srtPath, srtText)
    return srtPath
  } catch (err) {
    console.log(`Subtitle fetch failed: ${err.message}`)
    return null
  }
}

// --- Chromecast Control (via cast.py — bulletproof with retry) ---
const CAST_PY = join(dirname(fileURLToPath(import.meta.url)), 'cast.py')
const CAST_PYTHON = join(homedir(), '.local/share/pipx/venvs/catt/bin/python3')

function castCommand(cmd, args = []) {
  try {
    const result = execSync(
      `${CAST_PYTHON} ${CAST_PY} ${cmd} ${args.map(a => `"${a}"`).join(' ')}`,
      { encoding: 'utf8', timeout: 30000 }
    )
    return JSON.parse(result.trim())
  } catch (err) {
    const stderr = err.stderr?.toString() || err.stdout?.toString() || err.message
    try { return JSON.parse(stderr.trim()) } catch {}
    return { error: `Cast command failed: ${stderr.slice(0, 200)}` }
  }
}

function discoverChromecasts() {
  const result = castCommand('scan')
  return result.devices || []
}

// --- Disk Watchdog ---
function checkDiskSpace() {
  try {
    const du = execSync(`du -sm ${MEDIA_DIR}`, { encoding: 'utf8' })
    const sizeMB = parseInt(du.split('\t')[0])
    if (sizeMB > 5000) return { error: `~/media/ is ${sizeMB}MB (>5GB cap). Clean up first.` }
  } catch {}
  return null
}

function cleanupOldFiles() {
  try {
    // Delete files older than 24 hours
    execSync(`find ${MEDIA_DIR} -type f -mmin +1440 -delete 2>/dev/null || true`)
    // Delete empty directories
    execSync(`find ${MEDIA_DIR} -type d -empty -delete 2>/dev/null || true`)
  } catch {}
}

// --- Playback ---
async function startStream(magnet, opts = {}) {
  const diskCheck = checkDiskSpace()
  if (diskCheck) return diskCheck

  cleanupOldFiles()

  const state = loadState()
  stopStream(true)

  const target = opts.target || state.preferences.default_target || 'chromecast'
  const castName = opts.cast_name || state.preferences.chromecast_name || 'Fredriks TV'
  const noSubs = opts.no_subs === true
  const subLang = opts.subtitle_lang || 'eng'

  // Start webtorrent as HTTP file server (NOT with --chromecast — it's broken)
  const wtArgs = ['download', magnet, '-o', MEDIA_DIR, '-p', '8888']
  if (opts.file_index != null) {
    wtArgs.push('-s', String(opts.file_index))
  }

  const logFile = join(STATE_DIR, 'webtorrent.log')
  const logFd = openSync(logFile, 'w')
  const proc = spawn('webtorrent', wtArgs, { detached: true, stdio: ['ignore', logFd, logFd], env: process.env })
  writeFileSync(PID_FILE, proc.pid.toString())
  proc.unref()

  // Wait for webtorrent HTTP server to be ready (parse log for server URL)
  let serverUrl = null
  for (let i = 0; i < 30; i++) {
    await new Promise(r => setTimeout(r, 1000))
    try {
      const log = readFileSync(logFile, 'utf8')
      const match = log.match(/Server running at: (http:\/\/localhost:\d+\/[^\n]+)/)
      if (match) {
        serverUrl = match[1].replace('localhost', '192.168.4.1').trim()
        break
      }
    } catch {}
  }

  if (!serverUrl) {
    return { error: 'webtorrent failed to start HTTP server within 30s', log: readFileSync(logFile, 'utf8').slice(-500) }
  }

  // Auto-detect audio codec and transcode if needed
  let finalUrl = serverUrl
  try {
    const probeResult = execSync(`ffprobe -v error -select_streams a -show_entries stream=codec_name "${serverUrl}" 2>&1`, { encoding: 'utf8', timeout: 15000 })
    const audioCodec = probeResult.match(/codec_name=(\w+)/)?.[1]

    if (audioCodec && ['ac3', 'eac3', 'dts', 'truehd'].includes(audioCodec)) {
      console.log(`Audio codec ${audioCodec} needs transcode → AAC`)
      const transcodedPath = join(MEDIA_DIR, 'transcoded_aac.mp4')
      // Start transcode in background — progressive, so casting can start once enough is ready
      const ffArgs = ['-i', serverUrl, '-c:v', 'copy', '-c:a', 'aac', '-ac', '2', '-b:a', '192k', '-movflags', '+faststart', '-y', transcodedPath]
      const ffProc = spawn('ffmpeg', ffArgs, { detached: true, stdio: 'ignore' })
      ffProc.unref()
      // Wait for initial transcode buffer (10s of content)
      await new Promise(r => setTimeout(r, 5000))
      // Serve transcoded file via python HTTP server
      const httpProc = spawn('python3', ['-m', 'http.server', '8889', '--directory', MEDIA_DIR], { detached: true, stdio: 'ignore' })
      httpProc.unref()
      finalUrl = `http://192.168.4.1:8889/transcoded_aac.mp4`
      await new Promise(r => setTimeout(r, 2000))
    }
  } catch (err) {
    console.log(`Audio probe failed (casting anyway): ${err.message}`)
  }

  // Fetch subtitles (unless --no-subs)
  let subtitlePath = null
  if (!noSubs && opts.imdb_id) {
    subtitlePath = await fetchSubtitles(opts.imdb_id, opts.season, opts.episode, subLang)
    if (subtitlePath) console.log(`Subtitles fetched: ${subtitlePath}`)
  }

  // Cast to Chromecast via cast.py (bulletproof retry)
  if (target === 'chromecast') {
    const castArgs = [castName, finalUrl]
    if (subtitlePath) castArgs.push('--subtitle', subtitlePath)
    const castResult = castCommand('cast', castArgs)
    if (castResult.error) return { error: `Cast failed: ${castResult.error}`, url: finalUrl }

    // Seek to saved position if resuming
    if (opts.seek_to) {
      await new Promise(r => setTimeout(r, 3000)) // Let cast settle
      castCommand('seek', [castName, String(opts.seek_to)])
    }
  } else if (target === 'vlc') {
    spawn('webtorrent', ['stream', magnet, '--vlc', '-o', MEDIA_DIR], { detached: true, stdio: 'ignore' })
  }

  state.current = {
    magnet: magnet.slice(0, 300),
    title: opts.title || 'Unknown',
    show: opts.show || null,
    season: opts.season || null,
    episode: opts.episode || null,
    imdb_id: opts.imdb_id || null,
    target: `${target}:${castName}`,
    url: finalUrl,
    started_at: new Date().toISOString(),
    pid: proc.pid,
    has_subtitles: !!subtitlePath,
    subtitle_lang: subtitlePath ? subLang : null
  }
  saveState(state)
  return { status: 'streaming', pid: proc.pid, target: state.current.target, title: state.current.title, subtitles: !!subtitlePath, url: finalUrl }
}

function stopStream(quiet = false) {
  try {
    if (existsSync(PID_FILE)) {
      const pid = parseInt(readFileSync(PID_FILE, 'utf8'))
      if (pid) try { process.kill(pid, 'SIGTERM') } catch {}
      try { execSync('pkill -f "webtorrent stream" 2>/dev/null || true') } catch {}
    }
    writeFileSync(PID_FILE, '')
  } catch {}

  if (!quiet) {
    const state = loadState()
    if (state.current) {
      state.history.unshift({ ...state.current, watched_at: new Date().toISOString() })
      state.history = state.history.slice(0, 50)
      state.current = null
      saveState(state)
    }
    return { status: 'stopped' }
  }
}

function getStatus() {
  const state = loadState()
  if (!state.current) return { status: 'idle' }
  let running = false
  try { if (state.current.pid) { process.kill(state.current.pid, 0); running = true } } catch {}
  return { status: running ? 'streaming' : 'process_dead', current: state.current, running }
}

// --- Next/Prev Episode ---
async function navigateEpisode(direction) {
  const state = loadState()
  if (!state.current?.show || !state.current?.season || !state.current?.episode) {
    return { error: 'No show/episode context — play a TV episode first' }
  }

  let season = state.current.season
  let episode = state.current.episode + (direction === 'next' ? 1 : -1)

  if (episode < 1) {
    season -= 1; episode = 99 // Will be clamped by search results
  }

  const result = await search(state.current.show, { season, episode })
  if (!result.torrent_available) {
    return { error: `No torrent found for S${String(season).padStart(2,'0')}E${String(episode).padStart(2,'0')}`, searched: result }
  }

  const best = result.results[0]
  return startStream(best.magnet, {
    title: `${state.current.show} S${String(season).padStart(2,'0')}E${String(episode).padStart(2,'0')}`,
    show: state.current.show,
    season, episode,
    imdb_id: result.show?.imdb_id,
    target: state.current.target?.split(':')[0],
    cast_name: state.current.target?.split(':')[1]
  })
}

// --- HTTP Server ---
const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`)
  res.setHeader('Content-Type', 'application/json')
  res.setHeader('Access-Control-Allow-Origin', '*')
  if (req.method === 'OPTIONS') { res.setHeader('Access-Control-Allow-Methods', 'GET,POST'); res.setHeader('Access-Control-Allow-Headers', 'Content-Type'); res.writeHead(200); res.end(); return }

  let body = {}
  if (req.method === 'POST') {
    const chunks = []; for await (const c of req) chunks.push(c)
    try { body = JSON.parse(Buffer.concat(chunks).toString()) } catch {}
  }

  const json = (status, data) => { res.writeHead(status); res.end(JSON.stringify(data)) }

  try {
    switch (url.pathname) {
      case '/search': {
        const q = url.searchParams.get('q') || body.q
        if (!q) { json(400, { error: 'Missing q parameter' }); return }
        const season = url.searchParams.get('season') || body.season
        const episode = url.searchParams.get('episode') || body.episode
        const result = await search(q, {
          movie: url.searchParams.has('movie') || body.movie,
          season: season ? parseInt(season) : undefined,
          episode: episode ? parseInt(episode) : undefined
        })
        json(200, result); break
      }
      case '/play': {
        const magnet = body.magnet || url.searchParams.get('magnet')
        if (!magnet) { json(400, { error: 'Missing magnet' }); return }
        json(200, startStream(magnet, body)); break
      }
      case '/stop':    json(200, stopStream()); break
      case '/status':  json(200, getStatus()); break
      case '/pause': {
        const s = loadState()
        const device = s.current?.target?.split(':')?.[1] || 'Fredriks TV'
        json(200, castCommand('pause', [device])); break
      }
      case '/resume': {
        const s = loadState()
        const device = s.current?.target?.split(':')?.[1] || 'Fredriks TV'
        json(200, castCommand('resume', [device])); break
      }
      case '/seek': {
        const s = loadState()
        const device = s.current?.target?.split(':')?.[1] || 'Fredriks TV'
        const secs = url.searchParams.get('t') || body.t || body.seconds
        if (!secs) { json(400, { error: 'Missing t (seconds) parameter' }); break }
        json(200, castCommand('seek', [device, String(secs)])); break
      }
      case '/volume': {
        const s = loadState()
        const device = s.current?.target?.split(':')?.[1] || 'Fredriks TV'
        const vol = url.searchParams.get('level') || body.level
        if (!vol) { json(400, { error: 'Missing level (0-100) parameter' }); break }
        json(200, castCommand('volume', [device, String(vol)])); break
      }
      case '/cast-info': {
        const s = loadState()
        const device = body.device || s.current?.target?.split(':')?.[1] || 'Fredriks TV'
        json(200, castCommand('info', [device])); break
      }
      case '/targets': json(200, { targets: discoverChromecasts() }); break
      case '/history': json(200, { history: loadState().history.slice(0, 20) }); break
      case '/next':    json(200, await navigateEpisode('next')); break
      case '/prev':    json(200, await navigateEpisode('prev')); break
      case '/config': {
        const state = loadState()
        if (req.method === 'POST') { Object.assign(state.preferences, body); saveState(state) }
        json(200, { preferences: state.preferences }); break
      }
      default: json(404, { error: `Unknown: ${url.pathname}`, endpoints: ['/search','/play','/stop','/status','/targets','/history','/next','/prev','/config'] })
    }
  } catch (err) { json(500, { error: err.message }) }
})

server.listen(PORT, '0.0.0.0', () => {
  console.log(`spela server listening on http://0.0.0.0:${PORT}`)
  console.log('Endpoints: /search /play /stop /status /targets /history /next /prev /config')
})
