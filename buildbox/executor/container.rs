//! A [`Runner`] that runs commands in containers, made from a container
//! image in the artifact store.

use artifacts::{ArtifactError, ArtifactStore};
use async_trait::async_trait;
use container::{ContainerError, ContainerRuntime, ContainerSpec, Mount, Network};
use executor::ExecuteError;
use executor_sandboxed::{ProcessOutput, RunRequest, Runner};
use std::path::Path;
use std::sync::Arc;

/// Where the sandbox is mounted in the container. It is the same for every
/// action, so paths in outputs don't depend on where buildbox keeps
/// sandboxes.
const WORKSPACE: &str = "/workspace";

/// Runs each command in a new container, whose root is the image, with the
/// sandbox mounted at [`WORKSPACE`].
pub struct ContainerRunner {
    artifacts: Arc<dyn ArtifactStore>,
    image: String,
    network: Network,
    runtime: Arc<dyn ContainerRuntime>,
}

impl ContainerRunner {
    /// Create a runner for containers from the image called `image` in
    /// `artifacts`. The image is looked up for each command, so it can be
    /// imported or replaced while the server runs.
    pub fn new(
        artifacts: Arc<dyn ArtifactStore>,
        image: String,
        network: Network,
        runtime: Arc<dyn ContainerRuntime>,
    ) -> ContainerRunner {
        ContainerRunner {
            artifacts,
            image,
            network,
            runtime,
        }
    }
}

#[async_trait]
impl Runner for ContainerRunner {
    async fn run(&self, request: &RunRequest<'_>) -> Result<ProcessOutput, ExecuteError> {
        let command = request.command;
        if command.arguments.is_empty() {
            return Err(ExecuteError::InvalidAction("command has no arguments".to_string()));
        }

        // Held until the command finishes, so the image isn't deleted under it.
        let image = self.artifacts.container_image(&self.image).await.map_err(|error| self.image_error(error))?;

        // The command's variables override the image's.
        let mut env = image.env.clone();
        for variable in &command.environment_variables {
            env.retain(|(name, _)| *name != variable.name);
            env.push((variable.name.clone(), variable.value.clone()));
        }

        let spec = ContainerSpec {
            rootfs: image.rootfs.clone(),
            mounts: vec![Mount {
                source: request.sandbox.to_path_buf(),
                target: WORKSPACE.into(),
            }],
            args: command.arguments.clone(),
            env,
            working_dir: Path::new(WORKSPACE).join(request.working_dir),
            network: self.network,
        };
        let output = self.runtime.run(&spec, request.timeout).await;
        drop(image);
        output.map_err(execute_error)
    }
}

impl ContainerRunner {
    fn image_error(&self, error: ArtifactError) -> ExecuteError {
        let message = match error {
            ArtifactError::NotFound(_) => format!(
                "the container image {:?} isn't in the artifact store; import it with `bb artifacts import-image`",
                self.image
            ),
            error => format!("failed to open the container image {:?}: {error}", self.image),
        };
        ExecuteError::Internal(message)
    }
}

