//! A command's output, streamed back to the client, as Bazel's UI shows
//! it: messages, and the progress bar beneath them.

use bazel_options::ParsedOptions;
use bazel_progress::{Snapshot, Tracker};
use bazel_rust_connect::buildbox::bazel::v1::{EnvironmentVariable, Exec, RunResponse, run_response};
use bazel_ui::{Kind, Phase, Screen, Terminal, UiOptions};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

/// How often the progress bar is checked for changes.
const REFRESH_INTERVAL: Duration = Duration::from_millis(200);

/// How often the progress bar is printed, at most, when it can't be
/// redrawn in place.
const PRINT_INTERVAL: Duration = Duration::from_secs(1);

/// Where a command writes its output: the client's stdout and stderr.
#[derive(Clone)]
pub struct Output {
    ui: Arc<Mutex<Ui>>,
}

/// What's been written, and what the progress bar shows.
struct Ui {
    sender: mpsc::Sender<RunResponse>,
    terminal: Terminal,
    screen: Screen,
    phase: Phase,
    tracker: Option<Arc<Tracker>>,
    /// The progress bar as last drawn in place.
    drawn: Vec<String>,
    /// The progress bar as last printed, without times, and when.
    printed: Option<(Vec<String>, Instant)>,
}

impl Output {
    /// Output to a client with `terminal`, refreshing its progress bar until
    /// every clone of the output is dropped.
    pub fn new(sender: mpsc::Sender<RunResponse>, terminal: Terminal) -> Output {
        let options = UiOptions::new(&terminal, None, None, true);
        let ui = Ui {
            sender,
            terminal,
            screen: Screen::new(options),
            phase: Phase::Idle,
            tracker: None,
            drawn: Vec::new(),
            printed: None,
        };
        let ui = Arc::new(Mutex::new(ui));
        tokio::spawn(refresh_until_dropped(Arc::downgrade(&ui)));
        Output { ui }
    }

    /// Set how output looks from a command's `--color`, `--curses` and
    /// `--[no]show_progress`.
    pub async fn configure(&self, options: &ParsedOptions) {
        let mut ui = self.ui.lock().await;
        let options = UiOptions::new(&ui.terminal, options.get("color"), options.get("curses"), options.flag("show_progress", true));
        let cleared = ui.screen.set_options(options);
        ui.drawn.clear();
        ui.send_stderr(cleared).await;
    }

    /// Show what `tracker` records in the progress bar.
    pub async fn track(&self, tracker: Arc<Tracker>) {
        self.ui.lock().await.tracker = Some(tracker);
    }

    /// What the tracked progress has recorded so far.
    pub async fn snapshot(&self) -> Snapshot {
        self.ui.lock().await.snapshot()
    }

    /// Start a new phase of the command, showing it at once.
    pub async fn phase(&self, phase: Phase) {
        let mut ui = self.ui.lock().await;
        ui.phase = phase;
        ui.printed = None;
        ui.refresh().await;
    }

    pub async fn stdout(&self, text: &str) {
        let mut ui = self.ui.lock().await;
        let bar = ui.bar(Some(Instant::now()));
        let cleared = ui.screen.redraw(&[]);
        ui.send_stderr(cleared).await;
        ui.send(run_response::Output::Stdout(text.as_bytes().to_vec())).await;
        let drawn = ui.screen.redraw(&bar);
        ui.drawn = bar;
        ui.send_stderr(drawn).await;
    }

    /// Output for stderr as it is, such as what an action printed.
    pub async fn stderr(&self, text: &str) {
        let mut ui = self.ui.lock().await;
        let bar = ui.bar(Some(Instant::now()));
        let written = ui.screen.output(text, &bar);
        ui.drawn = bar;
        ui.send_stderr(written).await;
    }

    /// Report an error, as Bazel does: `ERROR: message`.
    pub async fn error(&self, message: &str) {
        self.message(Kind::Error, message).await;
    }

    pub async fn warning(&self, message: &str) {
        self.message(Kind::Warning, message).await;
    }

    pub async fn info(&self, message: &str) {
        self.message(Kind::Info, message).await;
    }

    async fn message(&self, kind: Kind, message: &str) {
        let mut ui = self.ui.lock().await;
        let bar = ui.bar(Some(Instant::now()));
        let written = ui.screen.message(kind, message, &bar);
        ui.drawn = bar;
        ui.send_stderr(written).await;
    }

    /// Clear the progress bar, for the end of the command.
    pub async fn finish(&self) {
        let mut ui = self.ui.lock().await;
        ui.phase = Phase::Idle;
        let written = ui.screen.finish();
        ui.drawn.clear();
        ui.send_stderr(written).await;
    }

    /// Tell the client to run a program, as `bazel run` does.
    pub async fn exec(&self, argv: Vec<String>, working_directory: &std::path::Path, env: Vec<(String, String)>) {
        self.finish().await;
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
        self.ui.lock().await.send(run_response::Output::Exec(Box::new(exec))).await;
    }
}

impl Ui {
    fn snapshot(&self) -> Snapshot {
        match &self.tracker {
            Some(tracker) => tracker.snapshot(),
            None => Snapshot::default(),
        }
    }

    /// The progress bar's lines; with `now` as `None`, without times.
    fn bar(&self, now: Option<Instant>) -> Vec<String> {
        if !self.screen.options().show_progress {
            return Vec::new();
        }
        bazel_ui::progress_bar(&self.phase, &self.snapshot(), now, self.screen.options())
    }

    /// Show the progress bar if it has changed: redrawn in place on a
    /// terminal that allows it, otherwise printed, at most once a second
    /// unless the phase has changed, and while executing, only when actions
    /// are running.
    async fn refresh(&mut self) {
        let now = Instant::now();
        if self.screen.options().cursor_control {
            let bar = self.bar(Some(now));
            if bar != self.drawn {
                let written = self.screen.redraw(&bar);
                self.drawn = bar;
                self.send_stderr(written).await;
            }
            return;
        }
        if self.phase == Phase::Executing && self.snapshot().running.is_empty() {
            return;
        }
        let timeless = self.bar(None);
        let due = match &self.printed {
            None => true,
            Some((printed, at)) => *printed != timeless && now.duration_since(*at) >= PRINT_INTERVAL,
        };
        if due {
            let written = self.screen.print_bar(&self.bar(Some(now)));
            self.printed = Some((timeless, now));
            self.send_stderr(written).await;
        }
    }

    async fn send_stderr(&self, text: String) {
        if !text.is_empty() {
            self.send(run_response::Output::Stderr(text.into_bytes())).await;
        }
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

/// Keep the progress bar up to date while the output is in use.
async fn refresh_until_dropped(ui: Weak<Mutex<Ui>>) {
    let mut interval = tokio::time::interval(REFRESH_INTERVAL);
    loop {
        interval.tick().await;
        let Some(ui) = ui.upgrade() else {
            return;
        };
        ui.lock().await.refresh().await;
    }
}
