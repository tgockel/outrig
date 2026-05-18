fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if cfg!(all(feature = "cuda", not(feature = "local-llm"))) {
        println!(
            "cargo:warning=feature `cuda` has no effect unless feature `local-llm` is also enabled"
        );
    }
    if cfg!(all(feature = "metal", not(feature = "local-llm"))) {
        println!(
            "cargo:warning=feature `metal` has no effect unless feature `local-llm` is also enabled"
        );
    }
}
