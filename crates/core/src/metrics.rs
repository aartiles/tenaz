//! Engine-side timings from a Prometheus `/metrics` endpoint, matched to the
//! harness's per-request token counts, and the summaries built from them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The counters the runner reads, with the series names engines use for them.
pub const ENGINE_SERIES: &[(&str, &[&str])] = &[
    ("requests", &["time_to_first_token_seconds_count"]),
    ("ttft_s", &["time_to_first_token_seconds_sum"]),
    (
        "decode_s",
        &[
            "request_decode_seconds_sum",
            "request_decode_time_seconds_sum",
        ],
    ),
    ("prefill_s", &["request_prefill_time_seconds_sum"]),
    (
        "latency_s",
        &[
            "request_latency_seconds_sum",
            "e2e_request_latency_seconds_sum",
        ],
    ),
    ("prompt_tokens", &["prompt_tokens_total"]),
    (
        "cached_tokens",
        &["prompt_tokens_cached_total", "prefix_cache_tokens_total"],
    ),
    ("output_tokens", &["generation_tokens_total"]),
    (
        "drafted",
        &["mtp_drafted_total", "spec_decode_num_draft_tokens_total"],
    ),
    (
        "accepted",
        &[
            "mtp_accepted_total",
            "spec_decode_num_accepted_tokens_total",
        ],
    ),
];

/// Cumulative engine counters at one moment (only known keys).
pub type EngineCounters = BTreeMap<String, f64>;

static SAMPLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[\w]+:)?(\w+?)(?:\{[^}]*\})?\s+([\d.eE+-]+)$").expect("static regex")
});

/// Parses a Prometheus text exposition, summing labelled series by name, and
/// keeps the runner's counters. `None` when no request counter is present.
pub fn parse_prometheus(text: &str) -> Option<EngineCounters> {
    let mut values: BTreeMap<&str, f64> = BTreeMap::new();
    for line in text.lines() {
        if let Some(caps) = SAMPLE.captures(line)
            && let Ok(v) = caps[2].parse::<f64>()
        {
            let name = caps.get(1).map_or("", |m| m.as_str());
            *values.entry(name).or_insert(0.0) += v;
        }
    }
    let mut out = EngineCounters::new();
    for (key, names) in ENGINE_SERIES {
        if let Some(value) = names.iter().find_map(|n| values.get(n)) {
            out.insert((*key).to_owned(), *value);
        }
    }
    out.contains_key("requests").then_some(out)
}

/// `now - before` per counter (missing before counts as 0), rounded to microseconds.
pub fn delta(now: &EngineCounters, before: &EngineCounters) -> EngineCounters {
    now.iter()
        .map(|(k, v)| {
            (
                k.clone(),
                ((v - before.get(k).copied().unwrap_or(0.0)) * 1e6).round() / 1e6,
            )
        })
        .collect()
}

/// One model response, with the harness's counts and the engine's deltas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub session: String,
    pub sequence: u32,
    pub stop_reason: Option<String>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    /// The part of `output_tokens` spent reasoning, when the harness reports it.
    #[serde(default)]
    pub reasoning_tokens: u64,
    pub client_first_token_s: Option<f64>,
    pub client_total_s: Option<f64>,
    pub engine: Option<EngineCounters>,
    /// Exactly one engine request finished and its counts agree with the harness's.
    pub engine_matched: bool,
}

impl RequestRecord {
    /// Decides `engine_matched` from the deltas.
    #[must_use]
    pub fn with_engine(mut self, engine: Option<EngineCounters>) -> Self {
        let get = |e: &EngineCounters, k: &str| e.get(k).copied();
        #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
        let matched = engine.as_ref().is_some_and(|e| {
            get(e, "requests") == Some(1.0)
                && get(e, "prompt_tokens") == Some(self.prompt_tokens as f64)
                && get(e, "output_tokens") == Some(self.output_tokens as f64)
        });
        self.engine = engine;
        self.engine_matched = matched;
        self
    }
}

/// Medians of engine-matched requests and the output-weighted decode rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineSummary {
    pub requests: usize,
    pub engine_matched: usize,
    pub ttft_median_s: Option<f64>,
    pub decode_tps_median: Option<f64>,
    pub decode_tps_weighted: Option<f64>,
    pub ttft_s_total: f64,
    pub decode_s_total: f64,
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite());
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        return None;
    }
    let m = if n % 2 == 1 {
        values[n / 2]
    } else {
        f64::midpoint(values[n / 2 - 1], values[n / 2])
    };
    Some((m * 1000.0).round() / 1000.0)
}

/// `None` without requests; medians are `None` without matched requests.
pub fn engine_summary(requests: &[RequestRecord]) -> Option<EngineSummary> {
    if requests.is_empty() {
        return None;
    }
    let matched: Vec<&EngineCounters> = requests
        .iter()
        .filter(|r| r.engine_matched)
        .filter_map(|r| r.engine.as_ref())
        .collect();
    let get = |e: &EngineCounters, k: &str| e.get(k).copied().unwrap_or(0.0);
    let decode_s: f64 = matched.iter().map(|e| get(e, "decode_s")).sum();
    let output: f64 = matched.iter().map(|e| get(e, "output_tokens")).sum();
    Some(EngineSummary {
        requests: requests.len(),
        engine_matched: matched.len(),
        ttft_median_s: median(matched.iter().map(|e| get(e, "ttft_s")).collect()),
        decode_tps_median: median(
            matched
                .iter()
                .filter(|e| get(e, "decode_s") > 0.0)
                .map(|e| get(e, "output_tokens") / get(e, "decode_s"))
                .collect(),
        ),
        decode_tps_weighted: (decode_s > 0.0).then(|| (output / decode_s * 100.0).round() / 100.0),
        ttft_s_total: (matched.iter().map(|e| get(e, "ttft_s")).sum::<f64>() * 1000.0).round()
            / 1000.0,
        decode_s_total: (decode_s * 1000.0).round() / 1000.0,
    })
}

