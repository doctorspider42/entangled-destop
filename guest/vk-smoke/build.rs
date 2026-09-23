//! Compiles the WGSL shaders in `shaders/` to SPIR-V with naga, so building
//! vk-smoke needs neither the Vulkan SDK nor glslang.

use std::path::{Path, PathBuf};

use naga::back::spv;
use naga::valid::{Capabilities, ValidationFlags, Validator};

fn compile(manifest_dir: &Path, out_dir: &Path, name: &str) {
    let src_path = manifest_dir.join("shaders").join(format!("{name}.wgsl"));
    println!("cargo:rerun-if-changed={}", src_path.display());
    let source = std::fs::read_to_string(&src_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", src_path.display()));

    let module = naga::front::wgsl::parse_str(&source).unwrap_or_else(|e| {
        panic!(
            "{}",
            e.emit_to_string_with_path(&source, src_path.to_string_lossy().as_ref())
        )
    });
    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .unwrap_or_else(|e| {
            panic!(
                "{}",
                e.emit_to_string_with_path(&source, src_path.to_string_lossy().as_ref())
            )
        });

    // No ADJUST_COORDINATE_SPACE: positions stay raw Vulkan clip space (y down),
    // which is what src/raster.rs assumes. SPIR-V 1.0 so a Vulkan 1.0 device
    // accepts it.
    let options = spv::Options {
        lang_version: (1, 0),
        flags: spv::WriterFlags::empty(),
        ..spv::Options::default()
    };
    // `None`: every entry point of the file goes into one module.
    let words = spv::write_vec(&module, &info, &options, None)
        .unwrap_or_else(|e| panic!("SPIR-V for {name}: {e}"));
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let out = out_dir.join(format!("{name}.spv"));
    std::fs::write(&out, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    println!("cargo:rerun-if-changed=build.rs");
    compile(&manifest_dir, &out_dir, "compute");
    compile(&manifest_dir, &out_dir, "triangle");
}
