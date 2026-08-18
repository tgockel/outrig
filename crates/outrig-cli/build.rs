fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // The deprecation, said at the moment the cost is being paid. A user
    // enabling this feature is about to compile several hundred extra crates,
    // which is the point at which "this is going away" is most worth hearing --
    // and unlike the runtime warning, it reaches whoever builds outrig even if
    // they never run a local model themselves (CI, a distro packager).
    if cfg!(feature = "local-llm") {
        println!(
            "cargo:warning=feature `local-llm` is deprecated and will be removed in a \
             future release; run local models under an OpenAI-compatible server (Ollama, \
             vLLM, llama.cpp) and point a style=\"openai\" provider at its localhost \
             base-url instead"
        );
    }

    // These two stay worded as they were. They report a build that asked for a
    // backend it cannot use, which is a mistake to fix rather than a deprecation
    // to plan around -- and the `local-llm` warning above already fires for
    // anyone who resolves them the recommended way.
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
