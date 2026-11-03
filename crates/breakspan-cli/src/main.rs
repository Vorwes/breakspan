use breakspan_core::{Trace, compare_steps, render_tree};
use breakspan_otlp::{DEFAULT_MAX_RECORDING_BYTES, Recorder, RecordingOptions};
use breakspan_store::{MemoryStore, StoreLimits};
use clap::{Parser, Subcommand};
use std::{
    error::Error,
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
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
        /// Save raw OTLP exports, which may contain sensitive data.
        #[arg(long, value_name = "FILE")]
        record: Option<PathBuf>,
        /// Replace an existing recording instead of refusing to overwrite it.
        #[arg(long, requires = "record")]
        force: bool,
        /// Maximum recording size, including the header and frame lengths.
        #[arg(long, requires = "record", default_value_t = DEFAULT_MAX_RECORDING_BYTES, value_parser = clap::value_parser!(u64).range(8..))]
        max_recording_bytes: u64,
    },
    /// Load a recording and print its stored execution-tree snapshots once.
    Show {
        file: PathBuf,
        #[arg(long, default_value_t = DEFAULT_MAX_RECORDING_BYTES, value_parser = clap::value_parser!(u64).range(8..))]
        max_recording_bytes: u64,
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..))]
        max_spans: u32,
    },
    /// Compare two single-trace recordings and report their first divergent step.
    ///
    /// Exits 0 without divergence, 1 when the runs diverge and 2 on errors.
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Applies to each recording.
        #[arg(long, default_value_t = DEFAULT_MAX_RECORDING_BYTES, value_parser = clap::value_parser!(u64).range(8..))]
        max_recording_bytes: u64,
        /// Applies to each recording.
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..))]
        max_spans: u32,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Returning the error from main would print its Debug form instead of the
    // reviewed, telemetry-free Display text.
    run(Cli::parse()).await.unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        ExitCode::FAILURE
    })
}

