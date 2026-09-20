use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use sum100::{
    book::{Applied, BookStore},
    clock::{Clock, ReplayClock, WallClock},
    config::Config,
    discovery::{
        DiscoveryCache, KalshiDiscovery, PolymarketDiscovery, Universe, infer_groups,
        register_polymarket_fees,
    },
    engine::{Engine, EngineConfig, EngineState, SignalLog},
    exec::{OrderClient, kalshi::KalshiOrderClient, paper::PaperOrderClient},
    feed::{
        Feed,
        kalshi::{self, Credentials, Environment, KalshiFeed},
        merged::MergedFeed,
        polymarket::PolymarketFeed,
        replay::{Pace, ReplayFeed, ReplayOptions},
        rest::Rest,
    },
    portfolio::Portfolio,
    record::Recorder,
    registry::Registry,
    types::{BookState, Venue},
    verify::{Expected, GapLog, StateDigest},
};
use tokio::{net::TcpListener, sync::broadcast};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(
    version,
    about = "Read-only prediction-market feed, recorder, and book dump"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, ValueEnum)]
enum VenueArg {
    Kalshi,
    /// Public market data only. The CLOB market channel needs no credentials,
    /// and nothing on this venue can be traded yet.
    Polymarket,
}
#[derive(Clone, Copy, ValueEnum)]
enum PaceArg {
    /// Sleep recorded inter-arrival gaps.
    Realtime,
    /// Run flat out.
    Max,
}
#[derive(Subcommand)]
enum Command {
    Record {
        #[arg(long, value_enum)]
        venue: VenueArg,
        #[arg(long)]
        prod: bool,
        #[arg(long, value_delimiter = ',', required = true)]
        tickers: Vec<String>,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        /// Stop gracefully after this many seconds; otherwise run until Ctrl-C.
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Live best bid/ask table for visual check against the Kalshi web UI.
    Dump {
        #[arg(long, value_enum)]
        venue: VenueArg,
        #[arg(long)]
        prod: bool,
        #[arg(long, value_delimiter = ',', required = true)]
        tickers: Vec<String>,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        #[arg(long)]
        seconds: Option<u64>,
        /// How often to reprint the table to stdout (milliseconds).
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Also write the final state digest to this file.
        #[arg(long)]
        digest_out: Option<PathBuf>,
    },
    /// Replay a recorded file offline through the live parse and book path.
    Replay {
        #[arg(long, value_enum)]
        venue: VenueArg,
        /// Recorded daily file, e.g. data/production/kalshi-2026-09-14.ndjson.gz.
        #[arg(long)]
        file: PathBuf,
        /// Required for files recorded before session markers.
        #[arg(long, value_delimiter = ',')]
        tickers: Vec<String>,
        /// 1-based session within the file; required when it holds several.
        #[arg(long)]
        session: Option<usize>,
        #[arg(long, value_enum, default_value = "max")]
        pace: PaceArg,
        /// Compare final book and gap log hashes against expected values.
        #[arg(long)]
        verify: bool,
        /// Expected book hash, repeatable.
        #[arg(long, value_name = "TICKER=SHA256", requires = "verify")]
        expect_book: Vec<String>,
        /// Expected gap log hash.
        #[arg(long, value_name = "SHA256", requires = "verify")]
        expect_gaps: Option<String>,
        /// Digest file, as written by `dump --digest-out`.
        #[arg(long, value_name = "PATH", requires = "verify")]
        expect_file: Option<PathBuf>,
        #[arg(long)]
        digest_out: Option<PathBuf>,
    },
    Probe {
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        prod: bool,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
    Markets {
        #[arg(long)]
        series: String,
        #[arg(long)]
        prod: bool,
    },
    /// Run the engine: apply books, solve dirty constraint groups, report signals.
    Scan {
        /// Stream from the venue. Mutually exclusive with --replay.
        #[arg(long, conflicts_with = "replay")]
        live: bool,
        /// Drive a recorded session instead. No credentials, no network.
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
        #[arg(long, default_value = "config/example.toml")]
        config: PathBuf,
        /// Override `registry.path` from the config file.
        #[arg(long, value_name = "FILE")]
        registry: Option<PathBuf>,
        #[arg(long)]
        prod: bool,
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        #[arg(long, value_enum, default_value = "max")]
        pace: PaceArg,
        #[arg(long)]
        session: Option<usize>,
        /// Write the accepted signal log as JSON.
        #[arg(long, value_name = "PATH")]
        signals_out: Option<PathBuf>,
        /// Infer the constraint graph from venue metadata instead of the file.
        #[arg(long)]
        auto_discover: bool,
        /// Serve the dashboard API on this address. Omitted, nothing listens.
        #[arg(long, value_name = "ADDR")]
        serve: Option<SocketAddr>,
    },
    /// Run the engine and place orders. Paper mode unless --live-orders.
    Trade {
        #[arg(long, conflicts_with = "replay")]
        live: bool,
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
        #[arg(long, default_value = "config/example.toml")]
        config: PathBuf,
        #[arg(long, value_name = "FILE")]
        registry: Option<PathBuf>,
        #[arg(long)]
        prod: bool,
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long, default_value = "data")]
        out: PathBuf,
        #[arg(long, value_enum, default_value = "max")]
        pace: PaceArg,
        #[arg(long)]
        session: Option<usize>,
        /// Starting capital in cents. Overrides risk.starting_capital_cents.
        #[arg(long, value_name = "CENTS")]
        capital: Option<i64>,
        #[arg(long, value_name = "CENTS")]
        daily_loss_limit: Option<i64>,
        #[arg(long, value_name = "CENTS")]
        max_per_event: Option<i64>,
        #[arg(long, value_name = "CENTS")]
        max_per_theme: Option<i64>,
        /// Simulate fills against the live book. This is the default; the flag
        /// exists so a command can say so out loud.
        #[arg(long)]
        paper_mode: bool,
        /// PLACE REAL ORDERS WITH REAL MONEY. Requires --prod and --live.
        #[arg(long, conflicts_with = "paper_mode")]
        live_orders: bool,
        #[arg(long, value_name = "PATH")]
        signals_out: Option<PathBuf>,
        /// Infer the constraint graph from venue metadata instead of the file.
        #[arg(long)]
        auto_discover: bool,
    },
    /// Registry inspection.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    /// Load the registry and report what it defines, without running the engine.
    Validate {
        #[arg(long, default_value = "config/example.toml")]
        config: PathBuf,
        #[arg(long, value_name = "FILE")]
        registry: Option<PathBuf>,
        /// Also confirm every ticker exists, against venue market metadata.
        #[arg(long)]
        live: bool,
        #[arg(long)]
        prod: bool,
        /// Ignore any cached metadata and refetch.
        #[arg(long)]
        refresh: bool,
        /// Validate the inferred registry instead of the file.
        #[arg(long)]
        discover: bool,
        /// Override discovery.cache_dir for this validation run.
        #[arg(long, value_name = "DIR")]
        cache_dir: Option<PathBuf>,
    },
    /// Read the constraint graph off the venue and report what it found.
    Discover {
        #[arg(long, default_value = "config/example.toml")]
        config: PathBuf,
        #[arg(long)]
        prod: bool,
        /// Ignore any cached snapshot and refetch from the selected Kalshi API.
        #[arg(long)]
        refresh: bool,
        /// Fetch the live listing even when the configured cache is fresh.
        #[arg(long)]
        live: bool,
        /// Override discovery.cache_dir for this run.
        #[arg(long, value_name = "DIR")]
        cache_dir: Option<PathBuf>,
        /// One series, such as KXBTCD. Overrides discovery.series.
        #[arg(long, value_name = "SERIES")]
        series: Option<String>,
        /// Write the inferred graph as TOML, for reading and for promoting
        /// groups by hand. The engine does not load it; use --auto-discover.
        #[arg(long, value_name = "PATH", default_value = "config/auto_registry.toml")]
        out: Option<PathBuf>,
    },
}

