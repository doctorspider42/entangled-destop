//! Read-only extraction of a minimal Ubuntu ARM64 linker sysroot for vk-smoke.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let image = args
        .next()
        .ok_or("usage: extract_sysroot IMAGE DESTINATION")?;
    let dest = std::path::PathBuf::from(args.next().ok_or("missing destination")?);
    let fs = ext4_view::Ext4::load_from_path(image)?;
    let libs = [
        "crt1.o",
        "Scrt1.o",
        "crti.o",
        "crtn.o",
        "libc.so",
        "libc.so.6",
        "libc_nonshared.a",
        "libm.so",
        "libm.so.6",
        "libmvec.so.1",
        "libdl.a",
        "libpthread.a",
        "librt.a",
        "libutil.a",
        "libgcc_s.so.1",
        "ld-linux-aarch64.so.1",
    ];
    let gcc = [
        "crtbegin.o",
        "crtbeginS.o",
        "crtend.o",
        "crtendS.o",
        "libgcc.a",
        "libgcc_s.so",
    ];
    for (dir, files) in [
        ("usr/lib/aarch64-linux-gnu", libs.as_slice()),
        ("usr/lib/gcc/aarch64-linux-gnu/13", gcc.as_slice()),
    ] {
        let target = dest.join(dir);
        std::fs::create_dir_all(&target)?;
        for name in files {
            let source = format!("/{dir}/{name}");
            if fs.metadata(source.as_str())?.len() > 32 << 20 {
                return Err("linker file exceeds cap".into());
            }
            // read resolves guest symlinks inside the image, never on the host.
            let data = fs.read(source.as_str())?;
            std::fs::write(target.join(name), &data)?;
            println!("{source}: {} bytes", data.len());
        }
    }
    // GNU libc's linker script uses /lib, while Ubuntu stores merged-/usr files.
    // Copy the needed runtime names instead of creating absolute host symlinks.
    let runtime = dest.join("lib/aarch64-linux-gnu");
    std::fs::create_dir_all(&runtime)?;
    for name in [
        "libc.so.6",
        "libm.so.6",
        "libmvec.so.1",
        "libgcc_s.so.1",
        "ld-linux-aarch64.so.1",
    ] {
        std::fs::copy(
            dest.join("usr/lib/aarch64-linux-gnu").join(name),
            runtime.join(name),
        )?;
    }
    std::fs::copy(
        runtime.join("ld-linux-aarch64.so.1"),
        dest.join("lib/ld-linux-aarch64.so.1"),
    )?;
    Ok(())
}
