//! Engine configuration, read from TOML at startup.
//!
//! Malformed configuration is a startup error, never a runtime surprise. Unknown
//! keys are rejected rather than ignored, because a silently dropped
//! `max_position_size` typo is the kind of mistake that only surfaces as a
//! position four times larger than intended.
//!
//! This is the file half of the boundary. The registry file is separate and is
//! read by [`crate::registry::Registry::from_toml`]; this module only says where
//! it lives.

use crate::exec::atomic::DEFAULT_TIMEOUT_MS;
use crate::fees::{FeeModels, KalshiFees, PolymarketFees};
use crate::risk::RiskLimits;
use crate::solver::SolverConfig;
use crate::types::Cents;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: SolverConfig,
    pub venues: Venues,
    pub registry: RegistrySettings,
    pub recorder: RecorderSettings,
    pub risk: RiskSettings,
    pub executor: ExecutorSettings,
    pub discovery: DiscoverySettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoverySettings {
    pub enabled: bool,
    pub cache_dir: PathBuf,
    pub cache_max_age_secs: u64,
    /// Build the registry from venue metadata at startup instead of from the
    /// registry file.
    pub auto_discover_on_startup: bool,
    /// Venue status filter. Empty takes every market the venue lists.
    pub status: String,
    /// One series, such as `KXBTCD`. Empty walks the whole venue, which on
    /// Kalshi is over twelve thousand open markets across ten thousand events.
    pub series: String,
    pub max_pages: usize,
    /// Whether an inferred group may be traded with a live order client. False
    /// means discovery can find and price opportunities but a human must
    /// promote the group before real money follows.
    pub allow_inferred_live_orders: bool,
}

impl Default for DiscoverySettings {
    fn default() -> Self {
        DiscoverySettings {
            enabled: true,
            cache_dir: PathBuf::from(".cache/kalshi"),
            cache_max_age_secs: 3_600,
            auto_discover_on_startup: false,
            status: "open".into(),
            series: String::new(),
            max_pages: 100,
            allow_inferred_live_orders: false,
        }
    }
}

