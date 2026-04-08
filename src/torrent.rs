use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::time::{sleep, Duration};

const WEBTORRENT_PROCESS_PATTERN: &str = "WebTorrent|webtorrent";
const SPELA_FFMPEG_PROCESS_PATTERN: &str = "ffmpeg.*transcoded_aac\\.mp4|transcoded_aac\\.mp4";
const SIGTERM: i32 = 15;

/// Start webtorrent-cli as an HTTP file server.
/// Returns (PID, HTTP URL for the served file).
pub async fn start_webtorrent(
    magnet: &str,
    file_index: Option<u32>,
    media_dir: &Path,
    stream_host: &str,
    log_path: &Path,
) -> Result<(u32, String)> {
    std::fs::create_dir_all(media_dir)?;

    let log_file = std::fs::File::create(log_path)?;
    let log_err = log_file.try_clone()?;

    let mut args = vec![
        "download".to_string(),
        magnet.to_string(),
        "-o".to_string(),
        media_dir.to_string_lossy().to_string(),
        "-p".to_string(),
        "8888".to_string(),
        "--keep-seeding".to_string(),
    ];
    if let Some(idx) = file_index {
        args.push("-s".to_string());
        args.push(idx.to_string());
    }

    let mut child = Command::new("webtorrent")
        .args(&args)
        .env("NODE_OPTIONS", "--max-old-space-size=4096")
        .stdout(log_file)
        .stderr(log_err)
        .spawn()?;

    let pid = child.id().ok_or_else(|| anyhow!("Failed to get webtorrent PID"))?;
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => tracing::debug!("webtorrent process {} exited with {}", pid, status),
            Err(err) => tracing::warn!("failed to reap webtorrent process {}: {}", pid, err),
        }
    });

    // Wait for HTTP server to be ready (parse log for URL)
    let mut server_url = None;
    for i in 0..300 {
        sleep(Duration::from_secs(1)).await;

        // Check if process is still alive
        if unsafe { !kill_check(pid) } {
            let log_content = std::fs::read_to_string(log_path).unwrap_or_default();
            return Err(anyhow!("Webtorrent process (PID {}) died during startup. Log: {}", pid, log_content));
        }

        if let Ok(log) = std::fs::read_to_string(log_path) {
            if i % 10 == 0 && log.contains("verifying existing torrent data") {
                tracing::info!("Webtorrent: verifying existing data (hashing)...");
            }
            if let Some(cap) = log.lines().find(|l| l.contains("Server running at:")) {
                if let Some(url_start) = cap.find("http://") {
                    let url = cap[url_start..].trim().to_string();
                    server_url = Some(url.replace("localhost", stream_host));
                    break;
                }
            }
        }
    }

    match server_url {
        Some(url) => Ok((pid, url)),
        None => {
            kill_pid(pid);
            let log_tail = std::fs::read_to_string(log_path)
                .unwrap_or_default()
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            Err(anyhow!("webtorrent failed to start HTTP server within 30s. Log: {}", log_tail))
        }
    }
}

/// Check if webtorrent is making download progress by parsing the log.
/// Returns true if any data has been downloaded (> 0%).
pub async fn check_progress(log_path: &Path, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        sleep(Duration::from_secs(2)).await;
        if let Ok(log) = std::fs::read_to_string(log_path) {
            // webtorrent logs progress like "12% ... 1.2 MB/s"
            // Any percentage > 0 or any non-zero speed means we're downloading
            for line in log.lines().rev().take(10) {
                // Check for percentage progress
                if let Some(pct_pos) = line.find('%') {
                    if pct_pos > 0 {
                        let before = &line[..pct_pos];
                        let num_str: String = before.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
                        if let Ok(pct) = num_str.parse::<u32>() {
                            if pct > 0 { return true; }
                        }
                    }
                }
                // Check for non-zero download speed
                if line.contains("MB/s") || line.contains("KB/s") {
                    let has_speed = line.split_whitespace().any(|w| {
                        w.parse::<f64>().map(|v| v > 0.0).unwrap_or(false)
                    });
                    if has_speed { return true; }
                }
            }
        }
    }
    false
}

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

/// Kill all WebTorrent workers except explicitly allowed PIDs.
pub fn kill_webtorrent_except(allowed_pids: &[u32]) -> Vec<u32> {
    kill_matching(WEBTORRENT_PROCESS_PATTERN, allowed_pids)
}

/// Kill all WebTorrent workers.
pub fn kill_all_webtorrent() -> Vec<u32> {
    kill_webtorrent_except(&[])
}

/// Kill Spela-owned ffmpeg workers that write the transient transcode artifact.
pub fn kill_spela_ffmpeg_workers() -> Vec<u32> {
    kill_matching(SPELA_FFMPEG_PROCESS_PATTERN, &[])
}

/// Emergency worker-only cleanup. Sends SIGTERM and does not delete media or
/// mutate playback state.
pub fn kill_all_workers() -> (Vec<u32>, Vec<u32>) {
    (kill_all_webtorrent(), kill_spela_ffmpeg_workers())
}

/// Stop stream by PID file.
pub fn stop_by_pid_file(pid_path: &Path) -> Vec<u32> {
    let mut killed = Vec::new();
    if let Ok(text) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = text.trim().parse::<u32>() {
            if pid > 0 {
                kill_pid(pid);
                killed.push(pid);
            }
        }
    }
    let _ = std::fs::write(pid_path, "");
    for pid in kill_all_webtorrent() {
        if !killed.contains(&pid) {
            killed.push(pid);
        }
    }
    killed
}

/// Write PID to file.
pub fn save_pid(pid_path: &PathBuf, pid: u32) -> Result<()> {
    std::fs::write(pid_path, pid.to_string())?;
    Ok(())
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
    let allowed: HashSet<u32> = allowed_pids.iter().copied().filter(|pid| *pid > 0).collect();
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
        assert_eq!(parse_pgrep_pids("123\nnot-a-pid\n456 webtorrent\n0\n"), vec![123, 456]);
    }
}

// Minimal libc bindings for kill()
mod libc {
    extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}
