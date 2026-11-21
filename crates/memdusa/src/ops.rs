//! Operational plumbing for `memdusa serve`: rate limiting and Prometheus
//! metrics. Deliberately small and dependency-free.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- rate limiting

/// Token bucket per caller (user, or IP for anonymous access).
pub struct RateLimiter {
    per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    /// `per_sec` sustained requests with bursts up to `burst`.
    pub fn new(per_sec: f64, burst: f64) -> Self {
        Self { per_sec, burst, buckets: Mutex::new(HashMap::new()) }
    }

    /// `Err(retry_after)` when the caller is over its limit.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        if buckets.len() > 10_000 {
            // forget callers idle for 10 minutes (their bucket would be full anyway)
            buckets.retain(|_, (_, last)| now.duration_since(*last) < Duration::from_secs(600));
        }
        let (tokens, last) = buckets.entry(key.to_owned()).or_insert((self.burst, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * self.per_sec).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / self.per_sec))
        }
    }
}

// ---------------------------------------------------------------- metrics

const BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

/// Labels are route *templates* (`/v1/runs/{id}`), never ids, project names
/// or users, so metrics can't leak data and cardinality stays bounded.
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<(String, String, u16), u64>>,
    latency: Mutex<BTreeMap<String, Histogram>>,
    hooks: Mutex<BTreeMap<String, u64>>,
    pub rate_limited: AtomicU64,
    pub auth_failures: AtomicU64,
}

/// Values read from the database at scrape time.
pub struct Gauges {
    pub users: i64,
    pub runs_running: i64,
    pub runs_stale: i64,
}

impl Metrics {
    pub fn observe(&self, method: &str, route: &str, status: u16, elapsed: Duration) {
        *self.requests.lock().unwrap().entry((method.into(), route.into(), status)).or_default() += 1;
        let secs = elapsed.as_secs_f64();
        let mut lat = self.latency.lock().unwrap();
        let h = lat.entry(route.into()).or_default();
        for (i, b) in BUCKETS.iter().enumerate() {
            if secs <= *b {
                h.counts[i] += 1;
            }
        }
        h.sum += secs;
        h.count += 1;
    }

    pub fn hook(&self, event: &str) {
        *self.hooks.lock().unwrap().entry(event.into()).or_default() += 1;
    }

    pub fn render(&self, g: &Gauges) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "# HELP memdusa_build_info Build information.\n# TYPE memdusa_build_info gauge");
        let _ = writeln!(o, "memdusa_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));

        let _ = writeln!(o, "# HELP memdusa_http_requests_total HTTP requests by route and status.");
        let _ = writeln!(o, "# TYPE memdusa_http_requests_total counter");
        for ((m, r, s), n) in self.requests.lock().unwrap().iter() {
            let _ = writeln!(o, "memdusa_http_requests_total{{method=\"{m}\",route=\"{r}\",status=\"{s}\"}} {n}");
        }

        let _ = writeln!(o, "# HELP memdusa_http_request_duration_seconds Request latency by route.");
        let _ = writeln!(o, "# TYPE memdusa_http_request_duration_seconds histogram");
        for (r, h) in self.latency.lock().unwrap().iter() {
            for (i, b) in BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    o,
                    "memdusa_http_request_duration_seconds_bucket{{route=\"{r}\",le=\"{b}\"}} {}",
                    h.counts[i]
                );
            }
            let _ =
                writeln!(o, "memdusa_http_request_duration_seconds_bucket{{route=\"{r}\",le=\"+Inf\"}} {}", h.count);
            let _ = writeln!(o, "memdusa_http_request_duration_seconds_sum{{route=\"{r}\"}} {}", h.sum);
            let _ = writeln!(o, "memdusa_http_request_duration_seconds_count{{route=\"{r}\"}} {}", h.count);
        }

        let _ = writeln!(o, "# HELP memdusa_hook_events_total Hook events received, by event.");
        let _ = writeln!(o, "# TYPE memdusa_hook_events_total counter");
        for (e, n) in self.hooks.lock().unwrap().iter() {
            let _ = writeln!(o, "memdusa_hook_events_total{{event=\"{e}\"}} {n}");
        }

        for (name, help, v) in [
            (
                "memdusa_rate_limited_total",
                "Requests rejected by the rate limiter.",
                self.rate_limited.load(Ordering::Relaxed),
            ),
            (
                "memdusa_auth_failures_total",
                "Requests with missing or invalid credentials.",
                self.auth_failures.load(Ordering::Relaxed),
            ),
        ] {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
        }
        for (name, help, v) in [
            ("memdusa_users", "Configured users.", g.users),
            ("memdusa_runs_running", "Durable runs in progress.", g.runs_running),
            ("memdusa_runs_stale", "Running durable runs whose lease expired (owner gone).", g.runs_stale),
        ] {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let rl = RateLimiter::new(10.0, 3.0);
        assert!((0..3).all(|_| rl.check("alice").is_ok()), "burst allowed");
        let wait = rl.check("alice").unwrap_err();
        assert!(wait <= Duration::from_millis(100) && wait > Duration::ZERO, "{wait:?}");
        assert!(rl.check("bob").is_ok(), "per-caller buckets");
        std::thread::sleep(Duration::from_millis(120));
        assert!(rl.check("alice").is_ok(), "refills over time");
    }

    #[test]
    fn prometheus_text() {
        let m = Metrics::default();
        m.observe("GET", "/v1/runs/{id}", 200, Duration::from_millis(20));
        m.hook("post-tool-use");
        let text = m.render(&Gauges { users: 2, runs_running: 1, runs_stale: 0 });
        assert!(text.contains(r#"memdusa_http_requests_total{method="GET",route="/v1/runs/{id}",status="200"} 1"#));
        assert!(text.contains(r#"memdusa_http_request_duration_seconds_bucket{route="/v1/runs/{id}",le="0.025"} 1"#));
        assert!(text.contains(r#"memdusa_http_request_duration_seconds_bucket{route="/v1/runs/{id}",le="0.01"} 0"#));
        assert!(text.contains(r#"memdusa_hook_events_total{event="post-tool-use"} 1"#));
        assert!(text.contains("memdusa_users 2"));
    }
}
