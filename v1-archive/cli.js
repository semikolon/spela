#!/usr/bin/env node
// spela CLI — thin HTTP client for the spela server
// Runs on any machine. Talks to server over HTTP.
// SPELA_SERVER env var or --server flag sets the server address.

const SERVER = process.env.SPELA_SERVER || (() => {
  const i = process.argv.indexOf('--server')
  return i >= 0 ? process.argv[i + 1] : 'darwin:7890'
})()

const human = process.argv.includes('--human')
const args = process.argv.slice(2).filter(a => a !== '--human' && a !== '--server' && process.argv[process.argv.indexOf(a) - 1] !== '--server')
const command = args[0]
const rest = args.slice(1)

async function api(method, path, body) {
  try {
    const opts = { method, headers: { 'Content-Type': 'application/json' } }
    if (body) opts.body = JSON.stringify(body)
    const r = await fetch(`http://${SERVER}${path}`, opts)
    return await r.json()
  } catch (e) { return { error: `Cannot reach server at ${SERVER}: ${e.message}` } }
}

function out(data) {
  if (!human) { console.log(JSON.stringify(data)); return }
  if (data.error) { console.error(`Error: ${data.error}`); process.exit(1) }
  if (data.results?.length) {
    if (data.show) console.log(`${data.show.title} (IMDB: ${data.show.imdb_id}) — ${data.show.status || ''}`)
    if (data.searching) console.log(`Searching: S${String(data.searching.season).padStart(2,'0')}E${String(data.searching.episode).padStart(2,'0')}`)
    console.log()
    data.results.forEach((r, i) => {
      console.log(`  ${i+1}. [${r.quality}] ${r.title}`)
      console.log(`     ${r.seeds} seeds · ${r.size} · ${r.source}`)
      console.log(`     ${r.magnet.slice(0, 80)}...`)
    })
    if (data.show?.next_episode) {
      const ne = data.show.next_episode
      console.log(`\nNext episode: S${String(ne.season).padStart(2,'0')}E${String(ne.episode).padStart(2,'0')} on ${ne.air_date}`)
    }
    return
  }
  if (data.status) {
    console.log(`Status: ${data.status}`)
    if (data.current) console.log(`  Playing: ${data.current.title} → ${data.current.target}`)
  }
  if (data.targets) data.targets.forEach(t => console.log(`  ${t.name} (${t.ip})`))
  if (data.history) data.history.slice(0, 10).forEach(h => console.log(`  ${h.watched_at?.slice(0,16)} ${h.title}`))
  if (data.preferences) console.log(JSON.stringify(data.preferences, null, 2))
  if (data.pid) console.log(`  PID: ${data.pid}`)
}

async function main() {
  switch (command) {
    case 'search': {
      const flags = rest.filter(a => a.startsWith('--'))
      const words = rest.filter(a => !a.startsWith('--')).join(' ')
      if (!words) { out({ error: 'Usage: spela search "show name" [--movie] [--season N --episode N]' }); return }
      const params = new URLSearchParams({ q: words })
      if (flags.includes('--movie')) params.set('movie', '1')
      const si = rest.indexOf('--season'); if (si >= 0) params.set('season', rest[si+1])
      const ei = rest.indexOf('--episode'); if (ei >= 0) params.set('episode', rest[ei+1])
      out(await api('GET', `/search?${params}`)); break
    }
    case 'play': {
      const magnet = rest.find(a => a.startsWith('magnet:'))
      if (!magnet) { out({ error: 'Usage: spela play "magnet:..." [--vlc] [--cast Name] [--title "..."]' }); return }
      const target = rest.includes('--vlc') ? 'vlc' : rest.includes('--mpv') ? 'mpv' : 'chromecast'
      const ci = rest.indexOf('--cast'); const castName = ci >= 0 ? rest[ci+1] : undefined
      const ti = rest.indexOf('--title'); const title = ti >= 0 ? rest[ti+1] : undefined
      out(await api('POST', '/play', { magnet, target, cast_name: castName, title })); break
    }
    case 'stop':    out(await api('POST', '/stop')); break
    case 'status':  out(await api('GET', '/status')); break
    case 'targets': out(await api('GET', '/targets')); break
    case 'history': out(await api('GET', '/history')); break
    case 'next':    out(await api('POST', '/next')); break
    case 'prev':    out(await api('POST', '/prev')); break
    case 'config':  out(rest.length >= 2 ? await api('POST', '/config', { [rest[0]]: rest[1] }) : await api('GET', '/config')); break
    default: out({
      usage: 'spela [--server HOST:PORT] <command> [args] [--human]',
      server: SERVER,
      commands: { search: '"query" [--movie] [--season N --episode N]', play: '"magnet:..." [--vlc] [--cast Name]', stop: '', status: '', targets: '', history: '', next: 'play next episode', prev: 'play previous episode', config: '[key value]' }
    })
  }
}

main()
