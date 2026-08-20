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
    /// A suspend is in flight: the vCPUs are parked, their state has been read
    /// and the machine is being written to a file (ADR-0006).
    ///
    /// A state of its own rather than a flavour of `Paused`, because it is the
    /// one long-running lifecycle operation — a desktop-sized guest takes
    /// seconds — and a GUI that cannot tell "frozen" from "being written to
    /// disk" would show a Resume button that must not be pressed.
    Suspending,
    /// The snapshot is on disk and the vCPUs are still parked. The VM is about
    /// to be torn down; it is never resumed from here in the same process,
    /// because a suspended VM's whole point is that the *file* is the VM now.
    Suspended,
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
    ///   Paused   -> Suspending -> Suspended -> Stopping   save (ADR-0006)
    /// ```
    ///
    /// The suspend arm is one-way on purpose. A snapshot is taken *because* the
    /// VM is about to stop existing in this process; letting `Suspended` go
    /// back to `Running` would leave two live copies of the same machine — the
    /// file and the process — both convinced they own the same disks.
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
                | (Running, Suspending)
                | (Paused, Suspending)
                | (Suspending, Suspended)
                | (Suspending, Paused)
                | (Suspended, Stopping)
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
        for s in [
            Created, Running, Paused, Suspending, Suspended, Resetting, Stopping, Stopped, Crashed,
        ] {
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

    /// Suspend is a one-way arm: a VM whose state is on disk is torn down, not
    /// resumed in place — two live copies of one machine would both believe
    /// they own its disks. A *failed* suspend goes back to `Paused`, which is
    /// where the seam actually leaves it.
    #[test]
    fn suspend_is_one_way_and_ends_in_stopping() {
        let s = Running.transition(Suspending).unwrap();
        assert!(Paused.transition(Suspending).is_ok());
        assert!(s.transition(Paused).is_ok(), "a failed suspend holds");
        let s = s.transition(Suspended).unwrap();
        assert!(!s.is_terminal());
        assert!(
            s.transition(Running).is_err(),
            "a suspended VM does not resume"
        );
        assert!(s.transition(Paused).is_err());
        assert!(s.transition(Resetting).is_err());
        let s = s.transition(Stopping).unwrap();
        assert_eq!(s.transition(Stopped).unwrap(), Stopped);
        // And a VM that never ran cannot be suspended.
        assert!(Created.transition(Suspending).is_err());
        assert!(Stopped.transition(Suspending).is_err());
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
