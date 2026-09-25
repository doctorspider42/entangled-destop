# vk-smoke — the guest-side Vulkan acceptance test

A small standalone binary that runs **inside a guest** and proves the GPU
behind the guest's Vulkan driver really did the work: it computes every
expected result on the CPU and compares bytes, not return codes. It is the
acceptance test for the Venus renderer's phase-5 stages (ADR-0004): 5b.1
device memory + buffers/images, 5b.2 pipelines/descriptors/command buffers,
5b.3 queue submit, fences and semaphores. Host-side unit tests cannot show
that; a guest that renders an exact image can.

It is not a workspace member (its own `[workspace]` table) and depends on
`ash` 0.38 (the Vulkan loader is `dlopen`ed at run time) and, at build time
only, `naga` 25, which compiles the WGSL shaders in `shaders/` to SPIR-V — so
building needs neither the Vulkan SDK nor glslang. Both are MIT/Apache.

## Output

```text
SMOKE <n> <name> PASS|FAIL|SKIP <detail>
SMOKE DONE pass=<p> fail=<f> skip=<s>
```

Lines starting with `#` are diagnostics (every physical device, the memory
types). Exit status: 0 when nothing failed, 1 when something did, 2 when the
watchdog ended a hung run. A failing check never stops a later one that does
not depend on it; a dependent one prints `SKIP` and says why. Failures name
the call and the `VkResult` (`vkQueueSubmit: VK_ERROR_DEVICE_LOST`), or the
first wrong words/pixels against the CPU's expectation.

| n | name | proves | depends on |
|---|---|---|---|
| 1 | `instance` | loader loads, `vkCreateInstance`, every physical device listed (name, type, vendor, apiVersion, driverName); PASS when a non-CPU device is selected | — |
| 2 | `device` | `vkCreateDevice` with one GRAPHICS+COMPUTE queue, a command pool; enables timeline semaphores and dynamic rendering (core or KHR) when present | 1 |
| 3 | `host-memory` | 1 MiB `HOST_VISIBLE\|HOST_COHERENT` buffer: map, write a pattern, unmap, remap, read back; plus a remap at an offset | 2 |
| 4 | `transfer` | `vkCmdFillBuffer` ×3 + `vkCmdUpdateBuffer` into a device-local buffer, `vkCmdCopyBuffer` (two regions, halves swapped) into a host-visible one, submit + fence, verify all 16 Ki words | 2 |
| 5 | `compute` | a compute shader writes `f(i)` (integer hash) to 1 Mi elements of a storage buffer through a descriptor set; all verified on the CPU | 2 |
| 6 | `graphics` | a triangle split into exact red/green/blue regions, drawn from a vertex buffer into a 256×256 RGBA8 image with a classic render pass, copied to a buffer; six probe pixels, **every** pixel against a CPU reference, and an FNV-1a checksum | 2 |
| 7 | `dynamic-rendering` | the same image through `vkCmdBeginRendering` (1.3 core or `VK_KHR_dynamic_rendering`) with a different clear colour; SKIP without either | 2 |
| 8 | `timeline-sync` | submit A signals a timeline semaphore to 1, a separate submit B waits for 1, copies, signals 2; host `vkWaitSemaphores(2)` + `vkGetSemaphoreCounterValue`; then `vkQueueWaitIdle` and `vkDeviceWaitIdle` each as the only sync before a host read; SKIP without timeline semaphores | 2 |
| 9 | `many-submits` | 1000 command buffers, 1000 separate `vkQueueSubmit`s with 1000 fences, each writing its own word; wait all, every fence signalled, no word lost; prints the timing | 2 |
| 10 | `exhaust` | **hostile, only when named in `--checks`**: 64 MiB device-local allocations until refused (at most 16 GiB), held `VK_SMOKE_HOLD_SECS`, freed; then 4 KiB buffers until refused (at most 200 000); PASS when both are refused with a Vulkan error. Mesa allocates asynchronously, so the refusal reaches the program only with `VN_PERF=no_async_mem_alloc,no_async_buffer_create`; without it the context ends at the first free of a refused allocation (ADR-0004, the resource-exhaustion amendment) | 2 |

Stage expectations for our renderer: after 5b.1, checks 1–3 must PASS (4–9
need a submit); after 5b.2, pipeline and descriptor creation no longer fail;
after 5b.3, all nine PASS. `--checks 3` runs just the memory check.

