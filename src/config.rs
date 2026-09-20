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

use crate::fees::{FeeModels, KalshiFees, PolymarketFees};
use crate::solver::SolverConfig;
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
        assert_eq!(fees.kalshi.taker_fee(60, 100), 84);
        assert_eq!(KalshiFees::default().taker_fee(60, 100), 168);
        // An absent multiplier is exactly 1, not 0.999-something.
        let plain = Config::parse("").unwrap().fee_models();
        assert_eq!(plain.kalshi.taker_fee(60, 100), 168);
    }
}
