//! Group summaries for `compare`.

use serde_json::{json, Value};

/// Nearest-rank percentile of an unsorted sample. None when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

fn num(row: &Value, field: &str) -> f64 {
    row[field].as_f64().unwrap_or(0.0)
}

pub fn summarise(by: &str, key: &str, rows: &[&Value]) -> Value {
    let n = rows.len();
    let ok: Vec<&&Value> = rows.iter().filter(|r| r["failed"] != true).collect();
    let latency: Vec<f64> = ok.iter().map(|r| num(r, "latency_ms")).collect();
    // TTFT is only reported for streamed requests.
    let ttft: Vec<f64> = ok.iter().map(|r| num(r, "ttft_ms")).filter(|v| *v > 0.0).collect();
    let sum = |field: &str| ok.iter().map(|r| num(r, field)).sum::<f64>();
    let prompt = sum("input_tokens") + sum("cache_read_tokens") + sum("cache_write_tokens");
    let output = sum("output_tokens");
    let seconds = latency.iter().sum::<f64>() / 1000.0;
    let share = |a: f64, b: f64| if b > 0.0 { (a / b * 10_000.0).round() / 10_000.0 } else { 0.0 };
    json!({
        by: key,
        "n": n,
        "failure_rate": share((n - ok.len()) as f64, n as f64),
        "latency_ms_p50": percentile(&latency, 50.0),
        "latency_ms_p90": percentile(&latency, 90.0),
        "ttft_ms_p50": percentile(&ttft, 50.0),
        "ttft_ms_p90": percentile(&ttft, 90.0),
        "ttft_samples": ttft.len(),
        "output_tokens_mean": if ok.is_empty() { 0.0 } else { (output / ok.len() as f64).round() },
        "output_tokens_per_second": if seconds > 0.0 { (output / seconds * 10.0).round() / 10.0 } else { 0.0 },
        "cache_read_share": share(sum("cache_read_tokens"), prompt),
        "cache_misses": rows.iter().filter(|r| r["cache_miss"] == true).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 50.0), Some(3.0));
        assert_eq!(percentile(&v, 90.0), Some(5.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn failures_are_counted_but_excluded_from_timing() {
        let a = json!({"failed": false, "latency_ms": 100, "ttft_ms": 20, "output_tokens": 50, "input_tokens": 10, "cache_read_tokens": 90});
        let b = json!({"failed": true, "latency_ms": 9000});
        let s = summarise("executor", "ws", &[&a, &b]);
        assert_eq!(s["executor"], "ws");
        assert_eq!(s["n"], 2);
        assert_eq!(s["failure_rate"], 0.5);
        assert_eq!(s["latency_ms_p50"], 100.0);
        assert_eq!(s["ttft_samples"], 1);
        assert_eq!(s["cache_read_share"], 0.9);
        assert_eq!(s["output_tokens_per_second"], 500.0);
    }
}
