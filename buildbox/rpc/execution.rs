use super::read_digest;
use super::ResponseStream;
use bytes::BytesMut;
use common::Error;
use prost::Message;
use proto::bazel::exec::{
    Action, ActionResult, Command, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, Execution, FileNode, OutputFile, SymlinkNode, WaitExecutionRequest,
};
use proto::google::{
    longrunning::{operation, Operation},
    protobuf::Any,
    rpc,
};
use sandbox::{
    DentryTemplate, DirTemplate, ExecCommand, FileTemplate, Sandbox, SandboxTemplate,
    SymlinkTemplate,
};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use storage::Storage;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct ExecutionService {
    storage: Storage,
    sandbox: Sandbox,
}

impl ExecutionService {
    /// Create new [`ExecutionService`] instance.
    #[must_use]
    pub fn new(storage: Storage, sandbox: Sandbox) -> Self {
        Self { storage, sandbox }
    }

    async fn execute(&self, req: &ExecuteRequest) -> Result<ExecuteResponse, Error> {
        let action = req
            .action_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing action digest"))
            .and_then(|digest| read_digest::<Action>(&self.storage, &digest))?;

        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing input root"))
            .and_then(|digest| read_digest::<Directory>(&self.storage, &digest))?;

        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing command digest"))
            .and_then(|digest| read_digest::<Command>(&self.storage, &digest))?;

        let template = self.build_sandbox_template(&input_root)?;
        let mut sandbox = self.sandbox.spawn(&template)?;
        sandbox.prepare()?;

        let cmd = ExecCommand {
            args: command.arguments,
            env: vec![],
            outputs: command.output_paths.clone(),
        };

        let res = sandbox.exec(&cmd)?;
        tracing::info!("Sandbox result: {res:?}");

        let output_files = res
            .outputs
            .iter()
            .map(|output| OutputFile {
                path: output.path.to_string_lossy().to_string(),
                digest: Some(output.digest.clone()),
                is_executable: false,
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let action_res = ActionResult {
            output_files: output_files,
            output_file_symlinks: vec![],
            output_symlinks: vec![],
            output_directories: vec![],
            output_directory_symlinks: vec![],
            exit_code: res.exit_code,
            stdout_raw: vec![],
            stdout_digest: Some(res.stdout),
            stderr_raw: vec![],
            stderr_digest: Some(res.stderr),
            execution_metadata: None,
        };

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
            message: "success".to_string(),
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

                actions.push(DentryTemplate::File(FileTemplate {
                    path: self.relative_path(&entry.path, &file.name),
                    digest: digest.clone(),
                    executable: file.is_executable,
                }));
            }

            for symlink in &entry.dir.symlinks {
                actions.push(DentryTemplate::Symlink(SymlinkTemplate {
                    path: self.relative_path(&entry.path, &symlink.name),
                    target: self.relative_path(&entry.path, &symlink.target),
                }));
            }

            for dir_node in &entry.dir.directories {
                let dir = dir_node
                    .digest
                    .as_ref()
                    .ok_or_else(|| Error::invalid("missing directory"))
                    .and_then(|digest| read_digest::<Directory>(&self.storage, &digest))?;

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
impl Execution for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        tracing::info!("Execution::execute {req:?}");
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
            name: "operations/fake-id".to_string(),
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
        todo!()
    }
}
