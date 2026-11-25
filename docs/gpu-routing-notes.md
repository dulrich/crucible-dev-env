# GPU routing: iGPU for the desktop, dGPU for heavy work (notes, 2026-11-13; updated 2026-11-24)

Status: **manual setup live on host-a (2026-11-24)**: the monitors are on the iGPU and the desktop renders there, by cable move plus two environment variables, with no CDE code change. The "auto-magic" per-class routing and the GPU mode below stay deferred until there is a need. This note first expanded a short "auto-magic iGPU/GPU split routing" wish so a spike would start from facts, not prose.

## Goal

When the discrete GPU is wanted for inference (llama-server and similar), the desktop and the "GPU-accelerated but light" apps should not hold dGPU VRAM or compete for it:

- **dGPU (discrete):** inference, Steam games and other heavy GPU users.
- **iGPU (integrated):** CDE itself, Firefox, cvault-editor and similar light accelerated apps — at least while the dGPU is wanted for inference.
- **Auto-magic** means the user does not hand-pick a GPU per launch: CDE routes each launch by class/policy, and ideally follows the machine's mode (inference vs not).

## Current setup on host-a (2026-11-24)

- **Cabling:** both LG UltraFine 4K monitors are on the motherboard (Raphael iGPU, PCI 12:00.0): the left one (serial ...M828) on **DP-5**, the right one (...M713) on **HDMI-A-2**, both at 3840x2160. Nothing is on the RX 9070 (03:00.0).
- **Config** (the private config repo `dotfiles/cde/`, commits `6acfbca`, `3033dd9`): `config-host-a.kdl` has `output "DP-5" { position 0 0 }` and `output "HDMI-A-2" { position 3840 0 }`. Without positions the strip follows connect order, and a power-cycled monitor reappears on the right edge. `profile.sh` exports, on host-a only, `CDE_DRM_DEVICE=/dev/dri/by-path/pci-0000:12:00.0-render` and `WGPU_POWER_PREF=low`.
- **Why `CDE_DRM_DEVICE` is needed:** with no override CDE renders on the seat's primary GPU, the boot VGA device, and that stays the dGPU (`boot_vga=1` on 03:00.0) whatever the cabling. It would then render on the dGPU and copy every frame to the iGPU outputs. Use the by-path name: the card/renderD numbering has swapped between boots (plan 11 recorded the dGPU as card1, 2026-11-13 and 2026-11-24 as card0).
- **Why `WGPU_POWER_PREF=low` is needed:** cvault-editor (iced 0.14 on wgpu 27, Vulkan) asks for `PowerPreference::HighPerformance` unless `WGPU_POWER_PREF` is set (`iced_wgpu-0.14.0/src/window/compositor.rs:85`), so wgpu picked the dGPU even with CDE on the iGPU. Its own `VK_LOADER_DRIVERS_SELECT=*radeon*` default matches both GPUs. Any other wgpu app follows the same variable.
- **Measured after the switch** (DRM fdinfo, 2026-11-24): on the dGPU, crucible holds 14 MiB (CDE opens a small renderer on every GPU, for dGPU hotplug and cross-GPU import; not worth a code change), and Xwayland, Firefox and cvault-editor hold nothing. On the iGPU: crucible 339 MiB VRAM + 205 MiB GTT, cvault-editor 8 MiB VRAM + 168 MiB GTT, Xwayland 8 MiB, Firefox about 24 MiB. The desktop's dGPU footprint dropped from about 0.9 GiB to 14 MiB. The user moved Firefox to hardware WebRender on the iGPU the same evening. Its earlier forced software WebRender (`gfx.webrender.software`, deliberate on 2026-11-13) is the setting to check in `about:support` → Graphics → Compositing.
- **Gotcha: the text console goes blind.** fbcon attaches to the boot VGA framebuffer (`fbcon: amdgpudrmfb (fb0) is primary device`, fb0 = 03:00.0), so with no monitor on the dGPU every text tty draws to a screen that is not connected, and recovery after the restart was a blind login on tty2. The fix is to set the iGPU as the primary display in the BIOS, which makes 12:00.0 the boot VGA. `CDE_DRM_DEVICE` then becomes redundant but harmless. A weaker fallback is `fbcon=map:1` on the kernel command line, which leaves the firmware and GRUB blind. Pending: INITIATIVE `# Blocked on User`.
- **Inference:** the `inference-engine-llama` container gets all of `/dev/dri`. llama.cpp's Vulkan backend skips integrated GPUs today and only opens a 12 KiB probe on the iGPU, but passing only the 03:00.0 nodes (or setting `GGML_VK_VISIBLE_DEVICES`) would pin it. Optional.

## Measured on host-a before the switch (2026-11-13)

