use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use skyframe::{encoded_value, Ctx, Engine, EngineConfig, ErrorKind, Globals, Graph, Key, Persistence, SkyError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A workspace's files, which `File` keys read.
#[derive(Default)]
struct Disk(Mutex<HashMap<String, String>>);

impl Disk {
    fn write(&self, name: &str, contents: &str) {
        self.0.lock().unwrap().insert(name.to_string(), contents.to_string());
    }
}

/// How many times each kind of key was computed, across workspaces.
#[derive(Default)]
struct Counts {
    files: AtomicU64,
    lines: AtomicU64,
    totals: AtomicU64,
    /// Computations of [`Gated`] keys dropped before they finished.
    cancelled: AtomicU64,
}

impl Counts {
    fn get(&self) -> (u64, u64, u64) {
        (
            self.files.load(Ordering::SeqCst),
            self.lines.load(Ordering::SeqCst),
            self.totals.load(Ordering::SeqCst),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Text(String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Number(u64);

encoded_value!(Text, Number);

impl Encode for Text {
    fn encode(&self, e: &mut Encoder) {
        e.str(&self.0);
    }
}

impl Decode for Text {
    fn decode(d: &mut Decoder<'_>) -> Result<Text, DecodeError> {
        Ok(Text(d.str()?.to_string()))
    }
}

impl Encode for Number {
    fn encode(&self, e: &mut Encoder) {
        e.u64(self.0);
    }
}

impl Decode for Number {
    fn decode(d: &mut Decoder<'_>) -> Result<Number, DecodeError> {
        Ok(Number(d.u64()?))
    }
}

macro_rules! string_key {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        struct $name(String);

        impl Encode for $name {
            fn encode(&self, e: &mut Encoder) {
                e.str(&self.0);
            }
        }

        impl Decode for $name {
            fn decode(d: &mut Decoder<'_>) -> Result<$name, DecodeError> {
                Ok($name(d.str()?.to_string()))
            }
        }
    };
}

string_key!(File);
string_key!(Lines);
string_key!(Total);
string_key!(Fails);
string_key!(Panics);
string_key!(Flaky);
string_key!(Waits);
string_key!(Gated);

/// Holds [`Gated`] keys' computations until it's opened.
struct Gate(tokio::sync::Semaphore);

/// Counts a [`Gated`] computation as cancelled if it's dropped unfinished.
struct Unfinished(Arc<Counts>);

impl Drop for Unfinished {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}

impl Key for File {
    type Value = Text;
    const KIND: &'static str = "FILE";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<Text, SkyError> {
        ctx.global::<Counts>()?.files.fetch_add(1, Ordering::SeqCst);
        let disk = ctx.global::<Disk>()?;
        let contents = disk.0.lock().unwrap().get(&self.0).cloned();
        let contents = contents.ok_or_else(|| SkyError::new(format!("no such file: {}", self.0)))?;
        Ok(Text(contents))
    }
}

impl Key for Lines {
    type Value = Number;
    const KIND: &'static str = "LINES";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        ctx.global::<Counts>()?.lines.fetch_add(1, Ordering::SeqCst);
        let text = ctx.compute(&File(self.0.clone())).await?;
        Ok(Number(text.0.lines().count() as u64))
    }
}

/// The total number of lines in the files named in the comma-separated
/// file `self.0`.
impl Key for Total {
    type Value = Number;
    const KIND: &'static str = "TOTAL";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        ctx.global::<Counts>()?.totals.fetch_add(1, Ordering::SeqCst);
        let list = ctx.compute(&File(self.0.clone())).await?;
        let keys: Vec<Lines> = list.0.split(',').map(|name| Lines(name.trim().to_string())).collect();
        let mut total = 0;
        for lines in ctx.compute_many(&keys).await {
            total += lines?.0;
        }
        Ok(Number(total))
    }
}

impl Key for Fails {
    type Value = Number;
    const KIND: &'static str = "FAILS";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        ctx.global::<Counts>()?.totals.fetch_add(1, Ordering::SeqCst);
        Err(SkyError::new(format!("{} always fails", self.0)))
    }
}

impl Key for Flaky {
    type Value = Number;
    const KIND: &'static str = "FLAKY";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        ctx.global::<Counts>()?.totals.fetch_add(1, Ordering::SeqCst);
        Err(SkyError::transient(format!("{} failed this time", self.0)))
    }
}

/// Is the value of a [`Gated`] key.
impl Key for Waits {
    type Value = Number;
    const KIND: &'static str = "WAITS";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        ctx.compute(&Gated(self.0.clone())).await.map(|number| Number(number.0))
    }
}

