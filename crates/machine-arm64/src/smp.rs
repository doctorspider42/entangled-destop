//! Bounded PSCI CPU power state. No host hypervisor types or guest pointers.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use crate::{MachineError, MAX_CPUS};

const INVALID_PARAMS: u64 = (-2i64) as u64;
const DENIED: u64 = (-3i64) as u64;
const ALREADY_ON: u64 = (-4i64) as u64;
const ON_PENDING: u64 = (-5i64) as u64;
const INVALID_ADDRESS: u64 = (-9i64) as u64;
const AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuStart {
    pub entry: u64,
    pub context: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Off,
    Pending(CpuStart),
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Return(u64),
    CpuOff,
    SystemOff,
}

struct Inner {
    cpus: Vec<Power>,
    stopped: bool,
}

pub struct CpuPower {
    inner: Mutex<Inner>,
    changed: Condvar,
    stopped: AtomicBool,
    ram_start: u64,
    ram_end: u64,
}

impl CpuPower {
    pub fn new(count: u32, ram_start: u64, ram_bytes: u64) -> Result<Self, MachineError> {
        let ram_end = ram_start
            .checked_add(ram_bytes)
            .filter(|_| ram_bytes >= 4)
            .ok_or_else(|| {
                MachineError::Invalid("CPU entry RAM range is empty or overflows".into())
            })?;
        if count == 0 || count > MAX_CPUS {
            return Err(MachineError::Invalid("CPU count must be 1..64".into()));
        }
        let mut cpus = vec![Power::Off; count as usize];
        cpus[0] = Power::On;
        Ok(Self {
            inner: Mutex::new(Inner {
                cpus,
                stopped: false,
            }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
            ram_start,
            ram_end,
        })
    }

    /// Only called with all CPU threads parked at the lifecycle gate.
    pub fn save(&self) -> Vec<Power> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cpus
            .clone()
    }

    pub fn load(&self, cpus: &[Power]) -> Result<(), MachineError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if cpus.len() != inner.cpus.len() {
            return Err(MachineError::Invalid("PSCI CPU inventory differs".into()));
        }
        for state in cpus {
            if let Power::Pending(start) = state {
                if start.entry & 3 != 0
                    || start.entry < self.ram_start
                    || start
                        .entry
                        .checked_add(4)
                        .is_none_or(|end| end > self.ram_end)
                {
                    return Err(MachineError::Invalid("invalid saved PSCI entry".into()));
                }
            }
        }
        inner.cpus.copy_from_slice(cpus);
        Ok(())
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.stopped = true;
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    pub fn wake(&self) {
        let _inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.changed.notify_all();
    }

    /// Park a powered-off host thread. A pending entry is consumed once only.
    pub fn wait_for_start(&self, cpu: u32) -> Result<Option<CpuStart>, MachineError> {
        self.wait_for_start_interruptible(cpu, || false)
    }

