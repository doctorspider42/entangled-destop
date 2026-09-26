//! The Vulkan half of the GPU boost ([`crate::boost`], ADR-0004, the
//! GPU-boost amendment): `VK_NV_low_latency2`'s `vkSetLatencySleepModeNV`
//! with `lowLatencyBoost` on the window's own swapchain.
//!
//! The window's device is wgpu's, opened by `gpu_scanout::request_device`,
//! which adds the extension (and `VK_KHR_present_id`, which it requires)
//! when the adapter has both and the policy wants it. Nothing else of the
//! extension is used: no latency markers, no `vkLatencySleepNV` pacing — the
//! window does not pace the guest, and `lowLatencyMode` stays off.
//!
//! Two things the driver taught (RTX 2070, 580.88):
//!
//! * The spec lets `pSleepModeInfo` be NULL to switch everything off; this
//!   driver **crashes** in `vkSetLatencySleepModeNV` when it is. Off is
//!   therefore always an explicit structure with every member false.
//! * wgpu makes the swapchain, so it is created without
//!   `VkSwapchainLatencyCreateInfoNV`. The spec asks for nothing more, and
//!   the boost works on it exactly as on one created with it (the
//!   `gpu-boost-probe`, both ways).
//!
//! A reconfigured surface is a new `VkSwapchainKHR` whose sleep mode is the
//! default (off), so the renderer applies the request again after every
//! configure.

use ash::vk;
use wgpu::hal::api::Vulkan;

/// The device extensions the boost needs, in the order they are enabled.
pub(crate) const EXTENSIONS: [&std::ffi::CStr; 2] =
    [ash::nv::low_latency2::NAME, ash::khr::present_id::NAME];

/// `vkSetLatencySleepModeNV` on the window's device.
pub(crate) struct LatencyBoost {
    fns: ash::nv::low_latency2::Device,
    /// Kept for the lifetime of `fns`, whose device handle is wgpu's.
    _device: wgpu::Device,
}

impl LatencyBoost {
    /// The boost on `device`, when it was opened with [`EXTENSIONS`]; `None`
    /// otherwise (not a Vulkan device, an adapter without the extension, a
    /// policy that did not ask for it).
    pub(crate) fn new(device: &wgpu::Device) -> Option<Self> {
        // SAFETY: the hal device is only used inside this block, while
        // `device` is alive; the function table loaded from its raw handle is
        // kept together with a clone of `device` (fields of one value), so it
        // never outlives the device wgpu owns.
        let fns = unsafe {
            let hal = device.as_hal::<Vulkan>()?;
            let enabled = hal.enabled_device_extensions();
            if !EXTENSIONS.iter().all(|needed| enabled.contains(needed)) {
                return None;
            }
            ash::nv::low_latency2::Device::new(
                hal.shared_instance().raw_instance(),
                hal.raw_device(),
            )
        };
        Some(Self {
            fns,
            _device: device.clone(),
        })
    }

    /// Asks for the boost (`on`) or withdraws it, on `surface`'s current
    /// swapchain.
    ///
    /// # Errors
    /// The surface has no swapchain yet (unconfigured), or the driver refused.
    pub(crate) fn apply(&self, surface: &wgpu::Surface<'_>, on: bool) -> Result<(), String> {
        // Never NULL: see the module docs.
        let info = vk::LatencySleepModeInfoNV::default()
            .low_latency_mode(false)
            .low_latency_boost(on)
            .minimum_interval_us(0);
        // SAFETY: the hal surface is only used inside this block, while
        // `surface` is borrowed; the swapchain handle it names belongs to the
        // device `fns` was loaded from (the window configures its surface on
        // that device only), and the extension allows the call from any
        // thread ("access to the swapchain data associated with this
        // extension must be atomic within the implementation").
        unsafe {
            let hal = surface
                .as_hal::<Vulkan>()
                .ok_or_else(|| "the window's surface is not a Vulkan one".to_owned())?;
            let swapchain = hal
                .raw_swapchain()
                .ok_or_else(|| "the window's surface has no swapchain yet".to_owned())?;
            self.fns
                .set_latency_sleep_mode(swapchain, Some(&info))
                .map_err(|e| format!("vkSetLatencySleepModeNV refused ({e})"))
        }
    }
}