/// Is 7 once the [`Gate`] opens.
impl Key for Gated {
    type Value = Number;
    const KIND: &'static str = "GATED";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        let counts = ctx.global::<Counts>()?;
        counts.totals.fetch_add(1, Ordering::SeqCst);
        let unfinished = Unfinished(counts);
        let gate = ctx.global::<Gate>()?;
        let _permit = gate.0.acquire().await.map_err(|_| SkyError::new("the gate closed"))?;
        std::mem::forget(unfinished);
        Ok(Number(7))
    }
}

impl Key for Panics {
    type Value = Number;
    const KIND: &'static str = "PANICS";

    async fn compute(&self, _ctx: &Ctx) -> Result<Number, SkyError> {
        panic!("{} panics", self.0)
    }
}

/// `Cycle(n)` depends on `Cycle((n + 1) % 3)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cycle(u64);

impl Encode for Cycle {
    fn encode(&self, e: &mut Encoder) {
        e.u64(self.0);
    }
}

impl Decode for Cycle {
    fn decode(d: &mut Decoder<'_>) -> Result<Cycle, DecodeError> {
        Ok(Cycle(d.u64()?))
    }
}

impl Key for Cycle {
    type Value = Number;
    const KIND: &'static str = "CYCLE";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        let next = ctx.compute(&Cycle((self.0 + 1) % 3)).await?;
        Ok(Number(next.0 + 1))
    }
}

/// Waits a while, then is 1.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Slow;

impl Encode for Slow {
    fn encode(&self, _: &mut Encoder) {}
}

impl Decode for Slow {
    fn decode(_: &mut Decoder<'_>) -> Result<Slow, DecodeError> {
        Ok(Slow)
    }
}

impl Key for Slow {
    type Value = Number;
    const KIND: &'static str = "SLOW";

    async fn compute(&self, _: &Ctx) -> Result<Number, SkyError> {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(Number(1))
    }
}

/// Depends on a cycle and, at the same time, on [`Slow`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BesideCycle;

impl Encode for BesideCycle {
    fn encode(&self, _: &mut Encoder) {}
}

impl Decode for BesideCycle {
    fn decode(_: &mut Decoder<'_>) -> Result<BesideCycle, DecodeError> {
        Ok(BesideCycle)
    }
}

impl Key for BesideCycle {
    type Value = Number;
    const KIND: &'static str = "BESIDE_CYCLE";

    async fn compute(&self, ctx: &Ctx) -> Result<Number, SkyError> {
        let (cycle, slow) = futures::join!(ctx.compute(&Cycle(0)), ctx.compute(&Slow));
        Ok(Number(cycle?.0 + slow?.0))
    }
}

fn workspace(engine: &Engine, counts: &Arc<Counts>) -> (Graph, Arc<Disk>) {
    let disk = Arc::new(Disk::default());
    disk.write("list", "a, b");
    disk.write("a", "1\n2\n3");
    disk.write("b", "1\n2");
    let globals = Globals::new().with(disk.clone()).with(counts.clone());
    (engine.graph(globals), disk)
}

async fn total(graph: &Graph) -> u64 {
    graph.session().await.evaluate(&Total("list".into())).await.unwrap().0
}

#[tokio::test]
async fn values_are_computed_once() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _disk) = workspace(&engine, &counts);

    assert_eq!(total(&graph).await, 5);
    assert_eq!(total(&graph).await, 5);
    assert_eq!(counts.get(), (3, 2, 1));
}

#[tokio::test]
async fn invalidated_keys_are_recomputed() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);

    disk.write("a", "1\n2\n3\n4");
    graph.invalidate(&[File("a".into())]).await;
    assert_eq!(total(&graph).await, 6);
    assert_eq!(counts.get(), (4, 3, 2));
}

#[tokio::test]
async fn invalidating_keys_the_graph_lacks_keeps_its_revision() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);
    let revision = graph.stats().revision;

    graph.invalidate(&[File("never read".into())]).await;
    assert_eq!(graph.stats().revision, revision);
    graph.invalidate(&[File("a".into())]).await;
    assert_eq!(graph.stats().revision, revision + 1);
}

#[tokio::test]
async fn invalidating_a_file_verifies_only_its_dependents() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);
    let verified = |kind: &str| engine.stats().kinds.iter().find(|row| row.name == kind).map_or(0, |row| row.verified);
    let lines_before = verified("LINES");

    disk.write("b", "1\n2\n3");
    graph.invalidate(&[File("b".into())]).await;
    assert_eq!(total(&graph).await, 6);
    assert_eq!(verified("LINES"), lines_before, "Lines of a, which b doesn't reach, isn't verified again");
}

