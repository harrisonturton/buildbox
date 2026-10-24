//! The `Bazel` RPC service.

use crate::output::Output;
use crate::{BazelServer, Invocation};
use bazel_ui::Terminal;
use bazel_rust_connect::buildbox::bazel::v1::{Bazel, RunRequest, RunResponse, run_response};
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use futures::Stream;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How many output messages may wait for a slow client before the command
/// waits for it.
const OUTPUT_BUFFER: usize = 256;

/// Runs Bazel commands for clients.
pub struct BazelService {
    server: Arc<BazelServer>,
}

impl BazelService {
    pub fn new(server: Arc<BazelServer>) -> BazelService {
        BazelService { server }
    }
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl Bazel for BazelService {
    async fn run(&self, _ctx: RequestContext, request: ServiceRequest<'_, RunRequest>) -> ServiceResult<ServiceStream<RunResponse>> {
        let request = request.to_owned_message();
        let invocation = Invocation {
            args: request.args,
            working_directory: PathBuf::from(request.working_directory),
            env: request.env.into_iter().map(|var| (var.name, var.value)).collect(),
        };
        let terminal = Terminal {
            interactive: request.interactive,
            columns: request.terminal_columns,
            term: invocation.env.get("TERM").cloned(),
        };
        let (sender, receiver) = mpsc::channel(OUTPUT_BUFFER);
        let server = self.server.clone();
        let task = tokio::spawn(async move {
            let out = Output::new(sender.clone(), terminal);
            let code = server.run(invocation, &out).await;
            out.finish().await;
            drop(out);
            let done = RunResponse {
                output: Some(run_response::Output::ExitCode(code)),
                ..Default::default()
            };
            let _ = sender.send(done).await;
        });
        let stream: ServiceStream<RunResponse> = Box::pin(CommandStream { receiver, task });
        Response::ok(stream)
    }
}

/// A command's output. Dropping it, as happens when the client goes away,
/// stops the command.
struct CommandStream {
    receiver: mpsc::Receiver<RunResponse>,
    task: JoinHandle<()>,
}

impl Stream for CommandStream {
    type Item = Result<RunResponse, connectrpc::ConnectError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx).map(|message| message.map(Ok))
    }
}

impl Drop for CommandStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
