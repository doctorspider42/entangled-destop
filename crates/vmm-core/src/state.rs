use thiserror::Error;

/// Lifecycle states of a VM (backlog MVP-1203).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// Resources allocated, vCPUs not running yet.
    Created,
    /// vCPU threads executing guest code.
    Running,
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
    /// allowed for aborting a VM that never ran.
    pub fn transition(self, to: VmState) -> Result<VmState, VmStateError> {
        use VmState::*;
        let ok = matches!(
            (self, to),
            (_, Crashed)
                | (Created, Running)
                | (Created, Stopping)
                | (Running, Stopping)
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
        for s in [Created, Running, Stopping, Stopped, Crashed] {
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
}
