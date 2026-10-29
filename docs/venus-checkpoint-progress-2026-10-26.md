# Venus disk checkpoint acceptance

The x86 engine can opt into experimental disk checkpoints of accelerated Venus
sessions. The checkpoint carries guest CPU/RAM, native Vulkan reconstruction
recipes, GPU allocation contents, cross-context shared payloads and synchronization
state. All reconstruction completes before guest/device workers are released.
This is a disk checkpoint, independent of the original VMM process.

## Enabling it

For a Venus VM, opt in before starting the session:

```toml
[display]
venus = true
venus_checkpoints = true
```

The option defaults to false and is omitted from serialized legacy profiles.
Resume detects an accelerated GPU section and selects Venus plus recipe retention
automatically. Unsupported GPU states refuse capture and keep the source running.
The snapshot does not copy the writable disk: retain the matching RAW/NVRAM and
never resume a historical checkpoint onto a disk advanced by a later session.

## Native Windows acceptance, 2026-10-29

The tested Ubuntu GNOME session used 8 vCPUs, 8 GiB RAM and an NVIDIA RTX 2070.
A saved session was restored after an actual Windows restart, using local build
`0.2.0-dev.venus.20261028.5`. The guest boot ID, GNOME compositor PID and open
Nautilus PID were unchanged. The application remained usable after restore.

The prepared visual acceptance checked a fixed 500x350 region of the desktop:

- The restored region matched the saved reference exactly.
- Opening another empty directory produced a new GPU-rendered window. The region
  changed by 6.49%; the expected contents in the cascaded window matched exactly.
- A subsequent full save completed in 6.03 s, wrote 1,458,186,602 bytes and exited
  successfully. The resulting file was promoted atomically and its SHA-256 checked.

Earlier fresh-process cycles on the original session saved in 6.87 s and 7.62 s.
These are observations of this guest on this host, not general performance targets.
Durable acceptance logs, images, exact hashes and paired VM files are kept outside
this source checkout. The fixed visual geometry belongs to that acceptance fixture.

## Regression coverage

Native separate-process Vulkan fixtures reconstruct private buffers, color and
depth/stencil images, shared exporter/importer allocations, object graphs, rings,
host pages and synchronization state. They compare GPU-read contents and submit
new guest work after restore. Injected write failures preserve the source.

Shared payloads distinguish stable external releases, proven unused Initial
allocations, and pending layout-changing ownership transfers. The latter retain
both old/new layouts, including COLOR_ATTACHMENT to GENERAL. The restored guest
acquires the same pair and successfully draws new pixels.

GPU device state v3 preserves the independent hardware cursor plane, even after
its source resource is freed; v2 remains readable. User networking retains the
learned MAC and inbound port allocator so inbound connections work before guest TX.
Host TCP sockets are re-established rather than serialized. WASAPI playback is
paced by the host audio clock, including after idle gaps.

Pre-publication verification on the unchanged implementation:

- Windows: 112 suites, 2,588 passed, 19 ignored; workspace clippy passed.
- Linux/WSL: 112 suites, 2,601 passed, 26 ignored; workspace clippy passed.
- Formatting, generated image-format geometry, paired skill copies and cargo deny
  passed. The checkpoint geometry generator is checked by CI.

WSL on this machine has no KVM and exposes no usable native GPU for these fixtures;
those tests self-skip there. Native GPU and WHP acceptance ran on Windows.

## Scope and remaining limitations

This establishes suspend/resume of the tested Ubuntu session, including a Windows
restart. It does not establish arbitrary games, every Vulkan workload, migration
between GPUs/drivers or native macOS/KVM runtime acceptance.

Recipe retention and metadata are bounded. Unsupported image shapes, active aliases,
exposed device addresses, query results, ambiguous ownership and outstanding
future-dependent work can refuse capture. GPU/driver and disk fingerprints are
validated. See [ADR-0006](adr/0006-suspend-restore.md) for the format and refusal rules.

A blank Nautilus rendering issue was also reproduced without suspend; the accepted
session renders correctly, but this work does not claim a general shader-cache fix.
The captured-pointer toolbar change has regression tests; a final native UI check
was not completed because the Windows window-capture helper timed out.