/// Fetch or reuse a venue snapshot, honouring the discovery config.
async fn load_universe(
    config: &Config,
    prod: bool,
    refresh: bool,
    series: Option<String>,
    cache_dir: Option<&Path>,
) -> Result<Universe> {
    let clock: Arc<dyn Clock> = Arc::new(WallClock);
    let rest = Rest::new(environment(prod), clock.clone())?;
    let discovery = KalshiDiscovery::new(rest);
    let cache = DiscoveryCache::new(cache_dir.unwrap_or(&config.discovery.cache_dir));
    let mut scope = config.discovery.scope();
    if let Some(series) = series {
        scope.series = Some(series);
    }
    let max_age = if refresh {
        0
    } else {
        config.discovery.cache_max_age_secs
    };
    cache
        .load_or_fetch(&discovery, &scope, max_age, clock.now_ms())
        .await
}

/// Load the config, then the registry it points at.
///
/// Both are startup errors: an engine that begins trading on a half-understood
/// constraint graph is worse than one that refuses to start.
fn load_registry(config_path: &Path, override_path: Option<&Path>) -> Result<(Config, Registry)> {
    let config = Config::load(config_path)?;
    let registry_path = override_path.unwrap_or(&config.registry.path);
    let registry = Registry::from_toml(registry_path)?;
    tracing::info!(
        config = %config_path.display(),
        registry = %registry_path.display(),
        events = registry.events().len(),
        groups = registry.groups().len(),
        kalshi_contracts = registry.tickers(Venue::Kalshi).len(),
        polymarket_contracts = registry.tickers(Venue::Polymarket).len(),
        "registry loaded"
    );
    Ok((config, registry))
}

/// Distinct Kalshi series prefixes, the unit its discovery endpoints take.
fn series_of(tickers: &[String]) -> Vec<String> {
    let mut series: Vec<String> = tickers
        .iter()
        .filter_map(|ticker| ticker.split('-').next())
        .map(str::to_owned)
        .collect();
    series.sort();
    series.dedup();
    series
}

/// Fetch a series' markets, reusing a cached copy when one exists.
///
/// Market listings change when a series rolls, not between two runs a minute
/// apart, so re-fetching on every load spends rate limit for nothing.
async fn cached_markets(
    rest: &Rest,
    series: &str,
    cache_dir: Option<&Path>,
    refresh: bool,
) -> Result<Vec<String>> {
    let cache_file = cache_dir.map(|dir| dir.join(format!("markets-{series}.json")));
    if !refresh
        && let Some(path) = &cache_file
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let tickers: Vec<String> = serde_json::from_str(&text)
            .with_context(|| format!("reading cached metadata {}", path.display()))?;
        tracing::info!(series, cached = tickers.len(), "metadata from cache");
        return Ok(tickers);
    }
    let tickers: Vec<String> = rest
        .series_markets(series)
        .await?
        .into_iter()
        .map(|market| market.ticker)
        .collect();
    if let Some(path) = &cache_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(&tickers)?)
            .with_context(|| format!("writing cache {}", path.display()))?;
    }
    Ok(tickers)
}
fn environment(prod: bool) -> Environment {
    if prod {
        tracing::warn!("PRODUCTION selected explicitly; read-only market data");
        Environment::Production
    } else {
        Environment::Demo
    }
}