The picture in checks 6 and 7 is exact by construction (`src/raster.rs`):
vertices on whole pixels, every edge and region boundary at least 1/6 px from
every pixel centre (a unit test nudges each sample and proves it), only 0.0
and 1.0 written per channel, clear colours in steps of 0.2. So any conformant
GPU produces the same bytes — the RTX 2070 and lavapipe both give:

```text
check 6 (clear 0.2,0.4,0.6): red/green/blue/clear = 8362/8363/8363/40448 px, fnv1a=0x2678f2a0e39fba1b
check 7 (clear 0.6,0.4,0.2): red/green/blue/clear = 8362/8363/8363/40448 px, fnv1a=0xd79d631c4d62403b
```

## Options

| flag | env | meaning |
|---|---|---|
| `--device-index N` | `VK_SMOKE_DEVICE_INDEX` | test device N from the `# device[N]` lines, CPU devices included |
| `--allow-cpu` | `VK_SMOKE_ALLOW_CPU=1` | let automatic selection take a CPU device (lavapipe/llvmpipe) |
| `--checks 3,5,6` | `VK_SMOKE_CHECKS` | run only these of 3..9 (1 and 2 always run) |
| `--timeout-secs N` | `VK_SMOKE_TIMEOUT_SECS` | per-wait GPU timeout, default 10 s; a timeout is a FAIL and from then on nothing is destroyed (the GPU may still use it) |
| `--api-cap 1.2` | `VK_SMOKE_API_CAP` | request at most that Vulkan version, so a 1.3 device takes the 1.2/1.1 paths (KHR extensions) the way a Venus guest reporting 1.2 does |
| `--repeat N` | `VK_SMOKE_REPEAT` | checks 4–7 submit their work N times (default 10), then N empty command buffers; see "Timing" |

## Timing

Checks 4–7 time every submit two ways, and print both:

```text
GPU time 13.75 ms (first submit, wall; timestamps 0.126 ms); warm x9: wall 2.136 ms, timestamps 0.112 ms; empty submit x10: wall 1.921 ms
```

- **wall** is `vkQueueSubmit` to `vkWaitForFences` returning: submission, the
  GPU, and the signal coming back. "GPU time" is the first submit's wall time,
  as it always was. It is **not** GPU time: in a Venus guest every object the
  check created just before — memory, buffers, pipelines — went to the host
  without a reply, and the host creates them, compiling pipelines, before it
  gets to the submit. So the first wall time includes the host's pipeline
  compiles (a cold NVIDIA shader cache on a new VMM binary: 10–15 ms), and
  only the warm ones (the median of the other N−1) measure a submit.
- **timestamps** is `vkCmdWriteTimestamp` `TOP_OF_PIPE` before the work and
  `BOTTOM_OF_PIPE` after it in the same command buffer: what the GPU spent.
  The query pool exists when the queue family has `timestampValidBits`; the
  `# timestamps:` line prints the family's bits and `timestampPeriod`.
- **empty submit** is the round trip with no work: the latency floor.

Check 5 also runs the dispatch into a **device-local** buffer and times a
4 MiB copy from it into the host-visible one, which separates what the
buffer's placement costs from what the dispatch costs. Checks 6 and 7 print
the memory types of the image, the vertex buffer and the readback buffer.

A GPU's clocks change the timestamps several-fold. A native process gets
full clocks for about two seconds from creating its device; a guest's device
is created inside a VMM that has had its own for a long time, and an RTX 2070
runs a guest's sparse work at idle clocks (P8, 300 MHz core, 405 MHz memory)
where the native run gets P0. `nvidia-smi --query-gpu=pstate,clocks.gr,clocks.mem
--format=csv -lms 50` on the host shows which (ADR-0004, 2026-09-25).

A watchdog ends any check that makes no progress for `2 × timeout + 10 s`
(`vkQueueWaitIdle` has no timeout of its own, and a broken renderer can block
any call) and still prints the remaining lines and `SMOKE DONE`.

Automatic selection prefers discrete > integrated > virtual > other; with no
override, a machine with only CPU devices FAILs check 1. In a Venus guest the
llvmpipe ICD is usually present too, which is fine — Venus reports the host's
device type and wins. To make sure only Venus is loaded:
`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json` (older loaders:
`VK_ICD_FILENAMES`).

