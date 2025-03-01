use storage::Storage;
pub use proto::bazel::exec::{
    Action, ActionCache, ActionResult, Command, Digest, GetActionResultRequest, OutputFile, UpdateActionResultRequest
};
use tonic::{Request, Response, Status};
use common::Error;
use super::read_digest;

#[derive(Debug)]
pub struct ActionCacheService {
    store: Storage,
}

impl ActionCacheService {
    pub fn new(store: Storage) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ActionCache for ActionCacheService {

    async fn get_action_result(
        &self,
        req: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = req.into_inner();
        let hash = req.action_digest.clone().unwrap().hash;
        tracing::info!("ActionCache::get_action_result {req:?}");

        // let action = req
        //     .action_digest
        //     .clone()
        //     .as_ref()
        //     .ok_or_else(|| Error::invalid("missing action digest"))
        //     .and_then(|digest| read_digest::<Action>(&self.store, &digest))
        //     .map_err(|err| Status::internal("failed to read action"))?;

        // let command = action
        //     .command_digest
        //     .as_ref()
        //     .ok_or_else(|| Error::invalid("missing command digest"))
        //     .and_then(|digest| read_digest::<Command>(&self.store, &digest))
        //     .map_err(|err| Status::internal("failed to read command"))?;

        // // When we return a value here, the reset of the execution API is not invoked.
        // if hash == "9898f9576b5f7bdbb95c2899f203b69153628a1144e5b8d75c74d259b89bed57" || hash == "11661c6f121e46dbef42f9ca8d595fd2129f5d45e51fbe8b9b2ba15a23689979" {
        //     tracing::info!("GOT KNOWN HASH");
        //     let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        //     let digest = context.finish();
        //     let hash = data_encoding::HEXLOWER.encode(digest.as_ref());

        //     let mut output_files = vec![];
        //     for output_path in &command.output_paths {
        //         output_files.push(OutputFile {
        //             path: output_path.to_owned(),
        //             digest: Some(Digest {
        //                 hash: hash.clone(),
        //                 size_bytes: 0,
        //             }),
        //             is_executable: false,
        //             contents: vec![],
        //             node_properties: None,
        //         });
        //     }

        //     return Ok(Response::new(ActionResult {
        //         output_files: output_files,
        //         output_file_symlinks: vec![],
        //         output_symlinks: vec![],
        //         output_directories: vec![],
        //         output_directory_symlinks: vec![],
        //         exit_code: 0,
        //         stdout_raw: vec![],
        //         stdout_digest: None,
        //         stderr_raw: vec![],
        //         stderr_digest: None,
        //         execution_metadata: None,
        //     }));
        // }

        Err(Status::not_found("action not found"))
    }

    async fn update_action_result(
        &self,
        req: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = req.into_inner();
        let hash = req.action_digest.unwrap().hash;
        tracing::info!("ActionCache::update_action_result {hash}");
        Ok(Response::new(ActionResult::default()))
    }
}
