//! Event-driven Rune agent prototype.
//! The durable state is [`Invocation`]'s host scope, never a provider transcript.

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use rune::runtime::{Globals, RuntimeContext, Value};
use rune::sync::Arc as RuneArc;
use rune::{Any, Context, Diagnostics, Module, Source, Sources, Statics, Vm};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Notify};

pub const MODEL_VISIBLE_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalEvent {
    UserInput { id: u64, text: String },
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationCause {
    ExternalEvent(ExternalEvent),
    RuneObservation(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationRequest {
    pub instructions: String,
    pub cause: ActivationCause,
    pub observation: String,
    pub capability_summaries: Vec<String>,
    pub bindings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    ExecuteRune { source: String },
    Emit { text: String },
}

#[async_trait(?Send)]
pub trait ModelBackend {
    async fn activate(&mut self, request: ActivationRequest) -> Result<Decision>;
}

#[derive(Default)]
pub struct ScriptedModel {
    decisions: VecDeque<Decision>,
    pub requests: Vec<ActivationRequest>,
}
impl ScriptedModel {
    pub fn new(decisions: impl IntoIterator<Item = Decision>) -> Self {
        Self {
            decisions: decisions.into_iter().collect(),
            requests: vec![],
        }
    }
}
#[async_trait(?Send)]
impl ModelBackend for ScriptedModel {
    async fn activate(&mut self, request: ActivationRequest) -> Result<Decision> {
        self.requests.push(request);
        self.decisions
            .pop_front()
            .context("scripted model exhausted")
    }
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    attempted: usize,
    truncated: bool,
}
impl Capture {
    fn append(&mut self, text: &str, newline: bool) {
        self.attempted += text.len() + usize::from(newline);
        let room = MODEL_VISIBLE_LIMIT.saturating_sub(self.bytes.len());
        let take = room.min(text.len());
        self.bytes.extend_from_slice(&text.as_bytes()[..take]);
        self.truncated |= take < text.len();
        if newline {
            if self.bytes.len() < MODEL_VISIBLE_LIMIT {
                self.bytes.push(b'\n');
            } else {
                self.truncated = true;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CapabilityDef {
    pub name: &'static str,
    pub summary: &'static str,
    pub methods: &'static [(&'static str, &'static str)],
}
impl CapabilityDef {
    fn render(&self) -> String {
        let mut s = format!("trait {} — {}", self.name, self.summary);
        for (signature, summary) in self.methods {
            s.push_str(&format!("\n  {signature} — {summary}"));
        }
        s
    }
}
pub static FILE_SYSTEM: CapabilityDef = CapabilityDef {
    name: "FileSystem",
    summary: "Read UTF-8 regular files beneath the configured repository root.",
    methods: &[(
        "fn read(path: String) -> String",
        "Synchronously canonicalizes beneath the repo, rejects escapes, and tracks reads.",
    )],
};

pub trait FileSystem: Send + Sync {
    fn read(&self, path: &str) -> Result<String>;
    fn reads(&self) -> usize;
}

#[derive(Clone, Any)]
struct RepoFs {
    root: Arc<PathBuf>,
    reads: Arc<Mutex<usize>>,
}
impl FileSystem for RepoFs {
    fn read(&self, path: &str) -> Result<String> {
        let candidate = self.root.join(path);
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("canonicalize {}", candidate.display()))?;
        if !canonical.starts_with(self.root.as_path()) {
            bail!("path escapes repository root: {path}")
        }
        if !canonical.is_file() {
            bail!("not a regular file: {}", canonical.display())
        }
        let text = std::fs::read_to_string(&canonical)
            .with_context(|| format!("read UTF-8 file {}", canonical.display()))?;
        *self.reads.lock().expect("read counter") += 1;
        Ok(text)
    }
    fn reads(&self) -> usize {
        *self.reads.lock().expect("read counter")
    }
}
impl RepoFs {
    fn read_rune(&self, path: &str) -> String {
        self.read(path)
            .unwrap_or_else(|e| format!("[fs error: {e}]"))
    }
}

struct WaitSlot {
    epoch: u64,
    sender: Option<oneshot::Sender<String>>,
}
/// One-waiter bridge. Registration happens before the native future first yields.
pub struct EventBridge {
    slot: Mutex<WaitSlot>,
    registered: Notify,
    delivered: Mutex<Vec<String>>,
}
impl EventBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(WaitSlot {
                epoch: 0,
                sender: None,
            }),
            registered: Notify::new(),
            delivered: Mutex::new(vec![]),
        })
    }
    pub async fn waiter_registered(&self) {
        if self.has_waiter() {
            return;
        }
        self.registered.notified().await;
    }
    pub fn has_waiter(&self) -> bool {
        self.slot.lock().expect("wait slot").sender.is_some()
    }
    fn register(self: &Arc<Self>) -> Result<(u64, oneshot::Receiver<String>)> {
        let (tx, rx) = oneshot::channel();
        let epoch = {
            let mut slot = self.slot.lock().expect("wait slot");
            if slot.sender.is_some() {
                bail!("events::next supports one waiter")
            }
            slot.epoch += 1;
            slot.sender = Some(tx);
            slot.epoch
        };
        self.registered.notify_waiters();
        Ok((epoch, rx))
    }
    fn clear(&self, epoch: u64) {
        let mut slot = self.slot.lock().expect("wait slot");
        if slot.epoch == epoch {
            slot.sender.take();
        }
    }
    pub fn try_deliver(&self, event: &ExternalEvent) -> bool {
        let text = match event {
            ExternalEvent::UserInput { text, .. } => text.clone(),
            ExternalEvent::Eof => return false,
        };
        let sender = self.slot.lock().expect("wait slot").sender.take();
        if let Some(sender) = sender {
            if sender.send(text.clone()).is_ok() {
                self.delivered.lock().expect("delivered").push(text);
                return true;
            }
        }
        false
    }
    pub fn delivered(&self) -> Vec<String> {
        self.delivered.lock().expect("delivered").clone()
    }
    async fn next(self: Arc<Self>) -> String {
        let Ok((epoch, rx)) = self.register() else {
            return "[events error: waiter already active]".into();
        };
        struct Guard {
            bridge: Arc<EventBridge>,
            epoch: u64,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                self.bridge.clear(self.epoch);
            }
        }
        let _guard = Guard {
            bridge: self.clone(),
            epoch,
        };
        rx.await.unwrap_or_else(|_| "[events closed]".into())
    }
}

#[derive(Default)]
struct HostScope {
    values: BTreeMap<String, Value>,
    known_sizes: BTreeMap<String, usize>,
}

pub struct Invocation {
    context: Context,
    runtime: RuneArc<RuntimeContext>,
    scope: HostScope,
    capture: Arc<Mutex<Capture>>,
    fs: RepoFs,
    bridge: Arc<EventBridge>,
    hang_started: Arc<Notify>,
    cancellations: Arc<Mutex<usize>>,
    units: usize,
}
impl Invocation {
    pub fn new(repo: impl AsRef<Path>, bridge: Arc<EventBridge>) -> Result<Self> {
        let root = repo
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalize repo {}", repo.as_ref().display()))?;
        if !root.is_dir() {
            bail!("repo is not a directory: {}", root.display())
        }
        let capture = Arc::new(Mutex::new(Capture::default()));
        let fs = RepoFs {
            root: Arc::new(root),
            reads: Arc::new(Mutex::new(0)),
        };
        let mut module = Module::new();
        module.ty::<RepoFs>()?;
        {
            let mut trait_ = module.define_trait([FILE_SYSTEM.name])?;
            trait_.docs([FILE_SYSTEM.summary])?;
            trait_
                .function("read")?
                .docs([FILE_SYSTEM.methods[0].0, FILE_SYSTEM.methods[0].1])?;
        }
        module.implement_trait::<RepoFs>(rune::item!(FileSystem))?;
        module.associated_function("read", RepoFs::read_rune)?;
        let docs = FILE_SYSTEM.render();
        module
            .function("doc", move |value: Value| -> String {
                if value.borrow_ref::<RepoFs>().is_ok() {
                    docs.clone()
                } else {
                    format!("[no capability documentation for {}]", value.type_info())
                }
            })
            .build()?;
        module
            .function("preview", |value: Value, start: i64, end: i64| -> String {
                let Ok(text) = value.borrow_string_ref() else {
                    return "[preview: value is not String]".into();
                };
                let (start, end) = (start.max(0) as usize, end.max(0) as usize);
                text.get(start.min(text.len())..end.min(text.len()))
                    .unwrap_or("")
                    .to_string()
            })
            .build()?;
        let mut events = Module::with_crate("events")?;
        let event_bridge = bridge.clone();
        events
            .function("next", move || {
                let b = event_bridge.clone();
                async move { b.next().await }
            })
            .build()?;
        let hang_started = Arc::new(Notify::new());
        let hang_signal = hang_started.clone();
        module
            .function("hang", move || {
                let signal = hang_signal.clone();
                async move {
                    signal.notify_waiters();
                    std::future::pending::<()>().await
                }
            })
            .build()?;
        let mut io = Module::with_crate_item("std", ["io"])?;
        let sink = capture.clone();
        io.function("print", move |text: &str| {
            sink.lock().expect("capture").append(text, false)
        })
        .build()?;
        let sink = capture.clone();
        io.function("println", move |text: &str| {
            sink.lock().expect("capture").append(text, true)
        })
        .build()?;
        let mut context = Context::with_config(false)?;
        context.install(module)?;
        context.install(events)?;
        context.install(io)?;
        let runtime = RuneArc::try_new(context.runtime()?)?;
        let mut scope = HostScope::default();
        scope.values.insert("fs".into(), Value::new(fs.clone())?);
        Ok(Self {
            context,
            runtime,
            scope,
            capture,
            fs,
            bridge,
            hang_started,
            cancellations: Arc::new(Mutex::new(0)),
            units: 0,
        })
    }
    pub fn bridge(&self) -> Arc<EventBridge> {
        self.bridge.clone()
    }
    pub async fn hang_started(&self) {
        self.hang_started.notified().await;
    }
    pub fn cancellations(&self) -> usize {
        *self.cancellations.lock().expect("cancellations")
    }
    pub fn reads(&self) -> usize {
        self.fs.reads()
    }
    pub fn units(&self) -> usize {
        self.units
    }
    pub fn binding_inventory(&self) -> Vec<String> {
        self.scope
            .values
            .iter()
            .map(|(name, value)| {
                let ty = value.type_info().to_string();
                match self.scope.known_sizes.get(name) {
                    Some(n) => format!("{name}: {ty} ({n} bytes, retained)"),
                    None => format!("{name}: {ty} (retained)"),
                }
            })
            .collect()
    }
    pub fn string_binding(&self, name: &str) -> Option<String> {
        self.scope
            .values
            .get(name)?
            .borrow_string_ref()
            .ok()
            .map(|s| s.to_string())
    }

    pub async fn execute(&mut self, source: &str) -> Result<String> {
        *self.capture.lock().expect("capture") = Capture::default();
        let promoted = promote_top_level_lets(source)?;
        let mut names: Vec<String> = self.scope.values.keys().cloned().collect();
        for name in &promoted.names {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let mut statics = Statics::new();
        for name in &names {
            statics.insert([name.as_str()])?;
        }
        let wrapped = format!("pub async fn __outrig_main() {{\n{}\n}}", promoted.source);
        let mut sources = Sources::new();
        sources.insert(Source::new(
            format!("activation-{}.rn", self.units + 1),
            wrapped,
        )?)?;
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&self.context)
            .with_statics(&statics)
            .with_diagnostics(&mut diagnostics)
            .build();
        if !diagnostics.is_empty() {
            let mut bytes = Vec::new();
            diagnostics.emit(&mut rune::termcolor::NoColor::new(&mut bytes), &sources)?;
            let text = String::from_utf8_lossy(&bytes);
            self.capture.lock().expect("capture").append(&text, false);
        }
        let unit = RuneArc::try_new(unit.context("compile Rune activation")?)?;
        self.units += 1;
        let globals = Globals::new(unit.clone())?;
        for (name, value) in &self.scope.values {
            globals.set([name.as_str()], value.clone())?;
        }
        let mut vm = Vm::new(self.runtime.clone(), unit).with_globals(globals.clone());
        let mut execution = vm.execute(["__outrig_main"], ())?.into_owned();
        struct RunGuard {
            counter: Arc<Mutex<usize>>,
            complete: bool,
        }
        impl Drop for RunGuard {
            fn drop(&mut self) {
                if !self.complete {
                    *self.counter.lock().expect("cancellations") += 1;
                }
            }
        }
        let mut guard = RunGuard {
            counter: self.cancellations.clone(),
            complete: false,
        };
        execution
            .async_complete()
            .await
            .context("execute Rune activation")?;
        guard.complete = true;
        for name in names {
            if let Some(value) = globals.get([name.as_str()])? {
                if let Ok(s) = value.borrow_string_ref() {
                    self.scope.known_sizes.insert(name.clone(), s.len());
                }
                self.scope.values.insert(name, value);
            }
        }
        Ok(self.observation())
    }
    fn observation(&self) -> String {
        let capture = self.capture.lock().expect("capture");
        let inventory = if self.scope.values.is_empty() {
            "(none)".into()
        } else {
            self.binding_inventory().join(", ")
        };
        let suffix0 = format!("\n---\nattempted_bytes={} captured_bytes={{}} truncated={{}}\nretained_bindings: {inventory}", capture.attempted);
        let room = MODEL_VISIBLE_LIMIT.saturating_sub(suffix0.len() + 32);
        let take = room.min(capture.bytes.len());
        let truncated = capture.truncated || take < capture.bytes.len();
        let mut result = String::from_utf8_lossy(&capture.bytes[..take]).into_owned();
        result.push_str(&format!("\n---\nattempted_bytes={} captured_bytes={} truncated={}\nretained_bindings: {inventory}", capture.attempted, take, truncated));
        if result.len() > MODEL_VISIBLE_LIMIT {
            let mut end = MODEL_VISIBLE_LIMIT;
            while !result.is_char_boundary(end) {
                end -= 1;
            }
            result.truncate(end);
        }
        result
    }
}

struct Promoted {
    source: String,
    names: Vec<String>,
}
/// Bounded prototype transformer: recognizes top-level `let IDENT =` while skipping
/// strings/comments/nested delimiters, and rewrites it to a predeclared static assignment.
fn promote_top_level_lets(input: &str) -> Result<Promoted> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut names = vec![];
    let (mut i, mut depth) = (0usize, 0i32);
    let mut quote = None;
    let mut line_comment = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if line_comment {
            out.push(c);
            i += 1;
            if c == '\n' {
                line_comment = false;
            }
            continue;
        }
        if let Some(q) = quote {
            out.push(c);
            i += 1;
            if c == '\\' && i < bytes.len() {
                out.push(bytes[i] as char);
                i += 1;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            out.push_str("//");
            i += 2;
            line_comment = true;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        if matches!(c, '(' | '[' | '{') {
            depth += 1;
            out.push(c);
            i += 1;
            continue;
        }
        if matches!(c, ')' | ']' | '}') {
            depth -= 1;
            out.push(c);
            i += 1;
            continue;
        }
        if depth == 0
            && input[i..].starts_with("let")
            && (i == 0 || !is_ident(bytes[i - 1] as char))
            && (i + 3 == bytes.len() || !is_ident(bytes[i + 3] as char))
        {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            let start = j;
            if j < bytes.len() && ((bytes[j] as char).is_ascii_alphabetic() || bytes[j] == b'_') {
                j += 1;
                while j < bytes.len() && is_ident(bytes[j] as char) {
                    j += 1;
                }
            }
            if j == start {
                bail!("top-level let pattern must bind an identifier")
            }
            let name = &input[start..j];
            let mut k = j;
            while k < bytes.len() && (bytes[k] as char).is_whitespace() {
                k += 1;
            }
            if k >= bytes.len() || bytes[k] != b'=' {
                bail!("top-level let {name} must use a simple `let name = value` pattern")
            }
            names.push(name.to_string());
            out.push_str(name);
            out.push_str(&input[j..=k]);
            i = k + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(Promoted { source: out, names })
}
fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub struct AgentDriver<M> {
    pub model: M,
    invocation: Invocation,
    input: mpsc::UnboundedReceiver<ExternalEvent>,
    output: mpsc::UnboundedSender<String>,
    queued: VecDeque<ExternalEvent>,
    instructions: String,
}
impl<M: ModelBackend> AgentDriver<M> {
    pub fn new(
        model: M,
        invocation: Invocation,
        input: mpsc::UnboundedReceiver<ExternalEvent>,
        output: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self { model, invocation, input, output, queued: VecDeque::new(), instructions: "Return exactly one Decision. Execute Rune to observe or retain state; Emit is the only user-facing output. No provider history exists. Available: persistent fs, doc(value), preview(value,start,end), events::next().await. Discover filesystem capability with doc(fs), then call fs.read(relative_path).".into() }
    }
    fn request(&self, cause: ActivationCause, observation: String) -> ActivationRequest {
        ActivationRequest {
            instructions: self.instructions.clone(),
            cause,
            observation,
            capability_summaries: vec![
                FILE_SYSTEM.render(),
                "events::next().await — await one external terminal event in the same Rune run"
                    .into(),
            ],
            bindings: self.invocation.binding_inventory(),
        }
    }
    pub async fn run(mut self) -> Result<Self> {
        loop {
            let event = match self.queued.pop_front() {
                Some(e) => e,
                None => match self.input.recv().await {
                    Some(e) => e,
                    None => ExternalEvent::Eof,
                },
            };
            if event == ExternalEvent::Eof {
                break;
            }
            let mut cause = ActivationCause::ExternalEvent(event);
            let mut observation = String::new();
            loop {
                let decision = self
                    .model
                    .activate(self.request(cause.clone(), observation.clone()))
                    .await?;
                while let Ok(e) = self.input.try_recv() {
                    self.queued.push_back(e);
                }
                match decision {
                    Decision::Emit { text } => {
                        let _ = self.output.send(text);
                        break;
                    }
                    Decision::ExecuteRune { source } => {
                        // Events that arrived while Activating were not awaited by a Rune run;
                        // preserve order and make the oldest the next exact activation cause.
                        if let Some(event) = self.queued.pop_front() {
                            if event == ExternalEvent::Eof {
                                return Ok(self);
                            }
                            observation.clear();
                            cause = ActivationCause::ExternalEvent(event);
                            continue;
                        }
                        let bridge = self.invocation.bridge();
                        let mut run = Box::pin(self.invocation.execute(&source));
                        enum RunEnd {
                            Completed(Result<String>),
                            Interrupted(ExternalEvent),
                            Eof,
                        }
                        let outcome = loop {
                            tokio::select! { biased;
                                done = &mut run => break RunEnd::Completed(done),
                                incoming = self.input.recv() => {
                                    let incoming = incoming.unwrap_or(ExternalEvent::Eof);
                                    if incoming == ExternalEvent::Eof { break RunEnd::Eof; }
                                    if bridge.try_deliver(&incoming) { continue; }
                                    break RunEnd::Interrupted(incoming);
                                }
                            }
                        };
                        drop(run); // owned Rune execution cancellation is cooperative and commit-as-executed.
                        match outcome {
                            RunEnd::Completed(done) => {
                                observation = done.unwrap_or_else(|e| format!("Rune error: {e:#}"));
                                cause = ActivationCause::RuneObservation(observation.clone());
                            }
                            RunEnd::Interrupted(event) => {
                                observation.clear();
                                cause = ActivationCause::ExternalEvent(event);
                            }
                            RunEnd::Eof => return Ok(self),
                        }
                    }
                }
            }
        }
        Ok(self)
    }
}

pub fn channels() -> (
    mpsc::UnboundedSender<ExternalEvent>,
    mpsc::UnboundedReceiver<ExternalEvent>,
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<String>,
) {
    let (itx, irx) = mpsc::unbounded_channel();
    let (otx, orx) = mpsc::unbounded_channel();
    (itx, irx, otx, orx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn inv(dir: &TempDir) -> Result<Invocation> {
        Invocation::new(dir.path(), EventBridge::new())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scope_across_units_and_arbitrary_binding() -> Result<()> {
        let d = TempDir::new()?;
        let mut i = inv(&d)?;
        i.execute("let arbitrary_name_42 = \"hello\";").await?;
        let o = i.execute("println!(\"{}\", arbitrary_name_42);").await?;
        assert!(o.starts_with("hello"));
        assert_eq!(i.units(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn global_fs_has_python_like_dynamic_docs_across_units() -> Result<()> {
        let d = TempDir::new()?;
        let mut i = inv(&d)?;
        i.execute("let unrelated = 7;").await?;
        let docs = i.execute("println!(\"{}\", doc(fs));").await?;
        assert!(docs.contains("trait FileSystem"), "got: {docs}");
        assert!(
            docs.contains("fn read(path: String) -> String"),
            "got: {docs}"
        );
        assert_eq!(i.units(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_file_read_once_retained_and_previewed() -> Result<()> {
        let d = TempDir::new()?;
        std::fs::write(d.path().join("big"), "α".repeat(50_000))?;
        let mut i = inv(&d)?;
        let first = i
            .execute("let source = fs.read(\"big\"); println!(\"{}\", source);")
            .await?;
        assert!(first.len() <= MODEL_VISIBLE_LIMIT);
        assert_eq!(i.reads(), 1);
        let second = i
            .execute("println!(\"{}\", preview(source, 0, 20));")
            .await?;
        assert!(second.contains("α"));
        assert_eq!(i.reads(), 1);
        assert_eq!(i.string_binding("source").unwrap().len(), 100_000);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn awaited_event_resumes_same_run_and_retains() -> Result<()> {
        let d = TempDir::new()?;
        let mut i = inv(&d)?;
        let bridge = i.bridge();
        let observation = {
            let run = i.execute(
                "let received_event = events::next().await; println!(\"{}\", received_event);",
            );
            tokio::pin!(run);
            tokio::select! { biased; r=&mut run => r?, _=bridge.waiter_registered()=> { assert!(bridge.try_deliver(&ExternalEvent::UserInput{id:1,text:"hello".into()})); run.as_mut().await? } }
        };
        assert!(observation.contains("hello"));
        assert_eq!(i.string_binding("received_event").as_deref(), Some("hello"));
        assert_eq!(bridge.delivered(), vec!["hello"]);
        Ok(())
    }

    #[test]
    fn path_boundary_rejects_traversal_and_symlink_escape() -> Result<()> {
        let base = TempDir::new()?;
        let repo = base.path().join("repo");
        std::fs::create_dir(&repo)?;
        std::fs::write(base.path().join("outside"), "no")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.path().join("outside"), repo.join("link"))?;
        let fs = RepoFs {
            root: Arc::new(repo.canonicalize()?),
            reads: Arc::new(Mutex::new(0)),
        };
        assert!(fs.read("../outside").is_err());
        #[cfg(unix)]
        assert!(fs.read("link").is_err());
        Ok(())
    }

    #[test]
    fn output_cap_includes_suffix() {
        let c = Capture {
            bytes: vec![b'x'; MODEL_VISIBLE_LIMIT],
            attempted: MODEL_VISIBLE_LIMIT,
            truncated: false,
        };
        let d = TempDir::new().unwrap();
        let i = inv(&d).unwrap();
        *i.capture.lock().unwrap() = c;
        assert!(i.observation().len() <= MODEL_VISIBLE_LIMIT);
        assert!(i.observation().contains("attempted_bytes="));
    }

    #[test]
    fn transformer_supports_identifiers_and_ignores_nested_let() -> Result<()> {
        let p = promote_top_level_lets("let any_identifier_7 = 1; if true { let local = 2; }")?;
        assert_eq!(p.names, vec!["any_identifier_7"]);
        assert!(p.source.contains("any_identifier_7 ="));
        assert!(p.source.contains("let local"));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fresh_activations_emit_idle_and_scope_retained() -> Result<()> {
        let d = TempDir::new()?;
        let invocation = inv(&d)?;
        let model = ScriptedModel::new([
            Decision::ExecuteRune {
                source: "let durable_name = \"kept\";".into(),
            },
            Decision::Emit {
                text: "first".into(),
            },
            Decision::Emit {
                text: "second".into(),
            },
        ]);
        let (tx, rx, otx, mut orx) = channels();
        tx.send(ExternalEvent::UserInput {
            id: 1,
            text: "remember".into(),
        })?;
        let driver = AgentDriver::new(model, invocation, rx, otx);
        let mut running = Box::pin(driver.run());
        assert_eq!(
            tokio::select! {v=orx.recv()=>v.unwrap(), _r=&mut running=>panic!("driver ended unexpectedly")},
            "first"
        );
        tx.send(ExternalEvent::UserInput {
            id: 2,
            text: "later".into(),
        })?;
        assert_eq!(
            tokio::select! {v=orx.recv()=>v.unwrap(), _r=&mut running=>panic!("driver ended unexpectedly")},
            "second"
        );
        tx.send(ExternalEvent::Eof)?;
        let driver = running.await?;
        assert_eq!(driver.model.requests.len(), 3);
        assert!(matches!(
            driver.model.requests[1].cause,
            ActivationCause::RuneObservation(_)
        ));
        assert!(driver.model.requests[1]
            .bindings
            .iter()
            .any(|b| b.starts_with("durable_name:")));
        assert!(matches!(
            driver.model.requests[2].cause,
            ActivationCause::ExternalEvent(ExternalEvent::UserInput { id: 2, .. })
        ));
        let serialized = serde_json::to_string(&driver.model.requests[1])?;
        assert!(
            !serialized.contains("let durable_name"),
            "previous model response leaked into activation"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn driver_awaited_event_is_rune_observation_not_external_cause() -> Result<()> {
        let d = TempDir::new()?;
        let bridge = EventBridge::new();
        let invocation = Invocation::new(d.path(), bridge.clone())?;
        let model = ScriptedModel::new([
            Decision::ExecuteRune {
                source: "let awaited_value = events::next().await;".into(),
            },
            Decision::Emit {
                text: "done".into(),
            },
        ]);
        let (tx, rx, otx, mut orx) = channels();
        tx.send(ExternalEvent::UserInput {
            id: 1,
            text: "start".into(),
        })?;
        let mut running = Box::pin(AgentDriver::new(model, invocation, rx, otx).run());
        tokio::select! {_=bridge.waiter_registered()=>{}, _r=&mut running=>panic!("driver ended unexpectedly")};
        tx.send(ExternalEvent::UserInput {
            id: 2,
            text: "awaited".into(),
        })?;
        assert_eq!(
            tokio::select! {v=orx.recv()=>v.unwrap(), _r=&mut running=>panic!("driver ended unexpectedly")},
            "done"
        );
        tx.send(ExternalEvent::Eof)?;
        let driver = running.await?;
        assert!(matches!(
            driver.model.requests[1].cause,
            ActivationCause::RuneObservation(_)
        ));
        assert_eq!(
            driver.invocation.string_binding("awaited_value").as_deref(),
            Some("awaited")
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn driver_unawaited_event_cancels_and_becomes_exact_cause() -> Result<()> {
        let d = TempDir::new()?;
        let invocation = inv(&d)?;
        let started = invocation.hang_started.clone();
        let model = ScriptedModel::new([
            Decision::ExecuteRune {
                source: "hang().await;".into(),
            },
            Decision::Emit {
                text: "interrupted".into(),
            },
        ]);
        let (tx, rx, otx, mut orx) = channels();
        tx.send(ExternalEvent::UserInput {
            id: 1,
            text: "start".into(),
        })?;
        let mut running = Box::pin(AgentDriver::new(model, invocation, rx, otx).run());
        tokio::select! {_=started.notified()=>{}, _r=&mut running=>panic!("driver ended unexpectedly")};
        tx.send(ExternalEvent::UserInput {
            id: 9,
            text: "requirements changed".into(),
        })?;
        assert_eq!(
            tokio::select! {v=orx.recv()=>v.unwrap(), _r=&mut running=>panic!("driver ended unexpectedly")},
            "interrupted"
        );
        tx.send(ExternalEvent::Eof)?;
        let driver = running.await?;
        assert_eq!(driver.invocation.cancellations(), 1);
        assert!(!driver.invocation.bridge().has_waiter());
        assert_eq!(
            driver.model.requests[1].cause,
            ActivationCause::ExternalEvent(ExternalEvent::UserInput {
                id: 9,
                text: "requirements changed".into()
            })
        );
        Ok(())
    }
}
