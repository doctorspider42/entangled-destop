//! Exercise the actual renderer's admission policy, including host-memory
//! imports. Finding a Vulkan device alone is insufficient for a Venus guest.
fn main() -> Result<(), String> {
    let host = virtio_gpu::host_vulkan::AshVulkan::load()?;
    for device in host.usable_devices()? {
        println!(
            "{}: Venus admitted; import alignment={} bytes, host-visible types={:#x}, shared scanout export={}",
            device.name(), device.import_alignment, device.host_visible_types(), device.memory_export
        );
    }
    Ok(())
}