    /// Wake a parked CPU to service a lifecycle request without powering it on.
    /// The predicate must be nonblocking (typically an atomic lifecycle flag).
    pub fn wait_for_start_interruptible(
        &self,
        cpu: u32,
        interrupted: impl Fn() -> bool,
    ) -> Result<Option<CpuStart>, MachineError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if inner.stopped || interrupted() {
                return Ok(None);
            }
            match inner.cpus.get_mut(cpu as usize) {
                Some(state) => match *state {
                    Power::Pending(start) => {
                        *state = Power::On;
                        return Ok(Some(start));
                    }
                    Power::Off => {
                        inner = self.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
                    }
                    Power::On => {
                        return Err(MachineError::Invalid(
                            "only a powered-off CPU can await CPU_ON".into(),
                        ))
                    }
                },
                None => {
                    return Err(MachineError::Invalid(
                        "CPU is outside the configured set".into(),
                    ))
                }
            }
        }
    }

    fn request_on(&self, target: u64, entry: u64, context: u64) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if target & !AFFINITY_MASK != 0 || target >= inner.cpus.len() as u64 {
            return INVALID_PARAMS;
        }
        if entry & 3 != 0
            || entry < self.ram_start
            || entry.checked_add(4).is_none_or(|end| end > self.ram_end)
        {
            return INVALID_ADDRESS;
        }
        if inner.stopped {
            return DENIED;
        }
        match inner.cpus[target as usize] {
            Power::On => ALREADY_ON,
            Power::Pending(_) => ON_PENDING,
            Power::Off => {
                inner.cpus[target as usize] = Power::Pending(CpuStart { entry, context });
                self.changed.notify_all();
                0
            }
        }
    }

    fn affinity_info(&self, target: u64, level: u64) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if level > 3 || target & !AFFINITY_MASK != 0 {
            return INVALID_PARAMS;
        }
        let state = if level == 0 {
            let Some(state) = usize::try_from(target)
                .ok()
                .and_then(|index| inner.cpus.get(index))
            else {
                return INVALID_PARAMS;
            };
            *state
        } else {
            // This platform has one flat cluster: Aff1..Aff3 are all zero.
            if target & !0xff != 0 {
                return INVALID_PARAMS;
            }
            if inner.cpus.contains(&Power::On) {
                Power::On
            } else if let Some(pending) = inner
                .cpus
                .iter()
                .find(|state| matches!(state, Power::Pending(_)))
            {
                *pending
            } else {
                Power::Off
            }
        };
        match state {
            Power::On => 0,
            Power::Off => 1,
            Power::Pending(_) => 2,
        }
    }

    pub fn psci(&self, cpu: u32, function: u64, arguments: [u64; 3]) -> Action {
        let [target, entry, context] = if function & (1 << 30) == 0 {
            arguments.map(|value| u64::from(value as u32))
        } else {
            arguments
        };
        match function {
            0x8400_0002 => {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                let Some(state @ Power::On) = inner.cpus.get_mut(cpu as usize) else {
                    return Action::Return(DENIED);
                };
                *state = Power::Off;
                self.changed.notify_all();
                Action::CpuOff
            }
            0x8400_0003 | 0xc400_0003 => Action::Return(self.request_on(target, entry, context)),
            0x8400_0004 | 0xc400_0004 => Action::Return(self.affinity_info(target, entry)),
            0x8400_0008 => Action::SystemOff,
            0x8400_000a
                if matches!(
                    target,
                    0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004
                ) =>
            {
                Action::Return(0)
            }
            _ => match crate::psci(function, target) {
                crate::PsciResult::Return(value) => Action::Return(value),
                crate::PsciResult::PowerOff => Action::SystemOff,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: u64 = 0x4000_0000;
    fn power() -> CpuPower {
        CpuPower::new(4, BASE, 65536).unwrap()
    }

    #[test]
    fn on_pending_running_off_and_warm_boot_preserve_context() {
        let power = power();
        assert_eq!(power.psci(0, 0xc4000004, [1, 0, 0]), Action::Return(1));
        assert_eq!(
            power.psci(0, 0xc4000003, [1, BASE, u64::MAX]),
            Action::Return(0)
        );
        assert_eq!(
            power.psci(0, 0xc4000003, [1, BASE, 0]),
            Action::Return(ON_PENDING)
        );
        assert_eq!(power.psci(0, 0xc4000004, [1, 0, 0]), Action::Return(2));
        assert_eq!(
            power.wait_for_start(1).unwrap(),
            Some(CpuStart {
                entry: BASE,
                context: u64::MAX
            })
        );
        assert_eq!(
            power.psci(0, 0xc4000003, [1, BASE, 0]),
            Action::Return(ALREADY_ON)
        );
        assert_eq!(power.psci(1, 0x84000002, [0; 3]), Action::CpuOff);
        assert_eq!(
            power.psci(0, 0xc4000003, [1, BASE + 4, 42]),
            Action::Return(0)
        );
        assert_eq!(
            power.wait_for_start(1).unwrap(),
            Some(CpuStart {
                entry: BASE + 4,
                context: 42
            })
        );
    }

    #[test]
    fn invalid_guest_targets_entries_and_affinities_never_change_power() {
        let power = power();
        for target in [4, 64, 1 << 31, u64::MAX] {
            assert_eq!(
                power.psci(0, 0xc4000003, [target, BASE, 0]),
                Action::Return(INVALID_PARAMS)
            );
        }
        for entry in [0, BASE - 4, BASE + 1, BASE + 65536, u64::MAX - 3] {
            assert_eq!(
                power.psci(0, 0xc4000003, [1, entry, 0]),
                Action::Return(INVALID_ADDRESS)
            );
        }
        for args in [[4, 0, 0], [0x100, 1, 0], [0, 4, 0], [u64::MAX, 0, 0]] {
            assert_eq!(
                power.psci(0, 0xc4000004, args),
                Action::Return(INVALID_PARAMS)
            );
        }
        assert_eq!(power.psci(0, 0xc4000004, [1, 0, 0]), Action::Return(1));
        assert_eq!(power.psci(0, 0xc4000004, [0, 1, 0]), Action::Return(0));
    }

    #[test]
    fn aarch32_arguments_are_zero_extended_and_features_are_truthful() {
        let power = power();
        assert_eq!(
            power.psci(0, 0x84000003, [0x100000001, BASE, 0x10000002a]),
            Action::Return(0)
        );
        assert_eq!(power.wait_for_start(1).unwrap().unwrap().context, 42);
        for function in [0x84000002, 0x84000003, 0xc4000003, 0x84000004, 0xc4000004] {
            assert_eq!(
                power.psci(0, 0x8400000a, [function, 0, 0]),
                Action::Return(0)
            );
        }
        assert_eq!(
            power.psci(0, 0x8400000a, [0x84000009, 0, 0]),
            Action::Return(u64::MAX)
        );
    }

    #[test]
    fn simultaneous_cpu_on_has_one_winner_and_stop_releases_parked_threads() {
        let power = std::sync::Arc::new(power());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let power = power.clone();
                std::thread::spawn(move || power.psci(0, 0xc4000003, [1, BASE, 7]))
            })
            .collect();
        let answers: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            answers
                .iter()
                .filter(|answer| **answer == Action::Return(0))
                .count(),
            1
        );
        assert_eq!(
            answers
                .iter()
                .filter(|answer| **answer == Action::Return(ON_PENDING))
                .count(),
            7
        );
        let sleeper = {
            let power = power.clone();
            std::thread::spawn(move || power.wait_for_start(3).unwrap())
        };
        power.stop();
        assert_eq!(sleeper.join().unwrap(), None);
        assert!(power.stopped());
        assert_eq!(
            power.psci(0, 0xc4000003, [2, BASE, 0]),
            Action::Return(DENIED)
        );
    }
    #[test]
    fn restored_psci_state_keeps_pending_context_and_powered_off_cpus() {
        let source = power();
        assert_eq!(
            source.psci(0, 0xc4000003, [2, BASE + 4, 42]),
            Action::Return(0)
        );
        let saved = source.save();
        let restored = power();
        restored.load(&saved).unwrap();
        assert_eq!(restored.psci(0, 0xc4000004, [1, 0, 0]), Action::Return(1));
        assert_eq!(restored.psci(0, 0xc4000004, [2, 0, 0]), Action::Return(2));
        assert_eq!(
            restored.wait_for_start(2).unwrap(),
            Some(CpuStart {
                entry: BASE + 4,
                context: 42
            })
        );
        assert_eq!(restored.psci(0, 0xc4000004, [2, 0, 0]), Action::Return(0));
        let mut damaged = saved;
        damaged[2] = Power::Pending(CpuStart {
            entry: BASE - 4,
            context: 0,
        });
        let before = restored.save();
        assert!(restored.load(&damaged).is_err());
        assert_eq!(restored.save(), before);
    }
    #[test]
    fn lifecycle_interrupt_wakes_a_psci_wait_without_powering_the_cpu_on() {
        let power = std::sync::Arc::new(power());
        let interrupted = std::sync::Arc::new(AtomicBool::new(false));
        let worker = {
            let power = power.clone();
            let interrupted = interrupted.clone();
            std::thread::spawn(move || {
                power
                    .wait_for_start_interruptible(1, || interrupted.load(Ordering::Acquire))
                    .unwrap()
            })
        };
        interrupted.store(true, Ordering::Release);
        power.wake();
        assert_eq!(worker.join().unwrap(), None);
        assert_eq!(power.psci(0, 0xc4000004, [1, 0, 0]), Action::Return(1));
        assert!(!power.stopped());
    }
}
