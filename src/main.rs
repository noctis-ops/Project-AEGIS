use project_aegis::{
    alpha::{AlphaConfig, AlphaEngine, CapitalSizer},
    c2::{C2Command, SharedStatus, StatusProvider, SystemStatus, TelegramC2},
    data::{
        BinanceMarketDataClient, BookError, LatestEventBus, LobSynchronizer, SyncOutcome,
        SyncState, TransportConfig,
    },
    domain::{unix_time_ns, BookSnapshot, Fixed, MarketEvent, Symbol},
    execution::{
        BinanceLiveExecution, BinanceUserStream, ExchangeFilters, ExecutionMode, ExecutionVenue,
        LiveCredentials, OrderManager, OrderManagerConfig, TokenBucket, UserDataEvent,
    },
    risk::{BreakerLevel, FundingGuard, RiskConfig, RiskManager, TradeOutcome},
    simulation::{
        BacktestEngine, DataFeed, HistoricalDataLake, HistoricalRecord, ShadowConfig,
        ShadowExecutionEngine,
    },
    telemetry::{init_logging, Telemetry, TelemetryEvent},
};
use std::{env, str::FromStr, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::sleep};
use tracing::{error, info, warn};

fn main() -> anyhow::Result<()> {
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get());
    let core_ids = Arc::new(core_affinity::get_core_ids().unwrap_or_default());
    let next_core = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_cores = Arc::clone(&core_ids);
    let worker_index = Arc::clone(&next_core);
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime
        .worker_threads(workers)
        .on_thread_start(move || {
            if !worker_cores.is_empty() {
                let index = worker_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % worker_cores.len();
                let _ = core_affinity::set_for_current(worker_cores[index]);
            }
        })
        .enable_all();
    runtime.build()?.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let json_logs = matches!(env::var("AEGIS_JSON_LOGS"), Ok(value) if value == "1");
    let _log_guard = init_logging(json_logs);
    let arguments: Vec<String> = env::args().collect();
    match arguments.get(1).map(String::as_str) {
        Some("shadow") => {
            let symbol = Symbol::new(arguments.get(2).map_or("BTCUSDT", String::as_str))?;
            let equity = env_fixed("AEGIS_SHADOW_EQUITY", "1000")?;
            let shadow = Arc::new(ShadowExecutionEngine::new(ShadowConfig {
                starting_equity: equity,
                ..ShadowConfig::default()
            }));
            let venue: Arc<dyn ExecutionVenue> = shadow.clone();
            run_market_engine(symbol, venue, Some(shadow), None, equity).await
        }
        Some("live") => {
            let symbol = Symbol::new(arguments.get(2).map_or("BTCUSDT", String::as_str))?;
            let acknowledgement = env::var("AEGIS_LIVE_TRADING").unwrap_or_default();
            let credentials = LiveCredentials::from_env()?;
            let live = Arc::new(BinanceLiveExecution::new(credentials, &acknowledgement)?);
            let api_key = env::var("BINANCE_API_KEY")?;
            let user_stream = BinanceUserStream::new(api_key)?;
            let (user_tx, user_rx) = mpsc::channel(4_096);
            tokio::spawn(async move { user_stream.run(user_tx).await });
            let venue: Arc<dyn ExecutionVenue> = live;
            let equity = env_fixed("AEGIS_LIVE_EQUITY_BOOTSTRAP", "100")?;
            run_market_engine(symbol, venue, None, Some(user_rx), equity).await
        }
        Some("backtest") => {
            let path = arguments.get(2).ok_or_else(|| {
                anyhow::anyhow!("usage: project-aegis backtest DATA.parquet [EQUITY]")
            })?;
            let equity = arguments
                .get(3)
                .map_or_else(|| Ok(Fixed::from_f64(1_000.0)), |raw| Fixed::from_str(raw))?;
            run_backtest(path, equity).await
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => anyhow::bail!("unknown command: {command}"),
    }
}

