// Embeds the Windows resources into the executable: the application icon (`assets/xpra.ico`, also
// what the tray icon is loaded from at runtime - see `src/client/tray.rs`) and the DPI-awareness
// manifest (`exe.manifest`, which until now was carried in the repo but referenced by nothing).
//
// The gate is `CARGO_CFG_TARGET_OS`, not `cfg!(windows)`: a build script is compiled and run for
// the *host*, so `cfg!(windows)` would ask the wrong question and silently drop the resources from
// a Linux -> Windows cross build. For the same reason `winresource` is a plain build-dependency
// rather than a `[target.'cfg(windows)'.build-dependencies]` one, which Cargo also resolves against
// the host. It is a pure-Rust build-time dependency; nothing of it ends up in the binary.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/xpra.ico");
    println!("cargo:rerun-if-changed=exe.manifest");

    let mut res = winresource::WindowsResource::new();
    // name id "1": `tray.rs` loads this same icon back at runtime with `LoadImageW(hinst, 1, ..)`,
    // and the lowest integer id is also the one the shell picks as the executable's icon.
    res.set_icon_with_id("assets/xpra.ico", "1");
    // per-monitor-V2 DPI awareness. winit asks for the same level programmatically when it builds
    // the event loop (`become_dpi_aware`), so this only makes it authoritative from process start.
    res.set_manifest_file("exe.manifest");
    if let Err(e) = res.compile() {
        // don't fail the build over this: without the resource the executable simply has no icon,
        // and the tray falls back to the stock application icon (see `Tray::load_icon`).
        println!("cargo:warning=failed to compile the Windows resources: {e}");
    }
}
