# VR for Roblox under Rosemary (Quadra)

Quadra gives Roblox's macOS client an OpenXR headset by injecting a library into
`RobloxPlayer` (`libQuadraVR.dylib`, built from Quadra's `vr_device.c` and
`vr_xr.c`). Under Darling that library is part of `libQuadraShims.dylib`. This
document is what Rosemary adds so the same sources run here, how to build and
start it, and what has and has not been checked. It documents the `quadra-vr`
branch of this fork; none of it is upstream.

## What the library does

`vr_device.c` finds the client's `DeviceGL` and `DebugDeviceVR` classes by RTTI,
patches `DeviceGL`'s vtable (`beginFrame`, `setVR`) and attaches a
`DebugDeviceVR` whose methods read poses from, and submit eyes to, OpenXR
(`vr_xr.c`). Roblox's own VR code does the rest. See Quadra's `docs/VR.md`.

It needs four things from the host process, and Rosemary supplies each:

| Needs | Darling | Rosemary |
| --- | --- | --- |
| to be loaded into a hardened `RobloxPlayer` | `DYLD_INSERT_LIBRARIES` | `ROSEMARY_DYLD_INSERT_LIBRARIES` + `ROSEMARY_DYLD_INSERT_FORCE=1` |
| to find itself, by `dlsym(RTLD_DEFAULT, "_elfcalls")` | its own table | `dlsym(RTLD_DEFAULT)` searches the program's dylibs (inserted ones included) |
| host (Linux) libraries: the OpenXR loader, EGL, libc | `_elfcalls`: dlopen / dlsym of ELF | `quadra_host_dlopen` / `quadra_host_dlsym` / `quadra_host_dlerror` / `quadra_host_native`, behind a small `_elfcalls` table in the glue |
| a GL context it can bind OpenXR to | Darling's EGL context | SDL3's EGL context (see Graphics) |

## Changes in this fork

* **`ROSEMARY_DYLD_INSERT_FORCE=1`** (`crates/rosemary/src/dyld.rs`). A program
  with the hardened runtime or a `__RESTRICT` segment ignores inserted
  libraries, as dyld would, and RobloxPlayer is hardened. With this set in the
  *host's* environment the libraries are inserted anyway and a line says so on
  stderr. The default is unchanged and a guest cannot set it for itself
  (`ROSEMARY_ENVIRON` is ignored for this). Tests: `tests/dyld_insert.rs`.
* **Global `dlsym` covers the program's own dylibs** (`dyld.rs`,
  `program_image_symbol`). `dlsym(RTLD_DEFAULT, ...)` used to search `dlopen`ed
  images and the executable only, so a symbol exported by an inserted library
  was invisible. It is now the last thing consulted, so nothing Rosemary
  answers is displaced. Native backend only: the emulated backend does not
  record the program's dylibs. Test: `global_dlsym_finds_an_inserted_library_symbol`.
* **`quadra_host_*`** (`crates/rosemary/src/hostbridge.rs`). The host loader's
  own `dlopen` and `dlsym`, bound by any guest image through a flat lookup
  (`-undefined dynamic_lookup`):

  ```c
  int         quadra_host_native(void);                      /* 1: returned pointers are directly callable */
  void       *quadra_host_dlopen(const char *path, int mode); /* mode: Darwin's RTLD_LAZY/NOW/GLOBAL */
  void       *quadra_host_dlsym(void *handle, const char *name); /* handle NULL: the whole process */
  const char *quadra_host_dlerror(void);
  ```

  A guest `dlopen("libEGL.so.1")` already worked, but a guest `dlsym` returns
  Rosemary's implementation for names whose host and Darwin behavior differ
  (`getenv`, `setenv`, `getuid`, ...). The OpenXR loader reads the *process's*
  environment, so the library needs the host's `setenv` itself. These exports
  return the host's symbol unmodified. Libraries are never closed. They refuse
  under `ROSEMARY_NO_PASSTHROUGH` and under the emulated backend, with a
  `quadra_host_dlerror` message. Tests: `tests/hostbridge.rs`.

### Calling host functions from guest threads