fn print_help() {
    println!(
        "Project AEGIS\n\n  project-aegis shadow [SYMBOL]\n  project-aegis backtest DATA.parquet [EQUITY]\n  project-aegis live [SYMBOL]  # requires --features live-trading and runtime acknowledgement\n\nShadow is the safe default. See README.md and docs/OPERATIONS.ar.md."
    );
}

async fn run_market_engine(
    symbol: Symbol,
    venue: Arc<dyn ExecutionVenue>,
    shadow: Option<Arc<ShadowExecutionEngine>>,
    mut user_data: Option<mpsc::Receiver<UserDataEvent>>,
    bootstrap_equity: Fixed,
) -> anyhow::Result<()> {
    let mode = venue.mode();
    let market_client = BinanceMarketDataClient::new(TransportConfig::default())?;
    let market_bus = LatestEventBus::new(10_000);
    let transport_core = core_affinity::get_core_ids().and_then(|cores| cores.last().copied());
    let _transport_thread =
        market_client
            .clone()
            .spawn_dedicated(vec![symbol], market_bus.clone(), transport_core)?;

    let (open_interest_tx, mut open_interest_rx) = mpsc::channel(8);
    let open_interest_client = market_client.clone();
    tokio::spawn(async move {
        loop {
            match open_interest_client.open_interest(symbol).await {
                Ok(value) => {
                    if open_interest_tx.send(value).await.is_err() {
                        break;
                    }
                }
                Err(error) => warn!(%error, "open-interest poll failed"),
            }
            sleep(Duration::from_secs(1)).await;
        }
    });

    let snapshot = market_client.snapshot(symbol).await?;
    let mut synchronizer = LobSynchronizer::<2_048>::new(symbol, 10_000);
    synchronize_initial_book(&market_client, &market_bus, &mut synchronizer, snapshot).await?;

    let filters = default_filters();
    let sizer = CapitalSizer {
        risk_fraction: 0.01,
        min_notional: filters.min_notional,
        max_notional: filters.max_notional,
        quantity_step: filters.quantity_step,
    };
    let mut alpha = AlphaEngine::new(symbol, AlphaConfig::default(), sizer);
    let funding_guard = FundingGuard::default();
    let mut last_funding_rate = 0.0_f64;
    let orders = Arc::new(OrderManager::new(
        venue,
        OrderManagerConfig {
            filters,
            max_exit_slippage_ticks: 3,
        },
        TokenBucket::new(1_200, 20.0, 100),
    ));
    // A live restart always cancels and flattens venue ground truth before any
    // new entry can be considered. Shadow starts with an empty virtual account.
    if mode == ExecutionMode::Live {
        orders.orphan_protection(symbol, unix_time_ns()).await?;
    }
    let mut account_ready = mode == ExecutionMode::Shadow;
    let mut risk = RiskManager::new(RiskConfig::default(), bootstrap_equity);
    let statsd_target =
        env::var("AEGIS_STATSD_ADDR").unwrap_or_else(|_| "127.0.0.1:8125".to_owned());
    let telemetry = Telemetry::start(&statsd_target, 8_192);
    let status = SharedStatus::new(SystemStatus {
        equity: bootstrap_equity,
        exposure: Fixed::ZERO,
        daily_pnl: Fixed::ZERO,
        breaker: BreakerLevel::Green,
        market_connected: true,
        user_stream_connected: mode == ExecutionMode::Shadow,
        mode,
    });
    let (command_tx, mut command_rx) = mpsc::channel(64);
    let notifications = start_c2_if_configured(command_tx, status.clone())?;
    if let Some(sender) = &notifications {
        sender.send(format!("✅ بدأ AEGIS في وضع {mode:?} للرمز {symbol}."));
    }

    let mut pending_snapshot: Option<BookSnapshot> = None;
    info!(%symbol, ?mode, "AEGIS engine started");
    loop {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                C2Command::Status => {}
                C2Command::HaltConfirmed => {
                    risk.force_red(unix_time_ns());
                    if let Err(error) = orders.orphan_protection(symbol, unix_time_ns()).await {
                        error!(%error, "C2 halt orphan protection failed");
                    }
                    if let Some(sender) = &notifications {
                        sender.send("🔴 تم تفعيل قاطع الحماية الأحمر وتسوية التعرض.".to_owned());
                    }
                }
                C2Command::Resume => {
                    let healthy = synchronizer.state() == SyncState::Synchronized;
                    let resumed = risk.resume(unix_time_ns(), healthy);
                    if let Some(sender) = &notifications {
                        sender.send(if resumed {
                            "🟢 اجتاز النظام فحص الصحة واستؤنف بحجم 25%.".to_owned()
                        } else {
                            "🟠 رُفض الاستئناف: فترة التبريد أو فحص الصحة غير مكتمل.".to_owned()
                        });
                    }
                }
                C2Command::SetRiskFraction(fraction) => {
                    risk.set_base_risk_fraction(fraction);
                }
                C2Command::ChangeMode(requested) => {
                    warn!(?requested, "mode changes require a supervised safe restart");
                    if let Some(sender) = &notifications {
                        sender.send("ℹ️ تم تسجيل تغيير الوضع؛ يلزم إعادة تشغيل آمنة.".to_owned());
                    }
                }
            }
        }

        if let Some(receiver) = user_data.as_mut() {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    UserDataEvent::Execution(update) => {
                        if let Some(net_pnl) = update.realized_pnl {
                            risk.record_trade(TradeOutcome {
                                net_pnl,
                                mae: Fixed::ZERO,
                                mfe: Fixed::ZERO,
                            });
                        }
                        if let Some(fill) = update.last_fill {
                            if let Some(sender) = &notifications {
                                sender.send(format!(
                                    "تنفيذ {:?}: {} {} بسعر {}",
                                    fill.side, fill.quantity, fill.symbol, fill.price
                                ));
                            }
                        }
                        if let Err(error) = orders.on_execution_update(update).await {
                            error!(%error, "execution reconciliation failed");
                            risk.force_red(unix_time_ns());
                        }
                    }
                    UserDataEvent::AccountEquity { wallet, .. } => {
                        account_ready = true;
                        risk.synchronize_live_equity(wallet);
                        status.update(|state| {
                            state.equity = wallet;
                            state.user_stream_connected = true;
                        });
                    }
                    UserDataEvent::Disconnected { at_ns } => {
                        account_ready = false;
                        status.update(|state| state.user_stream_connected = false);
                        risk.force_red(at_ns);
                        let _ = orders.orphan_protection(symbol, at_ns).await;
                        if let Some(sender) = &notifications {
                            sender.send(
                                "🔴 انقطع تدفق الحساب؛ أُلغيت الأوامر وسُوّيت المراكز.".to_owned(),
                            );
                        }
                    }
                }
            }
        }

        while let Ok(open_interest) = open_interest_rx.try_recv() {
            alpha.on_open_interest(symbol, open_interest);
        }

        if let Some(event) = market_bus.pop() {
            match event {
                MarketEvent::Depth(delta) => {
                    let timestamp = delta.event_time_ns;
                    match synchronizer.on_delta(delta) {
                        Ok(SyncOutcome::Applied) => {
                            let book = synchronizer.book().top::<5>();
                            if let Some(shadow) = &shadow {
                                shadow.on_book(book);
                            }
                            let risk_snapshot = risk.snapshot();
                            let _ = alpha.set_risk_fraction(risk_snapshot.risk_fraction);
                            if let Some(intent) = alpha.on_book(&book, risk_snapshot.live_equity) {
                                let expected_edge = intent
                                    .price
                                    .checked_mul(intent.quantity)
                                    .filter(|notional| notional.0 > 0)
                                    .map_or(0.0, |notional| {
                                        intent.expected_net_value.as_f64() / notional.as_f64()
                                    });
                                if !funding_guard.permits(
                                    intent.side,
                                    last_funding_rate,
                                    expected_edge,
                                ) {
                                    warn!("signal blocked by adverse funding guard");
                                } else {
                                    if let Some(context) = alpha.last_context() {
                                        telemetry.emit(TelemetryEvent::Signal {
                                            symbol,
                                            side: intent.side,
                                            obi: context.obi,
                                            vpin: context.vpin,
                                        });
                                    }
                                    if let Err(error) = orders
                                        .submit_intent(
                                            intent,
                                            timestamp,
                                            risk_snapshot.entries_allowed && account_ready,
                                        )
                                        .await
                                    {
                                        warn!(%error, "trade intent was not accepted");
                                    }
                                }
                            }
                        }
                        Ok(SyncOutcome::ResyncRequired) | Err(_) => {
                            warn!("depth sequence gap; entries suspended for resynchronization");
                            synchronizer.begin_resync();
                            pending_snapshot = Some(market_client.snapshot(symbol).await?);
                        }
                        Ok(SyncOutcome::Buffered | SyncOutcome::IgnoredOld) => {
                            if let Some(snapshot) = pending_snapshot.as_ref() {
                                if synchronizer.install_snapshot(snapshot).is_ok() {
                                    pending_snapshot = None;
                                    status.update(|state| state.market_connected = true);
                                }
                            }
                        }
                    }
                }
                MarketEvent::Trade(trade) => {
                    alpha.on_trade(&trade);
                    if let Some(shadow) = &shadow {
                        for update in shadow.on_trade(&trade) {
                            if let Err(error) = orders.on_execution_update(update).await {
                                error!(%error, "shadow execution update failed");
                            }
                        }
                        for net_pnl in shadow.drain_realized_outcomes() {
                            risk.record_trade(TradeOutcome {
                                net_pnl,
                                mae: Fixed::ZERO,
                                mfe: Fixed::ZERO,
                            });
                        }
                        let report = shadow.report();
                        risk.synchronize_live_equity(report.ending_equity);
                        status.update(|state| {
                            state.equity = report.ending_equity;
                            state.daily_pnl = report.ending_equity - report.starting_equity;
                        });
                    }
                }
                MarketEvent::MarkPrice(mark) => {
                    last_funding_rate = mark.funding_rate.as_f64();
                }
                MarketEvent::ForceOrder(order) => {
                    info!(?order, "forced-liquidation market event");
                }
                MarketEvent::MarketDataHalt { reason, .. } => {
                    warn!(reason, "market data halted; canceling open orders");
                    status.update(|state| state.market_connected = false);
                    let _ = orders.cancel_all(symbol).await;
                    synchronizer.begin_resync();
                    pending_snapshot = Some(market_client.snapshot(symbol).await?);
                    if let Some(sender) = &notifications {
                        sender.send(
                            "🟠 انقطع تدفق السوق؛ أُلغيت الأوامر وبدأت إعادة المزامنة.".to_owned(),
                        );
                    }
                }
            }
        } else {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result?;
                    info!("shutdown signal received");
                    let _ = orders.cancel_all(symbol).await;
                    break;
                }
                _ = sleep(Duration::from_micros(100)) => {}
            }
        }

        let risk_snapshot = risk.snapshot();
        status.update(|state| state.breaker = risk_snapshot.level);
        telemetry.emit(TelemetryEvent::Risk {
            equity: risk_snapshot.live_equity,
            exposure: Fixed::ZERO,
            level: risk_snapshot.level,
        });
    }
    Ok(())
}

