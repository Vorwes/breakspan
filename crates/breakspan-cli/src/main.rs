use breakspan_core::render_tree;
use breakspan_store::{MemoryStore, StoreLimits};
use clap::{Parser, Subcommand};
use std::{
    error::Error,
    io::{self, Write},
    net::SocketAddr,
};
use tokio::{net::TcpListener, sync::oneshot};

type CliResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Parser)]
#[command(
    name = "breakspan",
    version,
    about = "Find where your agent went off track."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Receive OTLP/gRPC traces and print provisional execution trees.
    Listen {
        #[arg(long, default_value = "127.0.0.1:4317")]
        bind: SocketAddr,
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..))]
        max_spans: u32,
    },
}

#[tokio::main]
async fn main() -> CliResult<()> {
    match Cli::parse().command {
        Command::Listen { bind, max_spans } => listen(bind, max_spans as usize).await,
    }
}

async fn listen(bind: SocketAddr, max_spans: usize) -> CliResult<()> {
    if !bind.ip().is_loopback() {
        return Err("Breakspan is local-only; --bind must use a loopback address".into());
    }
    let listener = TcpListener::bind(bind).await?;
    eprintln!(
        "Breakspan listening on {} (OTLP/gRPC). Ctrl-C stops; traces are in memory only.",
        listener.local_addr()?
    );
    let store = MemoryStore::new(StoreLimits {
        max_spans,
        ..Default::default()
    });
    let mut revisions = store.subscribe();
    let (stop, stopped) = oneshot::channel();
    let receiver_store = store.clone();
    let mut server = tokio::spawn(breakspan_otlp::serve(listener, receiver_store, async {
        let _ = stopped.await;
    }));
    let result = loop {
        tokio::select! {
            received = &mut server => return received?.map_err(Into::into),
            signal = tokio::signal::ctrl_c() => break signal.map_err(Into::into),
            update = revisions.changed() => {
                if update.is_err() { break Ok(()); }
                let snapshots = store.snapshots().await;
                let printed = tokio::task::spawn_blocking(move || -> io::Result<()> {
                    let stdout = io::stdout();
                    let mut output = stdout.lock();
                    for trace in snapshots { writeln!(output, "{}", render_tree(&trace))?; }
                    output.flush()
                }).await;
                match printed {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => break Err(error.into()),
                    Err(error) => break Err(error.into()),
                }
            }
        }
    };
    let _ = stop.send(());
    server.await??;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_limits_are_explicit() {
        match Cli::try_parse_from(["breakspan", "listen"])
            .unwrap()
            .command
        {
            Command::Listen { bind, max_spans } => {
                assert_eq!(bind.to_string(), "127.0.0.1:4317");
                assert_eq!(max_spans, 10_000);
            }
        }
        assert!(Cli::try_parse_from(["breakspan", "listen", "--max-spans", "0"]).is_err());
    }

    #[tokio::test]
    async fn non_local_bind_is_refused() {
        assert!(
            listen("0.0.0.0:4317".parse().unwrap(), 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("local-only")
        );
    }
}