impl DiscoverySettings {
    pub fn scope(&self) -> crate::discovery::DiscoveryScope {
        crate::discovery::DiscoveryScope {
            status: self.status.clone(),
            series: (!self.series.is_empty()).then(|| self.series.clone()),
            max_pages: self.max_pages.max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskSettings {
    pub max_per_event_cents: Cents,
    pub max_per_theme_cents: Cents,
    pub max_concurrent_trades: usize,
    /// Realized loss that closes the day. Lives here rather than on
    /// [`RiskLimits`] because the portfolio, not the risk check, is what tracks
    /// and resets it.
    pub max_daily_loss_cents: Cents,
    /// Capital the engine starts with.
    pub starting_capital_cents: Cents,
}

impl Default for RiskSettings {
    fn default() -> Self {
        let limits = RiskLimits::default();
        RiskSettings {
            max_per_event_cents: limits.max_per_event_cents,
            max_per_theme_cents: limits.max_per_theme_cents,
            max_concurrent_trades: limits.max_concurrent_trades,
            max_daily_loss_cents: 100_000,
            starting_capital_cents: 1_000_000,
        }
    }
}

impl RiskSettings {
    pub fn limits(&self) -> RiskLimits {
        RiskLimits {
            max_per_event_cents: self.max_per_event_cents,
            max_per_theme_cents: self.max_per_theme_cents,
            max_concurrent_trades: self.max_concurrent_trades,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutorSettings {
    /// True means simulate. The default is true and the CLI requires an
    /// explicit flag to change it, because the failure modes are not symmetric:
    /// an unintended paper run costs a rerun, an unintended live run costs money.
    pub paper_mode: bool,
    pub atomic_timeout_ms: u64,
    /// Idle time before the venue is treated as unreachable.
    pub max_idle_ms: u64,
}

impl Default for ExecutorSettings {
    fn default() -> Self {
        ExecutorSettings {
            paper_mode: true,
            atomic_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_idle_ms: crate::health::DEFAULT_MAX_IDLE_MS,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Venues {
    pub kalshi: VenueSettings,
    pub polymarket: VenueSettings,
}

impl Default for Venues {
    fn default() -> Self {
        Venues {
            kalshi: VenueSettings {
                enabled: true,
                ..VenueSettings::default()
            },
            polymarket: VenueSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VenueSettings {
    /// A disabled venue contributes no feed and no metadata lookups. Its
    /// registry members still load, so a group spanning it is visible and is
    /// skipped for a stated reason rather than silently missing.
    pub enabled: bool,
    pub metadata_url: Option<String>,
    pub cache_dir: Option<PathBuf>,
    /// Venue fee multiplier. Quantized to thousandths on the way in so the fee
    /// schedule stays integer arithmetic; this is an operator knob, not a price.
    pub fee_multiplier: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegistrySettings {
    pub path: PathBuf,
}

impl Default for RegistrySettings {
    fn default() -> Self {
        RegistrySettings {
            path: PathBuf::from("config/registry.toml"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderSettings {
    pub enabled: bool,
    pub output_dir: PathBuf,
    pub compress: bool,
}

impl Default for RecorderSettings {
    fn default() -> Self {
        RecorderSettings {
            enabled: true,
            output_dir: PathBuf::from("data"),
            compress: true,
        }
    }
}

impl Config {
    /// Read a config file. A missing file is an error; use [`Config::default`]
    /// when no path was supplied at all.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.engine.max_position_size > 0,
            "engine.max_position_size must be positive"
        );
        anyhow::ensure!(
            self.engine.min_net_edge_cents > 0,
            "engine.min_net_edge_cents must be positive; a zero-edge trade is not a trade"
        );
        anyhow::ensure!(
            self.engine.min_annualized_return.is_finite()
                && self.engine.min_annualized_return >= 0.0,
            "engine.min_annualized_return must be a non-negative number"
        );
        anyhow::ensure!(
            self.risk.starting_capital_cents > 0,
            "risk.starting_capital_cents must be positive"
        );
        anyhow::ensure!(
            self.risk.max_daily_loss_cents > 0,
            "risk.max_daily_loss_cents must be positive"
        );
        anyhow::ensure!(
            self.risk.max_concurrent_trades > 0,
            "risk.max_concurrent_trades must be positive"
        );
        anyhow::ensure!(
            self.discovery.max_pages > 0,
            "discovery.max_pages must be positive"
        );
        anyhow::ensure!(
            self.executor.atomic_timeout_ms > 0,
            "executor.atomic_timeout_ms must be positive"
        );
        for (name, venue) in [
            ("kalshi", &self.venues.kalshi),
            ("polymarket", &self.venues.polymarket),
        ] {
            if let Some(multiplier) = venue.fee_multiplier {
                anyhow::ensure!(
                    multiplier.is_finite() && multiplier >= 0.0,
                    "venues.{name}.fee_multiplier must be a non-negative number"
                );
            }
        }
        Ok(())
    }

    /// Fee schedules with each venue's configured multiplier applied.
    ///
    /// The multiplier arrives as a decimal and leaves as an exact rational with
    /// a denominator of 1000, so nothing downstream of here touches a float in
    /// the money path. A multiplier finer than a thousandth is not expressible
    /// and rounds; that is a deliberate limit on an operator knob.
    pub fn fee_models(&self) -> FeeModels {
        let numer = |multiplier: Option<f64>| -> i64 {
            multiplier
                .map(|m| (m * 1000.0).round() as i64)
                .unwrap_or(1000)
        };
        FeeModels {
            kalshi: KalshiFees {
                multiplier_numer: numer(self.venues.kalshi.fee_multiplier),
                multiplier_denom: 1000,
            },
            polymarket: PolymarketFees::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContractId;

    #[test]
    fn the_shipped_example_parses() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/example.toml"),
        )
        .unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.engine.max_book_age_ms, 500);
        assert_eq!(config.engine.min_net_edge_cents, 1);
        assert_eq!(config.engine.max_position_size, 500);
        assert!(config.venues.kalshi.enabled);
        assert!(!config.venues.polymarket.enabled);
        // The shipped default must be simulation, never live orders.
        assert!(config.executor.paper_mode);
        assert_eq!(config.risk.max_per_event_cents, 50_000);
    }

    #[test]
    fn paper_mode_is_the_default_even_with_no_executor_block() {
        assert!(Config::parse("").unwrap().executor.paper_mode);
        assert!(
            Config::parse("[executor]\natomic_timeout_ms = 250\n")
                .unwrap()
                .executor
                .paper_mode
        );
        // Zero timeouts and zero capital are misconfigurations, not intent.
        assert!(Config::parse("[executor]\natomic_timeout_ms = 0\n").is_err());
        assert!(Config::parse("[risk]\nstarting_capital_cents = 0\n").is_err());
    }

    #[test]
    fn an_empty_config_is_the_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.engine, SolverConfig::default());
        assert_eq!(config.registry.path, PathBuf::from("config/registry.toml"));
    }

    #[test]
    fn a_typo_fails_at_startup_rather_than_being_ignored() {
        // The whole point of deny_unknown_fields: this would otherwise start
        // with the default position size and nobody would find out until later.
        let error = Config::parse("[engine]\nmax_positon_size = 2000\n").unwrap_err();
        assert!(
            error.to_string().contains("max_positon_size"),
            "error should name the bad key: {error}"
        );
        assert!(Config::parse("[engine]\nmax_position_size = 0\n").is_err());
    }

    #[test]
    fn fee_multiplier_becomes_an_exact_rational() {
        let config = Config::parse("[venues.kalshi]\nfee_multiplier = 0.5\n").unwrap();
        let fees = config.fee_models();
        assert_eq!(fees.kalshi.multiplier_numer, 500);
        assert_eq!(fees.kalshi.multiplier_denom, 1000);
        // Half the schedule is half the charge, to the cent, with the ceiling.
        use crate::fees::FeeModel;
        assert_eq!(fees.kalshi.taker_fee(ContractId(0), 60, 100), 84);
        assert_eq!(KalshiFees::default().taker_fee(ContractId(0), 60, 100), 168);
        // An absent multiplier is exactly 1, not 0.999-something.
        let plain = Config::parse("").unwrap().fee_models();
        assert_eq!(plain.kalshi.taker_fee(ContractId(0), 60, 100), 168);
    }
}