fn emit_digest(store: &BookStore, gaps: &GapLog, out: Option<&Path>) -> Result<StateDigest> {
    let digest = StateDigest::new(store, gaps);
    let text = digest.render();
    print!("{text}");
    if let Some(path) = out {
        std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(digest)
}

fn print_book_table(store: &BookStore, now_ms: u64) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{:<32} {:<14} {:>8} {:>12} {:>12} {:>12} {:>12} {:>8}",
        "ticker", "state", "seq", "yes_bid", "yes_ask", "no_bid", "no_ask", "age_ms"
    );
    for book in store.books() {
        let Some((_, ticker)) = store.contracts().resolve(book.contract_id) else {
            continue;
        };
        let state = match book.state {
            BookState::Uninitialized => "uninitialized",
            BookState::Resyncing => "resyncing",
            BookState::Live => "live",
        };
        let fmt_level = |level: Option<sum100::types::Level>| -> String {
            match level {
                Some(l) => format!("{}x{}", l.price, l.size),
                None => "-".into(),
            }
        };
        let yes_bid = fmt_level(book.best_bid());
        let yes_ask = fmt_level(book.best_ask());
        let no_bid = fmt_level(book.best_no_bid());
        let no_ask = match book.best_bid() {
            // Yes bid at P is no ask at 100-P.
            Some(l) => format!("{}x{}", 100 - l.price, l.size),
            None => "-".into(),
        };
        let age = if book.updated_at_ms == 0 {
            0
        } else {
            now_ms.saturating_sub(book.updated_at_ms)
        };
        let _ = writeln!(
            out,
            "{:<32} {:<14} {:>8} {:>12} {:>12} {:>12} {:>12} {:>8}",
            ticker, state, book.seq, yes_bid, yes_ask, no_bid, no_ask, age
        );
    }
    let _ = writeln!(out);
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Record {
            venue,
            prod,
            tickers,
            out,
            seconds,
        } => {
            let env = environment(prod);
            let dir = out.join(env.name());
            // Each venue keeps its concrete type here: shutdown drains the
            // worker and returns its metrics, which the trait does not carry.
            let metrics = match venue {
                VenueArg::Kalshi => {
                    let recorder = Recorder::new(dir, Venue::Kalshi)?;
                    let mut feed = KalshiFeed::start(env, tickers, recorder, Arc::new(WallClock))?;
                    drain_until_deadline(&mut feed, seconds).await;
                    feed.shutdown().await?
                }
                // Tokens, not tickers: Polymarket names a market by its CLOB
                // token id, which is what `--tickers` carries for this venue.
                VenueArg::Polymarket => {
                    let recorder = Recorder::new(dir, Venue::Polymarket)?;
                    let subscriptions: [(Venue, &[String]); 1] = [(Venue::Polymarket, &tickers)];
                    let mut feed =
                        PolymarketFeed::start(&subscriptions, recorder, Arc::new(WallClock))?;
                    drain_until_deadline(&mut feed, seconds).await;
                    feed.shutdown().await?
                }
            };
            tracing::info!(?metrics, "recording finished and gzip flushed");
        }
        Command::Dump {
            venue: _,
            prod,
            tickers,
            out,
            seconds,
            interval_ms,
            digest_out,
        } => {
            ensure!(interval_ms > 0, "interval-ms must be positive");
            let env = environment(prod);
            let clock: Arc<dyn Clock> = Arc::new(WallClock);
            let mut store = BookStore::new(Venue::Kalshi, &tickers, clock.clone())?;
            let mut gaps = GapLog::default();
            let recorder = Recorder::new(out.join(env.name()), Venue::Kalshi)?;
            let mut feed = KalshiFeed::start(env, tickers, recorder, clock.clone())?;
            let deadline = async {
                match seconds {
                    Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline);
            let mut tick = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = &mut deadline => break,
                    _ = tick.tick() => print_book_table(&store, clock.now_ms()),
                    event = feed.next() => match event {
                        Some(event) => {
                            let applied = gaps.apply(&mut store, &event);
                            if let Applied::Gap { expected, got } = applied {
                                tracing::warn!(expected, got, "sequence gap; requesting resync");
                                feed.request_resync();
                            }
                            tracing::debug!(?event, ?applied, "book update");
                        }
                        None => break,
                    },
                }
            }
            // Apply every event whose bytes were recorded, so this final state
            // is exactly what a replay of the file reproduces.
            feed.stop();
            while let Some(event) = feed.next().await {
                let applied = gaps.apply(&mut store, &event);
                tracing::debug!(?event, ?applied, "book update after stop");
            }
            print_book_table(&store, clock.now_ms());
            store.metrics.log(&store);
            let metrics = feed.shutdown().await?;
            tracing::info!(?metrics, "dump finished and gzip flushed");
            emit_digest(&store, &gaps, digest_out.as_deref())?;
        }
        Command::Probe {
            ticker,
            prod,
            seconds,
        } => {
            ensure!(!ticker.trim().is_empty(), "ticker is empty");
            let env = environment(prod);
            let credentials = Credentials::from_env()?;
            let mut socket = kalshi::connect(
                env,
                &credentials,
                &[ticker],
                &["ticker", "trade", "orderbook_delta"],
                &WallClock,
            )
            .await?;
            let deadline = tokio::time::sleep(Duration::from_secs(seconds));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = &mut deadline => break,
                    frame = socket.next() => match frame {
                        Some(Ok(Message::Text(text))) => { let mut out = std::io::stdout().lock(); out.write_all(text.as_bytes())?; if !text.ends_with('\n') { out.write_all(b"\n")?; } }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => return Err(error.into()),
                        _ => {},
                    }
                }
            }
            socket.close(None).await?;
        }
        Command::Replay {
            venue: _,
            file,
            tickers,
            session,
            pace,
            verify,
            expect_book,
            expect_gaps,
            expect_file,
            digest_out,
        } => {
            let mut expected = Expected::default();
            if let Some(path) = &expect_file {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                expected = Expected::parse(&text)?;
            }
            for arg in &expect_book {
                expected.add_book_arg(arg)?;
            }
            if let Some(hash) = &expect_gaps {
                expected.set_gaps(hash)?;
            }
            let options = ReplayOptions {
                tickers: (!tickers.is_empty()).then_some(tickers),
                pace: match pace {
                    PaceArg::Realtime => Pace::Realtime,
                    PaceArg::Max => Pace::Max,
                },
                session,
            };
            let mut feed = ReplayFeed::open(&file, options)?;
            let clock = feed.clock();
            let mut store = BookStore::new(Venue::Kalshi, feed.tickers(), Arc::new(clock.clone()))?;
            let mut gaps = GapLog::default();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => bail!("replay interrupted"),
                    event = feed.next() => match event {
                        Some(event) => {
                            let applied = gaps.apply(&mut store, &event);
                            if let Applied::Gap { expected, got } = applied {
                                // The recording already holds the live resync that followed.
                                tracing::warn!(expected, got, "sequence gap");
                            }
                            tracing::debug!(?event, ?applied, "book update");
                        }
                        None => break,
                    },
                }
            }
            let metrics = feed.finish()?;
            print_book_table(&store, clock.now_ms());
            store.metrics.log(&store);
            tracing::info!(?metrics, "replay finished");
            let digest = emit_digest(&store, &gaps, digest_out.as_deref())?;
            if verify {
                let failures = expected.check(&digest);
                if !failures.is_empty() {
                    for failure in &failures {
                        eprintln!("verify FAILED {failure}");
                    }
                    bail!(
                        "replay verification failed ({} mismatch(es))",
                        failures.len()
                    );
                }
                eprintln!(
                    "verify OK: {} book hash(es) and gap log hash match",
                    digest.books.len()
                );
            }
        }
        Command::Markets { series, prod } => {
            for market in Rest::new(environment(prod), Arc::new(WallClock))?
                .series_markets(&series)
                .await?
            {
                println!("{}", serde_json::to_string(&market)?);
            }
        }
        Command::Registry {
            command:
                RegistryCommand::Validate {
                    config,
                    registry,
                    live,
                    prod,
                    refresh,
                    discover,
                    cache_dir,
                },
        } => {
            let (config, registry) = if discover {
                let config = Config::load(&config)?;
                let universe =
                    load_universe(&config, prod, refresh || live, None, cache_dir.as_deref())
                        .await?;
                let validation =
                    sum100::discovery::validate_discovered_universe(&universe, WallClock.now_ms());
                ensure!(
                    validation.is_valid(),
                    "discovered registry validation failed: {}",
                    render_discovery_validation_errors(&validation)
                );
                let registry = Registry::from_universe(&universe)?;
                (config, registry)
            } else {
                load_registry(&config, registry.as_deref())?
            };
            print_registry_summary(&registry);
            if registry.inferred_count() > 0 {
                println!(
                    "inferred   {} of {} group(s) came from venue metadata, not a human",
                    registry.inferred_count(),
                    registry.groups().len()
                );
            }

            if live && !discover {
                let kalshi = registry.tickers(Venue::Kalshi);
                ensure!(!kalshi.is_empty(), "registry defines no Kalshi contracts");
                let rest = Rest::new(environment(prod), Arc::new(WallClock))?;
                let cache = cache_dir
                    .as_deref()
                    .or(config.venues.kalshi.cache_dir.as_deref());
                let mut available = Vec::new();
                for series in series_of(kalshi) {
                    available.extend(cached_markets(&rest, &series, cache, refresh).await?);
                }
                let missing =
                    registry.validate_against_markets(Venue::Kalshi, available.as_slice());
                for ticker in &missing {
                    eprintln!("validate FAILED kalshi {ticker} is not a listed market");
                }
                ensure!(
                    missing.is_empty(),
                    "{} registry ticker(s) do not exist on Kalshi",
                    missing.len()
                );
                println!(
                    "validate OK: {} Kalshi ticker(s) confirmed against {} listed market(s)",
                    kalshi.len(),
                    available.len()
                );
            }

            let polymarket = registry.tickers(Venue::Polymarket).len();
            if polymarket > 0 {
                // Loading them keeps a cross-venue group visible and skipped for
                // a stated reason, rather than absent and unexplained.
                println!(
                    "note: {polymarket} Polymarket contract(s) load but are not checked or traded; the feed lands in phase 7"
                );
            }
            println!("registry OK");
        }
        Command::Registry {
            command:
                RegistryCommand::Discover {
                    config,
                    prod,
                    refresh,
                    live,
                    cache_dir,
                    series,
                    out,
                },
        } => {
            let config = Config::load(&config)?;
            let universe =
                load_universe(&config, prod, refresh || live, series, cache_dir.as_deref()).await?;
            let (groups, report) = infer_groups(&universe);

            let registry = Registry::from_universe(&universe)?;
            let output = discovery_output(&universe, &groups, &report, &registry);
            println!("{}", serde_json::to_string_pretty(&output)?);

            if let Some(path) = out {
                std::fs::write(&path, render_registry_toml(&registry))
                    .with_context(|| format!("writing {}", path.display()))?;
                eprintln!(
                    "wrote {} group(s) to {} (for reading and hand-promotion; the engine uses --auto-discover)",
                    registry.groups().len(),
                    path.display()
                );
            }
        }
        Command::Scan {
            live,
            replay,
            config,
            registry,
            prod,
            seconds,
            out,
            pace,
            session,
            signals_out,
            auto_discover,
            serve,
        } => {
            run_engine(EngineRun {
                serve,
                live,
                replay,
                config,
                registry,
                prod,
                seconds,
                out,
                pace,
                session,
                capital: None,
                daily_loss_limit: None,
                max_per_event: None,
                max_per_theme: None,
                live_orders: false,
                signals_out,
                auto_discover,
            })
            .await?;
        }
        Command::Trade {
            live,
            replay,
            config,
            registry,
            prod,
            seconds,
            out,
            pace,
            session,
            capital,
            daily_loss_limit,
            max_per_event,
            max_per_theme,
            // Paper is already the default; the flag only lets a command say so.
            paper_mode: _,
            live_orders,
            signals_out,
            auto_discover,
        } => {
            run_engine(EngineRun {
                live,
                replay,
                config,
                registry,
                prod,
                seconds,
                out,
                pace,
                session,
                capital,
                daily_loss_limit,
                max_per_event,
                max_per_theme,
                live_orders,
                signals_out,
                auto_discover,
                // `trade` has no dashboard flag to pass on.
                serve: None,
            })
            .await?;
        }
    }
    Ok(())
}

