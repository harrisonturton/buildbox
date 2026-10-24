use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use skyframe::{encoded_value, Ctx, Engine, ErrorKind, Globals, Graph, Key, SkyError};
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
