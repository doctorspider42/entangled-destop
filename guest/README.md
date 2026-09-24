# Guest-side artifacts

Reproducible build configurations for guest components (backlog EPIC 11/13):

- `bootstrap-kernel/` — kconfig for the project-maintained boot kernel
  (virtio-blk/net/gpu/input built in, no modules needed before switch_root)
- `bootstrap-initramfs/` — init that finds the root partition (UUID or
  /dev/vda1), mounts it and `switch_root`s into the installed Debian
- `test-rootfs/` — minimal test images that print `VMHOST_GUEST_READY` and
  scripted probe markers (see the vm-testing skill)
- `vk-smoke/` — a Vulkan smoke test run *inside* a guest: nine numbered
  checks (memory, transfer, compute, an exact-pixel triangle, timeline sync,
  1000 submits) that prove the GPU behind the guest's driver did the work.
  The acceptance test for the Venus renderer; how to build it and get it into
  a usernet guest is in its README

Built binaries are cached artifacts — never commit them to git.
