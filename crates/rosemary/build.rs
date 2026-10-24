fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/glu/sgi");
    cc::Build::new()
        .include("src/glu/sgi")
        .files([
            "src/glu/sgi/project.c",
            "src/glu/sgi/quad.c",
            "src/glu/sgi/mipmap.c",
        ])
        .warnings(false)
        .compile("rosemary_glu");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("freebsd") {
        return;
    }
    println!("cargo::rerun-if-env-changed=LOCALBASE");
    let base = std::env::var("LOCALBASE").unwrap_or_else(|_| "/usr/local".into());
    println!("cargo::rustc-link-search=native={base}/lib");
}