/// Fetch the Polymarket catalogue and fold it into the registry and the fees.
///
/// Returns how many markets were added. Runs before the book store is built,
/// because extending the registry is what assigns their contract ids.
async fn discover_polymarket(
    config: &Config,
    registry: &mut Registry,
    fees: &mut sum100::fees::FeeModels,
) -> Result<usize> {
    let base = config
        .venues
        .polymarket
        .metadata_url
        .as_deref()
        .context("venues.polymarket.metadata_url must be set to discover")?;
    let discovery = PolymarketDiscovery::new(base)?;
    let markets: Vec<_> = discovery
        .fetch_representable(config.discovery.max_pages.max(1))
        .await?;
    let added = registry.extend_with_polymarket(&markets)?;
    // After extending, so every token has an id to register against.
    let registered = register_polymarket_fees(&markets, registry.contracts(), &mut fees.polymarket);
    tracing::info!(
        discovered = markets.len(),
        added,
        registered,
        "polymarket catalogue read"
    );
    Ok(added)
}

/// Read events until the deadline, Ctrl-C, or the feed closing.
///
/// Recording only needs the bytes on disk, which the feed's worker writes
/// before an event ever reaches here, so this drives the stream and discards it.
async fn drain_until_deadline<F: Feed + ?Sized>(feed: &mut F, seconds: Option<u64>) {
    let deadline = async {
        match seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut deadline => break,
            event = feed.next() => match event {
                Some(event) => tracing::debug!(?event, "feed event"),
                None => break,
            },
        }
    }
}

