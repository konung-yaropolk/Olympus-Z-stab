//! Embed the icon and the version information into the exe.
//!
//! Windows-only, and deliberately not fatal. `winres` shells out to the resource
//! compiler, which is present with the MSVC toolchain and absent on a cross
//! build — and a missing icon is not a reason to fail a build whose actual
//! deliverable is a working stabiliser. A warning says what happened.

fn main() {
    if !cfg!(target_os = "windows") {
        return;
    }
    println!("cargo:rerun-if-changed=icon/icon.ico");
    let mut res = winres::WindowsResource::new();
    res.set_icon("icon/icon.ico");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the icon ({e}); building without one");
    }
}
