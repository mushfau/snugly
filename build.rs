//! Builds the vendored libbsc (Apache-2.0) without OpenMP or CUDA; snugly runs blocks in
//! parallel itself.
fn main() {
    let root = std::path::Path::new("vendor/libbsc/src");
    let cpp = [
        "adler32/adler32.cpp",
        "bwt/bwt.cpp",
        "coder/coder.cpp",
        "coder/qlfc/qlfc.cpp",
        "coder/qlfc/qlfc_model.cpp",
        "filters/detectors.cpp",
        "filters/preprocessing.cpp",
        "libbsc/libbsc.cpp",
        "lzp/lzp.cpp",
        "platform/platform.cpp",
        "st/st.cpp",
    ];
    cc::Build::new()
        .cpp(true)
        .files(cpp.iter().map(|f| root.join(f)))
        .flag_if_supported("-std=c++14")
        // snugly only writes BWT blocks; leave the sort-transform decoder out entirely.
        .define("LIBBSC_NO_SORT_TRANSFORM", None)
        .flag_if_supported("-w")
        .warnings(false)
        .compile("bsc");
    cc::Build::new().file(root.join("bwt/libsais/libsais.c")).warnings(false).flag_if_supported("-w").compile("bsc_sais");
    println!("cargo:rerun-if-changed=vendor/libbsc");
}