fn execute_error(error: ContainerError) -> ExecuteError {
    match error {
        ContainerError::Timeout(timeout) => ExecuteError::Timeout(timeout),
        // The program is part of the action, so failing to start it is the
        // action's fault.
        ContainerError::Start(message) => ExecuteError::InvalidAction(message),
        error @ (ContainerError::Setup(_) | ContainerError::Internal(_)) => ExecuteError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifacts::{ArtifactInfo, ContainerImage, Usage};
    use cas::Digest;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::command::EnvironmentVariable;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::Command;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Has one image, "image", with PATH and HOME set.
    struct OneImage;

    #[async_trait]
    impl ArtifactStore for OneImage {
        async fn import_container_image(&self, _: &str, _: &Digest) -> Result<ArtifactInfo, ArtifactError> {
            unimplemented!()
        }

        async fn list(&self) -> Result<Vec<ArtifactInfo>, ArtifactError> {
            unimplemented!()
        }

        async fn delete(&self, _: &str) -> Result<(), ArtifactError> {
            unimplemented!()
        }

        async fn container_image(&self, name: &str) -> Result<ContainerImage, ArtifactError> {
            if name != "image" {
                return Err(ArtifactError::NotFound(name.to_string()));
            }
            Ok(ContainerImage {
                digest: Digest::of_bytes(b"manifest"),
                rootfs: PathBuf::from("/images/image/rootfs"),
                env: vec![
                    ("PATH".to_string(), "/usr/bin".to_string()),
                    ("HOME".to_string(), "/root".to_string()),
                ],
                lease: Box::new(()),
            })
        }

        async fn usage(&self) -> Result<Usage, ArtifactError> {
            unimplemented!()
        }
    }

    /// Records the spec it is asked to run, and returns `result`.
    struct RecordingRuntime {
        specs: Mutex<Vec<ContainerSpec>>,
        result: fn() -> Result<ProcessOutput, ContainerError>,
    }

    #[async_trait]
    impl ContainerRuntime for RecordingRuntime {
        async fn run(&self, spec: &ContainerSpec, _timeout: Option<Duration>) -> Result<ProcessOutput, ContainerError> {
            self.specs.lock().unwrap().push(spec.clone());
            (self.result)()
        }
    }

    fn exits_zero() -> Result<ProcessOutput, ContainerError> {
        Ok(ProcessOutput {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
        })
    }

    fn runner(image: &str, result: fn() -> Result<ProcessOutput, ContainerError>) -> (ContainerRunner, Arc<RecordingRuntime>) {
        let runtime = Arc::new(RecordingRuntime {
            specs: Mutex::new(vec![]),
            result,
        });
        let runner = ContainerRunner::new(Arc::new(OneImage), image.to_string(), Network::Isolated, runtime.clone());
        (runner, runtime)
    }

    fn command() -> Command {
        Command {
            arguments: vec!["cc".to_string(), "-c".to_string(), "a.c".to_string()],
            environment_variables: vec![EnvironmentVariable {
                name: "PATH".to_string(),
                value: "/opt/bin".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    async fn run(runner: &ContainerRunner, command: &Command) -> Result<ProcessOutput, ExecuteError> {
        let request = RunRequest {
            command,
            sandbox: Path::new("/sandboxes/action-1"),
            working_dir: Path::new("pkg"),
            timeout: None,
        };
        runner.run(&request).await
    }

    #[tokio::test]
    async fn runs_commands_in_the_image_with_the_sandbox_mounted() {
        let (runner, runtime) = runner("image", exits_zero);

        run(&runner, &command()).await.unwrap();

        let spec = runtime.specs.lock().unwrap()[0].clone();
        assert_eq!(spec.rootfs, Path::new("/images/image/rootfs"));
        assert_eq!(
            spec.mounts,
            [Mount {
                source: PathBuf::from("/sandboxes/action-1"),
                target: PathBuf::from("/workspace"),
            }]
        );
        assert_eq!(spec.working_dir, Path::new("/workspace/pkg"));
        assert_eq!(spec.args, ["cc", "-c", "a.c"]);
        let env: Vec<(&str, &str)> = spec.env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        assert_eq!(env, [("HOME", "/root"), ("PATH", "/opt/bin")]);
    }

    #[tokio::test]
    async fn explains_missing_images() {
        let (runner, _runtime) = runner("missing", exits_zero);

        let err = run(&runner, &command()).await.unwrap_err();

        assert!(matches!(&err, ExecuteError::Internal(message) if message.contains("import-image")), "{err}");
    }

    #[tokio::test]
    async fn reports_programs_that_cant_start_as_invalid_actions() {
        let (runner, _runtime) = runner("image", || Err(ContainerError::Start("no cc".to_string())));

        let err = run(&runner, &command()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let (runner, runtime) = runner("image", exits_zero);

        let err = run(&runner, &Command::default()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
        assert!(runtime.specs.lock().unwrap().is_empty());
    }
}
