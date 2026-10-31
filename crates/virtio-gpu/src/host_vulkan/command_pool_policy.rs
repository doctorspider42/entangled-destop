//! Native allocation retention is a host policy, not guest-visible state.
//! A command-pool reset already puts every child in its initial state and
//! requires that none is pending. Releasing its cached resources is therefore
//! safe at that same boundary, without an extra GPU wait or resetting live work.
use crate::venus::protocol::Command;

pub(super) fn invoke<R>(
    command: &mut Command<'_>,
    release_resources: bool,
    call: impl FnOnce(&mut Command<'_>) -> R,
) -> R {
    let original = match command {
        Command::ResetCommandPool(args) if release_resources => {
            let flags = args.flags;
            args.flags |= ash::vk::CommandPoolResetFlags::RELEASE_RESOURCES.as_raw();
            Some(flags)
        }
        _ => None,
    };
    let result = call(command);
    if let (Some(flags), Command::ResetCommandPool(args)) = (original, command) {
        // Keep the guest command/reconstruction recipe unchanged, including
        // when the native operation returned an error. Its VkResult survives.
        args.flags = flags;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venus::protocol::*;

    #[test]
    fn native_release_preserves_guest_flags_and_success_or_failure_results() {
        for enabled in [false, true] {
            for flags in [0, 1] {
                for result in [VK_SUCCESS, VK_ERROR_OUT_OF_DEVICE_MEMORY] {
                    let mut command = Command::ResetCommandPool(ResetCommandPoolArgs {
                        device: VkDevice(1),
                        command_pool: VkCommandPool(2),
                        flags,
                        ret: VK_SUCCESS,
                    });
                    let returned = invoke(&mut command, enabled, |command| {
                        let Command::ResetCommandPool(args) = command else {
                            unreachable!()
                        };
                        assert_eq!(args.flags, flags | u32::from(enabled));
                        args.ret = result;
                        Result::<(), i32>::Err(17)
                    });
                    assert_eq!(returned, Err(17));
                    let Command::ResetCommandPool(args) = command else {
                        unreachable!()
                    };
                    assert_eq!(args.flags, flags);
                    assert_eq!(args.ret, result);
                }
            }
        }
    }

    #[test]
    fn descriptor_resets_do_not_gain_command_pool_flags() {
        let mut command = Command::ResetDescriptorPool(ResetDescriptorPoolArgs {
            device: VkDevice(1),
            descriptor_pool: VkDescriptorPool(2),
            flags: 0,
            ret: VK_SUCCESS,
        });
        invoke(&mut command, true, |command| {
            let Command::ResetDescriptorPool(args) = command else {
                unreachable!()
            };
            assert_eq!(args.flags, 0);
        });
    }
}
