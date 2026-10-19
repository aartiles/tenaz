//! Memory samples (macOS pressure level, swap, compressor, wired, free, and
//! the engine server's footprint) and the per-session window built from them.

use serde::{Deserialize, Serialize};

/// One sample, as written to `.tenaz/private/memory.jsonl`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemorySample {
    pub at: String,
    pub session: Option<String>,
    /// 1 normal, 2 warning, 4 critical.
    pub pressure_level: Option<u32>,
    pub swap_used_gib: Option<f64>,
    pub swapins: Option<u64>,
    pub swapouts: Option<u64>,
    pub compressor_gib: Option<f64>,
    pub free_gib: Option<f64>,
    pub wired_gib: Option<f64>,
    pub server_pid: Option<u32>,
    pub server_mem_gib: Option<f64>,
    pub swapins_delta: u64,
    pub swapouts_delta: u64,
    pub page_bytes: u64,
}

/// The raw figures a probe reads; the core turns them into a sample.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryReading {
    pub pressure_level: Option<u32>,
    pub swap_used_gib: Option<f64>,
    pub swapins: Option<u64>,
    pub swapouts: Option<u64>,
    pub compressor_pages: Option<u64>,
    pub free_pages: Option<u64>,
    pub wired_pages: Option<u64>,
    pub page_bytes: u64,
    pub server_pid: Option<u32>,
    pub server_mem_gib: Option<f64>,
}

