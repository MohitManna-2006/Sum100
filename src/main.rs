use anyhow::{Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use std::{io::Write, path::PathBuf, time::Duration};
use sum100::{
    feed::{
        Feed,
        kalshi::{self, Credentials, Environment, KalshiFeed},
        rest::Rest,
    },
    record::Recorder,
};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
#[command(version, about = "Read-only prediction-market feed and raw recorder")]
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
