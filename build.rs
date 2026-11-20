fn main() {
    // WinFSP is only needed when a branch is mounted. Linking it as a normal import makes every
    // command fail to start on a machine without WinFSP installed, with a message about a missing DLL
    // that says nothing about mount. A delay load keeps `status` working and makes the dependency
    // appear only where it is used.
    //
    // The call is behind `cfg(windows)` rather than a runtime check: `winfsp` is declared under
    // `[target.'cfg(windows)'.build-dependencies]`, so on any other host the crate is not in scope and
    // the call would not compile even where it is never made.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();

    println!("cargo:rerun-if-changed=build.rs");
}