In the native backend (a host-architecture slice, here x86-64) the returned
pointers are called directly; there is no marshaling, and a function needs no
signature row. That is safe because:

* Both sides use the System V AMD64 ABI. The structs OpenXR and EGL pass are
  fixed-width types and pointers, laid out identically.
* Thread-local state does not collide: the host keeps its thread pointer in
  `%fs`, the guest keeps Darwin's thread-specific data in `%gs`
  (`darwin::tls`), and host code never reads `%gs`. A guest thread is a host
  thread, so glibc's TLS and `errno` work as in any host call (the host's
  `errno`, not the guest's).
* A guest thread runs on its own stack, so a host library's frames sit on the
  stack the guest thread already has. Stack-hungry host code on a small guest
  thread stack is the one thing to watch; the OpenXR loader and the Monado
  client ran on Roblox's render thread without trouble.
* Host code may call guest function pointers (a comparator passed to `qsort`
  ran on a secondary guest thread in the test), because `%gs` stays installed
  for the thread.

The emulated backend (an x86-64 slice on a non-x86-64 host) is refused rather
than approximated: its calls go through a per-function signature table and
the libraries' callbacks and floating-point arguments would need rows for
every OpenXR and EGL function. VR therefore needs an x86-64 host today.

## Graphics

* Roblox must run on OpenGL, not Metal (Quadra's client settings):
  `FFlagDebugGraphicsPreferOpenGL=True` and `FFlagDebugGraphicsDisableMetal=True`
  in `Contents/MacOS/ClientSettings/ClientAppSettings.json`.
* `vr_xr.c` binds OpenXR to the game's context with `XR_MNDX_egl_enable`, so the
  context must be **EGL**. SDL3 makes an EGL context on its Wayland and
  offscreen video drivers. On X11 it makes a GLX one unless told otherwise:
  set `SDL_VIDEO_FORCE_EGL=1` there. With GLX the library logs
  `The game's EGL context has no config` and VR stays off (inferred from the
  code; not run, because the Xvfb used for testing cannot set a swap interval
  under GLX and Roblox then aborts).
* Roblox's render thread owns the context when `DeviceGL::beginFrame` runs, so
  `eglGetCurrentDisplay/Context` called through the bridge on that thread return
  the game's own objects.

## Building

```sh
# Rosemary (this branch)
cargo build --release --bin rosemary

# The VR library, from a Quadra checkout. Cross-compiles Mach-O with clang+lld
# against the Darwin SDK that Quadra's runtime uses (DARLING_SYSROOT overrides it).
rosemary_vr/build_rosemary_vr.sh          # -> build/libQuadraVR.dylib
QUADRA_VR_TRACE=1 rosemary_vr/build_rosemary_vr.sh   # -> libQuadraVR-trace.dylib (also logs getState)
```

`rosemary_vr/rosemary_glue.c` is the only Rosemary-specific source. It defines
`_elfcalls`, a pointer to `{open, close, symbol}` of the shape Darling's loader
publishes, with the three functions forwarding to `quadra_host_*`.
`vr_device.c` and `vr_xr.c` are compiled unchanged.

## Running

The Quadra launcher has to arrange this (host environment of the `rosemary`
process unless noted):

| Setting | Value |
| --- | --- |
| `ROSEMARY_DYLD_INSERT_LIBRARIES` | absolute host path of `libQuadraVR.dylib` |
| `ROSEMARY_DYLD_INSERT_FORCE` | `1` |
| `ROSEMARY_ENVIRON` | newline-separated guest variables, at least `QUADRA_VR=openxr` (or `openxr-headset`, without the window mirror) and `QUADRA_XR_RUNTIME_JSON=<runtime manifest>`; optionally `QUADRA_VR_SCALE=<percent>` and `QUADRA_VR_FOLLOW_RATE=1`. Same meaning as under Darling (`launcher/quadra/vr.py` already produces them) |
| `ClientAppSettings.json` | the two OpenGL flags above |
| `SDL_VIDEO_FORCE_EGL=1` | only when SDL uses X11 |
| `XDG_RUNTIME_DIR` | must be the runtime's: its IPC socket lives there. The library falls back to `/run/user/<uid>` |

`QUADRA_XR_RUNTIME_JSON` is copied to the host's `XR_RUNTIME_JSON` through the
host's real `setenv`, which is why the bridge exists. Setting `XR_RUNTIME_JSON`
in the `rosemary` process's own environment works as well (run, with
`QUADRA_VR=openxr-headset`).

