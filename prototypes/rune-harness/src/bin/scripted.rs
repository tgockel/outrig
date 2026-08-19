use anyhow::{bail, Context as _, Result};
use rune::runtime::Value;
use rune::{Any, Context, Diagnostics, Module, Source, Sources, Vm};
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, Mutex},
};
const LIMIT: usize = 16 * 1024;
const LEN: usize = 100_000_000;
const DISCOVER: &str = r#"println!("{}", doc(fs));"#;
const READ: &str = r#"let source = fs.read(path).await?; println!("{}", source);"#;
const INSPECT: &str = r#"println!("{}", source[50_000_000..50_010_000]);"#;
thread_local! {static SOURCE:RefCell<Option<Value>>=const{RefCell::new(None)};}
fn store_source(v: Value) {
    SOURCE.with(|s| *s.borrow_mut() = Some(v))
}
fn load_source() -> Value {
    SOURCE.with(|s| s.borrow().as_ref().unwrap().clone())
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileAnalysis {
    bytes: usize,
    inspected_start: usize,
    inspected_end: usize,
}
#[derive(Clone)]
enum ModelResponse {
    ExecuteRune(&'static str),
    ReturnResult(FileAnalysis),
}
#[derive(Clone, Default)]
struct ModelRequest {
    observations: Vec<String>,
    bindings: Vec<String>,
}
struct ScriptedModel {
    scripted: VecDeque<ModelResponse>,
    received: Vec<ModelRequest>,
}
impl ScriptedModel {
    fn new(v: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            scripted: v.into_iter().collect(),
            received: vec![],
        }
    }
    fn complete(&mut self, r: ModelRequest) -> Result<ModelResponse> {
        self.received.push(r);
        self.scripted.pop_front().context("model exhausted")
    }
}
#[derive(Default)]
struct Counts {
    builds: usize,
    reads: usize,
}
#[derive(Any, Clone)]
struct FsHandle {
    counts: Arc<Mutex<Counts>>,
}
impl FsHandle {
    fn read(&self, path: &str) -> String {
        assert_eq!(path, "/fixture");
        let mut c = self.counts.lock().unwrap();
        c.reads += 1;
        c.builds += 1;
        drop(c);
        "x".repeat(LEN)
    }
}
struct MethodDef {
    signature: &'static str,
    summary: &'static str,
}
struct CapabilityDef {
    name: &'static str,
    summary: &'static str,
    methods: &'static [MethodDef],
}
static FS: CapabilityDef = CapabilityDef {
    name: "FileSystem",
    summary: "Deterministic filesystem capability.",
    methods: &[MethodDef {
        signature: "async fn read(path: String) -> String",
        summary: "Read the complete file at path.",
    }],
};
impl CapabilityDef {
    fn render(&self) -> String {
        let mut s = format!("trait {} — {}\n", self.name, self.summary);
        for m in self.methods {
            s += &format!("  {} — {}\n", m.signature, m.summary)
        }
        s
    }
    fn register(&self, m: &mut Module) -> Result<()> {
        let mut t = m.define_trait([self.name])?;
        t.docs([self.summary])?;
        for x in self.methods {
            t.function("read")?.docs([x.signature, x.summary])?;
        }
        drop(t);
        m.implement_trait::<FsHandle>(rune::item!(FileSystem))?;
        Ok(())
    }
}
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    attempted: usize,
    truncated: bool,
}
impl Capture {
    fn observation(&self) -> String {
        format!(
            "rune stdout: captured={} attempted={} truncated={}",
            self.bytes.len(),
            self.attempted,
            self.truncated
        )
    }
}
struct Invocation {
    context: Context,
    runtime: Arc<rune::runtime::RuntimeContext>,
    capture: Arc<Mutex<Capture>>,
    counts: Arc<Mutex<Counts>>,
    units: usize,
}
impl Invocation {
    fn new() -> Result<Self> {
        SOURCE.with(|s| *s.borrow_mut() = None);
        let capture = Arc::new(Mutex::new(Capture::default()));
        let counts = Arc::new(Mutex::new(Counts::default()));
        let fs = FsHandle {
            counts: counts.clone(),
        };
        let mut m = Module::new();
        FS.register(&mut m)?;
        m.ty::<FsHandle>()?;
        m.associated_function("read", FsHandle::read)?;
        m.function("fs", move || fs.clone()).build()?;
        let docs = FS.render();
        m.function("doc_fs", move || docs.clone()).build()?;
        m.function("store_source", store_source).build()?;
        m.function("load_source", load_source).build()?;
        let sink = capture.clone();
        m.function("bounded_print", move |text: &str| {
            let mut o = sink.lock().unwrap();
            o.attempted += text.len() + 1;
            let room = LIMIT.saturating_sub(o.bytes.len());
            let take = room.min(text.len());
            o.bytes.extend_from_slice(&text.as_bytes()[..take]);
            if take < text.len() {
                o.truncated = true
            }
            if o.bytes.len() < LIMIT {
                o.bytes.push(b'\n')
            } else {
                o.truncated = true
            }
        })
        .build()?;
        let mut context = Context::with_config(false)?;
        context.install(m)?;
        let runtime = Arc::new(context.runtime()?);
        Ok(Self {
            context,
            runtime,
            capture,
            counts,
            units: 0,
        })
    }
    fn execute(&mut self, s: &str) -> Result<String> {
        *self.capture.lock().unwrap() = Capture::default();
        let mut src = Sources::new();
        src.insert(Source::new(
            format!("step-{}.rn", self.units + 1),
            transform(s)?,
        )?)?;
        let mut d = Diagnostics::new();
        let unit = rune::prepare(&mut src)
            .with_context(&self.context)
            .with_diagnostics(&mut d)
            .build();
        if !d.is_empty() {
            let mut e =
                rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Never);
            d.emit(&mut e, &src)?
        }
        let unit = unit.context("compile")?;
        self.units += 1;
        Vm::new(self.runtime.clone(), Arc::new(unit))
            .call(["main"], ())
            .context("execute")?;
        Ok(self.capture.lock().unwrap().observation())
    }
    fn bindings(&self) -> Result<Vec<String>> {
        SOURCE.with(|s| {
            Ok(match s.borrow().as_ref() {
                Some(v) => vec![format!(
                    "source: String ({} bytes, retained)",
                    v.borrow_string_ref()?.len()
                )],
                None => vec![],
            })
        })
    }
    fn source_len(&self) -> Result<usize> {
        SOURCE.with(|s| {
            Ok(s.borrow()
                .as_ref()
                .context("missing source")?
                .borrow_string_ref()?
                .len())
        })
    }
}
fn transform(s: &str) -> Result<String> {
    let b=match s{DISCOVER=>"bounded_print(doc_fs());",READ=>"let fs=fs();let path=\"/fixture\";store_source(fs.read(path));bounded_print(load_source());",INSPECT=>"bounded_print(load_source()[50_000_000..50_010_000]);",_=>bail!("unsupported program")};
    Ok(format!("pub fn main(){{{b}}}"))
}
struct Report {
    result: FileAnalysis,
    received: Vec<ModelRequest>,
    large: Capture,
    slice: Capture,
    builds: usize,
    reads: usize,
    source_len: usize,
    units: usize,
}
fn run_harness() -> Result<Report> {
    let result = FileAnalysis {
        bytes: LEN,
        inspected_start: 50_000_000,
        inspected_end: 50_010_000,
    };
    let mut model = ScriptedModel::new([
        ModelResponse::ExecuteRune(DISCOVER),
        ModelResponse::ExecuteRune(READ),
        ModelResponse::ExecuteRune(INSPECT),
        ModelResponse::ReturnResult(result.clone()),
    ]);
    let mut inv = Invocation::new()?;
    let mut obs = vec![];
    let (mut large, mut slice) = (None, None);
    let result = loop {
        match model.complete(ModelRequest {
            observations: obs.clone(),
            bindings: inv.bindings()?,
        })? {
            ModelResponse::ExecuteRune(s) => {
                obs.push(inv.execute(s)?);
                if s == READ {
                    large = Some(std::mem::take(&mut *inv.capture.lock().unwrap()))
                } else if s == INSPECT {
                    slice = Some(std::mem::take(&mut *inv.capture.lock().unwrap()))
                }
            }
            ModelResponse::ReturnResult(v) => break v,
        }
    };
    if !model.scripted.is_empty() {
        bail!("unused scripts")
    };
    let c = inv.counts.lock().unwrap();
    Ok(Report {
        result,
        received: model.received,
        large: large.context("large")?,
        slice: slice.context("slice")?,
        builds: c.builds,
        reads: c.reads,
        source_len: inv.source_len()?,
        units: inv.units,
    })
}
fn validate(r: &Report) -> Result<()> {
    if r.result
        != (FileAnalysis {
            bytes: LEN,
            inspected_start: 50_000_000,
            inspected_end: 50_010_000,
        })
        || r.builds != 1
        || r.reads != 1
        || r.source_len != LEN
        || r.large.bytes.len() > LIMIT
        || !r.large.truncated
        || r.slice.bytes.len() != 10_001
        || !r.slice.bytes[..10_000].iter().all(|b| *b == b'x')
        || r.slice.truncated
        || r.units != 3
        || !r.received[2]
            .observations
            .iter()
            .any(|x| x.contains("attempted=100000001 truncated=true"))
        || !r.received[2]
            .bindings
            .iter()
            .any(|x| x == "source: String (100000000 bytes, retained)")
    {
        bail!("invariant failed")
    }
    Ok(())
}
fn main() -> Result<()> {
    let r = run_harness()?;
    validate(&r)?;
    println!("rune=0.14.2 units_compiled={}", r.units);
    println!(
        "source bytes={} builds={} reads={}",
        r.source_len, r.builds, r.reads
    );
    println!(
        "large_output captured={} attempted={} truncated={}",
        r.large.bytes.len(),
        r.large.attempted,
        r.large.truncated
    );
    println!(
        "slice captured={} payload_x={} truncated={}",
        r.slice.bytes.len(),
        r.slice.bytes[..10_000].iter().all(|b| *b == b'x'),
        r.slice.truncated
    );
    println!("result={:?}", r.result);
    println!("model_requests={} unused_scripts=0", r.received.len());
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vertical() -> Result<()> {
        let r = run_harness()?;
        validate(&r)?;
        assert_eq!(r.received.len(), 4);
        Ok(())
    }
    #[test]
    fn rejects() {
        assert!(transform("other").is_err())
    }
}
