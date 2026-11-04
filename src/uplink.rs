//! Uplink governor (2026-11-04): seeding yields to everything else on the connection.
//!
//! The owner's ruling, for a home on a 5G connection with limited upload: torrents may
//! seed so the swarm is not leeched from, *if there is no higher-priority traffic on the
//! limited upload capacity*. The server cannot see the other devices on the LAN, but it
//! can see what they do to the line: when anything fills the upload, packets queue in the
//! router and the round trip to the outside grows. That added delay is the signal, the
//! same one LEDBAT (the congestion control behind BitTorrent's uTP, RFC 6817) uses to
//! make background transfers step aside.
//!
//! Every few seconds the governor times a TCP connect to two public anycast hosts. The
//! lowest time seen in the last ten minutes is the line's base delay; the excess over it
//! is queueing. Above `HIGH_DELAY` the session's upload ceiling halves, down to a floor;
//! below `LOW_DELAY` it grows by a tenth, up to the fixed maximum. So seeding runs at full
//! speed on an idle line and shrinks to a trickle within seconds of a call, a backup or a
//! stream needing the upload, which is also what keeps its own download's acknowledgements
//! moving.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One 16 KiB chunk per second, doubled. librqbit waits for the whole chunk's quota at
/// once, so a ceiling below a chunk would stall every upload rather than slow it.
pub const FLOOR_BYTES_PER_SEC: u32 = 32 * 1024;
const PROBE_EVERY: Duration = Duration::from_secs(5);
const BASE_WINDOW: Duration = Duration::from_secs(10 * 60);
const HIGH_DELAY: Duration = Duration::from_millis(100);
const LOW_DELAY: Duration = Duration::from_millis(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_HOSTS: &[&str] = &["1.1.1.1:443", "9.9.9.9:443"];

/// The control law, separate from the probing so it can be tested without a network.
#[derive(Debug)]
pub struct Governor {
    pub limit: u32,
    pub max: u32,
    samples: VecDeque<(Instant, Duration)>,
}

impl Governor {
    pub fn new(max: u32) -> Self {
        Self {
            limit: max,
            max,
            samples: VecDeque::new(),
        }
    }

    /// The lowest round trip seen inside the window.
    pub fn base(&self) -> Option<Duration> {
        self.samples.iter().map(|(_, d)| *d).min()
    }

    /// Take one round-trip sample; answer the new ceiling if it changed.
    pub fn observe(&mut self, now: Instant, rtt: Duration) -> Option<u32> {
        self.samples.push_back((now, rtt));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > BASE_WINDOW)
        {
            self.samples.pop_front();
        }
        let base = self.base()?;
        let queue = rtt.saturating_sub(base);
        let before = self.limit;
        if queue > HIGH_DELAY {
            self.limit = (self.limit / 2).max(FLOOR_BYTES_PER_SEC);
        } else if queue < LOW_DELAY {
            let step = (self.limit / 10).max(64 * 1024);
            self.limit = self.limit.saturating_add(step).min(self.max);
        }
        (self.limit != before).then_some(self.limit)
    }
}

async fn probe_once() -> Option<Duration> {
    let mut best: Option<Duration> = None;
    for host in PROBE_HOSTS {
        let t = Instant::now();
        if let Ok(Ok(_s)) =
            tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(host)).await
        {
            let d = t.elapsed();
            best = Some(best.map_or(d, |b| b.min(d)));
        }
    }
    best
}

/// Run the governor for the life of the process. Permanent by design: it is the
/// server's upload policy, it is switched off with `uplink_governor = false` in
/// config.toml, and with it off the fixed maximum applies as before.
pub fn spawn(engine: Arc<crate::torrent_engine::TorrentEngine>) {
    tokio::spawn(async move {
        let mut g = Governor::new(crate::torrent_engine::MAX_UPLOAD_BYTES_PER_SEC);
        let mut tick = tokio::time::interval(PROBE_EVERY);
        let mut last_log = Instant::now() - Duration::from_secs(3600);
        loop {
            tick.tick().await;
            let Some(rtt) = probe_once().await else {
                continue;
            };
            if let Some(limit) = g.observe(Instant::now(), rtt) {
                engine.set_upload_limit(limit);
                // A log line on every step would be one every five seconds under load.
                if limit == FLOOR_BYTES_PER_SEC
                    || limit == g.max
                    || last_log.elapsed() > Duration::from_secs(300)
                {
                    tracing::info!(
                        "uplink: upload ceiling {} KB/s (round trip {} ms, base {} ms)",
                        limit / 1000,
                        rtt.as_millis(),
                        g.base().map(|b| b.as_millis()).unwrap_or(0)
                    );
                    last_log = Instant::now();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_queueing_line_halves_the_ceiling_down_to_the_floor() {
        let mut g = Governor::new(12_500_000);
        let t = Instant::now();
        g.observe(t, ms(30));
        let mut last = g.limit;
        for i in 1..40 {
            g.observe(t + Duration::from_secs(5 * i), ms(400));
            assert!(g.limit <= last);
            last = g.limit;
        }
        assert_eq!(g.limit, FLOOR_BYTES_PER_SEC);
    }

    #[test]
    fn an_idle_line_grows_it_back_to_the_maximum_and_no_further() {
        let mut g = Governor::new(12_500_000);
        g.limit = FLOOR_BYTES_PER_SEC;
        let t = Instant::now();
        for i in 0..400 {
            g.observe(t + Duration::from_secs(5 * i), ms(30));
        }
        assert_eq!(g.limit, 12_500_000);
    }

    #[test]
    fn delay_between_the_thresholds_holds_the_ceiling() {
        let mut g = Governor::new(12_500_000);
        let t = Instant::now();
        g.observe(t, ms(30));
        g.limit = 1_000_000;
        assert_eq!(g.observe(t + Duration::from_secs(5), ms(30 + 60)), None);
        assert_eq!(g.limit, 1_000_000);
    }

    #[test]
    fn the_base_forgets_samples_older_than_the_window() {
        let mut g = Governor::new(12_500_000);
        let t = Instant::now();
        g.observe(t, ms(10));
        g.observe(t + BASE_WINDOW + Duration::from_secs(1), ms(50));
        assert_eq!(g.base(), Some(ms(50)));
    }
}
