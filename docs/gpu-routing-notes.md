# GPU routing: iGPU for the desktop, dGPU for heavy work (notes, 2026-11-13)

Status: **deferred** — a measurement spike, then a feature sprint. This note expands a short "auto-magic iGPU/GPU split routing" wish so the spike starts from facts, not prose.

## Goal

When the discrete GPU is wanted for inference (llama-server and similar), the desktop and the "GPU-accelerated but light" apps should not hold dGPU VRAM or compete for it:

- **dGPU (discrete):** inference, Steam games and other heavy GPU users.
- **iGPU (integrated):** CDE itself, Firefox, cvault-editor and similar light accelerated apps — at least while the dGPU is wanted for inference.
- **Auto-magic** means the user does not hand-pick a GPU per launch: CDE routes each launch by class/policy, and ideally follows the machine's mode (inference vs not).

## Measured on host-a (2026-11-13)

- GPUs: card0 / renderD128 = Radeon RX 9070 (Navi 48, dGPU, boot VGA); card1 / renderD129 = Raphael iGPU (Ryzen 7000 integrated).
- **Both monitors are cabled to the dGPU** (card0 DP-1, DP-3); nothing is connected to the iGPU. CDE renders on renderD128 (`Using renderD128 as primary gpu` in `/tmp/cde/latest.log`).
- dGPU VRAM in use: 12.7 of 16.3 GiB. Per process (DRM fdinfo `drm-memory-vram`): llama-server 11.8 GiB, crucible 678 MiB, cvault-editor 180 MiB, firefox-bin 31 MiB, Xwayland 18 MiB.
- So the whole desktop holds about 0.9 GiB of dGPU VRAM (~5.5%). Firefox already renders in software (WebRender `is_software: true`, deliberate), so it holds almost nothing today; it is the prime candidate for iGPU acceleration rather than for VRAM savings.

## How a client ends up on a GPU (mechanics the design must respect)

- **GL/EGL Wayland clients (Mesa):** use the compositor's `linux-dmabuf` feedback **main device**, which CDE sets from its primary GPU (`udev/output.rs`, `DmabufFeedbackBuilder::new(primary_gpu…)`). `DRI_PRIME=<n|pci-id>` in the client's environment overrides it.
- **Vulkan clients (Mesa RADV):** pick a device by enumeration order; `DRI_PRIME` (Mesa device-select layer) or `MESA_VK_DEVICE_SELECT` steers it. Games launched by Steam/Proton mostly choose their own device; the Steam process's environment is what children inherit.
- **X11 clients through Xwayland:** Xwayland's glamor renders on one GPU for all X clients; an X client's own GL/Vulkan can still be steered with `DRI_PRIME`.
- **No live swap:** a process binds its GPU at context/device creation. A running app moves only by restarting it (or if it recreates its context itself — unverified for Firefox's GPU process and for cvault-editor). So "swap the ENV live" means *relaunch*, and CDE's own restore machinery (workspace persistence, `close-pid`, relaunch-on-restore) is the natural tool for that.
- **Per-surface feedback:** Smithay can send per-surface dmabuf feedback, but Mesa chooses its main device when the client connects, so per-surface feedback alone will not move an app between GPUs — to be confirmed in the spike.

## Topology consequence (the main open question)

With the monitors on the dGPU, anything rendered on the iGPU still has to reach the dGPU for composition and scanout:

- **App on iGPU, CDE on dGPU:** the app's buffers live in system RAM (the iGPU has no VRAM); CDE imports them as dma-bufs on the dGPU (or Smithay's `MultiRenderer` copies them). Likely cheap for a browser or editor, but unmeasured.
- **CDE itself on the iGPU** (`CDE_DRM_DEVICE=/dev/dri/renderD129`, works today, no code change): every composited frame is copied to the dGPU for scanout, and fullscreen games lose direct scanout. Saves CDE's 678 MiB, costs a cross-GPU copy per frame on two monitors.
- **Monitors on the motherboard (iGPU) instead:** the laptop "PRIME offload" shape. The desktop is all-iGPU for free; games on the dGPU are copied to the iGPU for display (the cost moves to games). A cabling change, not code — worth one measured evening before any feature work.

## Spike (before any plan)

Measure, do not reason:

1. VRAM and frame timing with CDE on renderD129 (`CDE_DRM_DEVICE`) vs renderD128, both monitors, idle and with a 4K video: dGPU VRAM freed, frame time, CPU, any tearing or stutter.
2. Firefox with hardware WebRender under `DRI_PRIME` pointing at the iGPU (and cvault-editor likewise): confirm via fdinfo which device holds its memory, and that CDE on the dGPU imports its buffers without a copy fallback.
3. A Steam game with `DRI_PRIME` at the dGPU while CDE is on the iGPU (and the reverse): frame rate vs today, whether direct scanout is lost.
4. Whether Firefox survives a GPU-process restart onto another device without a full relaunch (decides "relaunch" vs "live" for the browser).

## Feature sprint shape (after the spike, pending its numbers)

- **Per-class GPU routing for spawns:** a config mapping launcher classes / program names to `igpu` | `dgpu` | `default`, applied as `DRI_PRIME` (and, where needed, `MESA_VK_DEVICE_SELECT`) on spawn, restore and launcher rows. Small: CDE already owns every spawn path.
- **A GPU mode** (`gpu-mode inference|normal`, a verb, scriptable over IPC): changes the routing for new launches, and optionally offers to relaunch running iGPU-class windows through the existing restore path. The trigger can start manual (or a script watching llama-server) before any automatic contention detection.
- **CDE's own GPU:** only if the spike shows the cross-GPU scanout copy is cheap, or the monitors move to the motherboard.
- Out of scope: detecting "contention" inside CDE from GPU telemetry; that stays a script over IPC until the manual mode has proved itself.
