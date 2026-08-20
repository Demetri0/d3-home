//! Puts the icon into the executable's own resources on Windows.
//!
//! That is how a program carries its icon on that platform: Explorer, the
//! task bar and `ExtractAssociatedIcon` all read it from there. Windows has
//! neither an icon theme to name nor a convention of a file installed beside
//! the binary, so the notification code asks the running executable for its
//! own icon rather than looking anywhere on disk.
//!
//! The dependency is declared for Windows *hosts*, which means this path is
//! taken when building on a Windows machine -- what the release workflow
//! does. Cross-compiling to Windows from elsewhere produces a working
//! program with no icon resource, and the notification code falls back to
//! the system icon.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=../../assets/icons/d3home.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon();
    }
}

#[cfg(windows)]
fn embed_icon() {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("../../assets/icons/d3home.ico");
    if let Err(err) = resource.compile() {
        // Not fatal: a program without its icon still works, and a failure
        // here should not stop somebody building from source.
        println!("cargo::warning=could not embed the icon: {err}");
    }
}

/// Cross-compiling to Windows from somewhere else: the resource compiler is
/// not here, and neither is the crate that drives it.
#[cfg(not(windows))]
fn embed_icon() {
    println!(
        "cargo::warning=building for Windows from another platform, so the icon is not embedded"
    );
}
