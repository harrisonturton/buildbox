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
    let _profiler = dhat::Profiler::new_heap();
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
