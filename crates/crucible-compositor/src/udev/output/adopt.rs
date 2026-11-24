//! The `CrucibleState` side of the output lifecycle: connector connect, start-up adoption, adoption and disconnect.

use super::records::{due_adoptions, get_surface_dmabuf_feedback};
use super::*;

impl CrucibleState<UdevData> {
    /// Sets up a newly connected connector as a mapped [`Output`] and kicks off its first frame.
    pub(in crate::udev) fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(?crtc, "Setting up connector {}", output_name);
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            output = %output_name,
            "startup: connector set up"
        );

        let drm_device = device.drm_output_manager.device();
        let display_info = display_info::for_connector(drm_device, connector.handle());
        let make = display_info
            .as_ref()
            .and_then(|info| info.make())
            .unwrap_or_else(|| "Unknown".into());
        let model = display_info
            .as_ref()
            .and_then(|info| info.model())
            .unwrap_or_else(|| "Unknown".into());
        let serial_number = display_info
            .as_ref()
            .and_then(|info| info.serial())
            .unwrap_or_else(|| "Unknown".into());

        // Pick the connector's preferred mode (else the first one).
        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);
        let drm_mode = connector.modes()[mode_id];
        let wl_mode = WlMode::from(drm_mode);

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            output_name,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make,
                model,
                serial_number,
            },
        );
        let global = output.create_global::<CrucibleState<UdevData>>(&self.display_handle);

        // Place this output by the placement rule (its configured position, else the strip). The
        // configured scale (by connector name, e.g. `DP-1`) is set first, so its logical size,
        // the Space geometry, the strip and the model rect are all logical from the start.
        let id = UdevOutputId {
            device_id: node,
            crtc,
        };
        let scale = crate::config::configured_scale(&self.config, &output.name());
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), None, Some(scale), None);
        let (width, height) = super::logical_size(&output);
        let desired = super::desired_output_positions(self);
        let connect = plan::plan_connect(
            &output_strip(&self.space),
            plan::MappedOutput {
                key: id,
                name: output.name(),
                width,
                height,
                position: (0, 0),
            },
            &desired,
        );
        super::warn_plan(&connect);
        let position = connect
            .add
            .first()
            .map_or((0, 0), |placement| placement.position);
        output.change_current_state(None, None, None, Some(position.into()));
        self.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| id);

        // From here on the global's cleanup is owned (dropped on every early return).
        let global = OutputGlobal {
            dh: self.display_handle.clone(),
            output: output.clone(),
            global: Some(global),
        };

        let planes = match drm_device.planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                warn!("Failed to query crtc planes: {}", err);
                self.space.unmap_output(&output);
                return;
            }
        };

        let pending = PendingAdoption {
            global,
            output: output.clone(),
            connector: connector.handle(),
            mode: drm_mode,
            planes,
        };

        // Register this connector with the model: the first compositor output adopts the existing
        // `World::new()` output; subsequent connectors get their own independent workspace stack
        // via `add_output`, seeded off the live and saved names. A start-up output is registered
        // before its DRM adoption, so the wallpaper client binds and arranges on it meanwhile.
        self.register_output(&output);
        // Strip outputs the new output displaced move now (a positioned output returning pushes
        // an unpositioned neighbour along the strip).
        super::apply_survivor_reflow(self, &connect);

        // Start-up (plan 44): the first commit waits for this output's wallpaper buffer, or for
        // the adoption timeout. An output connected later adopts at once, as before.
        if self.backend_data.phase.defers_adoption() {
            info!(
                elapsed_ms = crate::startup::elapsed_ms(),
                output = %output.name(),
                ?crtc,
                "startup: adoption deferred until the wallpaper"
            );
            if let Some(device) = self.backend_data.backends.get_mut(&node) {
                device.pending.insert(crtc, pending);
            }
            return;
        }
        self.adopt_output(node, crtc, pending, false);
    }

    /// Adopts `node`'s start-up outputs [`due_adoptions`] names: the one a wallpaper `landed` on
    /// (if still pending), or all of them.
    pub(in crate::udev) fn adopt_pending(&mut self, node: DrmNode, landed: Option<crtc::Handle>) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        for (crtc, pending) in due_adoptions(&mut device.pending, landed) {
            self.adopt_output(node, crtc, pending, true);
        }
    }

    /// Closes the start-up adoption window (the timeout, or a background start's activation):
    /// adopts every still-pending output, then disables the CRTCs CDE did not adopt
    /// (`DrmDevice::new(fd, false)` keeps them as found).
    pub(in crate::udev) fn finish_startup_adoption(&mut self) {
        self.backend_data.phase.advance(StartupEvent::WindowClosed);
        let nodes: Vec<DrmNode> = self.backend_data.backends.keys().copied().collect();
        for node in &nodes {
            self.adopt_pending(*node, None);
        }
        for node in nodes {
            self.disable_unadopted_crtcs(node);
        }
    }

    /// The DRM adoption of one output: its `initialize_output` (the first commit), the surface
    /// insert and the first render.
    ///
    /// A `deferred` (start-up) adoption commits the real frame (the wallpaper) and counts as the
    /// output's locked presentation if a lock is pending (plan 44 invariant 5); a hot-plugged
    /// output commits the default (black) frame, as before. Should the commit fail from the state
    /// another DRM client left, that output's CRTCs are disabled and it retries once. If it cannot be
    /// initialized it is unregistered and unmapped (the dropped record removes its global).
    fn adopt_output(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        pending: PendingAdoption,
        deferred: bool,
    ) {
        let Some(device) = self.backend_data.backends.get(&node) else {
            return;
        };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let output = pending.output.clone();
        super::super::device::log_crtc_state(
            device.drm_output_manager.device(),
            "adoption commit",
            &[crtc],
        );
        info!(
            elapsed_ms = crate::startup::elapsed_ms(),
            output = %output.name(),
            deferred,
            "startup: adopting output"
        );

        let inputs = deferred.then(|| self.begin_frame(&output));
        let mut renderer = self
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .unwrap();
        let mut render_elements = DrmOutputRenderElements::<_, OutputElements<'_>>::default();
        if let Some(inputs) = &inputs {
            let view = frame_view!(self, &output, inputs, self.dnd_icon.as_ref());
            let (elements, clear_color) =
                frame_elements(view, &mut self.header_text, &mut self.cursor, &mut renderer);
            render_elements.add_output(&crtc, clear_color, elements);
        }

        let device = self
            .backend_data
            .backends
            .get_mut(&node)
            .expect("looked up above");
        let mut attempt = |device: &mut super::super::device::BackendData| {
            device.drm_output_manager.lock().initialize_output(
                crtc,
                pending.mode,
                &[pending.connector],
                &output,
                Some(pending.planes.clone()),
                &mut renderer,
                &render_elements,
            )
        };
        let result = match attempt(device) {
            Ok(drm_output) => Ok(drm_output),
            Err(err) => {
                warn!(%err, ?crtc, "first commit failed from the inherited state; resetting it and retrying once");
                // Reset this output's inherited state: its target CRTC and the CRTC its connector
                // is currently bound to (another client may have driven it from a different one).
                let drm = device.drm_output_manager.device();
                let bound = drm
                    .get_connector(pending.connector, false)
                    .ok()
                    .and_then(|info| info.current_encoder())
                    .and_then(|encoder| drm.get_encoder(encoder).ok())
                    .and_then(|info| info.crtc());
                for c in std::iter::once(crtc).chain(bound.filter(|&b| b != crtc)) {
                    if let Err(err) = drm.set_crtc(c, None, (0, 0), &[], None) {
                        warn!(%err, crtc = ?c, "failed to disable the inherited crtc state");
                    }
                }
                attempt(device)
            }
        };
        let drm_output = match result {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("Failed to initialize drm output: {}", err);
                drop(renderer);
                self.unregister_output(&output);
                self.space.unmap_output(&output);
                self.space.refresh();
                self.present_gate_prune();
                return;
            }
        };
        drop(renderer);

        let device = self
            .backend_data
            .backends
            .get_mut(&node)
            .expect("looked up above");
        let dmabuf_feedback = drm_output.with_compositor(|compositor| {
            get_surface_dmabuf_feedback(
                self.backend_data.primary_gpu,
                device.render_node,
                node,
                &mut self.backend_data.gpus,
                compositor.surface(),
            )
        });

        let surface = SurfaceData {
            _global: pending.global,
            output: output.clone(),
            drm_output,
            dmabuf_feedback,
            power: PowerMark::default(),
            first_frame: Default::default(),
            first_flip: Default::default(),
        };
        device.surfaces.insert(crtc, surface);
        // A new CRTC has no night-light record: make sure it is tinted if the schedule says so.
        self.night_light.cache.remove(&(node, crtc));
        self.evaluate_night_light();

        // The deferred adoption commit is synchronous and raises no VBlank: it presents the frame
        // rendered just now (a locked one while a lock is pending) for every gate arm. A
        // non-deferred adoption committed nothing rendered: the kick-off render below queues one,
        // and its flip reports as usual.
        if deferred {
            self.present_gate_presented_now(&output);
        }

        // An output hot-plugged while the idle machine has the outputs blanked comes up powered
        // off and lights up on the next wake; if its power-off fails it renders as usual.
        if self.idle.machine.blanked() && self.backend_data.power_off_surface(node, crtc) {
            info!(?crtc, "idle: new output comes up powered off (blanked)");
            self.present_gate_prune();
            return;
        }

        // Kick off rendering for the new output.
        self.handle
            .insert_idle(move |state| state.render_surface(node, crtc));
    }

    /// Tears down the output for a disconnected connector.
    ///
    /// Only the removal half of the plan is executed here: this runs inside a device rescan, which
    /// reflows the surviving strip once after all of the scan's events have been applied.
    pub(in crate::udev) fn connector_disconnected(
        &mut self,
        node: DrmNode,
        _connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let id = UdevOutputId {
            device_id: node,
            crtc,
        };
        let plan = plan::plan_disconnect(
            &output_strip(&self.space),
            &[id],
            &plan::DesiredPositions::new(),
        );
        // Only the removal is applied here; the rescan that reports it reflows once afterwards.
        for key in &plan.remove {
            drop_mapped_output(self, *key);
        }
    }
}
