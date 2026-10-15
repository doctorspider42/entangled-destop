//! Development-only extraction of boot files from a verified Ubuntu raw
//! ext4 image. No mounting, writes to the image, or hypervisor is involved.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let image = args.next().ok_or("usage: extract_boot IMAGE DESTINATION")?;
    let destination = std::path::PathBuf::from(args.next().ok_or("missing destination")?);
    let fs = ext4_view::Ext4::load_from_path(image)?;
    let mut kernels = Vec::new();
    for entry in fs.read_dir("/boot")? {
        let name = entry?.path().display().to_string();
        println!("{name}");
        if name.starts_with("/boot/vmlinuz-") {
            kernels.push(name);
        }
    }
    kernels.sort();
    let kernel = kernels.last().ok_or("no kernel in /boot")?;
    let version = kernel
        .strip_prefix("/boot/vmlinuz-")
        .ok_or("invalid kernel path")?;
    std::fs::create_dir_all(&destination)?;
    for (source, output) in [
        (kernel.clone(), "vmlinuz"),
        (format!("/boot/initrd.img-{version}"), "initrd.img"),
    ] {
        const MAX_BOOT_FILE_BYTES: u64 = 256 << 20;
        if fs.metadata(source.as_str())?.len() > MAX_BOOT_FILE_BYTES {
            return Err("boot file exceeds 256 MiB".into());
        }
        let bytes = fs.read(source.as_str())?;
        std::fs::write(destination.join(output), &bytes)?;
        println!("extracted {source}: {} bytes", bytes.len());
    }
    Ok(())
}