async fn synchronize_initial_book(
    client: &BinanceMarketDataClient,
    bus: &LatestEventBus<MarketEvent>,
    synchronizer: &mut LobSynchronizer<2_048>,
    mut snapshot: BookSnapshot,
) -> anyhow::Result<()> {
    loop {
        if let Some(event) = bus.pop() {
            if let MarketEvent::Depth(delta) = event {
                if synchronizer.on_delta(delta).is_err() {
                    synchronizer.begin_resync();
                    snapshot = client.snapshot(snapshot.symbol).await?;
                    continue;
                }
                match synchronizer.install_snapshot(&snapshot) {
                    Ok(()) => return Ok(()),
                    Err(BookError::SnapshotBridgeMissing)
                        if synchronizer.state() == SyncState::AwaitingSnapshot => {}
                    Err(_) => {
                        synchronizer.begin_resync();
                        snapshot = client.snapshot(snapshot.symbol).await?;
                    }
                }
            }
        } else {
            sleep(Duration::from_millis(1)).await;
        }
    }
}

fn start_c2_if_configured(
    commands: mpsc::Sender<C2Command>,
    status: SharedStatus,
) -> anyhow::Result<Option<project_aegis::c2::NotificationSender>> {
    let Ok(token) = env::var("TELEGRAM_BOT_TOKEN") else {
        return Ok(None);
    };
    let user_id = env::var("TELEGRAM_ALLOWED_USER_ID")?.parse::<i64>()?;
    let chat_id = env::var("TELEGRAM_NOTIFICATION_CHAT_ID")
        .unwrap_or_else(|_| user_id.to_string())
        .parse::<i64>()?;
    let confirmation = env::var("AEGIS_C2_CONFIRMATION_CODE")?;
    let provider: Arc<dyn StatusProvider> = Arc::new(status);
    let (service, notifications) =
        TelegramC2::new(token, user_id, chat_id, confirmation, commands, provider)?;
    tokio::spawn(async move {
        if let Err(error) = service.run().await {
            error!(%error, "Telegram C2 stopped");
        }
    });
    Ok(Some(notifications))
}

