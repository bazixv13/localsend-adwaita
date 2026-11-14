use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let data_dir = Path::new(&manifest_dir).join("data");
    let xml = data_dir.join("resources.gresource.xml");
    let target = Path::new(&out_dir).join("resources.gresource");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", xml.display());
    println!(
        "cargo:rerun-if-changed={}",
        data_dir.join("style.css").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        data_dir
            .join("icons/scalable/apps/io.github.localsend_adwaita.LocalSend.svg")
            .display()
    );

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&data_dir)
        .arg("--target")
        .arg(&target)
        .arg(&xml)
        .status()
        .expect("failed to run glib-compile-resources (is glib2 dev installed?)");

    if !status.success() {
        panic!("glib-compile-resources failed with {status}");
    }
}