impl MemoryReading {
    /// A sample with the deltas against the previous one.
    pub fn into_sample(
        self,
        at: String,
        session: Option<String>,
        previous: Option<&MemorySample>,
    ) -> MemorySample {
        #[allow(clippy::cast_precision_loss)]
        let gib = |pages: Option<u64>| {
            pages.map(|p| round2(p as f64 * self.page_bytes as f64 / f64::from(1u32 << 30)))
        };
        let delta = |now: Option<u64>, prev: Option<u64>| match (now, prev) {
            (Some(n), Some(p)) => n.saturating_sub(p),
            _ => 0,
        };
        MemorySample {
            at,
            session,
            pressure_level: self.pressure_level,
            swap_used_gib: self.swap_used_gib,
            swapins: self.swapins,
            swapouts: self.swapouts,
            compressor_gib: gib(self.compressor_pages),
            free_gib: gib(self.free_pages),
            wired_gib: gib(self.wired_pages),
            server_pid: self.server_pid,
            server_mem_gib: self.server_mem_gib,
            swapins_delta: delta(self.swapins, previous.and_then(|p| p.swapins)),
            swapouts_delta: delta(self.swapouts, previous.and_then(|p| p.swapouts)),
            page_bytes: self.page_bytes,
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// A session's memory summary for the journal.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryWindow {
    pub samples: u32,
    pub pressure_max: u32,
    pub swapins: u64,
    pub swapouts: u64,
    pub server_mem_max_gib: f64,
    pub swap_used_max_gib: f64,
    pub swap_in_gib: f64,
    pub swap_out_gib: f64,
}

impl MemoryWindow {
    pub fn add(&mut self, sample: &MemorySample) {
        self.samples += 1;
        self.pressure_max = self.pressure_max.max(sample.pressure_level.unwrap_or(0));
        self.swapins += sample.swapins_delta;
        self.swapouts += sample.swapouts_delta;
        self.server_mem_max_gib = self
            .server_mem_max_gib
            .max(sample.server_mem_gib.unwrap_or(0.0));
        self.swap_used_max_gib = self
            .swap_used_max_gib
            .max(sample.swap_used_gib.unwrap_or(0.0));
        #[allow(clippy::cast_precision_loss)]
        let gib =
            |pages: u64| round2(pages as f64 * sample.page_bytes as f64 / f64::from(1u32 << 30));
        self.swap_in_gib = gib(self.swapins);
        self.swap_out_gib = gib(self.swapouts);
    }
}

/// Parses `vm_stat` output: the page size and the named counters.
pub fn parse_vm_stat(text: &str) -> (u64, impl Fn(&str) -> Option<u64> + '_) {
    let page = text
        .lines()
        .find_map(|l| l.split("page size of ").nth(1))
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse().ok())
        .unwrap_or(16384);
    (page, move |name: &str| {
        text.lines().find_map(|l| {
            let (key, value) = l.split_once(':')?;
            (key.trim() == name).then(|| value.trim().trim_end_matches('.').parse().ok())?
        })
    })
}

/// Parses `sysctl -n vm.swapusage`'s `used = 1234.50M` figure in GiB.
pub fn parse_swap_used_gib(text: &str) -> Option<f64> {
    let rest = text.split("used = ").nth(1)?;
    parse_size_gib(rest.split_whitespace().next()?)
}

/// `1234.50M`, `2.5G`, `512K` (also with a decimal comma) in GiB.
pub fn parse_size_gib(text: &str) -> Option<f64> {
    let text = text.replace(',', ".");
    let digits: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let number: f64 = digits.parse().ok()?;
    let unit = text[digits.len()..].chars().next().unwrap_or('G');
    let factor = match unit {
        'K' => 2f64.powi(-20),
        'M' => 2f64.powi(-10),
        'T' => 1024.0,
        _ => 1.0,
    };
    Some(round2(number * factor))
}

/// The `Memory:` line of `status`.
pub fn memory_line(samples: &[MemorySample]) -> String {
    let Some(last) = samples.last() else {
        return "Memory: not sampled".to_owned();
    };
    let level = |n: Option<u32>| match n {
        Some(1) => "normal".to_owned(),
        Some(2) => "warning".to_owned(),
        Some(4) => "critical".to_owned(),
        Some(other) => other.to_string(),
        None => "null".to_owned(),
    };
    let worst = samples.iter().filter_map(|s| s.pressure_level).max();
    #[allow(clippy::cast_precision_loss)]
    let swap_in: f64 = samples.iter().map(|s| s.swapins_delta).sum::<u64>() as f64
        * last.page_bytes as f64
        / f64::from(1u32 << 30);
    let free_min = samples
        .iter()
        .filter_map(|s| s.free_gib)
        .fold(f64::INFINITY, f64::min);
    let opt = |v: Option<f64>| v.map_or_else(|| "?".to_owned(), |v| v.to_string());
    format!(
        "Memory: pressure {} now, worst {}; swapped in {swap_in:.1} GiB during the run; server {} GiB, wired {} GiB, free {} GiB now (min {}); {} samples",
        level(last.pressure_level),
        level(worst),
        opt(last.server_mem_gib),
        opt(last.wired_gib),
        opt(last.free_gib),
        if free_min.is_finite() {
            free_min.to_string()
        } else {
            "?".to_owned()
        },
        samples.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               12345.\nPages wired down:                        200000.\nSwapins:                                   1000.\nSwapouts:                                  2000.\nPages occupied by compressor:            300000.\n";

    #[test]
    fn parses_vm_stat_and_swapusage() {
        let (page, pages) = parse_vm_stat(VM_STAT);
        assert_eq!(page, 16384);
        assert_eq!(pages("Pages free"), Some(12345));
        assert_eq!(pages("Swapins"), Some(1000));
        assert_eq!(pages("Nope"), None);
        assert_eq!(
            parse_swap_used_gib("total = 4096.00M  used = 1536.50M  free = 2559.50M  (encrypted)"),
            Some(1.5)
        );
        assert_eq!(parse_size_gib("2,5G"), Some(2.5));
        assert_eq!(parse_size_gib("512K"), Some(0.0));
        assert_eq!(parse_size_gib("x"), None);
    }

    #[test]
    fn samples_carry_deltas_and_windows_accumulate() {
        let reading = |swapins, server| MemoryReading {
            pressure_level: Some(2),
            swap_used_gib: Some(1.0),
            swapins: Some(swapins),
            swapouts: Some(0),
            compressor_pages: Some(65536),
            free_pages: Some(65536),
            wired_pages: Some(131072),
            page_bytes: 16384,
            server_pid: Some(42),
            server_mem_gib: Some(server),
        };
        let first = reading(1000, 40.0).into_sample("t1".into(), None, None);
        assert_eq!(first.swapins_delta, 0);
        assert_eq!(first.compressor_gib, Some(1.0));
        assert_eq!(first.wired_gib, Some(2.0));
        let second = reading(1500, 44.5).into_sample(
            "t2".into(),
            Some("001-bootstrap".into()),
            Some(&first),
        );
        assert_eq!(second.swapins_delta, 500);
        let mut window = MemoryWindow::default();
        window.add(&first);
        window.add(&second);
        assert_eq!(window.samples, 2);
        assert_eq!(window.pressure_max, 2);
        assert_eq!(window.swapins, 500);
        assert_eq!(window.server_mem_max_gib, 44.5);
        assert!((window.swap_in_gib - 0.01).abs() < 1e-9);
        let line = memory_line(&[first, second]);
        assert!(line.starts_with("Memory: pressure warning now, worst warning; swapped in 0.0 GiB during the run; server 44.5 GiB, wired 2 GiB, free 1 GiB now (min 1); 2 samples"), "{line}");
        assert_eq!(memory_line(&[]), "Memory: not sampled");
    }
}
