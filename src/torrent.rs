use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::time::{sleep, Duration};

/// Start webtorrent-cli as an HTTP file server.
/// Returns (PID, HTTP URL for the served file).
pub async fn start_webtorrent(
    magnet: &str,
    file_index: Option<u32>,
    media_dir: &Path,
    lan_ip: &str,
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
    ];
    if let Some(idx) = file_index {
        args.push("-s".to_string());
        args.push(idx.to_string());
    }

    let child = Command::new("webtorrent")
        .args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .spawn()?;

    let pid = child.id().ok_or_else(|| anyhow!("Failed to get webtorrent PID"))?;

    // Wait for HTTP server to be ready (parse log for URL)
    let mut server_url = None;
    for _ in 0..30 {
        sleep(Duration::from_secs(1)).await;
        if let Ok(log) = std::fs::read_to_string(log_path) {
            if let Some(cap) = log.lines().find(|l| l.contains("Server running at:")) {
                if let Some(url_start) = cap.find("http://") {
                    let url = cap[url_start..].trim().to_string();
                    server_url = Some(url.replace("localhost", lan_ip));
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
        libc::kill(pid as i32, 15); // SIGTERM
    }
}

/// Check if a process is running (signal 0).
pub unsafe fn kill_check(pid: u32) -> bool {
    libc::kill(pid as i32, 0) == 0
}

/// Kill all webtorrent processes.
pub fn kill_all_webtorrent() {
    let _ = std::process::Command::new("pkill")
        .args(["-f", "webtorrent"])
        .output();
}

/// Stop stream by PID file.
pub fn stop_by_pid_file(pid_path: &Path) {
    if let Ok(text) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = text.trim().parse::<u32>() {
            if pid > 0 {
                kill_pid(pid);
            }
        }
    }
    let _ = std::fs::write(pid_path, "");
    kill_all_webtorrent();
}

/// Write PID to file.
pub fn save_pid(pid_path: &PathBuf, pid: u32) -> Result<()> {
    std::fs::write(pid_path, pid.to_string())?;
    Ok(())
}

// Minimal libc bindings for kill()
mod libc {
    extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}
