//! Wayland protocol handler and dispatch trait implementations for `OverlayRuntime`.

// SCTK handler traits covering registry, outputs, SHM, frames, layer-shell, seat, and input.
use smithay_client_toolkit::{
    compositor::CompositorHandler,
    delegate_registry,
    globals::GlobalData,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, ThemeSpec},
    },
    shell::wlr_layer::{LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    shm::{Shm, ShmHandler},
};
// Raw Wayland connection, dispatch traits, and protocol object handles.
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{Event as ViewportEvent, WpViewport},
    wp_viewporter::{Event as ViewporterEvent, WpViewporter},
};

// Runtime state coordinator and pointer/keyboard effect flags.
use super::{input::InputEffects, runtime::OverlayRuntime};

impl ProvidesRegistryState for OverlayRuntime
{
    fn registry(&mut self) -> &mut RegistryState
    {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl OutputHandler for OverlayRuntime
{
    fn output_state(&mut self) -> &mut OutputState
    {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl ShmHandler for OverlayRuntime
{
    fn shm_state(&mut self) -> &mut Shm
    {
        &mut self.shm
    }
}

impl CompositorHandler for OverlayRuntime
{
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: i32)
    {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    )
    {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _time: u32)
    {
        let is_image_surface = self
            .scene
            .as_ref()
            .is_some_and(|scene| *surface == scene.image_surface);
        if !is_image_surface
        {
            return;
        }

        self.frame_callback_pending = false;
        self.tick_playback();
    }

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput)
    {
    }

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput)
    {
    }
}

impl LayerShellHandler for OverlayRuntime
{
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface)
    {
        self.exit_requested = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    )
    {
        if let Some(scene) = &self.scene
            && *layer != scene.parent_layer
        {
            return;
        }

        let (cw, ch) = configure.new_size;
        if cw > 0 && ch > 0
        {
            self.layout.set_screen_size(cw, ch);
        }

        if !self.configured
        {
            self.configured = true;
            if let Ok(frame0) = self.cache.frame_argb(0)
                && let Some(scene) = &mut self.scene
                && let Err(err) = scene.initialize_first_frame(
                    &self.compositor,
                    &self.layout,
                    &self.input,
                    &frame0,
                )
            {
                tracing::warn!(error = %err, "Failed presenting initial overlay frame");
            }

            let initial_timer = InputEffects {
                reset_fade_timer: true,
                ..InputEffects::default()
            };
            self.apply_input_effects(initial_timer);
            self.schedule_playback_if_needed();
        }
        else if let Some(scene) = &mut self.scene
            && let Err(err) = scene.sync_layout(&self.compositor, &self.layout, &self.input)
        {
            tracing::warn!(error = %err, "Failed syncing Wayland overlay layout on configure");
        }
    }
}

impl SeatHandler for OverlayRuntime
{
    fn seat_state(&mut self) -> &mut SeatState
    {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    )
    {
        if capability == Capability::Pointer && self.themed_pointer.is_none()
        {
            match self.seat_state.get_pointer_with_theme::<Self, ()>(
                qh,
                &seat,
                self.shm.wl_shm(),
                self.cursor_surface.clone(),
                ThemeSpec::System,
            )
            {
                Ok(themed) =>
                {
                    tracing::debug!("Bound Wayland pointer with system cursor theme");
                    self.themed_pointer = Some(themed);
                }
                Err(err) =>
                {
                    tracing::warn!(error = %err, "Failed binding Wayland themed pointer");
                }
            }
        }

        if capability == Capability::Keyboard && self.keyboard.is_none()
        {
            match self.seat_state.get_keyboard(qh, &seat, None)
            {
                Ok(kbd) =>
                {
                    tracing::debug!("Bound Wayland keyboard on seat");
                    self.keyboard = Some(kbd);
                }
                Err(err) =>
                {
                    tracing::warn!(error = %err, "Failed binding Wayland keyboard");
                }
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    )
    {
        if capability == Capability::Pointer
        {
            let effects = self.input.on_pointer_detach();
            self.apply_input_effects(effects);
            if let Some(themed) = self.themed_pointer.take()
            {
                self.last_cursor = None;
                themed.pointer().release();
                tracing::debug!("Released Wayland pointer after capability removal");
            }
        }

        if capability == Capability::Keyboard
            && let Some(kbd) = self.keyboard.take()
        {
            kbd.release();
            tracing::debug!("Released Wayland keyboard after capability removal");
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl PointerHandler for OverlayRuntime
{
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    )
    {
        for event in events
        {
            let effects = match event.kind
            {
                PointerEventKind::Enter { .. } =>
                {
                    tracing::debug!(
                        x = event.position.0,
                        y = event.position.1,
                        editing = self.input.editing,
                        overlay = ?self.layout.overlay_rect(),
                        input = ?self.layout.input_rect(self.input.editing),
                        "Pointer entered overlay surface"
                    );
                    self.input.on_pointer_enter()
                }
                PointerEventKind::Leave { .. } =>
                {
                    self.last_cursor = None;
                    tracing::debug!(
                        x = event.position.0,
                        y = event.position.1,
                        "Pointer left overlay surface"
                    );
                    self.input.on_pointer_leave()
                }
                PointerEventKind::Motion { .. } => self
                    .input
                    .on_pointer_motion(event.position, &mut self.layout),
                PointerEventKind::Press { time, button, .. } =>
                {
                    tracing::debug!(
                        button,
                        x = event.position.0,
                        y = event.position.1,
                        editing = self.input.editing,
                        overlay = ?self.layout.overlay_rect(),
                        "Pointer button pressed"
                    );
                    self.input
                        .on_button_press(button, time, event.position, &self.layout)
                }
                PointerEventKind::Release { button, .. } =>
                {
                    tracing::debug!(
                        button,
                        x = event.position.0,
                        y = event.position.1,
                        overlay = ?self.layout.overlay_rect(),
                        "Pointer button released"
                    );
                    self.input.on_button_release(button)
                }
                PointerEventKind::Axis { vertical, .. } =>
                {
                    self.input.on_scroll(vertical, &mut self.layout)
                }
            };

            self.apply_input_effects(effects);
            if !matches!(event.kind, PointerEventKind::Leave { .. })
            {
                self.update_pointer_cursor(event.position);
            }
        }
    }
}

impl KeyboardHandler for OverlayRuntime
{
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    )
    {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: &WlSurface,
        _: u32,
    )
    {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    )
    {
        let effects = self.input.on_key_press(event.keysym, &mut self.layout);
        self.apply_input_effects(effects);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    )
    {
        let effects = self.input.on_key_repeat(event.keysym, &mut self.layout);
        self.apply_input_effects(effects);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    )
    {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    )
    {
    }
}

impl Dispatch<WpViewporter, GlobalData> for OverlayRuntime
{
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: ViewporterEvent,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    )
    {
    }
}

impl Dispatch<WpViewport, ()> for OverlayRuntime
{
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: ViewportEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    )
    {
    }
}

delegate_registry!(OverlayRuntime);
smithay_client_toolkit::delegate_dispatch2!(OverlayRuntime);
