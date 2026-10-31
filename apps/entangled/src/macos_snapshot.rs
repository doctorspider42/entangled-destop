//! Suspend transaction: failure always returns the live session to its gate state.
use super::*;
use machine_x86::state::SavedVirtioSlot;
use std::path::Path;

pub(super) struct Session {
    pub vm: HvfVm,
    pub gic: HvfGic,
    pub bus: Arc<MmioBus>,
    pub cfg: VmConfig,
    pub display: DisplayHandle,
}
impl Session {
    pub fn suspend(&self, group: &cpus::CpuGroup, path: &Path) {
        self.display
            .set_lifecycle_progress(Some(display::LifecyclePhase::Saving));
        let gate = self.bus.quiesce();
        let was_paused = gate.is_paused();
        let result = (|| -> Result<String, String> {
            let cpus = group.pause()?;
            gate.pause();
            if !gate.wait_until_idle(Duration::from_secs(10)) {
                return Err("device workers did not reach the snapshot gate".into());
            }
            self.bus.prepare_snapshot().map_err(|e| e.to_string())?;
            let devices = self
                .bus
                .save()
                .map_err(|e| e.to_string())?
                .into_iter()
                .enumerate()
                .map(|(slot, state)| SavedVirtioSlot {
                    slot: slot as u32,
                    state,
                })
                .collect::<Vec<_>>();
            let slots = device_slots(&devices);
            let state = vm_snapshot::arm64::State {
                cpus,
                power: group.power_state(),
                counter: HvfVm::counter(),
                gic: self.gic.save_state().map_err(|e| e.to_string())?,
                devices,
            };
            let mut gpu = GpuSave {
                bus: &self.bus,
                slots: self.bus.gpu_checkpoint_slots().map_err(|e| e.to_string())?,
            };
            let report = vm_snapshot::arm64::save(
                path,
                self.vm.memory(),
                crate::snapshot::metadata(&self.cfg, slots),
                &state,
                &mut gpu,
            )
            .map_err(|e| e.to_string())?;
            Ok(report.summary())
        })();
        match result {
            Ok(summary) => {
                println!("\n{PREFIX} saved {} {summary}", path.display());
                tracing::info!(path=%path.display(), %summary, "ARM64 VM suspended");
                // Do not reopen the device gate: shutdown retires the workers.
                group.stop();
            }
            Err(error) => {
                if !was_paused {
                    gate.resume();
                }
                group.resume();
                self.display.set_lifecycle_progress(None);
                self.display.notify("Suspend failed — session kept running");
                println!("\n{PREFIX} error save {error}; session kept Running");
                tracing::error!(%error, "ARM64 suspend failed; live session retained");
            }
        }
    }
}
pub(super) fn device_slots(devices: &[SavedVirtioSlot]) -> Vec<vm_snapshot::DeviceSlot> {
    devices
        .iter()
        .map(|d| vm_snapshot::DeviceSlot {
            device_type: d.state.device_type,
            slot: d.slot,
        })
        .collect()
}
struct GpuSave<'a> {
    bus: &'a MmioBus,
    slots: Vec<u32>,
}
impl vm_snapshot::vm::GpuSave for GpuSave<'_> {
    fn slots(&self) -> &[u32] {
        &self.slots
    }
    fn save(&mut self, slot: u32, out: &mut dyn Write) -> vm_snapshot::Result<()> {
        self.bus
            .save_streamed_state(slot, out)
            .map_err(|e| vm_snapshot::SnapshotError::Gpu(e.to_string()))
    }
}
