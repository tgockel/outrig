use anyhow::{bail, Context as _, Result};
use rune::runtime::Value;
use rune::{Any, Context, Diagnostics, Module, Source, Sources, Vm};
use serde::Deserialize;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MODEL_VISIBLE_LIMIT: usize = 16 * 1024;
thread_local! { static SOURCE: RefCell<Option<Value>> = const { RefCell::new(None) }; }

fn store_source(value: Value) {
    SOURCE.with(|slot| *slot.borrow_mut() = Some(value));
}
fn load_source() -> Value {
    SOURCE.with(|slot| slot.borrow().as_ref().expect("source checked").clone())
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    attempted: usize,
    truncated: bool,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub reads: usize,
    pub units: usize,
}

#[derive(Any, Clone)]
struct FsHandle {
    selected: Arc<PathBuf>,
    reads: Arc<Mutex<usize>>,
    pending: Arc<Mutex<Option<String>>>,
}
impl FsHandle {
    fn read(&self, path: &str) -> String {
        let expected = self.selected.to_string_lossy();
        assert_eq!(path, expected, "transform must bind the selected path");
        *self.reads.lock().expect("read count") += 1;
        self.pending
            .lock()
            .expect("pending selected file")
            .take()
            .expect("execute preloads exactly one selected-file read")
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
    summary: "Read-only access to the one scenario-selected file.",
    methods: &[MethodDef {
        signature: "async fn read(path: String) -> String",
        summary: "Read the selected file; no other path is permitted.",
    }],
};
impl CapabilityDef {
    fn render(&self) -> String {
        let mut out = format!("trait {} — {}\n", self.name, self.summary);
        for method in self.methods {
            out += &format!("  {} — {}\n", method.signature, method.summary);
        }
        out
    }
    fn register(&self, module: &mut Module) -> Result<()> {
        let mut trait_ = module.define_trait([self.name])?;
        trait_.docs([self.summary])?;
        for method in self.methods {
            trait_
                .function("read")?
                .docs([method.signature, method.summary])?;
        }
        drop(trait_);
        module.implement_trait::<FsHandle>(rune::item!(FileSystem))?;
        Ok(())
    }
}

pub struct Invocation {
    context: Context,
    runtime: Arc<rune::runtime::RuntimeContext>,
    capture: Arc<Mutex<Capture>>,
    reads: Arc<Mutex<usize>>,
    pending: Arc<Mutex<Option<String>>>,
    selected: PathBuf,
    units: usize,
}
impl Invocation {
    pub fn new(selected: PathBuf) -> Result<Self> {
        SOURCE.with(|slot| *slot.borrow_mut() = None);
        let capture = Arc::new(Mutex::new(Capture::default()));
        let reads = Arc::new(Mutex::new(0));
        let pending = Arc::new(Mutex::new(None));
        let fs = FsHandle {
            selected: Arc::new(selected.clone()),
            reads: reads.clone(),
            pending: pending.clone(),
        };
        let mut module = Module::new();
        FS.register(&mut module)?;
        module.ty::<FsHandle>()?;
        module.associated_function("read", FsHandle::read)?;
        module.function("fs", move || fs.clone()).build()?;
        let docs = FS.render();
        module.function("doc_fs", move || docs.clone()).build()?;
        module.function("store_source", store_source).build()?;
        module.function("load_source", load_source).build()?;
        let sink = capture.clone();
        module
            .function("bounded_print", move |text: &str| {
                let mut output = sink.lock().expect("capture");
                output.attempted += text.len() + 1;
                let room = MODEL_VISIBLE_LIMIT.saturating_sub(output.bytes.len());
                let take = room.min(text.len());
                output.bytes.extend_from_slice(&text.as_bytes()[..take]);
                output.truncated |= take < text.len();
                if output.bytes.len() < MODEL_VISIBLE_LIMIT {
                    output.bytes.push(b'\n');
                } else {
                    output.truncated = true;
                }
            })
            .build()?;
        let mut context = Context::with_config(false)?;
        context.install(module)?;
        let runtime = Arc::new(context.runtime()?);
        Ok(Self {
            context,
            runtime,
            capture,
            reads,
            pending,
            selected,
            units: 0,
        })
    }

