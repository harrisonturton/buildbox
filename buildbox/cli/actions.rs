//! `bb server actions`: list the actions a running server is handling.

use crate::client::ClientArgs;
use crate::format;
use admin_rust_connect::buildbox::admin::v1::{ActionInfo, ActionState};
use clap::Args;
use std::error::Error;
use std::time::Duration;

/// How much of each command line to show.
const MAX_COMMAND_WIDTH: usize = 100;

#[derive(Args, Debug)]
pub struct ActionsArgs {
    #[command(flatten)]
    client: ClientArgs,
}

pub async fn run(args: ActionsArgs) -> Result<(), Box<dyn Error>> {
    let admin = args.client.connect()?;
    let response = admin.list_actions().await?;

    if response.actions.is_empty() {
        println!("No actions are queued or running.");
        return Ok(());
    }

    println!("{:<8}{:<9}{:<10}{:<14}COMMAND", "ID", "STATE", "TIME", "ENVIRONMENT");
    for action in &response.actions {
        println!("{}", action_line(action));
    }
    Ok(())
}

fn action_line(action: &ActionInfo) -> String {
    let (state, millis) = match action.state.as_known() {
        Some(ActionState::ACTION_STATE_RUNNING) => ("running", action.running_millis),
        Some(ActionState::ACTION_STATE_QUEUED) => ("queued", action.queued_millis),
        Some(ActionState::ACTION_STATE_UNSPECIFIED) | None => ("unknown", action.queued_millis),
    };
    let time = format::duration(Duration::from_millis(millis));
    let command = format::truncate(&action.arguments.join(" "), MAX_COMMAND_WIDTH);
    let environment = format::truncate(&action.environment, 13);
    format!("{:<8}{state:<9}{time:<10}{environment:<14}{command}", action.id)
}
