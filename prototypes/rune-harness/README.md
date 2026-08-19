# Rune harness prototype

A deliberately small deterministic CodeAct vertical slice. This directory is its own Cargo workspace, so its dependencies do not enter OutRig's production workspace.

Rune is pinned to released **`rune = "=0.14.2"`**.

```console
$ cargo run --release --manifest-path prototypes/rune-harness/Cargo.toml
rune=0.14.2 units_compiled=3
source bytes=100000000 builds=1 reads=1
large_output captured=16384 attempted=100000001 truncated=true
slice captured=10001 payload_x=true truncated=false
result=FileAnalysis { bytes: 100000000, inspected_start: 50000000, inspected_end: 50010000 }
model_requests=4 unused_scripts=0
```

Verify:

```console
cargo fmt --manifest-path prototypes/rune-harness/Cargo.toml -- --check
cargo test --manifest-path prototypes/rune-harness/Cargo.toml
cargo run --release --manifest-path prototypes/rune-harness/Cargo.toml
```

## Honest boundaries

`CapabilityDef` is the single source for `Module::define_trait(["FileSystem"])` registration and `doc(fs)` rendering. A parallel public registry is necessary because Rune runtime doc lookup is private. Each execute step prepares a fresh Rune `Unit`. The 100 MB string stays live as a `rune::Value`; bounded print copies at most 16 KiB.

The model-facing read remains `let source = fs.read(path).await?; println!("{}", source);`. Rune 0.14.2 supports the registered trait, but the deterministic host method is synchronous, so the deliberately narrow three-program transform removes `.await?`, supplies `/fixture`, redirects print, and stores `source`. It is not a parser. The later supported syntax is `source[50_000_000..50_010_000]`, transformed only for that bare retained binding. A thread-local slot is used because `rune::Value` is `!Send`; this prototype is single-threaded. Typed completion uses the allowed direct scripted return response rather than a Rune callback. No HTTP, Rig, CocoClaw mock, concurrency, persistence, external interruption, Podman integration, or stable API is implemented.
