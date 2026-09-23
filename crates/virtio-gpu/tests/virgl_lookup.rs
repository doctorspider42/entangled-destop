//! Which library the renderer loads, and what happens when it cannot
//! ([ADR-0004](../../../docs/adr/0004-virtio-gpu-3d.md)).
//!
//! There are two environment variables naming a virglrenderer and they mean
//! different things. That difference is the whole reason the second one exists,
//! and it is invisible in the type system, so it is asserted here:
//!
//! * `ENTANGLED_VIRGL_LIB` is a **person's** instruction. A path that will not
//!   open is a configuration error, because quietly loading a different library
//!   turns a Venus run into a classic-virgl run that nobody notices.
//! * `ENTANGLED_VIRGL_LIB_DEFAULT` is **this program's** preference — the
//!   library `entangled fetch virglrenderer` put in the cache. A path that will
//!   not open falls through to whatever the host has. The case that makes this
//!   matter is a host with no `libvulkan.so.1`: a Venus-capable build lists it
//!   in `DT_NEEDED` and will not load at all there, while the distribution's
//!   own 0.9.x has no Vulkan in it and works fine. Preferring our download must
//!   not take 3D away from somebody who had it.
//!
//! Both cases point at a path that certainly does not exist, so neither test
//! needs a renderer, a GPU or a hypervisor — what is under test is the *error*,
//! not the library.

#![cfg(target_os = "linux")]

use virtio_gpu::virgl::{VirglRenderer, LIB_DEFAULT_ENV, LIB_ENV};

/// A path no host has.
const ABSENT: &str = "/nonexistent/entangled-test/libvirglrenderer.so.1";

/// Restores whatever the environment held, so the two cases cannot leak into
/// each other or into another test binary's idea of the world.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self(key, previous)
    }

    fn cleared(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self(key, previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

/// One test, not two: both halves write the same process-wide environment, and
/// cargo runs the tests in a binary on parallel threads.
#[test]
fn the_operator_variable_is_an_instruction_and_ours_is_a_preference() {
    // --- a path the operator named: a hard error that names the variable ---
    {
        let _default = EnvGuard::cleared(LIB_DEFAULT_ENV);
        let _explicit = EnvGuard::set(LIB_ENV, ABSENT);
        let Err(error) = VirglRenderer::load() else {
            panic!("a path that does not exist cannot load");
        };
        assert!(
            error.contains(LIB_ENV) && error.contains(ABSENT),
            "an operator's bad path must be reported as such, got: {error}"
        );
    }

    // --- a path we chose: fall through, and never that error ---
    {
        let _explicit = EnvGuard::cleared(LIB_ENV);
        let _default = EnvGuard::set(LIB_DEFAULT_ENV, ABSENT);
        match VirglRenderer::load() {
            // A host with a system libvirglrenderer: falling through is
            // exactly the point, and it loaded one.
            Ok(_renderer) => (),
            // A host with none: the generic "nothing to load" message, which
            // is the proof that the cached path was tried and abandoned rather
            // than treated as final.
            Err(error) => assert!(
                !error.contains(LIB_DEFAULT_ENV) && !error.contains(ABSENT),
                "a cached library that will not open must fall through to the system \
                 one, not end the search; got: {error}"
            ),
        }
    }
}