#[tokio::test]
async fn unchanged_values_stop_recomputation() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);

    disk.write("a", "x\ny\nz");
    graph.invalidate(&[File("a".into())]).await;
    assert_eq!(total(&graph).await, 5);
    assert_eq!(counts.get(), (4, 3, 1), "Total's dependency kept its value, so Total isn't recomputed");
}

#[tokio::test]
async fn reverting_a_change_reuses_the_old_result() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);

    disk.write("a", "1");
    graph.invalidate(&[File("a".into())]).await;
    assert_eq!(total(&graph).await, 3);

    disk.write("a", "1\n2\n3");
    graph.invalidate(&[File("a".into())]).await;
    assert_eq!(total(&graph).await, 5);
    assert_eq!(counts.get(), (5, 3, 2), "the original Lines and Total are reused");
}

#[tokio::test]
async fn workspaces_share_results() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (first, _) = workspace(&engine, &counts);
    let (second, second_disk) = workspace(&engine, &counts);

    assert_eq!(total(&first).await, 5);
    assert_eq!(total(&second).await, 5);
    assert_eq!(counts.get(), (6, 2, 1), "the second workspace only reads its files");

    second_disk.write("b", "1\n2\n3\n4");
    second.invalidate(&[File("b".into())]).await;
    assert_eq!(total(&second).await, 7);
    assert_eq!(total(&first).await, 5);
    assert_eq!(counts.get(), (7, 3, 2));

    let stats = engine.stats();
    let lines = stats.kinds.iter().find(|kind| kind.name == "LINES").unwrap();
    assert_eq!(lines.values.values, 3, "the value for a is stored once");
}

#[tokio::test]
async fn concurrent_requests_compute_once() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);

    let session = graph.session().await;
    let keys: Vec<Total> = (0..50).map(|_| Total("list".into())).collect();
    for result in session.evaluate_many(&keys).await {
        assert_eq!(result.unwrap().0, 5);
    }
    assert_eq!(counts.get(), (3, 2, 1));
}

#[tokio::test]
async fn errors_are_results() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);
    let session = graph.session().await;

    let error = session.evaluate(&Fails("x".into())).await.unwrap_err();
    assert_eq!(error.message(), "x always fails");
    assert_eq!(error.kind(), ErrorKind::Failed);
    session.evaluate(&Fails("x".into())).await.unwrap_err();
    assert_eq!(counts.get().2, 1, "the error is kept");

    let missing = session.evaluate(&Lines("missing".into())).await.unwrap_err();
    assert_eq!(missing.message(), "no such file: missing");
}

#[tokio::test]
async fn transient_results_are_kept_until_invalidated() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);
    {
        let session = graph.session().await;
        session.evaluate(&Flaky("x".into())).await.unwrap_err();
        session.evaluate(&Fails("y".into())).await.unwrap_err();
    }
    graph.session().await.evaluate(&Flaky("x".into())).await.unwrap_err();
    assert_eq!(counts.get().2, 2, "kept within a revision");

    assert_eq!(graph.invalidate_transient().await, 1, "only the transient error is marked");
    {
        let session = graph.session().await;
        session.evaluate(&Flaky("x".into())).await.unwrap_err();
        session.evaluate(&Fails("y".into())).await.unwrap_err();
    }
    assert_eq!(counts.get().2, 3, "the transient error is computed again, the other kept");
    assert_eq!(graph.invalidate_transient().await, 1);
}

fn gated_workspace(engine: &Engine, counts: &Arc<Counts>) -> (Graph, Arc<Gate>) {
    let gate = Arc::new(Gate(tokio::sync::Semaphore::new(0)));
    let globals = Globals::new().with(counts.clone()).with(gate.clone());
    (engine.graph(globals), gate)
}

/// Wait until `condition` holds, for work happening on other tasks.
async fn eventually(condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the condition never held");
}

