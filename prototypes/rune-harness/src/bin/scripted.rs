//! Small provider-free smoke proof; comprehensive deterministic coverage lives in lib tests.
use anyhow::Result;
use outrig_rune_harness_prototype::{EventBridge, Invocation, MODEL_VISIBLE_LIMIT};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let root = std::env::temp_dir().join(format!("outrig-rune-scripted-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("large.txt"), "x".repeat(100_000))?;
    let mut invocation = Invocation::new(&root, EventBridge::new())?;
    let docs = invocation.execute("println!(\"{}\", doc(fs));").await?;
    assert!(docs.contains("fn read(path: String) -> String"));
    let first = invocation
        .execute(
            "let arbitrary_source = fs.read(\"large.txt\"); println!(\"{}\", arbitrary_source);",
        )
        .await?;
    let second = invocation
        .execute("preview(arbitrary_source, 50000, 50100)")
        .await?;
    assert!(first.len() <= MODEL_VISIBLE_LIMIT);
    assert!(second.starts_with(&format!("final expression: {}", "x".repeat(100))));
    assert_eq!(invocation.reads(), 1);
    println!("event_driven=true units={} reads={} retained=arbitrary_source output_cap={MODEL_VISIBLE_LIMIT}", invocation.units(), invocation.reads());
    Ok(())
}
