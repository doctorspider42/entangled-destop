use thiserror::Error;

/// Lifecycle states of a VM (backlog MVP-1203).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// Resources allocated, vCPUs not running yet.
    Created,
    /// vCPU threads executing guest code.
    Running,
    /// Every vCPU is parked at a lifecycle checkpoint and the host device
    /// workers are quiesced; nothing touches guest memory (ADR-0005).
    ///
    /// Reachable only from `Running`, and only back to `Running` (resume, or a
    /// reset — which always ends running) or on to `Stopping`.
    Paused,
    /// A reset is in flight: the vCPUs are parked, devices are going back to
    /// their power-on state and the boot images are being reloaded. Ends in
    /// `Running` when the machine restarted, `Crashed` when it could not
    /// (ADR-0005).
    Resetting,
    /// Shutdown requested, vCPUs being stopped and devices drained.
    Stopping,
    /// Clean exit; all vCPU threads joined, devices released.
    Stopped,
    /// Abnormal exit (vCPU fault, device fault, host error).
    Crashed,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("cannot transition VM from {from:?} to {to:?}")]
pub struct VmStateError {
    pub from: VmState,
    pub to: VmState,
}

impl VmState {
    /// Validates a state transition, returning the new state on success.
    ///
    /// Any state may transition to `Crashed`; everything else follows
    /// `Created -> Running -> Stopping -> Stopped`, with `Created -> Stopping`
    /// allowed for aborting a VM that never ran, and two loops back into
    /// `Running` for the lifecycle operations (ADR-0005):
    ///
    /// ```text
    ///   Running <-> Paused          pause / resume
    ///   Running  -> Resetting -> Running   reset (also from Paused)
    /// ```
    ///
    /// A paused or resetting VM can still be stopped: neither is a state a
    /// shutdown has to wait out.
    pub fn transition(self, to: VmState) -> Result<VmState, VmStateError> {
        use VmState::*;
        let ok = matches!(
            (self, to),
            (_, Crashed)
                | (Created, Running)
                | (Created, Stopping)
                | (Running, Stopping)
                | (Running, Paused)
                | (Paused, Running)
                | (Paused, Stopping)
                | (Running, Resetting)
                | (Paused, Resetting)
                | (Resetting, Running)
                | (Resetting, Stopping)
                | (Stopping, Stopped)
        );
        if ok {
            Ok(to)
        } else {
            Err(VmStateError { from: self, to })
        }
    }

    /// True once the VM can no longer execute guest code.
    pub fn is_terminal(self) -> bool {
        matches!(self, VmState::Stopped | VmState::Crashed)
    }
}

#[cfg(test)]
mod tests {
    use super::VmState::*;

    #[test]
    fn happy_path() {
        let s = Created.transition(Running).unwrap();
        let s = s.transition(Stopping).unwrap();
        let s = s.transition(Stopped).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn abort_before_run() {
        assert!(Created.transition(Stopping).is_ok());
    }

    #[test]
    fn any_state_can_crash() {
        for s in [Created, Running, Paused, Resetting, Stopping, Stopped, Crashed] {
            assert!(s.transition(Crashed).is_ok());
        }
    }

    #[test]
    fn rejects_backwards_and_skips() {
        assert!(Stopped.transition(Running).is_err());
        assert!(Created.transition(Stopped).is_err());
        assert!(Running.transition(Created).is_err());
        assert!(Stopping.transition(Running).is_err());
    }

    /// Pause/resume is a loop through `Running`, and it is not a way to skip
    /// the rest of the machine: a paused VM may only resume, reset or stop.
    #[test]
    fn pause_and_resume_round_trip() {
        let s = Created.transition(Running).unwrap();
        let s = s.transition(Paused).unwrap();
        assert!(!s.is_terminal());
        assert!(s.transition(Paused).is_err(), "already paused");
        assert!(s.transition(Stopped).is_err());
        let s = s.transition(Running).unwrap();
        assert_eq!(s, Running);
        assert!(Paused.transition(Stopping).is_ok());
    }

    /// A reset is a state of its own, entered from either running state and
    /// leaving only into `Running` (it restarted), `Stopping` or `Crashed`.
    #[test]
    fn reset_is_a_loop_back_into_running() {
        assert!(Running.transition(Resetting).is_ok());
        assert!(Paused.transition(Resetting).is_ok());
        let s = Resetting.transition(Running).unwrap();
        assert_eq!(s, Running);
        assert!(Resetting.transition(Stopping).is_ok());
        assert!(Resetting.transition(Paused).is_err());
        assert!(Resetting.transition(Stopped).is_err());
        // A VM that never started cannot be reset or paused.
        assert!(Created.transition(Resetting).is_err());
        assert!(Created.transition(Paused).is_err());
        assert!(Stopped.transition(Resetting).is_err());
    }
}
