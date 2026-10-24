//! A command's output, streamed back to the client, as Bazel's UI shows
//! it: messages, and the progress bar beneath them.

use bazel_options::ParsedOptions;
use bazel_progress::{Event, Snapshot, Tracker};
use bazel_rust_connect::buildbox::bazel::v1::{EnvironmentVariable, Exec, RunResponse, run_response};
use bazel_ui::{FailureFormat, Kind, Phase, Screen, Shown, Terminal, UiOptions};
use std::collections::HashSet;
use std::path::PathBuf;
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
    failure_format: FailureFormat,
    /// Whether the command has started analyzing targets.
    analysis_started: bool,
    /// The errors of the failures already reported as events, which aren't
    /// reported again.
    reported: HashSet<String>,
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
            failure_format: FailureFormat::default(),
            analysis_started: false,
            reported: HashSet::new(),
        };
        let ui = Arc::new(Mutex::new(ui));
        tokio::spawn(refresh_until_dropped(Arc::downgrade(&ui)));
        Output { ui }
    }

    /// Set how output looks from a command's `--color`, `--curses`,
    /// `--[no]show_progress`, `--ui_actions_shown` and
    /// `--[no]verbose_failures`.
    pub async fn configure(&self, options: &ParsedOptions) {
        let mut ui = self.ui.lock().await;
        ui.failure_format.verbose = options.flag("verbose_failures", false);
        let mut ui_options = UiOptions::new(&ui.terminal, options.get("color"), options.get("curses"), options.flag("show_progress", true));
        let actions_shown = options.int("ui_actions_shown", bazel_ui::DEFAULT_ACTIONS_SHOWN as i64);
        ui_options.actions_shown = usize::try_from(actions_shown.max(1)).unwrap_or(bazel_ui::DEFAULT_ACTIONS_SHOWN);
        let cleared = ui.screen.set_options(ui_options);
        ui.drawn.clear();
        ui.send_stderr(cleared).await;
    }

    /// Where the command's actions run, which verbose failures show.
    pub async fn set_exec_root(&self, exec_root: PathBuf) {
        self.ui.lock().await.failure_format.exec_root = Some(exec_root);
    }

    /// Show what `tracker` records in the progress bar.
    pub async fn track(&self, tracker: Arc<Tracker>) {
        self.ui.lock().await.tracker = Some(tracker);
    }

    /// Whether the command has started analyzing targets.
    pub async fn analysis_started(&self) -> bool {
        self.ui.lock().await.analysis_started
    }

    /// What the tracked progress has recorded so far.
    pub async fn snapshot(&self) -> Snapshot {
        self.ui.lock().await.snapshot()
    }

    /// Start a new phase of the command, showing it at once.
    pub async fn phase(&self, phase: Phase) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        ui.analysis_started |= matches!(phase, Phase::Analyzing(_) | Phase::Executing);
        ui.phase = phase;
        ui.printed = None;
        ui.refresh().await;
    }

    pub async fn stdout(&self, text: &str) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        ui.write_stdout(text).await;
    }

    /// Output for stderr as it is, such as what an action printed.
    pub async fn stderr(&self, text: &str) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        ui.write_stderr(text).await;
    }

    /// Report an error, as Bazel does: `ERROR: message`, unless it's the
    /// error of a failure already reported as it happened.
    pub async fn error(&self, message: &str) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        if !ui.reported.contains(message) {
            ui.write_message(Kind::Error, message).await;
        }
    }

    pub async fn warning(&self, message: &str) {
        self.message(Kind::Warning, message).await;
    }

    pub async fn info(&self, message: &str) {
        self.message(Kind::Info, message).await;
    }

    async fn message(&self, kind: Kind, message: &str) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        ui.write_message(kind, message).await;
    }

    /// Clear the progress bar, for the end of the command.
    pub async fn finish(&self) {
        let mut ui = self.ui.lock().await;
        ui.show_events().await;
        ui.phase = Phase::Idle;
        let written = ui.screen.finish();
        ui.drawn.clear();
        ui.send_stderr(written).await;
    }

    /// Tell the client the command has started, with id `id`.
    pub async fn started(&self, id: &str) {
        self.ui.lock().await.send(run_response::Output::InvocationId(id.to_string())).await;
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
    async fn write_message(&mut self, kind: Kind, message: &str) {
        let bar = self.bar(Some(Instant::now()));
        let written = self.screen.message(kind, message, &bar);
        self.drawn = bar;
        self.send_stderr(written).await;
    }

    async fn write_stderr(&mut self, text: &str) {
        let bar = self.bar(Some(Instant::now()));
        let written = self.screen.output(text, &bar);
        self.drawn = bar;
        self.send_stderr(written).await;
    }

    /// Write to stdout, clearing the progress bar first in case stdout is
    /// the same terminal.
    async fn write_stdout(&mut self, text: &str) {
        let bar = self.bar(Some(Instant::now()));
        let cleared = self.screen.redraw(&[]);
        self.send_stderr(cleared).await;
        self.send(run_response::Output::Stdout(text.as_bytes().to_vec())).await;
        let drawn = self.screen.redraw(&bar);
        self.drawn = bar;
        self.send_stderr(drawn).await;
    }

    /// Show the events reported since they were last shown.
    async fn show_events(&mut self) {
        let Some(tracker) = &self.tracker else {
            return;
        };
        for event in tracker.take_events() {
            let shown = match &event {
                Event::ActionOutput(output) => bazel_ui::action_output(output),
                Event::ActionFailed(failure) => {
                    self.reported.insert(failure.error.clone());
                    bazel_ui::action_failure(failure, &self.failure_format)
                }
                Event::Error(message) => Shown {
                    messages: vec![(Kind::Error, message.clone())],
                    ..Shown::default()
                },
            };
            self.show(shown).await;
        }
    }

    async fn show(&mut self, shown: Shown) {
        for (kind, message) in &shown.messages {
            self.write_message(*kind, message).await;
        }
        if !shown.stderr.is_empty() {
            let mut stderr = shown.stderr;
            if !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            self.write_stderr(&stderr).await;
        }
        if !shown.stdout.is_empty() {
            self.write_stdout(&shown.stdout).await;
        }
    }

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
        self.show_events().await;
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