## Building

The guest is Ubuntu 26.04 x86_64. A binary built in WSL's Ubuntu 22.04 needs
glibc 2.34 and runs on any newer guest. Build it in WSL:

```powershell
# the checkout's path as WSL sees it, then where to copy the binary
wsl -d Ubuntu -e bash /mnt/f/Projects/entangled-destop/guest/vk-smoke/build.sh /mnt/f/VMs/Entangled/probes/vk-smoke
```

`build.sh` uses `$HOME/.cargo/bin/cargo`, `CARGO_TARGET_DIR=/mnt/f/cargo-targets/vk-smoke`
(never the repo, never the WSL VHDX), `-j 6`, `--locked`, and **waits while
any other `cargo`/`rustc` runs in WSL** (the machine has crashed under
concurrent build load; `NO_WAIT=1` makes it fail instead). The optional
argument is a directory to copy the binary into. Delete the target dir when
you are done: `rm -rf /mnt/f/cargo-targets/vk-smoke`.

It builds and runs natively on Windows too — that is the reference behaviour
of the real GPU with no renderer in between:

```powershell
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\entangled-target-vk-smoke"
cargo run --release --manifest-path guest\vk-smoke\Cargo.toml
cargo test --manifest-path guest\vk-smoke\Cargo.toml     # the CPU reference's own tests
```

The release binary is about 540 KiB (stripped, LTO).

## Getting it into the guest

**What usernet allows** (`crates/virtio-net/src/usernet/`): the guest sits on
`192.168.74.0/24`, gateway/DHCP/DNS at `192.168.74.1`, guest at `.15`. Guest
TCP is terminated inside the VMM (smoltcp) and re-made as an ordinary host
`connect()` to **the address the guest dialled** (`tcp.rs`, `open_if_new`).
Two kinds of destination are refused — the SYN is silently dropped, so the
guest's connect just hangs until it times out:

- anything on the guest's own segment, **including the gateway
  `192.168.74.1`** — there is no "host alias" address as in QEMU's slirp;
- loopback (`127.0.0.0/8`).

Everything else is allowed, and that includes the host's *own* non-loopback
addresses: its LAN IP, the `vEthernet (WSL)` address, a VMware host-only
adapter. The VMM connects to them from the host itself, so the connection
never crosses a real network (and Windows Firewall, which exempts loopback
traffic, does not see it as inbound — verified with a same-host `curl` to the
LAN IP). There is no port forwarding in the other direction and no UDP relay
beyond DNS, but guest→host TCP is all this needs.

**Serve it** from the host that runs `entangled` (Windows/WHP here), bound to
one of its addresses — not `127.0.0.1`, and not `0.0.0.0` if you would rather
not offer the directory to your LAN:

```powershell
Get-NetIPAddress -AddressFamily IPv4 | ? IPAddress -notlike '127.*' | ft IPAddress,InterfaceAlias
python -m http.server 8000 --bind 192.168.100.41 --directory F:\VMs\Entangled\probes\vk-smoke
```

If `entangled` runs in WSL (KVM host), serve from inside WSL instead, bound to
WSL's own address (`hostname -I`), because that is where the VMM's `connect()`
comes from.

**Fetch and run it** in the guest (Ubuntu desktop ships `wget`; `curl` works
the same if installed). The guest's user needs access to
`/dev/dri/renderD128` (the `render` group, or run it as root):

```bash
wget -O /tmp/vk-smoke http://192.168.100.41:8000/vk-smoke && chmod +x /tmp/vk-smoke
# or: curl -o /tmp/vk-smoke http://192.168.100.41:8000/vk-smoke && chmod +x /tmp/vk-smoke
/tmp/vk-smoke 2>&1 | tee /tmp/vk-smoke.log
```

For Mesa's side of a failure, add `VN_DEBUG=init,result MESA_LOG_LEVEL=debug`
(a release Mesa hides every `vn_log` below `MESA_LOG_LEVEL=debug`; ADR-0004,
correction of 2026-09-23). Compare the binary's SHA-256 on both sides if in
doubt (`sha256sum /tmp/vk-smoke` vs `Get-FileHash`).
