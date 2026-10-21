//! `buildbox actions`: list the actions a running server is handling.

use crate::client::ClientArgs;
use crate::format;
use admin_proto::buildbox::admin::v1::{ActionInfo, ActionState, ListActionsRequest};
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
    let mut client = args.client.connect().await?;
    let response = client.list_actions(ListActionsRequest {}).await?.into_inner();

    if response.actions.is_empty() {
        println!("No actions are queued or running.");
        return Ok(());
    }

    println!("{:<8}{:<9}{:<10}COMMAND", "ID", "STATE", "TIME");
    for action in &response.actions {
        println!("{}", action_line(action));
    }
    Ok(())
}

fn action_line(action: &ActionInfo) -> String {
    let (state, millis) = match action.state() {
        ActionState::Running => ("running", action.running_millis),
        ActionState::Queued => ("queued", action.queued_millis),
        ActionState::Unspecified => ("unknown", action.queued_millis),
    };
    let time = format::duration(Duration::from_millis(millis));
    let command = format::truncate(&action.arguments.join(" "), MAX_COMMAND_WIDTH);
    format!("{:<8}{state:<9}{time:<10}{command}", action.id)
}
