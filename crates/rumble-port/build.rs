//! Gives the exe the setup app's icon (see `src/icon.rs`).

#[allow(dead_code)]
#[path = "src/icon.rs"]
mod icon;

fn main() {
    let png = "assets/setup-icon.png";
    println!("cargo:rerun-if-changed={png}");
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let image = icon::Image::from_png(&std::fs::read(png).expect("reading the setup icon")).expect("decoding the setup icon");
    let res = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("icon.res");
    std::fs::write(&res, icon::res_file(&image.icon_images())).unwrap();
    // link.exe takes compiled resources as input files.
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