/// Everything `scan` and `trade` share. They are the same loop; `trade` just
/// exposes the capital and risk knobs and the live-order opt-in.
struct EngineRun {
    live: bool,
    replay: Option<PathBuf>,
    config: PathBuf,
    registry: Option<PathBuf>,
    prod: bool,
    seconds: Option<u64>,
    out: PathBuf,
    pace: PaceArg,
    session: Option<usize>,
    capital: Option<i64>,
    daily_loss_limit: Option<i64>,
    max_per_event: Option<i64>,
    max_per_theme: Option<i64>,
    live_orders: bool,
    signals_out: Option<PathBuf>,
    auto_discover: bool,
    /// Where to serve the dashboard API, if anywhere.
    serve: Option<SocketAddr>,
}

async fn run_engine(args: EngineRun) -> Result<()> {
    ensure!(
        args.live ^ args.replay.is_some(),
        "pass exactly one of --live or --replay <FILE>"
    );
    let config_from_file = Config::load(&args.config)?;
    let auto_discover = args.auto_discover || config_from_file.discovery.auto_discover_on_startup;
    let (config, mut registry) = if auto_discover {
        let config = config_from_file;
        ensure!(
            config.discovery.enabled,
            "--auto-discover needs discovery.enabled = true in {}",
            args.config.display()
        );
        let universe = load_universe(&config, args.prod, false, None, None).await?;
        let registry = Registry::from_universe(&universe)?;
        tracing::info!(
            groups = registry.groups().len(),
            inferred = registry.inferred_count(),
            contracts = registry.tickers(Venue::Kalshi).len(),
            "registry inferred from the venue"
        );
        (config, registry)
    } else {
        load_registry(&args.config, args.registry.as_deref())?
    };
    // Polymarket discovery runs before anything reads the registry's ticker
    // lists: extending it assigns contract ids, and the book store has to
    // intern exactly the same identifiers in the same order.
    let mut fees = config.fee_models();
    if args.live && config.venues.polymarket.enabled {
        match discover_polymarket(&config, &mut registry, &mut fees).await {
            Ok(added) => tracing::info!(markets = added, "polymarket markets registered"),
            // A venue that cannot be reached is one this run does without. The
            // Kalshi side is unaffected, and a half-registered Polymarket would
            // leave books nothing ever feeds.
            Err(error) => {
                tracing::warn!(%error, "polymarket discovery failed; continuing without it")
            }
        }
    }

    let tickers = registry.tickers(Venue::Kalshi).to_vec();
    ensure!(!tickers.is_empty(), "registry defines no Kalshi contracts");
    let polymarket_tokens = registry.tickers(Venue::Polymarket).to_vec();

    let starting_capital = args.capital.unwrap_or(config.risk.starting_capital_cents);
    let daily_loss_limit = args
        .daily_loss_limit
        .unwrap_or(config.risk.max_daily_loss_cents);
    ensure!(starting_capital > 0, "--capital must be positive");
    ensure!(daily_loss_limit > 0, "--daily-loss-limit must be positive");
    let mut risk = config.risk.limits();
    if let Some(limit) = args.max_per_event {
        risk.max_per_event_cents = limit;
    }
    if let Some(limit) = args.max_per_theme {
        risk.max_per_theme_cents = limit;
    }

    // Three separate things must all be true before real money can move: the
    // flag, a live feed, and production. Paper is what happens otherwise.
    let live_orders = args.live_orders && !config.executor.paper_mode;
    if args.live_orders {
        ensure!(
            args.live,
            "--live-orders needs --live; a recording cannot place orders"
        );
        ensure!(args.prod, "--live-orders needs --prod");
        ensure!(
            !config.executor.paper_mode,
            "--live-orders needs executor.paper_mode = false in {}",
            args.config.display()
        );
        tracing::warn!(
            "LIVE ORDERS ENABLED: this run places real orders against production with real money"
        );
    }

    let engine_config = EngineConfig {
        solver: config.engine,
        risk,
        max_idle_ms: config.executor.max_idle_ms,
        atomic_timeout_ms: config.executor.atomic_timeout_ms,
        allow_live_orders: live_orders,
        allow_inferred_live_orders: config.discovery.allow_inferred_live_orders,
    };

    let (broadcast_tx, _) = broadcast::channel::<EngineState>(256);
    // Bound before the feed opens, which for a replay means decoding the whole
    // file: a dashboard that starts the engine watches for this port and would
    // otherwise give up while the recording was still being read.
    let dashboard = match args.serve {
        Some(address) => {
            let listener = TcpListener::bind(address)
                .await
                .with_context(|| format!("serving the dashboard API on {address}"))?;
            tracing::info!(%address, "dashboard API listening");
            Some(tokio::spawn(sum100::api::serve(
                listener,
                broadcast_tx.clone(),
            )))
        }
        None => None,
    };
    let mut states = broadcast_tx.subscribe();
    let drain = tokio::spawn(async move {
        let (mut received, mut lagged) = (0u64, 0u64);
        loop {
            match states.recv().await {
                Ok(_) => received += 1,
                Err(broadcast::error::RecvError::Lagged(skipped)) => lagged += skipped,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        (received, lagged)
    });

    let engine = match args.replay {
        Some(file) => {
            let options = ReplayOptions {
                tickers: Some(tickers.clone()),
                pace: match args.pace {
                    PaceArg::Realtime => Pace::Realtime,
                    PaceArg::Max => Pace::Max,
                },
                session: args.session,
            };
            let mut feed = ReplayFeed::open(&file, options)?;
            let clock: ReplayClock = feed.clock();
            let store = BookStore::new(Venue::Kalshi, feed.tickers(), Arc::new(clock.clone()))?;
            let portfolio = Portfolio::new(starting_capital, daily_loss_limit, clock.now_ms());
            let mut engine = Engine::new(
                registry,
                store,
                SignalLog::default(),
                fees.clone(),
                engine_config,
                Arc::new(clock),
                portfolio,
            );
            // A recording can never place a real order, whatever was asked for.
            let client = PaperOrderClient::new(fees);
            tokio::select! {
                _ = tokio::signal::ctrl_c() => bail!("run interrupted"),
                () = engine.run(&mut feed, &client, &broadcast_tx) => {},
            }
            let metrics = feed.finish()?;
            tracing::info!(?metrics, "replay finished");
            engine
        }
        None => {
            let env = environment(args.prod);
            let clock: Arc<dyn Clock> = Arc::new(WallClock);
            let dir = args.out.join(env.name());
            // Interned Kalshi first, then Polymarket, which is the order the
            // registry assigned ids in.
            // One interning order, shared by the store and every feed's parser.
            let subscriptions: [(Venue, &[String]); 2] = [
                (Venue::Kalshi, &tickers),
                (Venue::Polymarket, &polymarket_tokens),
            ];
            let store = BookStore::multi_venue(&subscriptions, clock.clone())?;
            let portfolio = Portfolio::new(starting_capital, daily_loss_limit, clock.now_ms());
            let client: Box<dyn OrderClient> = if live_orders {
                Box::new(KalshiOrderClient::new(env, clock.clone())?)
            } else {
                Box::new(PaperOrderClient::new(fees.clone()))
            };
            let mut venues: Vec<Box<dyn Feed>> = vec![Box::new(KalshiFeed::start(
                env,
                tickers.clone(),
                Recorder::new(&dir, Venue::Kalshi)?,
                clock.clone(),
            )?)];
            if !polymarket_tokens.is_empty() {
                // Its own recorder, so each venue's bytes land in its own daily
                // file and either can be replayed on its own.
                venues.push(Box::new(PolymarketFeed::start(
                    &subscriptions,
                    Recorder::new(&dir, Venue::Polymarket)?,
                    clock.clone(),
                )?));
            }
            tracing::info!(
                kalshi = tickers.len(),
                polymarket = polymarket_tokens.len(),
                "subscribing"
            );
            let mut feed = MergedFeed::new(venues);
            let mut engine = Engine::new(
                registry,
                store,
                SignalLog::default(),
                fees,
                engine_config,
                clock,
                portfolio,
            );
            let deadline = async {
                match args.seconds {
                    Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline);
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = &mut deadline => {},
                () = engine.run(&mut feed, client.as_ref(), &broadcast_tx) => {},
            }
            // Apply every event whose bytes were recorded, so the log describes
            // exactly the stream a replay will see. No orders here: the run is
            // over and any fill would be priced off a book nobody is watching.
            feed.stop();
            while let Some(event) = feed.next().await {
                engine.step(&event);
            }
            for (venue, metrics) in feed.metrics_by_venue() {
                tracing::info!(?venue, ?metrics, "live run finished");
            }
            engine
        }
    };

    // Before the sender is dropped: the server holds a clone of it, so the
    // drain below would never see the channel close while it is still alive.
    if let Some(task) = dashboard {
        task.abort();
        let _ = task.await;
    }
    drop(broadcast_tx);
    let (states_received, states_lagged) = drain.await?;
    print_scan_report(&engine, states_received, states_lagged);
    if let Some(path) = args.signals_out {
        std::fs::write(&path, serde_json::to_string_pretty(&engine.sink().signals)?)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "wrote {} signal(s) to {}",
            engine.sink().signals.len(),
            path.display()
        );
    }
    Ok(())
}

fn render_discovery_validation_errors(
    validation: &sum100::discovery::DiscoveryValidation,
) -> String {
    validation
        .structural_errors
        .iter()
        .chain(validation.closed_markets.iter())
        .chain(validation.ladder_errors.iter())
        .take(8)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ")
}

/// JSON intended for both humans and scripts.  The complete registry remains
/// in memory; the terminal output carries a bounded sample so a full Kalshi
/// universe does not turn one command into a multi-megabyte log line.
fn discovery_output(
    universe: &Universe,
    groups: &[sum100::discovery::InferredGroup],
    report: &sum100::discovery::InferenceReport,
    registry: &Registry,
) -> serde_json::Value {
    let sample = groups.iter().take(10).map(|group| {
        let title = universe
            .event(&group.event_ticker)
            .map(|event| event.title.clone())
            .unwrap_or_default();
        serde_json::json!({
            "event_id": group.event_ticker,
            "event_title": title,
            "relation": format!("{:?}", group.relation_kind).to_ascii_lowercase(),
            "members": group.tickers,
        })
    });
    serde_json::json!({
        "venue": "kalshi",
        "events": report.events,
        "markets": report.markets,
        "constraint_groups": groups.len(),
        "groups_by_type": {
            "binary_events": report.binary_events,
            "binary_complements": report.complements,
            "exhaustive_sets": report.exhaustive_sets,
            "ladders": report.ladders,
            "other_events": report
                .events
                .saturating_sub(report.binary_events)
                .saturating_sub(report.ladders),
        },
        "registry": {
            "events": registry.events().len(),
            "kalshi_contracts": registry.tickers(Venue::Kalshi).len(),
            "inferred_groups": registry.inferred_count(),
        },
        "sample_groups": sample.collect::<Vec<_>>(),
    })
}

/// Render an inferred registry in the file loader's own TOML shape.
///
/// This is for reading and for promoting groups by hand, not for the engine:
/// nothing loads it back automatically, because a file that silently became the
/// source of truth would hide the fact that a machine wrote it.
fn render_registry_toml(registry: &Registry) -> String {
    use std::fmt::Write as _;
    use sum100::registry::Relation;

    let ticker = |contract: sum100::types::ContractId| -> String {
        registry
            .binding(contract)
            .map(|b| b.venue_ticker.clone())
            .unwrap_or_default()
    };
    let members = |ids: &[sum100::types::ContractId]| -> String {
        ids.iter()
            .map(|id| {
                format!(
                    "  {{ venue = \"kalshi\", ticker = \"{}\" }},\n",
                    ticker(*id)
                )
            })
            .collect()
    };

    let mut out = String::from(
        "# Inferred from Kalshi metadata by `sum100 registry discover`.\n\
         # Read it, promote what you trust into config/registry.toml, and delete\n\
         # the rest. The engine does not load this file; it uses --auto-discover.\n",
    );
    for event in registry.events() {
        let groups: Vec<_> = registry
            .groups()
            .iter()
            .filter(|g| {
                g.members()
                    .first()
                    .and_then(|c| registry.binding(*c))
                    .is_some_and(|b| b.event == event.id)
            })
            .collect();
        if groups.is_empty() {
            continue;
        }
        let _ = write!(
            out,
            "\n[[event]]\nid = \"{}\"\ndescription = \"{}\"\nresolves_at = \"{}\"\nresolution_source = \"{}\"\n",
            event.key,
            event.description.replace('"', "'"),
            event.resolves_at.to_rfc3339(),
            event.resolution_source,
        );
        if let Some(theme) = &event.theme {
            let _ = writeln!(out, "theme = \"{theme}\"");
        }
        for group in groups {
            let (kind, ids) = match &group.relation {
                Relation::Complement { contract } => ("complement", vec![*contract]),
                Relation::Exhaustive { members } => ("exhaustive", members.clone()),
                Relation::Monotone { ordered } => ("monotone", ordered.clone()),
                Relation::Implies {
                    antecedent,
                    consequent,
                } => ("implies", vec![*antecedent, *consequent]),
                Relation::Equivalent { a, b, .. } => ("equivalent", vec![*a, *b]),
            };
            let _ = write!(
                out,
                "\n[[event.group]]\ntype = \"{kind}\"\nmembers = [\n{}]\n",
                members(&ids)
            );
        }
    }
    out
}

fn print_registry_summary(registry: &Registry) {
    use sum100::registry::Relation;
    let mut counts = [0usize; 5];
    for group in registry.groups() {
        let slot = match group.relation {
            Relation::Complement { .. } => 0,
            Relation::Exhaustive { .. } => 1,
            Relation::Monotone { .. } => 2,
            Relation::Implies { .. } => 3,
            Relation::Equivalent { .. } => 4,
        };
        counts[slot] += 1;
    }
    println!("events     {}", registry.events().len());
    for event in registry.events() {
        println!(
            "  {:<24} resolves {}  {}",
            event.key,
            event.resolves_at.to_rfc3339(),
            event.description
        );
    }
    println!(
        "groups     {} (complement {}, exhaustive {}, monotone {}, implies {}, equivalent {})",
        registry.groups().len(),
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4]
    );
    println!(
        "contracts  kalshi {}, polymarket {}",
        registry.tickers(Venue::Kalshi).len(),
        registry.tickers(Venue::Polymarket).len()
    );
}