- GPUs: card0 / renderD128 = Radeon RX 9070 (Navi 48, dGPU, boot VGA); card1 / renderD129 = Raphael iGPU (Ryzen 7000 integrated).
- **Both monitors are cabled to the dGPU** (card0 DP-1, DP-3); nothing is connected to the iGPU. CDE renders on renderD128 (`Using renderD128 as primary gpu` in `/tmp/cde/latest.log`).
- dGPU VRAM in use: 12.7 of 16.3 GiB. Per process (DRM fdinfo `drm-memory-vram`): llama-server 11.8 GiB, crucible 678 MiB, cvault-editor 180 MiB, firefox-bin 31 MiB, Xwayland 18 MiB.
- So the whole desktop holds about 0.9 GiB of dGPU VRAM (~5.5%). Firefox already renders in software (WebRender `is_software: true`, deliberate), so it holds almost nothing today; it is the prime candidate for iGPU acceleration rather than for VRAM savings.

## How a client ends up on a GPU (mechanics the design must respect)

- **GL/EGL Wayland clients (Mesa):** use the compositor's `linux-dmabuf` feedback **main device**, which CDE sets from its primary GPU (`udev/output/records.rs`, `DmabufFeedbackBuilder::new(primary_gpu…)`). `DRI_PRIME=<n|pci-id>` in the client's environment overrides it.
- **Vulkan clients (Mesa RADV):** pick a device by enumeration order; `DRI_PRIME` (Mesa device-select layer) or `MESA_VK_DEVICE_SELECT` steers it. Games launched by Steam/Proton mostly choose their own device; the Steam process's environment is what children inherit.
- **X11 clients through Xwayland:** Xwayland's glamor renders on one GPU for all X clients; an X client's own GL/Vulkan can still be steered with `DRI_PRIME`.
- **No live swap:** a process binds its GPU at context/device creation. A running app moves only by restarting it (or if it recreates its context itself — unverified for Firefox's GPU process and for cvault-editor). So "swap the ENV live" means *relaunch*, and CDE's own restore machinery (workspace persistence, `close-pid`, relaunch-on-restore) is the natural tool for that.
- **Per-surface feedback:** Smithay can send per-surface dmabuf feedback, but Mesa chooses its main device when the client connects, so per-surface feedback alone will not move an app between GPUs — to be confirmed in the spike.

## Topology consequence (settled 2026-11-24: monitors on the motherboard)

The options as weighed on 2026-11-13. The third was chosen; the cost it moves to games (item 3 of the spike) is still unmeasured. With the monitors on the dGPU, anything rendered on the iGPU still has to reach the dGPU for composition and scanout:

- **App on iGPU, CDE on dGPU:** the app's buffers live in system RAM (the iGPU has no VRAM); CDE imports them as dma-bufs on the dGPU (or Smithay's `MultiRenderer` copies them). Likely cheap for a browser or editor, but unmeasured.
- **CDE itself on the iGPU** (`CDE_DRM_DEVICE=/dev/dri/renderD129`, works today, no code change): every composited frame is copied to the dGPU for scanout, and fullscreen games lose direct scanout. Saves CDE's 678 MiB, costs a cross-GPU copy per frame on two monitors.
- **Monitors on the motherboard (iGPU) instead:** the laptop "PRIME offload" shape. The desktop is all-iGPU for free; games on the dGPU are copied to the iGPU for display (the cost moves to games). A cabling change, not code — worth one measured evening before any feature work.

## Spike (before any plan)

Status 2026-11-24: item 1 is superseded by the cable move (no cross-GPU copy for the desktop). Item 2 is answered for the new topology: Firefox and cvault-editor render on the iGPU, which drives the outputs, so no import or copy is involved. Items 3 and 4 are open and matter only for the auto-magic sprint. Measure, do not reason:

1. VRAM and frame timing with CDE on renderD129 (`CDE_DRM_DEVICE`) vs renderD128, both monitors, idle and with a 4K video: dGPU VRAM freed, frame time, CPU, any tearing or stutter.
2. Firefox with hardware WebRender under `DRI_PRIME` pointing at the iGPU (and cvault-editor likewise): confirm via fdinfo which device holds its memory, and that CDE on the dGPU imports its buffers without a copy fallback.
3. A Steam game with `DRI_PRIME` at the dGPU while CDE is on the iGPU (and the reverse): frame rate vs today, whether direct scanout is lost.
4. Whether Firefox survives a GPU-process restart onto another device without a full relaunch (decides "relaunch" vs "live" for the browser).

## Feature sprint shape (after the spike, pending its numbers)

- **Per-class GPU routing for spawns:** a config mapping launcher classes / program names to `igpu` | `dgpu` | `default`, applied as `DRI_PRIME` (and, where needed, `MESA_VK_DEVICE_SELECT`) on spawn, restore and launcher rows. Small: CDE already owns every spawn path.
- **A GPU mode** (`gpu-mode inference|normal`, a verb, scriptable over IPC): changes the routing for new launches, and optionally offers to relaunch running iGPU-class windows through the existing restore path. The trigger can start manual (or a script watching llama-server) before any automatic contention detection.
- **CDE's own GPU:** done for host-a without code (`CDE_DRM_DEVICE`, with the monitors on the motherboard). With the routing config, the same choice could move into `config.kdl`.
- Out of scope: detecting "contention" inside CDE from GPU telemetry; that stays a script over IPC until the manual mode has proved itself.
