use super::ResponseStream;
use bytes::BytesMut;
use common::Error;
use executor::{
    DentryTemplate, DirTemplate, ExecCommand, Executor, FileTemplate, SandboxHandle,
    SandboxTemplate, SymlinkTemplate,
};
use prost::Message;
use proto::bazel::exec::{
    Action, ActionResult, Command, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, ExecutedActionMetadata, Execution, FileNode, OutputFile, SymlinkNode,
    WaitExecutionRequest,
};
use proto::google::{
    longrunning::{operation, Operation},
    protobuf::Any,
    rpc,
};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use storage::{ProtoStoreExt, Store};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct ExecutionService<S, E> {
    store: S,
    executor: E,
}

impl<S, E> ExecutionService<S, E>
where
    S: Store + 'static,
    E: Executor + 'static,
{
    /// Create new [`ExecutionService`] instance.
    #[must_use]
    pub fn new(store: S, executor: E) -> Self {
        Self { store, executor }
    }

    async fn execute(&self, req: &ExecuteRequest) -> Result<ExecuteResponse, Error> {
        tracing::trace!("{req:#?}");

        let action = req
            .action_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing action digest"))
            .and_then(|digest| self.store.read_message::<Action>(&digest))?;

        tracing::trace!("{action:#?}");

        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing input root"))
            .and_then(|digest| self.store.read_message::<Directory>(&digest))?;

        tracing::trace!("{input_root:#?}");

        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing command digest"))
            .and_then(|digest| self.store.read_message::<Command>(&digest))?;

        tracing::error!("COMMAND: {command:#?}");

        let template = self.build_sandbox_template(&input_root)?;
        let mut sandbox = self.executor.spawn(&template)?;
        sandbox.prepare()?;

        let mut env = HashMap::new();
        for var in &command.environment_variables {
            env.insert(var.name.clone(), var.value.clone());
        }

        tracing::info!("workdir {:?}", command.working_directory);

        // output_paths is only implemented from v2.1
        if !command.output_paths.is_empty() {
            tracing::error!("received command with populated output_paths");
            std::process::exit(1);
        }

        let cmd = ExecCommand {
            env,
            args: command.arguments.clone(),
            output_files: command.output_files.clone(),
            output_dirs: command.output_directories.clone(),
        };

        let res = sandbox.exec(&cmd)?;

        tracing::info!("sandbox command finished with exit_code={}", res.exit_code);
        tracing::info!("found outputs files in sandbox: {:?}", res.output_files);
        tracing::info!("found outputs dirs in sandbox: {:?}", res.output_dirs);

        if res.exit_code == 1 {
            tracing::error!("failed input root: {input_root:?}");
            // tracing::error!("failed sandbox template: {template:#?}");
            tracing::error!("failed command: {command:#?}");
        }

        let output_files = res
            .output_files
            .iter()
            .map(|output| OutputFile {
                path: output.path.to_string_lossy().to_string(),
                digest: Some(output.digest.clone()),
                is_executable: true,
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let action_res = ActionResult {
            output_files: output_files,
            output_file_symlinks: vec![],
            output_symlinks: vec![],
            output_directories: res.output_dirs,
            output_directory_symlinks: vec![],
            exit_code: res.exit_code,
            stdout_raw: vec![],
            stdout_digest: Some(res.stdout),
            stderr_raw: vec![],
            stderr_digest: Some(res.stderr),
            execution_metadata: Some(ExecutedActionMetadata {
                worker: "buildbox".to_string(),
                ..Default::default()
            }),
        };

        tracing::info!("action result: {action_res:#?}");

        let status = rpc::Status {
            code: 0,
            message: "succcess".to_string(),
            details: vec![],
        };

        let res = ExecuteResponse {
            result: Some(action_res),
            cached_result: false,
            status: Some(status),
            server_logs: HashMap::new(),
            message: "exec response".to_string(),
        };

        Ok(res)
    }

    fn build_sandbox_template(&self, input_root: &Directory) -> Result<SandboxTemplate, Error> {
        struct DirEntry {
            path: PathBuf,
            dir: Directory,
        };

        let mut next = VecDeque::new();
        let mut actions = vec![];

        next.push_back(DirEntry {
            path: PathBuf::new(),
            dir: input_root.clone(),
        });

        while let Some(entry) = next.pop_front() {
            actions.push(DentryTemplate::Dir(DirTemplate {
                path: entry.path.clone(),
            }));

            for file in &entry.dir.files {
                let digest = file
                    .digest
                    .as_ref()
                    .ok_or_else(|| Error::invalid("missing file"))?;

                // tracing::info!("FILE: {:?}", &file.name);

                actions.push(DentryTemplate::File(FileTemplate {
                    path: self.relative_path(&entry.path, &file.name),
                    digest: digest.clone(),
                    executable: file.is_executable,
                }));
            }

            for symlink in &entry.dir.symlinks {
                // tracing::info!("SYMLINK: {:?}", &symlink.name);
                actions.push(DentryTemplate::Symlink(SymlinkTemplate {
                    path: self.relative_path(&entry.path, &symlink.name),
                    target: self.relative_path(&entry.path, &symlink.target),
                }));
            }

            for dir_node in &entry.dir.directories {
                // tracing::info!("DIR: {:?}", &dir_node.name);

                let dir = dir_node
                    .digest
                    .as_ref()
                    .ok_or_else(|| Error::invalid("missing directory"))
                    .and_then(|digest| self.store.read_message::<Directory>(&digest))?;

                // tracing::info!("{dir:?}");

                next.push_back(DirEntry {
                    path: self.relative_path(&entry.path, &dir_node.name),
                    dir,
                });
            }
        }

        Ok(SandboxTemplate {
            filesystem: actions,
        })
    }

    fn relative_path(&self, root: &PathBuf, child: &str) -> PathBuf {
        let mut path = root.clone();
        path.push(child.to_owned());
        path
    }
}

#[async_trait::async_trait]
impl<S, E> Execution for ExecutionService<S, E>
where
    S: Store + 'static,
    E: Executor + 'static,
{
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = req.into_inner();

        let (tx, rx) = mpsc::channel(1);

        let res = self
            .execute(&req)
            .await
            .map_err(|err| Status::internal("failed to execute"))?;

        let any = prost_types::Any::from_msg(&res)
            .map_err(|err| Status::internal("failed to map to any type"))?;

        let proto_any = Any {
            type_url: any.type_url,
            value: any.value,
        };

        let op = Operation {
            // TODO: Build directory of operations to allow awaiting them later.
            name: "operations/new-id".to_string(),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(proto_any)),
        };

        tx.send(Ok(op))
            .await
            .map_err(|err| Status::internal("failed to send execute response"))?;

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        req: tonic::Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        tracing::error!("unimplemented endpoint wait_execution invoked, stopping");
        std::process::exit(1)
    }
}
