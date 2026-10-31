//! The agent runs in a freshly installed Windows, which has no Visual C++
//! runtime (`VCRUNTIME140.dll`): link the C runtime statically, for this
//! binary only. Swapping the default import libraries for their static
//! twins is what `-C target-feature=+crt-static` does, without making every
//! other binary in the workspace static too.

fn main() {
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if !msvc {
        return;
    }
    for lib in ["msvcrt.lib", "vcruntime.lib", "ucrt.lib"] {
        println!("cargo:rustc-link-arg-bins=/NODEFAULTLIB:{lib}");
    }
    for lib in ["libcmt.lib", "libvcruntime.lib", "libucrt.lib"] {
        println!("cargo:rustc-link-arg-bins=/DEFAULTLIB:{lib}");
    }
}