#[tokio::test(flavor = "multi_thread")]
async fn evaluations_nobody_waits_for_are_cancelled() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, gate) = gated_workspace(&engine, &counts);
    let session = Arc::new(graph.session().await);

    let request = tokio::spawn({
        let session = session.clone();
        async move { session.evaluate(&Waits("x".into())).await.map(|number| number.0) }
    });
    eventually(|| counts.get().2 == 1).await;
    request.abort();
    eventually(|| counts.cancelled.load(Ordering::SeqCst) == 1).await;

    gate.0.add_permits(10);
    let value = session.evaluate(&Waits("x".into())).await.unwrap();
    assert_eq!(value.0, 7);
    assert_eq!(counts.get().2, 2, "the cancelled key is computed afresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn evaluations_continue_while_anyone_waits() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, gate) = gated_workspace(&engine, &counts);
    let session = Arc::new(graph.session().await);

    let spawn_request = |session: Arc<skyframe::Session>| tokio::spawn(async move { session.evaluate(&Gated("x".into())).await.map(|number| number.0) });
    let abandoned = spawn_request(session.clone());
    let kept = spawn_request(session.clone());
    eventually(|| counts.get().2 == 1).await;
    abandoned.abort();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(counts.cancelled.load(Ordering::SeqCst), 0);

    gate.0.add_permits(10);
    assert_eq!(kept.await.unwrap().unwrap(), 7);
    assert_eq!(counts.get().2, 1, "computed once, for both");
}

#[tokio::test]
async fn panics_are_errors() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);
    let error = graph.session().await.evaluate(&Panics("p".into())).await.unwrap_err();
    assert!(error.message().contains("p panics"), "{error}");
}

#[tokio::test]
async fn cycles_are_errors() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);
    let error = graph.session().await.evaluate(&Cycle(0)).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cycle, "{error}");
    assert!(error.message().contains("CYCLE:Cycle(1)"), "{error}");
}

#[tokio::test]
async fn cycles_are_found_once_other_work_finishes() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    let (graph, _) = workspace(&engine, &counts);
    let session = graph.session().await;
    let evaluation = tokio::time::timeout(std::time::Duration::from_secs(10), session.evaluate(&BesideCycle)).await;
    let error = evaluation.expect("the cycle was never found").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cycle, "{error}");
}

#[tokio::test]
async fn dropping_a_graph_frees_its_values() {
    let engine = Engine::default();
    let counts = Arc::new(Counts::default());
    {
        let (graph, _) = workspace(&engine, &counts);
        assert_eq!(total(&graph).await, 5);
        assert!(engine.stats().store.values > 0);
    }
    let stats = engine.stats();
    let file = stats.kinds.iter().find(|kind| kind.name == "FILE").unwrap();
    assert_eq!(file.traces.values, 0, "external traces are only held by the graph");
    assert_eq!(file.values.values, 3, "file values are held by the traces that read them");
    let lines = stats.kinds.iter().find(|kind| kind.name == "LINES").unwrap();
    assert_eq!(lines.traces.values, 2, "traces are remembered for other workspaces");
}

fn persistence(dir: &std::path::Path) -> Persistence {
    Persistence {
        dir: dir.to_path_buf(),
        version: skycodec::Fingerprint::of(b"test build"),
        max_disk_bytes: 1 << 30,
    }
}

#[tokio::test]
async fn a_restored_engine_reuses_saved_results() {
    let dir = tempfile::tempdir().unwrap();
    let counts = Arc::new(Counts::default());
    {
        let engine = Engine::open(EngineConfig::default(), persistence(dir.path()));
        let (graph, _) = workspace(&engine, &counts);
        assert_eq!(total(&graph).await, 5);
        assert!(engine.checkpoint().unwrap().is_some());
    }
    let engine = Engine::open(EngineConfig::default(), persistence(dir.path()));
    let (graph, disk) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);
    assert_eq!(counts.get(), (6, 2, 1), "files are read again; lines and the total are reused");

    disk.write("b", "1\n2\n3\n4");
    graph.invalidate(&[File("b".into())]).await;
    assert_eq!(total(&graph).await, 7);
    assert_eq!(counts.get(), (7, 3, 2));
}

#[tokio::test]
async fn another_build_starts_afresh() {
    let dir = tempfile::tempdir().unwrap();
    let counts = Arc::new(Counts::default());
    {
        let engine = Engine::open(EngineConfig::default(), persistence(dir.path()));
        let (graph, _) = workspace(&engine, &counts);
        total(&graph).await;
        engine.checkpoint().unwrap();
    }
    let mut other = persistence(dir.path());
    other.version = skycodec::Fingerprint::of(b"another build");
    let engine = Engine::open(EngineConfig::default(), other);
    let (graph, _) = workspace(&engine, &counts);
    assert_eq!(total(&graph).await, 5);
    assert_eq!(counts.get(), (6, 4, 2), "nothing is reused");
}
