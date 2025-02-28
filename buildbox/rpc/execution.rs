use storage::blob::LocalBlobStore;
use sandbox::exec::{ExecCommand, LocalExecService, PrepareAction};
use proto::google::{
    longrunning::{operation, Operation},
    rpc,
};
use proto::google::protobuf::Any;
use proto::bazel::exec::{
    Action, ActionResult, Command, Digest, Directory, DirectoryNode, ExecuteRequest, ExecuteResponse, Execution, FileNode, OutputFile, SymlinkNode, WaitExecutionRequest
};
use common::Error;

use super::ResponseStream;
use bytes::BytesMut;
use prost::Message;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use super::read_digest;

#[derive(Debug)]
pub struct ExecutionService {
    store: LocalBlobStore,
    exec: LocalExecService,
}

impl ExecutionService {
    /// Create new [`ExecutionService`] instance.
    #[must_use]
    pub fn new(store: LocalBlobStore, exec: LocalExecService) -> Self {
        Self { store, exec }
    }

    pub async fn execute(&self, req: &ExecuteRequest) -> Result<ExecuteResponse, Error> {
        let action = req
            .action_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing action digest"))
            .and_then(|digest| read_digest::<Action>(&self.store, &digest))?;

        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing input root"))
            .and_then(|digest| read_digest::<Directory>(&self.store, &digest))?;

        let prepare_actions = gather_sandbox_actions(&self.store, &input_root)?;
        let mut sandbox = self.exec.open_sandbox()?;
        sandbox.prepare(&prepare_actions)?;

        let command = action
            .command_digest
            .as_ref()
            .ok_or_else(|| Error::invalid("missing command digest"))
            .and_then(|digest| read_digest::<Command>(&self.store, &digest))?;

        let cmd = ExecCommand {
            args: command.arguments,
            env: vec![],
            outputs: command.output_paths.clone(),
        };
        sandbox.exec(&cmd)?;

        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        let digest = context.finish();
        let hash = data_encoding::HEXLOWER.encode(digest.as_ref());

        let mut output_files = vec![];
        for output_path in &command.output_paths {
            output_files.push(OutputFile {
                path: output_path.to_owned(),
                digest: Some(Digest {
                    hash: hash.clone(),
                    size_bytes: 0,
                }),
                is_executable: false,
                contents: vec![],
                node_properties: None,
            });
        }

        let action_res = ActionResult {
            output_files: output_files,
            output_file_symlinks: vec![],
            output_symlinks: vec![],
            output_directories: vec![],
            output_directory_symlinks: vec![],
            exit_code: 0,
            stdout_raw: vec![],
            stdout_digest: None,
            stderr_raw: vec![],
            stderr_digest: None,
            execution_metadata: None,
        };

        let status = rpc::Status {
            code: 0,
            message: "error message".to_string(),
            details: vec![],
        };

        let res = ExecuteResponse {
            result: Some(action_res),
            cached_result: false,
            status: Some(status),
            server_logs: HashMap::new(),
            message: "Optional message".to_string(),
        };

        Ok(res)
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
            // name ending with operations/{unique_id}.
            name: "exec".to_string(),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(proto_any)),
        };

        tx.send(Ok(op))
            .await
            .map_err(|err| Status::internal("failed to execute"))
            .unwrap();

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

// Walk the proto-defined file tree and construct a set of copy actions
// that the exec service will use to populate the sandbox.
fn gather_sandbox_actions(
    store: &LocalBlobStore,
    input_root: &Directory,
) -> Result<Vec<PrepareAction>, Error> {
    let mut next_dirs = VecDeque::new();
    let mut prepare_actions = vec![];

    next_dirs.push_back((PathBuf::new(), input_root.clone()));

    while let Some((path, dir)) = next_dirs.pop_front() {
        prepare_actions.push(PrepareAction::CreateDir { path: path.clone() });

        for file in &dir.files {
            let digest = file
                .digest
                .as_ref()
                .ok_or_else(|| Error::invalid("missing file"))?;

            let mut path = path.clone();
            path.push(file.name.to_owned());

            prepare_actions.push(PrepareAction::CreateFile {
                path,
                sha256: digest.hash.to_owned(),
                executable: file.is_executable,
            });
        }

        for symlink in &dir.symlinks {
            let mut path = path.clone();
            path.push(symlink.name.to_owned());

            let target = PathBuf::from_str(&symlink.target)
                .map_err(Error::boxed_msg("could not create path for symlink"))?;

            prepare_actions.push(PrepareAction::CreateSymlink { path, target });
        }

        for dir_node in &dir.directories {
            let mut path = path.clone();
            path.push(dir_node.name.to_owned());

            let dir = dir_node
                .digest
                .as_ref()
                .ok_or_else(|| Error::invalid("missing directory"))
                .and_then(|digest| read_digest::<Directory>(store, &digest))?;

            next_dirs.push_back((path.clone(), dir.clone()));
        }
    }

    Ok(prepare_actions)
}