fn print_scan_report<S: sum100::engine::OpportunitySink>(
    engine: &Engine<S>,
    states_received: u64,
    states_lagged: u64,
) {
    let engine_metrics = &engine.metrics;
    let solver = engine.solver_metrics();
    println!();
    println!("events received      {}", engine_metrics.events_received);
    println!("books updated        {}", engine_metrics.books_updated);
    println!("groups dirtied       {}", engine_metrics.groups_dirtied);
    println!("groups evaluated     {}", solver.groups_evaluated);
    println!("candidates found     {}", solver.candidates_found);
    println!("signals accepted     {}", solver.opportunities_emitted);
    println!("sequence gaps        {}", engine_metrics.sequence_gaps);
    println!("states broadcast     {states_received} (lagged {states_lagged})");
    let books = &engine.book_store().metrics;
    let (uninitialized, resyncing, live) =
        sum100::book::BookMetrics::books_by_state(engine.book_store());
    println!(
        "books                {live} live, {resyncing} resyncing, {uninitialized} uninitialized"
    );
    println!(
        "  snapshots {}, deltas {}, skipped {}, crossed seen {}",
        books.snapshots_applied,
        books.deltas_applied,
        books.deltas_skipped_not_live,
        books.crossed_books_observed
    );
    println!();
    println!("trades placed        {}", engine_metrics.trades_placed);
    println!("trades missed        {}", engine_metrics.trades_no_fill);
    if engine_metrics.trades_needing_reconciliation > 0 {
        println!(
            "NEEDS RECONCILIATION {} (legged or timed out; a position may be live)",
            engine_metrics.trades_needing_reconciliation
        );
    }
    for (label, count) in [
        ("  venue unhealthy", engine_metrics.blocked_unhealthy),
        ("  no capital", engine_metrics.blocked_no_capital),
        ("  risk limit", engine_metrics.blocked_risk_limit),
        (
            "  unbound contract",
            engine_metrics.blocked_unbound_contract,
        ),
    ] {
        if count > 0 {
            println!("{label:<22} {count}");
        }
    }
    let portfolio = engine.portfolio.summary();
    println!();
    println!(
        "capital              {} available, {} locked",
        portfolio.capital_available_cents, portfolio.capital_locked_cents
    );
    println!("open positions       {}", portfolio.open_positions);
    println!(
        "pnl                  {} realized, {} unrealized",
        portfolio.pnl_realized_cents, portfolio.pnl_unrealized_cents
    );
    println!(
        "daily loss           {} of {}",
        portfolio.daily_loss_cents, portfolio.daily_loss_limit_cents
    );
    println!();
    println!("rejections           {}", solver.rejections());
    for (label, count) in [
        ("  not live", solver.rejected_not_live),
        ("  stale", solver.rejected_stale),
        ("  missing book", solver.rejected_missing_book),
        ("  unverified", solver.rejected_unverified),
        ("  no depth", solver.rejected_no_depth),
        ("  fees exceed gap", solver.rejected_fees_exceed_gap),
        ("  below min edge", solver.rejected_below_min_edge),
        ("  below min return", solver.rejected_below_min_return),
        (
            "  payoff not guaranteed",
            solver.rejected_payoff_not_guaranteed,
        ),
    ] {
        if count > 0 {
            println!("{label:<22} {count}");
        }
    }
}
