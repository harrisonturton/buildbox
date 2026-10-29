//! The Build Event Protocol: a command's events, written as Bazel's
//! `--build_event_json_file` and `--build_event_binary_file` write them.

pub mod events;

use buffa::Message;
use events::BuildEvent;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Where a command's build events go: a file of JSON lines and a file of
/// length-delimited protos, written in the order events are posted, by a
/// task of their own so that posting never waits on the disk.
pub struct BuildEventLog {
    /// Where events are sent to be written, until the log closes.
    events: Mutex<Option<mpsc::UnboundedSender<BuildEvent>>>,
    writer: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

impl BuildEventLog {
    /// Create the files the options name, or `None` if they name none.
    pub async fn open(json: Option<PathBuf>, binary: Option<PathBuf>) -> io::Result<Option<BuildEventLog>> {
        if json.is_none() && binary.is_none() {
            return Ok(None);
        }
        let json = match json {
            Some(path) => Some(BufWriter::new(create(&path).await?)),
            None => None,
        };
        let binary = match binary {
            Some(path) => Some(BufWriter::new(create(&path).await?)),
            None => None,
        };
        let (events, received) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_events(received, json, binary));
        Ok(Some(BuildEventLog {
            events: Mutex::new(Some(events)),
            writer: Mutex::new(Some(writer)),
        }))
    }

    /// Write an event, after those posted before it. Events posted after
    /// the log closes are dropped, and so are those posted after writing
    /// failed, which `close` reports.
    pub fn post(&self, event: BuildEvent) {
        let events = self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(events) = events.as_ref() {
            let _ = events.send(event);
        }
    }

    /// Write the last event, marked as last, and wait until every event is
    /// written. Closing a log again does nothing.
    pub async fn close(&self, mut last: BuildEvent) -> io::Result<()> {
        last.last_message = true;
        self.post(last);
        self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        let writer = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        match writer {
            Some(writer) => writer.await.map_err(io::Error::other)?,
            None => Ok(()),
        }
    }
}

async fn create(path: &std::path::Path) -> io::Result<tokio::fs::File> {
    tokio::fs::File::create(path)
        .await
        .map_err(|error| io::Error::new(error.kind(), format!("creating {}: {error}", path.display())))
}

/// Write each event as it comes, until the log closes.
async fn write_events(
    mut received: mpsc::UnboundedReceiver<BuildEvent>,
    mut json: Option<BufWriter<tokio::fs::File>>,
    mut binary: Option<BufWriter<tokio::fs::File>>,
) -> io::Result<()> {
    while let Some(event) = received.recv().await {
        if let Some(json) = &mut json {
            let mut line = serde_json::to_string(&event).map_err(io::Error::other)?;
            line.push('\n');
            json.write_all(line.as_bytes()).await?;
        }
        if let Some(binary) = &mut binary {
            binary.write_all(&length_delimited(&event)).await?;
        }
    }
    for file in [json.as_mut(), binary.as_mut()].into_iter().flatten() {
        file.flush().await?;
    }
    Ok(())
}

/// An event as a varint of its length, then its bytes, as Bazel writes
/// binary event files.
fn length_delimited(event: &BuildEvent) -> Vec<u8> {
    let bytes = event.encode_to_vec();
    let mut out = Vec::with_capacity(bytes.len() + 10);
    let mut length = bytes.len() as u64;
    loop {
        let byte = (length & 0x7f) as u8;
        length >>= 7;
        if length == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_as_bazels_json() {
        let event = events::workspace_status(&[("BUILD_USER".to_string(), "me".to_string())]);
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"id":{"workspaceStatus":{}},"workspaceStatus":{"item":[{"key":"BUILD_USER","value":"me"}]}}"#);
    }

    #[test]
    fn lengths_are_varints() {
        let event = events::build_metadata();
        let bytes = length_delimited(&event);
        assert_eq!(bytes[0] as usize, bytes.len() - 1);
    }
}
