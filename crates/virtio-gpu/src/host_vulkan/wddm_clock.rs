//! Clock calibration for the WDDM lab. Only device-domain ticks reach the
//! guest. The host performance counter and the Vulkan extension stay internal.

use std::ffi::CStr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ash::vk;

use crate::protocol::GpuClockReading;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(20);
const MAX_SAMPLE_ERROR: Duration = Duration::from_micros(500);

#[derive(Clone, Copy)]
pub(super) struct ClockCapability {
    pub extension: &'static CStr,
    function: &'static CStr,
    physical_id: [u8; vk::UUID_SIZE],
    frequency_hz: u64,
    timestamp_mask: u64,
}

impl ClockCapability {
    pub fn query(
        entry: &ash::Entry,
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        family: u32,
    ) -> Option<Self> {
        // SAFETY: both the instance and its enumerated physical device are live.
        let extensions =
            unsafe { instance.enumerate_device_extension_properties(physical) }.ok()?;
        let interfaces = [
            (
                c"VK_KHR_calibrated_timestamps",
                c"vkGetCalibratedTimestampsKHR",
                c"vkGetPhysicalDeviceCalibrateableTimeDomainsKHR",
            ),
            (
                c"VK_EXT_calibrated_timestamps",
                c"vkGetCalibratedTimestampsEXT",
                c"vkGetPhysicalDeviceCalibrateableTimeDomainsEXT",
            ),
        ];
        let (extension, function, domains) = interfaces.into_iter().find(|&(name, _, _)| {
            extensions.iter().any(|ext| {
                ext.extension_name_as_c_str()
                    .is_ok_and(|supported| supported == name)
            })
        })?;
        // SAFETY: the entry belongs to the live instance and the name is a C literal.
        let raw = unsafe { entry.get_instance_proc_addr(instance.handle(), domains.as_ptr()) }?;
        // SAFETY: the selected name is precisely the time-domain enumeration ABI.
        let enumerate = unsafe {
            std::mem::transmute::<
                unsafe extern "system" fn(),
                vk::PFN_vkGetPhysicalDeviceCalibrateableTimeDomainsKHR,
            >(raw)
        };
        let mut domains = [vk::TimeDomainKHR::default(); 8];
        let mut count = domains.len() as u32;
        // SAFETY: the count is the capacity of the local writable array.
        let status = unsafe { enumerate(physical, &mut count, domains.as_mut_ptr()) };
        if status != vk::Result::SUCCESS
            || !domains
                .get(..count as usize)?
                .contains(&vk::TimeDomainKHR::DEVICE)
        {
            return None;
        }
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        // SAFETY: the query targets a core Vulkan 1.2+ physical device; linked output is local.
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        let period = f64::from(properties.properties.limits.timestamp_period);
        let frequency = 1_000_000_000.0 / period;
        // SAFETY: the physical device belongs to this instance.
        let queues = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let bits = queues.get(family as usize)?.timestamp_valid_bits;
        if !(36..=64).contains(&bits)
            || !frequency.is_finite()
            || frequency < 1.0
            || frequency >= u64::MAX as f64
        {
            return None;
        }

        Some(Self {
            extension,
            function,
            physical_id: id.device_uuid,
            frequency_hz: frequency.round() as u64,
            timestamp_mask: u64::MAX >> (64 - bits),
        })
    }
}

#[derive(Clone, Copy)]
struct Source {
    device: vk::Device,
    capability: ClockCapability,
    sample: vk::PFN_vkGetCalibratedTimestampsKHR,
}

struct Anchor {
    time: Instant,
    ticks: u64,
}

#[derive(Default)]
struct ClockState {
    sources: Vec<Source>,
    attempted: Option<Instant>,
    anchor: Option<Anchor>,
}

#[derive(Default)]
pub(super) struct ClockRegistry(Mutex<ClockState>);

impl ClockRegistry {
    pub fn register(
        &self,
        instance: &ash::Instance,
        device: vk::Device,
        capability: ClockCapability,
    ) {
        // SAFETY: the live device was created with this extension enabled.
        let raw = unsafe { instance.get_device_proc_addr(device, capability.function.as_ptr()) };
        let Some(raw) = raw else { return };
        // SAFETY: the function was resolved from the matching Vulkan extension command.
        let sample = unsafe {
            std::mem::transmute::<unsafe extern "system" fn(), vk::PFN_vkGetCalibratedTimestampsKHR>(
                raw,
            )
        };
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.sources.try_reserve(1).is_ok() {
            state.sources.push(Source {
                device,
                capability,
                sample,
            });
        }
    }

    pub fn unregister(&self, device: vk::Device) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .sources
            .first()
            .is_some_and(|source| source.device == device)
        {
            state.anchor = None;
            state.attempted = None;
        }
        state.sources.retain(|source| source.device != device);
    }

    pub fn read(&self) -> Option<GpuClockReading> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let source = *state.sources.first()?;
        let clock = source.capability;
        if state.sources.iter().any(|other| {
            let other = other.capability;
            other.physical_id != clock.physical_id
                || other.frequency_hz != clock.frequency_hz
                || other.timestamp_mask != clock.timestamp_mask
        }) {
            return None;
        }
        let now = Instant::now();
        if state
            .attempted
            .is_none_or(|last| now.duration_since(last) >= SAMPLE_INTERVAL)
        {
            state.attempted = Some(now);
            state.anchor = sample(source);
        }
        let anchor = state.anchor.as_ref()?;
        let elapsed_ticks =
            anchor.time.elapsed().as_nanos() * u128::from(clock.frequency_hz) / 1_000_000_000;
        Some(GpuClockReading {
            frequency_hz: clock.frequency_hz,
            ticks: anchor.ticks.wrapping_add(elapsed_ticks as u64) & clock.timestamp_mask,
        })
    }
}

fn sample(source: Source) -> Option<Anchor> {
    let info = vk::CalibratedTimestampInfoKHR::default().time_domain(vk::TimeDomainKHR::DEVICE);
    let mut ticks = 0;
    let mut deviation = 0;
    let start = Instant::now();
    // SAFETY: the registry lock prevents destruction while this call uses the
    // device. The extension was enabled and supports the requested domain.
    let status = unsafe { (source.sample)(source.device, 1, &info, &mut ticks, &mut deviation) };
    let elapsed = start.elapsed();
    let error = elapsed / 2 + Duration::from_nanos(deviation);
    (status == vk::Result::SUCCESS && error <= MAX_SAMPLE_ERROR).then_some(Anchor {
        time: start + elapsed / 2,
        ticks,
    })
}
