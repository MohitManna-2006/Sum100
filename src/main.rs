use anyhow::{Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use std::{io::Write, path::PathBuf, time::Duration};
use sum100::{
    book::{Applied, BookStore},
    feed::{
        Feed,
        kalshi::{self, Credentials, Environment, KalshiFeed},
        rest::Rest,
    },
    record::Recorder,
    types::{BookState, Venue},
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
            let mut feed = KalshiFeed::start(env, tickers, recorder)?;
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
        } => {
            ensure!(interval_ms > 0, "interval-ms must be positive");
            let env = environment(prod);
            let mut store = BookStore::new(Venue::Kalshi, &tickers)?;
            let recorder = Recorder::new(out.join(env.name()))?;
            let mut feed = KalshiFeed::start(env, tickers, recorder)?;
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
                    _ = tick.tick() => {
                        let now = u64::try_from(chrono::Utc::now().timestamp_millis())?;
                        print_book_table(&store, now);
                    }
                    event = feed.next() => match event {
                        Some(event) => {
                            let applied = store.apply(&event);
                            if let Applied::Gap { expected, got } = applied {
                                tracing::warn!(expected, got, "sequence gap; requesting resync");
                                store.note_resync_request();
                                feed.request_resync();
                            }
                            tracing::debug!(?event, ?applied, "book update");
                        }
                        None => break,
                    },
                }
            }
            let now = u64::try_from(chrono::Utc::now().timestamp_millis())?;
            print_book_table(&store, now);
            store.metrics.log(&store);
            let metrics = feed.shutdown().await?;
            tracing::info!(?metrics, "dump finished and gzip flushed");
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
        Command::Markets { series, prod } => {
            for market in Rest::new(environment(prod))?
                .series_markets(&series)
                .await?
            {
                println!("{}", serde_json::to_string(&market)?);
            }
        }
    }
    Ok(())
}
