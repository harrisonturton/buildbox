//! `buildbox up`: run the server.

use crate::config_args::ConfigArgs;
use clap::Args;
use std::error::Error;
use telemetry::Telemetry;

#[derive(Args, Debug)]
pub struct UpArgs {
    #[command(flatten)]
    config: ConfigArgs,
}

pub async fn run(args: UpArgs) -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = write_profile_on_signal(dhat::Profiler::new_heap());
    #[cfg(feature = "dhat-heap")]
    tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let stats = dhat::HeapStats::get();
            eprintln!("dhat: {} bytes live in {} blocks", stats.curr_bytes, stats.curr_blocks);
        }
    });
    let config = args.config.load()?;
    bazel_server::configure_allocator();
    let telemetry = Telemetry::init(config.trace_file.as_deref())?;
    if let Some(path) = &config.trace_file {
        tracing::info!("writing traces to {}", path.display());
    }

    let result = server::run(&config).await;
    telemetry.shutdown().await;
    result?;
    Ok(())
}

/// Keep the heap profiler until the server gets SIGUSR2, then write its
/// profile, which records what's live at that moment as well as at the
/// heap's peak.
#[cfg(feature = "dhat-heap")]
fn write_profile_on_signal(profiler: dhat::Profiler) -> std::sync::Arc<std::sync::Mutex<Option<dhat::Profiler>>> {
    let profiler = std::sync::Arc::new(std::sync::Mutex::new(Some(profiler)));
    let held = profiler.clone();
    tokio::spawn(async move {
        let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2()) else {
            return;
        };
        signal.recv().await;
        let taken = held.lock().ok().and_then(|mut profiler| profiler.take());
        drop(taken);
        eprintln!("dhat: wrote the profile");
    });
    profiler
}
