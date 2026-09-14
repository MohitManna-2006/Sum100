use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use sum100::{
    book::{Applied, BookStore},
    clock::{Clock, WallClock},
    feed::{
        Feed,
        kalshi::{self, Credentials, Environment, KalshiFeed},
        replay::{Pace, ReplayFeed, ReplayOptions},
        rest::Rest,
    },
    record::Recorder,
    types::{BookState, Venue},
    verify::{Expected, GapLog, StateDigest},
};
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
            venue: _,
            prod,
            tickers,
            out,
            seconds,
        } => {
            let env = environment(prod);
            let recorder = Recorder::new(out.join(env.name()))?;
            let mut feed = KalshiFeed::start(env, tickers, recorder, Arc::new(WallClock))?;
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
                    event = feed.next() => match event { Some(event) => tracing::debug!(?event, "feed event"), None => break },
                }
            }
            let metrics = feed.shutdown().await?;
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
            let recorder = Recorder::new(out.join(env.name()))?;
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
    }
    Ok(())
}
