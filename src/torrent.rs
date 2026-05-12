// Apr 30, 2026 — slimmed down for v3.3.0 (librqbit-only).
//
// Pre-v3.3.0 this module wrapped the `webtorrent-cli` Node subprocess
// (`start_webtorrent`, `check_progress`, the webtorrent.pid plumbing,
// `stop_by_pid_file`, `save_pid`, `kill_all_webtorrent`,
// `kill_webtorrent_except`). Phase 3 deleted all of that — librqbit is
// in-process and lifecycle goes through `TorrentEngine::stop`. Cf. commit
// `a583c05` (Phase 1 foundation), `435f2ca` (Phase 2 wire-up), and the
// hard-won-lesson entry in CLAUDE.md § "webtorrent-cli weak peer discovery".
//
// What's left here is the generic process-PID utilities that the rest of
// spela still needs for **ffmpeg** lifecycle (the transcoder is still a
// subprocess) plus a defense-in-depth sweep for any pre-v3.3.0 webtorrent
// orphans that may still be running on a host that just upgraded.

use std::collections::HashSet;

const SIGTERM: i32 = 15;

// Match BOTH the legacy fragmented-MP4 path (transcoded_aac.mp4, retained
// for the Custom Cast Receiver flow) AND the Apr 15, 2026 HLS path
// (transcoded_hls/ directory containing playlist.m3u8 + segments). Either
// signature should be enough to fingerprint a spela-owned ffmpeg child
// without sweeping unrelated ffmpeg processes on the host.
const SPELA_FFMPEG_PROCESS_PATTERN: &str =
    "ffmpeg.*transcoded_aac\\.mp4|transcoded_aac\\.mp4|ffmpeg.*transcoded_hls|transcoded_hls/playlist\\.m3u8";

// Pre-v3.3.0 spela ran the Node `webtorrent-cli`; on first boot after
// upgrade there may still be one running from a stale launchd session.
// `reconcile_session_state_on_startup` fires this once at server start.
const LEGACY_WEBTORRENT_PROCESS_PATTERN: &str = "WebTorrent|webtorrent";

/// Kill a process by PID (SIGTERM).
pub fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as i32, SIGTERM);
    }
}

/// Check if a process is running (signal 0).
pub unsafe fn kill_check(pid: u32) -> bool {
    libc::kill(pid as i32, 0) == 0
}

/// One-shot startup defense: kill any lingering Node webtorrent workers
/// from pre-v3.3.0 deployments that survived an upgrade. Returns the PIDs
/// terminated, for logging.
pub fn kill_lingering_webtorrent_workers() -> Vec<u32> {
    kill_matching(LEGACY_WEBTORRENT_PROCESS_PATTERN, &[])
}

/// Kill Spela-owned ffmpeg workers that write the transient transcode artifact.
pub fn kill_spela_ffmpeg_workers() -> Vec<u32> {
    kill_matching(SPELA_FFMPEG_PROCESS_PATTERN, &[])
}

/// Liveness check: is at least one Spela-owned ffmpeg worker producing
/// HLS segments / fMP4 right now? This is the ground truth for "user
/// is watching something" — if ffmpeg is dead, the cast pipeline is
/// dead regardless of what the torrent engine says or what
/// `app_state.current.pid` claims.
///
/// Used by `handle_status` to replace the legacy
/// `is_process_running(current.pid)` check, which compared a librqbit
/// torrent ID (small u32 like 4/5/6) to the OS PID space — only
/// "worked" when the torrent ID happened to match a live OS process,
/// otherwise reported `process_dead` even with a perfectly healthy
/// stream. May 6, 2026 incident: status returned `process_dead` while
/// ffmpeg was actively encoding S05E06 to Fredriks TV. Ruby read the
/// dead-status, narrated failure, retried, narrated success, retried,
/// loop.
pub fn any_spela_ffmpeg_alive() -> bool {
    !pids_matching(SPELA_FFMPEG_PROCESS_PATTERN).is_empty()
}

/// Emergency worker-only cleanup. Sends SIGTERM and does not delete media or
/// mutate playback state. Belt-and-suspenders for diagnostic flows
/// (post-cast-failure, OPERATIONS.md emergency path); the normal lifecycle
/// goes through `TorrentEngine::stop` for torrents and the reaper for ffmpeg.
pub fn kill_all_workers() -> (Vec<u32>, Vec<u32>) {
    (
        kill_lingering_webtorrent_workers(),
        kill_spela_ffmpeg_workers(),
    )
}

fn pids_matching(pattern: &str) -> Vec<u32> {
    let output = std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output();
    match output {
        Ok(output) => parse_pgrep_pids(&String::from_utf8_lossy(&output.stdout)),
        Err(_) => Vec::new(),
    }
}

fn kill_matching(pattern: &str, allowed_pids: &[u32]) -> Vec<u32> {
    let allowed: HashSet<u32> = allowed_pids
        .iter()
        .copied()
        .filter(|pid| *pid > 0)
        .collect();
    let self_pid = std::process::id();
    let mut killed = Vec::new();
    for pid in pids_matching(pattern) {
        if pid == self_pid || allowed.contains(&pid) || unsafe { !kill_check(pid) } {
            continue;
        }
        kill_pid(pid);
        killed.push(pid);
    }
    killed
}

fn parse_pgrep_pids(output: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter_map(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pgrep_pids_ignores_non_pid_lines() {
        assert_eq!(
            parse_pgrep_pids("123\nnot-a-pid\n456 webtorrent\n0\n"),
            vec![123, 456]
        );
    }
}

// Minimal libc bindings for kill()
mod libc {
    extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}
