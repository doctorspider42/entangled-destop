# Coordinator note — priority clarified by the user

The user hit this on a machine running **the installer build**, and their
instruction is: *fix it so nothing has to be copied in by hand, ever again.*

So the ranking of the three routes is settled, and it is not the one the brief
implied:

1. **The installer must carry the firmware.** This is the fix. A machine that
   installed a release must be able to install Ubuntu with no extra step, no
   download and no token. EDK2 is BSD-2-Clause-Patent, so redistributing it is
   fine — attribute it where we attribute everything else.
2. **`release.yml` must therefore obtain the firmware while building the
   installer.** It runs on windows-latest and the firmware only builds on
   Linux, so it has to come from somewhere: the guest-artifacts release is the
   obvious source, and note that **CI's built-in `GITHUB_TOKEN` can download a
   private repository's own release assets** — `gh release download` inside the
   workflow works where an anonymous browser URL 404s. Verify the version the
   installer ships matches the pin, and fail the release rather than shipping
   an installer that cannot boot a UEFI guest.
3. **The end-user download is the fallback**, not the answer, precisely because
   the repository is private. Keep it, make its message honest.

Also: whatever a fresh install still cannot do, `doctor` and the manager's
pre-flight should say in one sentence with a fix the user can act on.

Delete this file before you finish.
