//! Memory sampling on macOS: the kernel's pressure level, swap, `vm_stat`
//! counters, and the footprint of the process listening on the engine's port.

use async_trait::async_trait;
use tenaz_core::memory::{MemoryReading, parse_size_gib, parse_swap_used_gib, parse_vm_stat};
use tenaz_core::ports::MemoryProbe;

/// Reads `sysctl`, `vm_stat`, `lsof` and `top`.
#[derive(Clone, Debug)]
pub struct MacMemoryProbe {
    engine_port: Option<u16>,
}

impl MacMemoryProbe {
    pub fn new(engine_port: Option<u16>) -> Self {
        Self { engine_port }
    }
}

async fn run(program: &str, args: &[&str]) -> String {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(program).args(args).output(),
    )
    .await;
    match output {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

/// The server's footprint from one `top` line: `PID MEM`, with MEM like `47G`.
pub fn parse_top_mem(text: &str, pid: &str) -> Option<f64> {
    let last = text.trim().lines().last()?;
    let mut fields = last.split_whitespace();
    (fields.next()? == pid).then(|| parse_size_gib(fields.next()?))?
}

#[async_trait]
impl MemoryProbe for MacMemoryProbe {
    async fn read(&self) -> Option<MemoryReading> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let port = self.engine_port.map(|p| format!("-iTCP:{p}"));
        let (pressure, swap, vm, pid) = tokio::join!(
            run("sysctl", &["-n", "kern.memorystatus_vm_pressure_level"]),
            run("sysctl", &["-n", "vm.swapusage"]),
            run("vm_stat", &[]),
            async {
                match &port {
                    Some(port) => run("lsof", &["-nP", port, "-sTCP:LISTEN", "-t"]).await,
                    None => String::new(),
                }
            }
        );
        let (page, pages) = parse_vm_stat(&vm);
        let server = pid
            .trim()
            .lines()
            .next()
            .map(str::to_owned)
            .filter(|s| !s.is_empty());
        // top's CMPRS column is not used: for a Metal process it reports GPU memory, not
        // compressed pages (it showed 46 GiB while the whole compressor held under 9).
        let server_mem_gib = match &server {
            Some(pid) => parse_top_mem(
                &run("top", &["-l", "1", "-pid", pid, "-stats", "pid,mem"]).await,
                pid,
            ),
            None => None,
        };
        Some(MemoryReading {
            pressure_level: pressure.trim().parse().ok(),
            swap_used_gib: parse_swap_used_gib(&swap),
            swapins: pages("Swapins"),
            swapouts: pages("Swapouts"),
            compressor_pages: pages("Pages occupied by compressor"),
            free_pages: pages("Pages free"),
            wired_pages: pages("Pages wired down"),
            page_bytes: page,
            server_pid: server.as_deref().and_then(|s| s.parse().ok()),
            server_mem_gib,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_top_line() {
        let text = "Processes: 600 total\n\nPID    MEM\n31139  47G\n";
        assert_eq!(parse_top_mem(text, "31139"), Some(47.0));
        assert_eq!(parse_top_mem(text, "1"), None);
        assert_eq!(parse_top_mem("", "1"), None);
    }

    #[tokio::test]
    #[cfg_attr(not(target_os = "macos"), ignore)]
    async fn reads_a_sample_on_macos() {
        let reading = MacMemoryProbe::new(None).read().await.unwrap();
        assert!(reading.page_bytes >= 4096);
        assert!(reading.free_pages.is_some());
    }
}
