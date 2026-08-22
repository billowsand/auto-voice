# Cross-platform UI design

## Goals

- Use one `eframe`/`egui` UI implementation on Windows, macOS, X11 and Wayland.
- Keep ASR, audio capture and LLM work off the UI thread.
- Preserve the input focus of the application into which the user is dictating.
- Treat global shortcuts, synthetic paste and overlay placement as platform capabilities,
  not assumptions.
- Degrade explicitly when a desktop environment does not expose a required capability.

## Process model

The main thread owns the native event loop, the egui viewports and the tray icon. This is
required by macOS and also matches the event-loop requirements of `tray-icon` on Linux.
Model loading, microphone capture, global-key handling and transcription remain on worker
threads.

```text
main thread                         worker threads
------------------------------      -------------------------------
eframe / winit event loop     <---- UI events (state, audio level)
settings viewport                   global PTT listener
transparent OSD viewport            microphone capture
tray icon                           ASR + optional LLM correction
```

`OsdHandle` is the stable boundary between the speech pipeline and the UI. The speech
pipeline publishes `Recording`, `Processing`, `Done` and level updates without knowing
anything about egui or native windows.

## Platform boundary

`platform.rs` reports runtime capabilities and owns small platform-specific decisions.
Native code must not leak into the UI or ASR modules.

| Capability | Windows | macOS | Linux X11 | Linux Wayland |
| --- | --- | --- | --- | --- |
| Settings UI | Full | Full | Full | Full |
| Audio input | WASAPI | CoreAudio | ALSA/PipeWire | PipeWire/ALSA |
| Global PTT | rdev | rdev + Accessibility | rdev | Portal backend required |
| Paste | Ctrl+V | Command+V | Ctrl+V | compositor-dependent |
| OSD placement | Exact | Exact/best effort | Exact | compositor-controlled |
| Always-on-top | Full | Full | Full | compositor-controlled |

Wayland deliberately prevents ordinary clients from choosing global window positions and
z-order. The application therefore promises a working UI and transcription pipeline on
Wayland, while the OSD and global PTT are capability-gated. The future Wayland shortcut
backend should implement `org.freedesktop.portal.GlobalShortcuts` and feed the same
press/release events into the existing PTT state machine.

## Viewports

- Root viewport: settings center. Hidden at startup; closing it hides it instead of exiting.
  Hiding is platform-dependent because winit's `set_visible` is a no-op on Wayland and Hyprland
  ignores `xdg_toplevel.set_minimized`: Hyprland parks the window on a dedicated special
  workspace via `hyprctl` (both the classic and the Lua config-manager dispatch syntax are
  supported), other Wayland compositors get a minimize request, and X11 uses `set_visible`.
- OSD viewport: transparent, undecorated, mouse-pass-through and excluded from the taskbar.
  It is only visible while recording, processing, or briefly showing completion.

  Transparency is per-pixel, so the card can have antialiased corners, a translucent surface
  and a soft shadow. Windows needs four things arranged by `platform::prepare_overlay_window`,
  because none of them come for free from winit: blur-behind over an empty region so DWM
  blends the alpha channel; `WS_EX_LAYERED` cleared, since a layered window composites from a
  colour key instead; `WS_CAPTION | WS_BORDER` cleared, or DWM frames the invisible canvas with
  a shadow; and a `WM_NCHITTEST` answer of `HTTRANSPARENT`, because `WS_EX_TRANSPARENT` only
  takes a window out of hit-testing while it is layered. `AUTO_VOICE_OVERLAY=colorkey` falls
  back to 1-bit transparency for a driver that refuses to composite the alpha channel.

  `cargo run -- osd-demo --phase listening|waiting|processing|done|notice` paints the overlay
  over a stand-in document without a microphone or a model, which is the only way to review
  the compositing.
- Tray menu: opens settings and exits the process. Events wake the egui event loop instead
  of being polled at 60 Hz.

## Configuration and files

Configuration keeps compatibility with a `config.toml` next to the current working
directory. New installations fall back to the platform configuration directory returned
by `directories::ProjectDirs`. Logs and downloaded models should progressively move to
the platform state/data directories because an installed application directory is often
read-only on macOS and Linux.

## Verification matrix

- Windows 10/11: focus retention, 100/150/200% DPI, multiple monitors, taskbar/Alt-Tab.
- macOS Intel and Apple Silicon: microphone and Accessibility onboarding, Command+V,
  menu-bar lifecycle.
- Linux X11: GTK/AppIndicator tray, global shortcut, ALSA/PipeWire capture.
- Linux Wayland: settings and transcription always work; shortcut/overlay capability is
  reported and safely degraded when the portal/compositor does not support it.