async fn run_backtest(path: &str, equity: Fixed) -> anyhow::Result<()> {
    let records = HistoricalDataLake::read(path)?;
    let snapshot = records
        .iter()
        .find_map(|record| match record {
            HistoricalRecord::Snapshot { snapshot, .. } => Some(snapshot.clone()),
            HistoricalRecord::Event(_) => None,
        })
        .ok_or_else(|| anyhow::anyhow!("data partition has no initial snapshot"))?;
    let symbol = snapshot.symbol;
    let feed = DataFeed::new(records)?;
    let filters = default_filters();
    let shadow = Arc::new(ShadowExecutionEngine::new(ShadowConfig {
        starting_equity: equity,
        ..ShadowConfig::default()
    }));
    let venue: Arc<dyn ExecutionVenue> = shadow.clone();
    let orders = Arc::new(OrderManager::new(
        venue,
        OrderManagerConfig {
            filters,
            max_exit_slippage_ticks: 3,
        },
        TokenBucket::new(1_000_000, 1_000_000.0, 100),
    ));
    let alpha = AlphaEngine::new(
        symbol,
        AlphaConfig::default(),
        CapitalSizer {
            risk_fraction: 0.01,
            min_notional: filters.min_notional,
            max_notional: filters.max_notional,
            quantity_step: filters.quantity_step,
        },
    );
    let risk = RiskManager::new(RiskConfig::default(), equity);
    let report = BacktestEngine::new(feed, alpha, risk, orders, shadow, &snapshot)?
        .run()
        .await?;
    println!(
        "Backtest complete\nevents={}\nstart={}\nend={}\nnet_return={:.4}%\nfees={}\nmax_drawdown={:.4}%\nfills={}\nmaker_fills={}",
        report.events_processed,
        report.shadow.starting_equity,
        report.shadow.ending_equity,
        report.net_return_fraction() * 100.0,
        report.shadow.total_fees,
        report.shadow.max_drawdown * 100.0,
        report.shadow.fills,
        report.shadow.maker_fills,
    );
    Ok(())
}

fn default_filters() -> ExchangeFilters {
    ExchangeFilters {
        min_notional: Fixed::from_f64(5.0),
        max_notional: Fixed::from_f64(250_000.0),
        tick_size: Fixed::from_f64(0.1),
        quantity_step: Fixed::from_f64(0.001),
        min_quantity: Fixed::from_f64(0.001),
    }
}

fn env_fixed(key: &str, default: &str) -> anyhow::Result<Fixed> {
    Ok(Fixed::from_str(
        &env::var(key).unwrap_or_else(|_| default.to_owned()),
    )?)
}
