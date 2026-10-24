//! A command's output, streamed back to the client.

use bazel_rust_connect::buildbox::bazel::v1::{EnvironmentVariable, Exec, RunResponse, run_response};
use tokio::sync::mpsc;

/// Where a command writes its output: the client's stdout and stderr.
#[derive(Clone)]
pub struct Output {
    sender: mpsc::Sender<RunResponse>,
}

impl Output {
    pub fn new(sender: mpsc::Sender<RunResponse>) -> Output {
        Output { sender }
    }

    pub async fn stdout(&self, text: &str) {
        self.send(run_response::Output::Stdout(text.as_bytes().to_vec())).await;
    }

    pub async fn stderr(&self, text: &str) {
        self.send(run_response::Output::Stderr(text.as_bytes().to_vec())).await;
    }

    /// Report an error, as Bazel does: `ERROR: message`.
    pub async fn error(&self, message: &str) {
        self.stderr(&format!("ERROR: {message}\n")).await;
    }

    pub async fn warning(&self, message: &str) {
        self.stderr(&format!("WARNING: {message}\n")).await;
    }

    pub async fn info(&self, message: &str) {
        self.stderr(&format!("INFO: {message}\n")).await;
    }

    /// Tell the client to run a program, as `bazel run` does.
    pub async fn exec(&self, argv: Vec<String>, working_directory: &std::path::Path, env: Vec<(String, String)>) {
        let env = env
            .into_iter()
            .map(|(name, value)| EnvironmentVariable {
                name,
                value,
                ..Default::default()
            })
            .collect();
        let exec = Exec {
            argv,
            working_directory: working_directory.display().to_string(),
            env,
            ..Default::default()
        };
        self.send(run_response::Output::Exec(Box::new(exec))).await;
    }

    /// A client that has gone away can't be told anything, so failures to
    /// send are ignored; the command finds out it should stop when it's
    /// cancelled.
    async fn send(&self, output: run_response::Output) {
        let response = RunResponse {
            output: Some(output),
            ..Default::default()
        };
        let _ = self.sender.send(response).await;
    }
}
