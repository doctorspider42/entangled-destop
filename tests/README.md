# Integration tests

- `boot/` — direct-boot tests: kernel to `VMHOST_GUEST_READY` marker (EPIC 2)
- `installer/` — Debian Installer flows, incl. automated preseed (EPIC 10)
- `graphical/` — resolution/screenshot comparison tests (EPIC 13/14),
  goldens under `graphical/golden/`

Conventions live in the `vm-testing` skill (`.claude/skills/vm-testing/`).
