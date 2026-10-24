//! `buildbox status`: summarise a running server.

use crate::client::ClientArgs;
use crate::format;
use admin_rust_connect::buildbox::admin::v1::StorageUsage;
use clap::Args;
use std::error::Error;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[command(flatten)]
    client: ClientArgs,
}

pub async fn run(args: StatusArgs) -> Result<(), Box<dyn Error>> {
    let admin = args.client.connect()?;
    let status = admin.get_status().await?;

    let uptime = format::duration(Duration::from_secs(status.uptime_seconds));
    println!("buildbox {}, up {uptime}", status.version);

    if let Some(cas) = status.cas.as_option() {
        println!("{:<14}{}", "CAS", storage_line(cas, "blobs"));
    }
    if let Some(action_cache) = status.action_cache.as_option() {
        println!("{:<14}{}", "Action cache", storage_line(action_cache, "entries"));
    }
    if let Some(execution) = status.execution.as_option() {
        println!(
            "{:<14}{} running, {} queued, limit {}",
            "Actions", execution.running_actions, execution.queued_actions, execution.max_concurrent_actions
        );
    }
    Ok(())
}

fn storage_line(usage: &StorageUsage, noun: &str) -> String {
    let summary = format!("{} {noun}, {}", usage.entries, format::bytes(usage.bytes));
    format!("{summary:<26}{}", usage.location)
}
