use crate::domain::Fixed;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BreakerLevel {
    Green,
    Yellow,
    Orange,
    Red,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAction {
    None,
    ReduceNewRisk,
    HaltEntries,
    FlattenAndShutdown,
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub base_risk_fraction: f64,
    pub yellow_latency_ms: f64,
    pub yellow_rejection_rate: f64,
    pub daily_drawdown_fraction: f64,
    pub weekly_drawdown_fraction: f64,
    pub flash_move_fraction: f64,
    pub toxic_vpin: f64,
    pub volatility_multiple: f64,
    pub user_stream_red_after_ns: u64,
    pub cooldown_ns: u64,
    pub rolling_ev_window: usize,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            base_risk_fraction: 0.01,
            yellow_latency_ms: 50.0,
            yellow_rejection_rate: 0.05,
            daily_drawdown_fraction: 0.02,
            weekly_drawdown_fraction: 0.05,
            flash_move_fraction: 0.03,
            toxic_vpin: 0.80,
            volatility_multiple: 4.0,
            user_stream_red_after_ns: 5_000_000_000,
            cooldown_ns: 15 * 60 * 1_000_000_000,
            rolling_ev_window: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RiskInputs {
    pub round_trip_latency_ms: f64,
    pub rejection_rate: f64,
    pub persistent_vpin: f64,
    pub volatility_multiple: f64,
    pub ten_second_move_fraction: f64,
    pub user_stream_silence_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TradeOutcome {
    /// Net PnL after fees and actual slippage.
    pub net_pnl: Fixed,
    pub mae: Fixed,
    pub mfe: Fixed,
}

#[derive(Debug, Clone, Copy)]
pub struct RiskSnapshot {
    pub level: BreakerLevel,
    pub live_equity: Fixed,
    pub risk_fraction: f64,
    pub daily_drawdown: f64,
    pub weekly_drawdown: f64,
    pub rolling_net_ev: f64,
    pub entries_allowed: bool,
    pub shutdown_required: bool,
}

pub struct RiskManager {
    config: RiskConfig,
    initial_equity: Fixed,
    live_equity: Fixed,
    day_start_equity: Fixed,
    week_start_equity: Fixed,
    level: BreakerLevel,
    rolling_net: VecDeque<Fixed>,
    consecutive_losses: u32,
    consecutive_wins: u32,
    recovery_multiplier: f64,
    recovery_wins: u32,
    halted_at_ns: Option<u64>,
    last_mae: Fixed,
    last_mfe: Fixed,
}

impl RiskManager {
    #[must_use]
    pub fn new(config: RiskConfig, live_equity: Fixed) -> Self {
        assert!(live_equity.0 > 0, "risk manager requires positive equity");
        assert!(config.rolling_ev_window > 0, "EV window must be non-zero");
        Self {
            config,
            initial_equity: live_equity,
            live_equity,
            day_start_equity: live_equity,
            week_start_equity: live_equity,
            level: BreakerLevel::Green,
            rolling_net: VecDeque::new(),
            consecutive_losses: 0,
            consecutive_wins: 0,
            recovery_multiplier: 1.0,
            recovery_wins: 0,
            halted_at_ns: None,
            last_mae: Fixed::ZERO,
            last_mfe: Fixed::ZERO,
        }
    }

    pub fn evaluate(&mut self, inputs: RiskInputs, now_ns: u64) -> RiskAction {
        let daily_drawdown = drawdown(self.day_start_equity, self.live_equity);
        let weekly_drawdown = drawdown(self.week_start_equity, self.live_equity);
        let rolling_negative =
            self.rolling_net.len() == self.config.rolling_ev_window && self.rolling_net_ev().0 < 0;

        let target = if inputs.ten_second_move_fraction.abs() >= self.config.flash_move_fraction
            || inputs.user_stream_silence_ns >= self.config.user_stream_red_after_ns
            || weekly_drawdown >= self.config.weekly_drawdown_fraction
        {
            BreakerLevel::Red
        } else if daily_drawdown >= self.config.daily_drawdown_fraction
            || inputs.persistent_vpin >= self.config.toxic_vpin
            || inputs.volatility_multiple >= self.config.volatility_multiple
            || rolling_negative
        {
            BreakerLevel::Orange
        } else if inputs.round_trip_latency_ms >= self.config.yellow_latency_ms
            || inputs.rejection_rate >= self.config.yellow_rejection_rate
        {
            BreakerLevel::Yellow
        } else {
            BreakerLevel::Green
        };

        // Red and Orange are latched. They can only be cleared by resume().
        if self.level < BreakerLevel::Orange || target > self.level {
            self.level = target;
        }
        if self.level >= BreakerLevel::Orange {
            self.halted_at_ns.get_or_insert(now_ns);
        }
        match self.level {
            BreakerLevel::Green => RiskAction::None,
            BreakerLevel::Yellow => RiskAction::ReduceNewRisk,
            BreakerLevel::Orange => RiskAction::HaltEntries,
            BreakerLevel::Red => RiskAction::FlattenAndShutdown,
        }
    }

    pub fn record_trade(&mut self, outcome: TradeOutcome) {
        self.live_equity = self.live_equity + outcome.net_pnl;
        self.last_mae = outcome.mae;
        self.last_mfe = outcome.mfe;
        if self.rolling_net.len() == self.config.rolling_ev_window {
            self.rolling_net.pop_front();
        }
        self.rolling_net.push_back(outcome.net_pnl);
        if outcome.net_pnl.0 < 0 {
            self.consecutive_losses = self.consecutive_losses.saturating_add(1);
            self.consecutive_wins = 0;
        } else if outcome.net_pnl.0 > 0 {
            self.consecutive_wins = self.consecutive_wins.saturating_add(1);
            if self.consecutive_wins >= 3 {
                self.consecutive_losses = 0;
            }
            if self.recovery_multiplier < 1.0 {
                self.recovery_wins = self.recovery_wins.saturating_add(1);
                if self.recovery_wins >= 5 {
                    self.recovery_multiplier = 1.0;
                } else {
                    self.recovery_multiplier = 0.25 + f64::from(self.recovery_wins) * 0.15;
                }
            }
        }
    }

    pub fn force_red(&mut self, now_ns: u64) -> RiskAction {
        self.level = BreakerLevel::Red;
        self.halted_at_ns = Some(now_ns);
        RiskAction::FlattenAndShutdown
    }

    /// Resumption requires the full cool-down and externally verified market,
    /// book, user-stream and funding health.
    pub fn resume(&mut self, now_ns: u64, health_check_passed: bool) -> bool {
        let cooled_down = self
            .halted_at_ns
            .is_some_and(|halted| now_ns.saturating_sub(halted) >= self.config.cooldown_ns);
        if !cooled_down || !health_check_passed {
            return false;
        }
        self.level = BreakerLevel::Green;
        self.halted_at_ns = None;
        self.recovery_multiplier = 0.25;
        self.recovery_wins = 0;
        true
    }

    pub fn set_base_risk_fraction(&mut self, fraction: f64) -> bool {
        if !(0.0..=0.05).contains(&fraction) || fraction == 0.0 {
            return false;
        }
        self.config.base_risk_fraction = fraction;
        true
    }

    /// Reconciles equity from the authenticated account stream. This changes
    /// sizing and drawdown state but never clears a latched circuit breaker.
    pub fn synchronize_live_equity(&mut self, equity: Fixed) {
        if equity.0 > 0 {
            self.live_equity = equity;
        }
    }

    pub fn reset_daily_baseline(&mut self) {
        self.day_start_equity = self.live_equity;
    }

    pub fn reset_weekly_baseline(&mut self) {
        self.week_start_equity = self.live_equity;
    }

    #[must_use]
    pub fn rolling_net_ev(&self) -> Fixed {
        if self.rolling_net.is_empty() {
            return Fixed::ZERO;
        }
        let total: i128 = self
            .rolling_net
            .iter()
            .map(|value| i128::from(value.0))
            .sum();
        Fixed::from_raw((total / self.rolling_net.len() as i128) as i64)
    }

    #[must_use]
    pub fn snapshot(&self) -> RiskSnapshot {
        let loss_dampener = 0.8_f64.powi(self.consecutive_losses.min(i32::MAX as u32) as i32);
        let breaker_multiplier = if self.level == BreakerLevel::Yellow {
            0.5
        } else {
            1.0
        };
        RiskSnapshot {
            level: self.level,
            live_equity: self.live_equity,
            risk_fraction: self.config.base_risk_fraction
                * loss_dampener
                * breaker_multiplier
                * self.recovery_multiplier,
            daily_drawdown: drawdown(self.day_start_equity, self.live_equity),
            weekly_drawdown: drawdown(self.week_start_equity, self.live_equity),
            rolling_net_ev: self.rolling_net_ev().as_f64(),
            entries_allowed: self.level <= BreakerLevel::Yellow,
            shutdown_required: self.level == BreakerLevel::Red,
        }
    }

    #[must_use]
    pub const fn initial_equity(&self) -> Fixed {
        self.initial_equity
    }

    #[must_use]
    pub const fn last_excursions(&self) -> (Fixed, Fixed) {
        (self.last_mae, self.last_mfe)
    }
}

fn drawdown(baseline: Fixed, current: Fixed) -> f64 {
    if baseline.0 <= 0 || current >= baseline {
        0.0
    } else {
        (baseline - current).as_f64() / baseline.as_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn losses_reduce_risk_without_martingale() {
        let mut risk = RiskManager::new(RiskConfig::default(), Fixed::from_f64(1_000.0));
        risk.record_trade(TradeOutcome {
            net_pnl: Fixed::from_f64(-1.0),
            mae: Fixed::from_f64(2.0),
            mfe: Fixed::ZERO,
        });
        assert!((risk.snapshot().risk_fraction - 0.008).abs() < 1e-12);
        risk.record_trade(TradeOutcome {
            net_pnl: Fixed::from_f64(-1.0),
            mae: Fixed::from_f64(2.0),
            mfe: Fixed::ZERO,
        });
        assert!((risk.snapshot().risk_fraction - 0.0064).abs() < 1e-12);
    }

    #[test]
    fn red_breaker_latches_and_requires_cooldown() {
        let mut config = RiskConfig::default();
        config.cooldown_ns = 100;
        let mut risk = RiskManager::new(config, Fixed::from_f64(1_000.0));
        assert_eq!(
            risk.evaluate(
                RiskInputs {
                    ten_second_move_fraction: 0.04,
                    ..RiskInputs::default()
                },
                1_000,
            ),
            RiskAction::FlattenAndShutdown
        );
        assert!(!risk.resume(1_099, true));
        assert!(risk.resume(1_100, true));
        assert_eq!(risk.snapshot().risk_fraction, 0.0025);
    }
}