async fn run(cli: Cli) -> CliResult<ExitCode> {
    match cli.command {
        Command::Listen {
            bind,
            max_spans,
            record,
            force,
            max_recording_bytes,
        } => {
            listen(
                bind,
                max_spans as usize,
                record,
                RecordingOptions {
                    force,
                    max_bytes: max_recording_bytes,
                },
            )
            .await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Show {
            file,
            max_recording_bytes,
            max_spans,
        } => show(file, max_recording_bytes, max_spans as usize).await,
        Command::Diff {
            a,
            b,
            max_recording_bytes,
            max_spans,
        } => {
            // Like diff(1), errors exit 2 because 1 means "the runs differ".
            Ok(diff(a, b, max_recording_bytes, max_spans as usize)
                .await
                .unwrap_or_else(|error| {
                    eprintln!("Error: {error}");
                    ExitCode::from(2)
                }))
        }
    }
}

async fn listen(
    bind: SocketAddr,
    max_spans: usize,
    record: Option<PathBuf>,
    recording_options: RecordingOptions,
) -> CliResult<()> {
    if !bind.ip().is_loopback() {
        return Err("Breakspan is local-only; --bind must use a loopback address".into());
    }
    let listener = TcpListener::bind(bind).await?;
    let recorder = match record {
        Some(path) => {
            eprintln!(
                "Warning: recording raw OTLP data without redaction; captures may contain sensitive prompts, tool arguments and credentials. Never commit captures."
            );
            Some(Recorder::create(path, recording_options).await?)
        }
        None => None,
    };
    let storage_notice = if recorder.is_some() {
        "raw OTLP recording enabled"
    } else {
        "traces are in memory only"
    };
    eprintln!(
        "Breakspan listening on {} (OTLP/gRPC). Ctrl-C stops; {storage_notice}.",
        listener.local_addr()?
    );
    let store = MemoryStore::new(StoreLimits {
        max_spans,
        ..Default::default()
    });
    let mut revisions = store.subscribe();
    let (stop, stopped) = oneshot::channel();
    let receiver_store = store.clone();
    let mut recording_monitor = recorder.clone();
    let mut server = tokio::spawn(breakspan_otlp::serve(
        listener,
        receiver_store,
        recorder,
        async {
            let _ = stopped.await;
        },
    ));
    let result = loop {
        tokio::select! {
            received = &mut server => return received?.map_err(Into::into),
            signal = tokio::signal::ctrl_c() => break signal.map_err(Into::into),
            error = async {
                match &recording_monitor {
                    Some(recorder) => recorder.failed().await,
                    None => std::future::pending().await,
                }
            } => {
                eprintln!("Warning: recording stopped: {error}; live ingestion continues.");
                recording_monitor = None;
            }
            update = revisions.changed() => {
                if update.is_err() { break Ok(()); }
                let snapshots = store.snapshots().await;
                if let Err(error) = print_snapshots(snapshots).await {
                    break Err(error);
                }
            }
        }
    };
    let _ = stop.send(());
    server.await??;
    result
}

async fn show(file: PathBuf, max_bytes: u64, max_spans: usize) -> CliResult<ExitCode> {
    let store = MemoryStore::new(StoreLimits {
        max_spans,
        ..Default::default()
    });
    let report = breakspan_otlp::load(file, store.clone(), max_bytes).await?;
    print_snapshots(store.snapshots().await).await?;
    if report.rejected_spans != 0 {
        eprintln!(
            "Warning: {} spans rejected while loading (invalid spans or memory limits).",
            report.rejected_spans
        );
    }
    if let Some(warning) = report.warning {
        eprintln!("Warning: showing only the valid recording prefix: {warning}");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

async fn diff(a: PathBuf, b: PathBuf, max_bytes: u64, max_spans: usize) -> CliResult<ExitCode> {
    let a = load_single_trace("a", a, max_bytes, max_spans).await?;
    let b = load_single_trace("b", b, max_bytes, max_spans).await?;
    let diverged = tokio::task::spawn_blocking(move || -> io::Result<bool> {
        let comparison = compare_steps(&a, &b);
        let stdout = io::stdout();
        let mut output = stdout.lock();
        writeln!(output, "{comparison}")?;
        output.flush()?;
        Ok(comparison.diverged())
    })
    .await??;
    Ok(ExitCode::from(u8::from(diverged)))
}

/// A partial load is an error: a truncated run must not look like a divergence.
async fn load_single_trace(
    side: &str,
    file: PathBuf,
    max_bytes: u64,
    max_spans: usize,
) -> CliResult<Trace> {
    let store = MemoryStore::new(StoreLimits {
        max_spans,
        ..Default::default()
    });
    let report = breakspan_otlp::load(file, store.clone(), max_bytes)
        .await
        .map_err(|error| format!("{side}: {error}"))?;
    if report.rejected_spans != 0 {
        eprintln!(
            "Warning: {side}: {} spans rejected while loading (invalid spans or memory limits).",
            report.rejected_spans
        );
    }
    if let Some(warning) = report.warning {
        return Err(format!("{side}: recording loaded only partially: {warning}").into());
    }
    match <[Trace; 1]>::try_from(store.snapshots().await) {
        Ok([trace]) => Ok(trace),
        Err(traces) => {
            Err(format!("{side}: expected exactly one trace, found {}", traces.len()).into())
        }
    }
}

async fn print_snapshots(snapshots: Vec<Trace>) -> CliResult<()> {
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        for trace in snapshots {
            writeln!(output, "{}", render_tree(&trace))?;
        }
        output.flush()
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_modifiers_require_recording() {
        assert!(Cli::try_parse_from(["breakspan", "listen", "--force"]).is_err());
        assert!(
            Cli::try_parse_from(["breakspan", "listen", "--max-recording-bytes", "128"]).is_err()
        );
    }

    #[test]
    fn limits_refuse_unusable_values() {
        for args in [
            vec!["breakspan", "listen", "--max-spans", "0"],
            vec!["breakspan", "show", "capture.bspan", "--max-spans", "0"],
            vec![
                "breakspan",
                "diff",
                "a.bspan",
                "b.bspan",
                "--max-spans",
                "0",
            ],
            vec!["breakspan", "diff", "a.bspan"],
            vec![
                "breakspan",
                "listen",
                "--record",
                "capture.bspan",
                "--max-recording-bytes",
                "7",
            ],
            vec![
                "breakspan",
                "show",
                "capture.bspan",
                "--max-recording-bytes",
                "7",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[tokio::test]
    async fn non_local_bind_is_refused() {
        assert!(
            listen(
                "0.0.0.0:4317".parse().unwrap(),
                1,
                None,
                RecordingOptions::default(),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("local-only")
        );
    }
}
