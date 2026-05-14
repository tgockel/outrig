fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if cfg!(all(feature = "cuda", not(feature = "mistralrs"))) {
        println!(
            "cargo:warning=feature `cuda` has no effect unless feature `mistralrs` is also enabled"
        );
    }
    if cfg!(all(feature = "metal", not(feature = "mistralrs"))) {
        println!(
            "cargo:warning=feature `metal` has no effect unless feature `mistralrs` is also enabled"
        );
    }
}
