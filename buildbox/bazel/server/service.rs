//! The `Bazel` RPC service.

use crate::output::Output;
use crate::{BazelServer, Invocation};
use bazel_ui::Terminal;
use bazel_rust_connect::buildbox::bazel::v1::{
    Bazel, CancelRequest, CancelResponse, EngineMetrics, GetServerRequest, GetServerResponse, KindMetrics, MemoryMetrics, Restart, RunRequest,
    RunResponse, RunningCommand, ShutdownRequest, ShutdownResponse, WorkspaceInfo, run_response,
};
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
        if let Some(reason) = self.refusal(&request.build_id) {
            return Response::ok(restart(reason));
        }
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

    async fn cancel(&self, _ctx: RequestContext, request: ServiceRequest<'_, CancelRequest>) -> ServiceResult<CancelResponse> {
        let request = request.to_owned_message();
        let cancelled = self.server.commands().cancel(&request.invocation_id);
        Response::ok(CancelResponse {
            cancelled,
            ..Default::default()
        })
    }

    async fn get_server(&self, _ctx: RequestContext, _request: ServiceRequest<'_, GetServerRequest>) -> ServiceResult<GetServerResponse> {
        Response::ok(describe(&self.server))
    }

    async fn shutdown(&self, _ctx: RequestContext, request: ServiceRequest<'_, ShutdownRequest>) -> ServiceResult<ShutdownResponse> {
        let request = request.to_owned_message();
        tracing::info!(cancel_commands = request.cancel_commands, "asked to shut down");
        self.server.request_shutdown(request.cancel_commands);
        Response::ok(ShutdownResponse::default())
    }
}

impl BazelService {
    /// Why the server won't run a client's command, if it won't.
    fn refusal(&self, client_build: &str) -> Option<String> {
        if self.server.is_shutting_down() {
            return Some("the server is shutting down".to_string());
        }
        let server_build = crate::build_id().unwrap_or_default();
        if !client_build.is_empty() && !server_build.is_empty() && client_build != server_build {
            return Some(format!("the server is build {server_build}, the client {client_build}"));
        }
        None
    }
}

/// A stream telling the client to restart the server, for `reason`.
fn restart(reason: String) -> ServiceStream<RunResponse> {
    let restart = Restart {
        reason,
        ..Default::default()
    };
    let response = RunResponse {
        output: Some(run_response::Output::Restart(Box::new(restart))),
        ..Default::default()
    };
    Box::pin(futures::stream::iter([Ok(response)]))
}

/// The server, as `GetServer` describes it.
fn describe(server: &BazelServer) -> GetServerResponse {
    let commands = server
        .commands()
        .list()
        .iter()
        .map(|command| RunningCommand {
            invocation_id: command.id.clone(),
            args: command.args.clone(),
            working_directory: command.working_directory.display().to_string(),
            running_millis: command.started.elapsed().as_millis() as u64,
            ..Default::default()
        })
        .collect();
    let workspaces = server
        .workspaces()
        .roots()
        .into_iter()
        .map(|root| {
            let stats = server.workspaces().get(&root).graph.stats();
            WorkspaceInfo {
                root: root.display().to_string(),
                nodes: stats.nodes,
                node_bytes: stats.node_bytes,
                revision: stats.revision,
                ..Default::default()
            }
        })
        .collect();
    GetServerResponse {
        version: crate::VERSION.to_string(),
        build_id: crate::build_id().unwrap_or_default().to_string(),
        pid: std::process::id(),
        uptime_seconds: server.uptime().as_secs(),
        shutting_down: server.is_shutting_down(),
        commands,
        workspaces,
        engine: engine_metrics(&server.workspaces().engine().stats()).into(),
        memory: memory_metrics().into(),
        ..Default::default()
    }
}

fn engine_metrics(stats: &skyframe::EngineStats) -> EngineMetrics {
    let kinds = stats
        .kinds
        .iter()
        .map(|kind| KindMetrics {
            name: kind.name.clone(),
            computed: kind.computed,
            verified: kind.verified,
            reused: kind.reused,
            values: kind.values.values,
            raw_bytes: kind.values.raw_bytes,
            stored_bytes: kind.values.stored_bytes,
            compute_millis: kind.compute_time.as_millis() as u64,
            ..Default::default()
        })
        .collect();
    EngineMetrics {
        keys: stats.keys,
        key_bytes: stats.key_bytes,
        memo_keys: stats.memo_keys,
        memo_traces: stats.memo_traces,
        values: stats.store.values,
        live_values: stats.store.live_values,
        raw_bytes: stats.store.raw_bytes,
        stored_bytes: stats.store.stored_bytes,
        cache_entries: stats.cache.entries,
        cache_bytes: stats.cache.bytes,
        kinds,
        ..Default::default()
    }
}

fn memory_metrics() -> MemoryMetrics {
    let Some(stats) = crate::memory::allocator_stats() else {
        return MemoryMetrics::default();
    };
    MemoryMetrics {
        allocated_bytes: stats.allocated,
        active_bytes: stats.active,
        resident_bytes: stats.resident,
        mapped_bytes: stats.mapped,
        ..Default::default()
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