/// The `Engine:` line of `status`.
pub fn engine_line(requests: &[RequestRecord]) -> String {
    let Some(e) = engine_summary(requests) else {
        return "Engine: no requests recorded".to_owned();
    };
    let prompt: u64 = requests.iter().map(|r| r.prompt_tokens).sum();
    let cached: u64 = requests.iter().map(|r| r.cached_tokens).sum();
    #[allow(clippy::cast_precision_loss)]
    let share = cached as f64 / prompt.max(1) as f64 * 100.0;
    let mut line = format!(
        "Engine: {} requests ({} with engine timings), prompt cache {share:.1}%",
        e.requests, e.engine_matched
    );
    if e.engine_matched > 0 {
        let _ = write!(
            line,
            ", TTFT median {} s, decode {} tok/s (output-weighted)",
            fmt(e.ttft_median_s),
            fmt(e.decode_tps_weighted)
        );
    }
    line
}

fn fmt(v: Option<f64>) -> String {
    v.map_or_else(|| "null".to_owned(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: &str = "# HELP x\ntime_to_first_token_seconds_count 3\ntime_to_first_token_seconds_sum 1.5\nrequest_decode_seconds_sum{model=\"a\"} 2.0\nrequest_decode_seconds_sum{model=\"b\"} 1.0\nprompt_tokens_total 1000\ngeneration_tokens_total 90\nvllm:prefix_cache_tokens_total 400\nunrelated 7\n";

    #[test]
    #[allow(clippy::float_cmp)]
    fn parses_known_series_and_sums_labels() {
        let c = parse_prometheus(METRICS).unwrap();
        assert_eq!(c["requests"], 3.0);
        assert_eq!(c["decode_s"], 3.0);
        assert_eq!(c["cached_tokens"], 400.0);
        assert!(!c.contains_key("drafted"));
        assert_eq!(parse_prometheus("prompt_tokens_total 5\n"), None);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn deltas_and_matching() {
        let before = parse_prometheus(METRICS).unwrap();
        let now = parse_prometheus(
            &METRICS
                .replace("_count 3", "_count 4")
                .replace("prompt_tokens_total 1000", "prompt_tokens_total 1250")
                .replace("generation_tokens_total 90", "generation_tokens_total 120")
                .replace("_sum 1.5", "_sum 1.9"),
        )
        .unwrap();
        let d = delta(&now, &before);
        assert_eq!(d["requests"], 1.0);
        assert_eq!(d["prompt_tokens"], 250.0);
        assert_eq!(d["ttft_s"], 0.4);
        let record = RequestRecord {
            session: "s".into(),
            sequence: 1,
            stop_reason: Some("stop".into()),
            prompt_tokens: 250,
            cached_tokens: 0,
            output_tokens: 30,
            reasoning_tokens: 0,
            client_first_token_s: None,
            client_total_s: None,
            engine: None,
            engine_matched: false,
        }
        .with_engine(Some(d.clone()));
        assert!(record.engine_matched);
        let other = RequestRecord {
            output_tokens: 31,
            ..record.clone()
        }
        .with_engine(Some(d));
        assert!(!other.engine_matched);
    }

    #[test]
    fn summary_uses_only_matched_requests() {
        let mk = |ttft: f64, decode: f64, out: f64, matched: bool| RequestRecord {
            session: "s".into(),
            sequence: 1,
            stop_reason: None,
            prompt_tokens: 1,
            cached_tokens: 1,
            output_tokens: 1,
            reasoning_tokens: 0,
            client_first_token_s: None,
            client_total_s: None,
            engine: Some(BTreeMap::from([
                ("ttft_s".to_owned(), ttft),
                ("decode_s".to_owned(), decode),
                ("output_tokens".to_owned(), out),
            ])),
            engine_matched: matched,
        };
        let requests = vec![
            mk(0.5, 1.0, 50.0, true),
            mk(1.5, 2.0, 50.0, true),
            mk(9.0, 9.0, 9.0, false),
            mk(1.0, 3.0, 30.0, true),
        ];
        let s = engine_summary(&requests).unwrap();
        assert_eq!((s.requests, s.engine_matched), (4, 3));
        assert_eq!(s.ttft_median_s, Some(1.0));
        assert_eq!(s.decode_tps_median, Some(25.0));
        assert_eq!(s.decode_tps_weighted, Some(21.67));
        assert_eq!(s.ttft_s_total, 3.0);
        assert_eq!(engine_summary(&[]), None);
        assert_eq!(engine_line(&[]), "Engine: no requests recorded");
        assert!(engine_line(&requests).starts_with("Engine: 4 requests (3 with engine timings), prompt cache 100.0%, TTFT median 1 s, decode 21.67 tok/s"));
        let unmatched = vec![mk(1.0, 1.0, 1.0, false)];
        assert_eq!(engine_summary(&unmatched).unwrap().ttft_median_s, None);
    }
}