Every `QUADRA_*` variable must be in `ROSEMARY_ENVIRON`: nothing else of the
host's environment reaches the guest (see `usage.md`).

## Testing without a headset

Quadra's `rosemary_vr/test_rosemary_vr.sh` does the whole thing: builds the
trace library, copies the installed Roblox app and makes a new prefix in a
temporary directory (the live prefix and its sign-in are not touched), starts
the Monado emulator (`~/.local/share/quadrahq/vr-emulator/run-service.sh`),
runs Roblox for 30 seconds on a private display and checks the log. Nothing
appears on the desktop. `DISPLAY_MODE=offscreen` (default), `xvfb` or `wayland`
choose how the window system is faked; `SCREENSHOT=file.png` with `xvfb`
saves the mirrored eyes.

## What has been checked

Roblox 0.745.0.7451137, Rosemary 0.5.0 + this branch, x86-64 Linux, NVIDIA
GPU, against Monado 25.1.0's headless null compositor (simulated HMD and
controllers). All of it on the login screen; no game was joined (that needs a
sign-in).

Run in Rosemary, evidence in the launch log and the runtime's trace:

* The inserted library loads into the hardened RobloxPlayer; every byte-pattern
  check in `vr_device.c` matches; `DeviceGL`'s vtable is patched.
* The OpenXR loader and the Monado client load through the bridge, find the
  runtime from `QUADRA_XR_RUNTIME_JSON`, and the session goes idle, ready,
  synchronized, visible, focused. One run: 900 frames at 70 fps against 72 Hz.
* The runtime's IPC trace of a 25 second run counts 1203 `xrWaitFrame`, 1202
  `xrBeginFrame` and 1201 `xrEndFrame`, with 2402 swapchain acquire, wait and
  release calls (both eyes, every frame) and 2404 view locates.
* Roblox reads the pose: a trace of `getState` shows the simulated head's
  position and rotation changing, hands valid, and the session state 2
  (focused). With controller presses injected into the emulator, the right
  trigger (input slot 12) goes 1.0 and 0.0 in `getState`.
* The eyes hold Roblox's rendering: with the window mirror on, a screenshot
  shows the login screen as a floating panel in both eyes, laser pointers from
  both controllers, drawn from the wobbling head pose.
* The same session starts under SDL's offscreen driver on the NVIDIA GPU, under
  a private Xvfb with `SDL_VIDEO_FORCE_EGL=1` and Mesa software GL, and under a
  private virtual `kwin_wayland` with SDL's Wayland driver and Mesa software GL.
* The three Rust-side changes have integration tests that run a Mach-O guest
  (`dyld_insert`, `hostbridge`). Those fixtures build with `cc -arch x86_64`,
  which this Linux host lacks; they were run with a `cc` wrapper around
  `clang -target x86_64-apple-darwin` and the Darwin SDK that Quadra installs.

## Not checked

* A real headset (Quest 2 through WiVRn). The emulator's controllers use the
  Khronos simple profile; the Touch bindings, thumbsticks, grips and haptics
  were not exercised. Pixels reaching a compositor were not seen either: the
  emulator discards them (the screenshot is of the window mirror, which shares
  the eye framebuffers the swapchain copy reads).
* Joining a game, or any frame after VR is on in a place.
* GLX (X11 without `SDL_VIDEO_FORCE_EGL`), the emulated backend, aarch64 hosts,
  and Rosemary running on its bundled glibc (`ROSEMARY_GLIBC=bundled`) against
  the host's OpenXR and EGL libraries.
* One test run ended with `XR_ERROR_INSTANCE_LOST` late in its 30 seconds,
  while the emulator service was shared with another job; two repeats of the
  same run (30 and 40 seconds) were clean, and the service log shows only a
  normal shutdown. Treated as the service being stopped underneath the run,
  not reproduced.
