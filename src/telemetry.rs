//! Non-blocking Layer 4 telemetry. Hot-path producers only perform `try_send`.

use crate::{
    domain::{Fixed, Side, Symbol},
    risk::BreakerLevel,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::{
    net::UdpSocket,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy)]
pub enum TelemetryEvent {
    Signal {
        symbol: Symbol,
        side: Side,
        obi: f64,
        vpin: f64,
    },
    Execution {
        symbol: Symbol,
        latency_us: u64,
        slippage: Fixed,
    },
    Risk {
        equity: Fixed,
        exposure: Fixed,
        level: BreakerLevel,
    },
}

#[derive(Clone)]
pub struct Telemetry {
    sender: Sender<TelemetryEvent>,
    dropped: Arc<AtomicU64>,
}

impl Telemetry {
    /// Starts a dedicated UDP/StatsD worker. A local exporter can expose these
    /// metrics to Prometheus without any scrape work in the trading process.
    #[must_use]
    pub fn start(udp_target: &str, capacity: usize) -> Self {
        let (sender, receiver) = bounded(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let target = udp_target.to_owned();
        thread::Builder::new()
            .name("aegis-telemetry".to_owned())
            .spawn(move || telemetry_worker(&target, receiver, worker_dropped))
            .expect("telemetry worker must start");
        Self { sender, dropped }
    }

    pub fn emit(&self, event: TelemetryEvent) {
        if matches!(self.sender.try_send(event), Err(TrySendError::Full(_))) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn telemetry_worker(target: &str, receiver: Receiver<TelemetryEvent>, dropped: Arc<AtomicU64>) {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return;
    };
    let mut last_sample = Instant::now() - Duration::from_millis(100);
    loop {
        let Ok(event) = receiver.recv_timeout(Duration::from_secs(1)) else {
            continue;
        };
        // Feature and portfolio gauges are downsampled to 10 Hz; execution
        // events pass through.
        let sampled = matches!(
            event,
            TelemetryEvent::Signal { .. } | TelemetryEvent::Risk { .. }
        );
        if sampled && last_sample.elapsed() < Duration::from_millis(100) {
            continue;
        }
        if sampled {
            last_sample = Instant::now();
        }
        for metric in encode_statsd(event) {
            let _ = socket.send_to(metric.as_bytes(), target);
        }
        let dropped_count = dropped.swap(0, Ordering::Relaxed);
        if dropped_count > 0 {
            let _ = socket.send_to(
                format!("aegis.telemetry.dropped:{dropped_count}|c").as_bytes(),
                target,
            );
        }
    }
}

fn encode_statsd(event: TelemetryEvent) -> Vec<String> {
    match event {
        TelemetryEvent::Signal {
            symbol,
            side,
            obi,
            vpin,
        } => vec![
            format!("aegis.obi:{obi}|g|#symbol:{symbol},side:{side:?}"),
            format!("aegis.vpin:{vpin}|g|#symbol:{symbol}"),
        ],
        TelemetryEvent::Execution {
            symbol,
            latency_us,
            slippage,
        } => vec![
            format!("aegis.execution.latency_us:{latency_us}|g|#symbol:{symbol}"),
            format!(
                "aegis.execution.slippage:{}|g|#symbol:{symbol}",
                slippage.as_f64()
            ),
        ],
        TelemetryEvent::Risk {
            equity,
            exposure,
            level,
        } => vec![
            format!("aegis.equity:{}|g", equity.as_f64()),
            format!("aegis.exposure:{}|g", exposure.as_f64()),
            format!("aegis.breaker.level:{}|g", level as u8),
        ],
    }
}

/// Initializes structured, non-blocking process logs. Keep the returned guard
/// alive for the process lifetime so the background writer can flush.
pub fn init_logging(json: bool) -> WorkerGuard {
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .init();
    }
    guard
}