    pub fn execute(&mut self, source: &str) -> Result<String> {
        *self.capture.lock().expect("capture") = Capture::default();
        let has_source = self.source_len().is_some();
        let is_first_read = !has_source
            && compact(source) == r#"letsource=fs.read(path).await?;println!("{}",source);"#;
        if is_first_read {
            let text = std::fs::read_to_string(&self.selected)
                .with_context(|| format!("read selected file {}", self.selected.display()))?;
            *self.pending.lock().expect("pending selected file") = Some(text);
        }
        let transformed = transform(source, &self.selected, has_source)?;
        let mut sources = Sources::new();
        sources.insert(Source::new(
            format!("step-{}.rn", self.units + 1),
            transformed,
        )?)?;
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&self.context)
            .with_diagnostics(&mut diagnostics)
            .build();
        if !diagnostics.is_empty() {
            let mut stderr =
                rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Never);
            diagnostics.emit(&mut stderr, &sources)?;
        }
        let unit = unit.context("compile transformed Rune")?;
        self.units += 1;
        Vm::new(self.runtime.clone(), Arc::new(unit))
            .call(["main"], ())
            .context("execute Rune")?;
        Ok(self.render_result())
    }

    fn render_result(&self) -> String {
        let capture = self.capture.lock().expect("capture");
        let inventory = self
            .source_len()
            .map(|n| format!("source: String ({n} bytes, retained)"))
            .unwrap_or_else(|| "(none)".into());
        let suffix = format!("\n---\nattempted_bytes={} captured_bytes={{captured}} truncated={}\nretained_bindings: {}", capture.attempted, capture.truncated, inventory);
        let reserve = suffix.len() + 24;
        let take = capture
            .bytes
            .len()
            .min(MODEL_VISIBLE_LIMIT.saturating_sub(reserve));
        let mut out = String::from_utf8_lossy(&capture.bytes[..take]).into_owned();
        let truncated = capture.truncated || take < capture.bytes.len();
        let suffix = format!(
            "\n---\nattempted_bytes={} captured_bytes={} truncated={}\nretained_bindings: {}",
            capture.attempted, take, truncated, inventory
        );
        out.push_str(&suffix);
        if out.len() > MODEL_VISIBLE_LIMIT {
            out.truncate(MODEL_VISIBLE_LIMIT);
        }
        out
    }

    pub fn stats(&self) -> Stats {
        Stats {
            reads: *self.reads.lock().expect("reads"),
            units: self.units,
        }
    }
    pub fn source_len(&self) -> Option<usize> {
        SOURCE.with(|slot| {
            slot.borrow()
                .as_ref()
                .and_then(|v| v.borrow_string_ref().ok().map(|s| s.len()))
        })
    }
}

fn compact(source: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in source.chars() {
        if quoted {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    out
}

pub fn transform(source: &str, selected: &Path, has_source: bool) -> Result<String> {
    let source = compact(source);
    let body = if source == r#"println!("{}",doc(fs));"# {
        "bounded_print(doc_fs());".to_string()
    } else if source == r#"letsource=fs.read(path).await?;println!("{}",source);"# {
        if has_source {
            "bounded_print(load_source());".to_string()
        } else {
            let path = serde_json::to_string(&selected.to_string_lossy().as_ref())?;
            format!("let fs=fs();let path={path};store_source(fs.read(path));bounded_print(load_source());")
        }
    } else if let Some(range) = source
        .strip_prefix(r#"println!("{}",source["#)
        .and_then(|s| s.strip_suffix("]);"))
    {
        if !has_source {
            bail!("source is not retained yet; first use: let source = fs.read(path).await?; println!(\"{}\", source);")
        }
        let (start, end) = range.split_once("..").context("slice must be START..END")?;
        let start: usize = start.parse().context("slice START must be numeric")?;
        let end: usize = end.parse().context("slice END must be numeric")?;
        if start > end {
            bail!("slice START must not exceed END")
        }
        format!("bounded_print(load_source()[{start}..{end}]);")
    } else {
        bail!("unsupported Rune; accepted forms: println!(\"{}\", doc(fs)); | let source = fs.read(path).await?; println!(\"{}\", source); | println!(\"{}\", source[START..END]);")
    };
    Ok(format!("pub fn main(){{{body}}}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteArgs {
    pub source: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transformer_tolerates_whitespace_and_numeric_ranges() {
        assert!(transform(" println!( \"{}\", doc( fs ) ); ", Path::new("/x"), false).is_ok());
        assert!(transform("println!(\"{}\", source[ 2 .. 9 ]);", Path::new("/x"), true).is_ok());
        assert!(transform("println!(\"{}\", source[a..9]);", Path::new("/x"), true).is_err());
    }
    #[test]
    fn read_is_persistent_and_results_are_bounded() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("outrig-rune-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("fixture");
        std::fs::write(&file, "x".repeat(40_000))?;
        let mut invocation = Invocation::new(file)?;
        let read = r#"let source = fs.read(path).await?; println!(\"{}\", source);"#;
        assert!(invocation.execute(read)?.len() <= MODEL_VISIBLE_LIMIT);
        invocation.execute(read)?;
        assert_eq!(invocation.stats().reads, 1);
        assert_eq!(invocation.source_len(), Some(40_000));
        Ok(())
    }
}
