use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use android_activity::input::{
    Axis, Button, InputEvent, KeyAction, Keycode, MotionAction, Source, ToolType,
};
use android_activity::{
    AndroidApp, AndroidAppWaker, ConfigurationRef, InputStatus, MainEvent, Rect,
};
use dpi::{PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use tracing::{debug, trace, warn};
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{Cursor, CustomCursor, CustomCursorSource};
use winit_core::error::{EventLoopError, NotSupportedError, RequestError};
use winit_core::event::{self, DeviceId, FingerId, Force, StartCause, SurfaceSizeWriter};
use winit_core::event_loop::pump_events::PumpStatus;
use winit_core::event_loop::{
    ActiveEventLoop as RootActiveEventLoop, ControlFlow, DeviceEvents,
    EventLoopProxy as CoreEventLoopProxy, EventLoopProxyProvider,
    OwnedDisplayHandle as CoreOwnedDisplayHandle,
};
#[cfg(feature = "game-activity")]
use winit_core::keyboard::{
    Key as CoreKey, KeyCode as CoreKeyCode, KeyLocation, NamedKey, PhysicalKey,
};
use winit_core::monitor::{Fullscreen, MonitorHandle as CoreMonitorHandle};
use winit_core::window::{
    self, CursorGrabMode, ImeCapabilities, ImePurpose, ImeRequest, ImeRequestError,
    ResizeDirection, Theme, Window as CoreWindow, WindowAttributes, WindowButtons, WindowId,
    WindowLevel,
};

use crate::keycodes;

static HAS_FOCUS: AtomicBool = AtomicBool::new(true);
static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);

/// Returns the minimum `Option<Duration>`, taking into account that `None`
/// equates to an infinite timeout, not a zero timeout (so can't just use
/// `Option::min`)
fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    a.map_or(b, |a_timeout| b.map_or(Some(a_timeout), |b_timeout| Some(a_timeout.min(b_timeout))))
}

#[derive(Clone, Debug)]
struct SharedFlagSetter {
    flag: Arc<AtomicBool>,
}
impl SharedFlagSetter {
    fn set(&self) -> bool {
        self.flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }
}

#[derive(Debug)]
struct SharedFlag {
    flag: Arc<AtomicBool>,
}

// Used for queuing redraws from arbitrary threads. We don't care how many
// times a redraw is requested (so don't actually need to queue any data,
// we just need to know at the start of a main loop iteration if a redraw
// was queued and be able to read and clear the state atomically)
impl SharedFlag {
    fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }

    fn setter(&self) -> SharedFlagSetter {
        SharedFlagSetter { flag: self.flag.clone() }
    }

    fn get_and_reset(&self) -> bool {
        self.flag.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

#[derive(Clone)]
struct RedrawRequester {
    flag: SharedFlagSetter,
    waker: AndroidAppWaker,
}

impl fmt::Debug for RedrawRequester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedrawRequester").field("flag", &self.flag).finish_non_exhaustive()
    }
}

impl RedrawRequester {
    fn new(flag: &SharedFlag, waker: AndroidAppWaker) -> Self {
        RedrawRequester { flag: flag.setter(), waker }
    }

    fn request_redraw(&self) {
        if self.flag.set() {
            // Only explicitly try to wake up the main loop when the flag
            // value changes
            self.waker.wake();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TouchContact {
    device_id: Option<DeviceId>,
    position: PhysicalPosition<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct TouchGestureState {
    pan_active: bool,
    transform_active: bool,
    centroid: Option<PhysicalPosition<f64>>,
    span: Option<f64>,
    angle_deg: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
struct TouchSnapshot {
    count: usize,
    centroid: PhysicalPosition<f64>,
    span: Option<f64>,
    angle_deg: Option<f64>,
}

#[derive(Debug)]
pub struct EventLoop {
    pub android_app: AndroidApp,
    window_target: ActiveEventLoop,
    redraw_flag: SharedFlag,
    loop_running: bool, // Dispatched `NewEvents<Init>`
    running: bool,
    pending_redraw: bool,
    cause: StartCause,
    primary_pointer: Option<FingerId>,
    touch_contacts: HashMap<FingerId, TouchContact>,
    touch_gestures: TouchGestureState,
    display_scale_factor: f64,
    ignore_volume_keys: bool,
    combining_accent: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformSpecificEventLoopAttributes {
    pub android_app: Option<AndroidApp>,
    pub ignore_volume_keys: bool,
}

impl Default for PlatformSpecificEventLoopAttributes {
    fn default() -> Self {
        Self { android_app: Default::default(), ignore_volume_keys: true }
    }
}

// Android currently only supports one window
const GLOBAL_WINDOW: WindowId = WindowId::from_raw(0);

impl EventLoop {
    pub fn new(attributes: &PlatformSpecificEventLoopAttributes) -> Result<Self, EventLoopError> {
        if EVENT_LOOP_CREATED.swap(true, Ordering::Relaxed) {
            // For better cross-platformness.
            return Err(EventLoopError::RecreationAttempt);
        }

        let android_app = attributes.android_app.as_ref().expect(
            "An `AndroidApp` as passed to android_main() is required to create an `EventLoop` on \
             Android",
        );

        let event_loop_proxy = Arc::new(EventLoopProxy::new(android_app.create_waker()));

        let redraw_flag = SharedFlag::new();
        let display_scale_factor = scale_factor(android_app);

        Ok(Self {
            android_app: android_app.clone(),
            primary_pointer: None,
            touch_contacts: HashMap::new(),
            touch_gestures: TouchGestureState::default(),
            display_scale_factor,
            window_target: ActiveEventLoop {
                app: android_app.clone(),
                control_flow: Cell::new(ControlFlow::default()),
                exit: Cell::new(false),
                redraw_requester: RedrawRequester::new(&redraw_flag, android_app.create_waker()),
                event_loop_proxy,
            },
            redraw_flag,
            loop_running: false,
            running: false,
            pending_redraw: false,
            cause: StartCause::Init,
            ignore_volume_keys: attributes.ignore_volume_keys,
            combining_accent: None,
        })
    }

    pub fn window_target(&self) -> &dyn RootActiveEventLoop {
        &self.window_target
    }

    fn single_iteration<A: ApplicationHandler>(
        &mut self,
        main_event: Option<MainEvent<'_>>,
        app: &mut A,
    ) {
        trace!("Mainloop iteration");

        let cause = self.cause;
        let mut pending_redraw = self.pending_redraw;
        let mut resized = false;

        app.new_events(&self.window_target, cause);

        if let Some(event) = main_event {
            trace!("Handling main event {:?}", event);

            match event {
                MainEvent::InitWindow { .. } => {
                    app.can_create_surfaces(&self.window_target);
                },
                MainEvent::TerminateWindow { .. } => {
                    self.cancel_touch_tracking(app);
                    app.destroy_surfaces(&self.window_target);
                },
                MainEvent::WindowResized { .. } => resized = true,
                MainEvent::RedrawNeeded { .. } => pending_redraw = true,
                MainEvent::ContentRectChanged { .. } => {
                    warn!("TODO: find a way to notify application of content rect change");
                },
                MainEvent::GainedFocus => {
                    HAS_FOCUS.store(true, Ordering::Relaxed);
                    let event = event::WindowEvent::Focused(true);
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                },
                MainEvent::LostFocus => {
                    HAS_FOCUS.store(false, Ordering::Relaxed);
                    self.cancel_touch_tracking(app);
                    let event = event::WindowEvent::Focused(false);
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                },
                MainEvent::ConfigChanged { .. } => {
                    let scale_factor = scale_factor(&self.android_app);
                    if (scale_factor - self.display_scale_factor).abs() > f64::EPSILON {
                        self.display_scale_factor = scale_factor;
                        let new_surface_size = Arc::new(Mutex::new(screen_size(&self.android_app)));
                        let event = event::WindowEvent::ScaleFactorChanged {
                            surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(
                                &new_surface_size,
                            )),
                            scale_factor,
                        };

                        app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                    }
                },
                MainEvent::LowMemory => {
                    app.memory_warning(&self.window_target);
                },
                MainEvent::Start => {
                    app.resumed(self.window_target());
                },
                MainEvent::Resume { .. } => {
                    debug!("App Resumed - is running");
                    // TODO: This is incorrect - will be solved in https://github.com/rust-windowing/winit/pull/3897
                    self.running = true;
                },
                MainEvent::SaveState { .. } => {
                    // XXX: how to forward this state to applications?
                    // XXX: also how do we expose state restoration to apps?
                    warn!("TODO: forward saveState notification to application");
                },
                MainEvent::Pause => {
                    debug!("App Paused - stopped running");
                    // TODO: This is incorrect - will be solved in https://github.com/rust-windowing/winit/pull/3897
                    self.running = false;
                },
                MainEvent::Stop => {
                    app.suspended(self.window_target());
                },
                MainEvent::Destroy => {
                    // GameActivity.onDestroy calls terminateNativeCode(), which waits for
                    // android_main to return. If Destroy is only logged and ignored here,
                    // Android can hang in Activity teardown and reopen to a black surface.
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
                        event::WindowEvent::CloseRequested,
                    );
                    self.window_target.exit();
                },
                MainEvent::InsetsChanged { .. } => {
                    // XXX: how to forward this state to applications?
                    warn!("TODO: handle Android InsetsChanged notification");
                },
                unknown => {
                    trace!("Unknown MainEvent {unknown:?} (ignored)");
                },
            }
        } else {
            trace!("No main event to handle");
        }

        // temporarily decouple `android_app` from `self` so we aren't holding
        // a borrow of `self` while iterating
        let android_app = self.android_app.clone();

        // Process input events
        match android_app.input_events_iter() {
            Ok(mut input_iter) => loop {
                let read_event =
                    input_iter.next(|event| self.handle_input_event(&android_app, event, app));

                if !read_event {
                    break;
                }
            },
            Err(err) => {
                tracing::warn!("Failed to get input events iterator: {err:?}");
            },
        }

        if self.window_target.event_loop_proxy.wake_up.swap(false, Ordering::Relaxed) {
            app.proxy_wake_up(&self.window_target);
        }

        if self.running {
            if resized {
                let size = if let Some(native_window) = self.android_app.native_window().as_ref() {
                    let width = native_window.width() as _;
                    let height = native_window.height() as _;
                    PhysicalSize::new(width, height)
                } else {
                    PhysicalSize::new(0, 0)
                };
                let event = event::WindowEvent::SurfaceResized(size);
                app.window_event(&self.window_target, GLOBAL_WINDOW, event);
            }

            pending_redraw |= self.redraw_flag.get_and_reset();
            if pending_redraw {
                pending_redraw = false;
                let event = event::WindowEvent::RedrawRequested;
                app.window_event(&self.window_target, GLOBAL_WINDOW, event);
            }
        }

        // This is always the last event we dispatch before poll again
        app.about_to_wait(&self.window_target);

        self.pending_redraw = pending_redraw;
    }

    fn handle_input_event<A: ApplicationHandler>(
        &mut self,
        android_app: &AndroidApp,
        event: &InputEvent<'_>,
        app: &mut A,
    ) -> InputStatus {
        let mut input_status = InputStatus::Handled;
        match event {
            InputEvent::MotionEvent(motion_event) => {
                let device_id = Some(DeviceId::from_raw(motion_event.device_id() as i64));
                let action = motion_event.action();
                trace!("Input event {device_id:?}, {action:?}, source={:?}", motion_event.source());

                match action {
                    MotionAction::Down | MotionAction::PointerDown => {
                        self.handle_contact_down(motion_event, device_id, action, app);
                    },
                    MotionAction::Move => {
                        self.handle_motion_move(motion_event, device_id, app);
                    },
                    MotionAction::Up | MotionAction::PointerUp => {
                        self.handle_contact_up(motion_event, device_id, app);
                    },
                    MotionAction::Cancel => {
                        self.handle_motion_cancel(motion_event, device_id, app);
                    },
                    MotionAction::HoverEnter
                    | MotionAction::HoverMove
                    | MotionAction::HoverExit => {
                        self.handle_hover_event(motion_event, device_id, action, app);
                    },
                    MotionAction::Scroll => {
                        self.handle_scroll_event(motion_event, device_id, app);
                    },
                    MotionAction::ButtonPress | MotionAction::ButtonRelease => {
                        self.handle_button_event(motion_event, device_id, action, app);
                    },
                    _ => {},
                }
            },
            InputEvent::KeyEvent(key) => {
                match key.key_code() {
                    // Flag keys related to volume as unhandled. While winit does not have a way for
                    // applications to configure what keys to flag as handled,
                    // this appears to be a good default until winit
                    // can be configured.
                    Keycode::VolumeUp | Keycode::VolumeDown | Keycode::VolumeMute
                        if self.ignore_volume_keys =>
                    {
                        input_status = InputStatus::Unhandled
                    },
                    keycode => {
                        let state = match key.action() {
                            KeyAction::Down => event::ElementState::Pressed,
                            KeyAction::Up => event::ElementState::Released,
                            _ => event::ElementState::Released,
                        };

                        let key_char = keycodes::character_map_and_combine_key(
                            android_app,
                            key,
                            &mut self.combining_accent,
                        );

                        let logical_key = keycodes::to_logical(key_char, keycode);
                        let text = if state == event::ElementState::Pressed {
                            logical_key.to_text().map(smol_str::SmolStr::new)
                        } else {
                            None
                        };

                        let event = event::WindowEvent::KeyboardInput {
                            device_id: Some(DeviceId::from_raw(key.device_id() as i64)),
                            event: event::KeyEvent {
                                state,
                                physical_key: keycodes::to_physical_key(keycode),
                                logical_key,
                                location: keycodes::to_location(keycode),
                                repeat: key.repeat_count() > 0,
                                text: text.clone(),
                                text_with_all_modifiers: text,
                                key_without_modifiers: keycodes::to_logical(key_char, keycode),
                            },
                            is_synthetic: false,
                        };

                        app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                    },
                }
            },
            #[cfg(feature = "game-activity")]
            InputEvent::TextEvent(state) => {
                if let Some((preedit, cursor)) = self.ime_preedit_from_state(state) {
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
                        event::WindowEvent::Ime(event::Ime::Preedit(preedit, cursor)),
                    );
                } else {
                    let text = state.text.clone();
                    if !text.is_empty() {
                        app.window_event(
                            &self.window_target,
                            GLOBAL_WINDOW,
                            event::WindowEvent::Ime(event::Ime::Preedit(String::new(), None)),
                        );
                        app.window_event(
                            &self.window_target,
                            GLOBAL_WINDOW,
                            event::WindowEvent::Ime(event::Ime::Commit(text)),
                        );
                        android_app.set_text_input_state(
                            android_activity::input::TextInputState::default(),
                        );
                    }
                }
            },
            #[cfg(feature = "game-activity")]
            InputEvent::TextAction(action) => {
                if let Some(event) = self.text_action_keyboard_event(*action) {
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                }
            },
            _ => {
                warn!("Unknown android_activity input event {event:?}")
            },
        }

        input_status
    }

    #[cfg(feature = "game-activity")]
    fn ime_preedit_from_state(
        &self,
        state: &android_activity::input::TextInputState,
    ) -> Option<(String, Option<(usize, usize)>)> {
        let span = state.compose_region?;
        if span.start >= span.end || span.end > state.text.len() {
            return None;
        }

        let preedit = state.text.get(span.start..span.end)?.to_string();
        if preedit.is_empty() {
            return None;
        }

        let selection = state.selection.start.min(state.selection.end);
        let cursor = if (span.start..=span.end).contains(&selection) {
            selection.saturating_sub(span.start)
        } else {
            preedit.len()
        };
        Some((preedit, Some((cursor, cursor))))
    }

    #[cfg(feature = "game-activity")]
    fn text_action_keyboard_event(
        &self,
        action: android_activity::input::TextInputAction,
    ) -> Option<event::WindowEvent> {
        let (logical_key, physical_key, key_without_modifiers) = match action {
            android_activity::input::TextInputAction::Unspecified
            | android_activity::input::TextInputAction::None
            | android_activity::input::TextInputAction::Go
            | android_activity::input::TextInputAction::Search
            | android_activity::input::TextInputAction::Send
            | android_activity::input::TextInputAction::Done => (
                CoreKey::Named(NamedKey::Enter),
                PhysicalKey::Code(CoreKeyCode::Enter),
                CoreKey::Named(NamedKey::Enter),
            ),
            android_activity::input::TextInputAction::Next => (
                CoreKey::Named(NamedKey::Tab),
                PhysicalKey::Code(CoreKeyCode::Tab),
                CoreKey::Named(NamedKey::Tab),
            ),
            _ => return None,
        };

        Some(event::WindowEvent::KeyboardInput {
            device_id: None,
            event: event::KeyEvent {
                state: event::ElementState::Pressed,
                physical_key,
                logical_key,
                location: KeyLocation::Standard,
                repeat: false,
                text: None,
                text_with_all_modifiers: None,
                key_without_modifiers,
            },
            is_synthetic: false,
        })
    }

    fn handle_contact_down<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        action: MotionAction,
        app: &mut A,
    ) {
        let pointer = motion_event.pointer_at_index(motion_event.pointer_index());
        let position = pointer_position(&pointer);
        let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
        let Some(pointer_state) =
            android_pointer_state(motion_event.source(), pointer.tool_type(), finger_id, &pointer)
        else {
            return;
        };

        let primary = match pointer_state.kind {
            event::PointerKind::Touch(_) => {
                let primary = action == MotionAction::Down;
                if primary {
                    self.primary_pointer = Some(finger_id);
                }
                self.touch_contacts.insert(finger_id, TouchContact { device_id, position });
                primary
            },
            _ => true,
        };

        self.emit_window_event(
            app,
            event::WindowEvent::PointerEntered {
                device_id,
                primary,
                position,
                kind: pointer_state.kind,
            },
        );
        self.emit_window_event(
            app,
            event::WindowEvent::PointerButton {
                device_id,
                primary,
                state: event::ElementState::Pressed,
                position,
                button: pointer_state.contact_button(),
            },
        );

        if matches!(pointer_state.kind, event::PointerKind::Touch(_)) {
            self.sync_touch_gestures(device_id, false, false, app);
        }
    }

    fn handle_motion_move<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        app: &mut A,
    ) {
        let mut touched = false;

        for pointer in motion_event.pointers() {
            let position = pointer_position(&pointer);
            let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
            let Some(pointer_state) = android_pointer_state(
                motion_event.source(),
                pointer.tool_type(),
                finger_id,
                &pointer,
            ) else {
                continue;
            };

            if matches!(pointer_state.kind, event::PointerKind::Touch(_)) {
                if let Some(contact) = self.touch_contacts.get_mut(&finger_id) {
                    contact.position = position;
                } else {
                    self.touch_contacts.insert(finger_id, TouchContact { device_id, position });
                }
                touched = true;
            }

            let primary = match pointer_state.kind {
                event::PointerKind::Touch(_) => self.primary_pointer == Some(finger_id),
                _ => true,
            };

            self.emit_window_event(
                app,
                event::WindowEvent::PointerMoved {
                    device_id,
                    primary,
                    position,
                    source: pointer_state.pointer_source(),
                },
            );
        }

        if touched {
            self.sync_touch_gestures(device_id, true, false, app);
        }
    }

    fn handle_contact_up<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        app: &mut A,
    ) {
        let pointer = motion_event.pointer_at_index(motion_event.pointer_index());
        let position = pointer_position(&pointer);
        let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
        let Some(pointer_state) =
            android_pointer_state(motion_event.source(), pointer.tool_type(), finger_id, &pointer)
        else {
            return;
        };

        let primary = match pointer_state.kind {
            event::PointerKind::Touch(_) => {
                let primary = self.primary_pointer == Some(finger_id);
                if primary {
                    self.primary_pointer = None;
                }
                self.touch_contacts.remove(&finger_id);
                primary
            },
            _ => true,
        };

        self.emit_window_event(
            app,
            event::WindowEvent::PointerButton {
                device_id,
                primary,
                state: event::ElementState::Released,
                position,
                button: pointer_state.contact_button(),
            },
        );
        self.emit_window_event(
            app,
            event::WindowEvent::PointerLeft {
                device_id,
                primary,
                position: Some(position),
                kind: pointer_state.kind,
            },
        );

        if matches!(pointer_state.kind, event::PointerKind::Touch(_)) {
            self.sync_touch_gestures(device_id, false, false, app);
        }
    }

    fn handle_motion_cancel<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        app: &mut A,
    ) {
        for pointer in motion_event.pointers() {
            let position = pointer_position(&pointer);
            let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
            let Some(pointer_state) = android_pointer_state(
                motion_event.source(),
                pointer.tool_type(),
                finger_id,
                &pointer,
            ) else {
                continue;
            };

            if matches!(pointer_state.kind, event::PointerKind::Touch(_)) {
                continue;
            }

            self.emit_window_event(
                app,
                event::WindowEvent::PointerLeft {
                    device_id,
                    primary: true,
                    position: Some(position),
                    kind: pointer_state.kind,
                },
            );
        }

        let touched = self.emit_tracked_touch_cancellations(app);
        if touched || self.touch_gestures.pan_active || self.touch_gestures.transform_active {
            self.sync_touch_gestures(device_id, false, true, app);
        }
    }

    fn handle_hover_event<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        action: MotionAction,
        app: &mut A,
    ) {
        let pointer = motion_event.pointer_at_index(motion_event.pointer_index());
        let position = pointer_position(&pointer);
        let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
        let Some(pointer_state) =
            android_pointer_state(motion_event.source(), pointer.tool_type(), finger_id, &pointer)
        else {
            return;
        };

        match action {
            MotionAction::HoverEnter => self.emit_window_event(
                app,
                event::WindowEvent::PointerEntered {
                    device_id,
                    primary: true,
                    position,
                    kind: pointer_state.kind,
                },
            ),
            MotionAction::HoverMove => self.emit_window_event(
                app,
                event::WindowEvent::PointerMoved {
                    device_id,
                    primary: true,
                    position,
                    source: pointer_state.pointer_source(),
                },
            ),
            MotionAction::HoverExit => self.emit_window_event(
                app,
                event::WindowEvent::PointerLeft {
                    device_id,
                    primary: true,
                    position: Some(position),
                    kind: pointer_state.kind,
                },
            ),
            _ => {},
        }
    }

    fn handle_scroll_event<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        app: &mut A,
    ) {
        let pointer = motion_event.pointer_at_index(motion_event.pointer_index());
        let position = pointer_position(&pointer);
        let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
        let Some(pointer_state) =
            android_pointer_state(motion_event.source(), pointer.tool_type(), finger_id, &pointer)
        else {
            return;
        };

        self.emit_window_event(
            app,
            event::WindowEvent::PointerMoved {
                device_id,
                primary: true,
                position,
                source: pointer_state.pointer_source(),
            },
        );

        let delta = event::MouseScrollDelta::LineDelta(
            -pointer.axis_value(Axis::Hscroll),
            -pointer.axis_value(Axis::Vscroll),
        );
        self.emit_window_event(
            app,
            event::WindowEvent::MouseWheel { device_id, delta, phase: event::TouchPhase::Moved },
        );
    }

    fn handle_button_event<A: ApplicationHandler>(
        &mut self,
        motion_event: &android_activity::input::MotionEvent<'_>,
        device_id: Option<DeviceId>,
        action: MotionAction,
        app: &mut A,
    ) {
        let pointer = motion_event.pointer_at_index(motion_event.pointer_index());
        let position = pointer_position(&pointer);
        let finger_id = FingerId::from_raw(pointer.pointer_id() as usize);
        let Some(pointer_state) =
            android_pointer_state(motion_event.source(), pointer.tool_type(), finger_id, &pointer)
        else {
            return;
        };

        let state = match action {
            MotionAction::ButtonPress => event::ElementState::Pressed,
            MotionAction::ButtonRelease => event::ElementState::Released,
            _ => return,
        };

        let Some(button) = action_button_source(motion_event.action_button(), &pointer_state)
        else {
            return;
        };

        self.emit_window_event(
            app,
            event::WindowEvent::PointerButton { device_id, primary: true, state, position, button },
        );
    }

    fn emit_window_event<A: ApplicationHandler>(&self, app: &mut A, event: event::WindowEvent) {
        app.window_event(&self.window_target, GLOBAL_WINDOW, event);
    }

    fn sync_touch_gestures<A: ApplicationHandler>(
        &mut self,
        device_id: Option<DeviceId>,
        allow_move: bool,
        cancelled: bool,
        app: &mut A,
    ) {
        let snapshot = touch_snapshot(&self.touch_contacts);
        let phase_end =
            if cancelled { event::TouchPhase::Cancelled } else { event::TouchPhase::Ended };

        let pan_should_be_active = snapshot.as_ref().is_some_and(|snapshot| snapshot.count >= 2);
        if !self.touch_gestures.pan_active && pan_should_be_active {
            self.touch_gestures.pan_active = true;
            self.emit_window_event(
                app,
                event::WindowEvent::PanGesture {
                    device_id,
                    delta: PhysicalPosition::new(0.0, 0.0),
                    phase: event::TouchPhase::Started,
                },
            );
        } else if self.touch_gestures.pan_active && !pan_should_be_active {
            self.touch_gestures.pan_active = false;
            self.emit_window_event(
                app,
                event::WindowEvent::PanGesture {
                    device_id,
                    delta: PhysicalPosition::new(0.0, 0.0),
                    phase: phase_end,
                },
            );
        }

        let transform_should_be_active =
            snapshot.as_ref().is_some_and(|snapshot| snapshot.count == 2);
        if !self.touch_gestures.transform_active && transform_should_be_active {
            self.touch_gestures.transform_active = true;
            self.emit_window_event(
                app,
                event::WindowEvent::PinchGesture {
                    device_id,
                    delta: 0.0,
                    phase: event::TouchPhase::Started,
                },
            );
            self.emit_window_event(
                app,
                event::WindowEvent::RotationGesture {
                    device_id,
                    delta: 0.0,
                    phase: event::TouchPhase::Started,
                },
            );
        } else if self.touch_gestures.transform_active && !transform_should_be_active {
            self.touch_gestures.transform_active = false;
            self.emit_window_event(
                app,
                event::WindowEvent::PinchGesture { device_id, delta: 0.0, phase: phase_end },
            );
            self.emit_window_event(
                app,
                event::WindowEvent::RotationGesture { device_id, delta: 0.0, phase: phase_end },
            );
        }

        if allow_move {
            if let Some(snapshot) = snapshot {
                if self.touch_gestures.pan_active {
                    if let Some(previous_centroid) = self.touch_gestures.centroid {
                        let delta = PhysicalPosition::new(
                            (snapshot.centroid.x - previous_centroid.x) as f32,
                            (snapshot.centroid.y - previous_centroid.y) as f32,
                        );
                        self.emit_window_event(
                            app,
                            event::WindowEvent::PanGesture {
                                device_id,
                                delta,
                                phase: event::TouchPhase::Moved,
                            },
                        );
                    }
                }

                if self.touch_gestures.transform_active {
                    if let (Some(previous_span), Some(span)) =
                        (self.touch_gestures.span, snapshot.span)
                    {
                        let delta = if previous_span.abs() > f64::EPSILON {
                            span / previous_span - 1.0
                        } else {
                            0.0
                        };
                        self.emit_window_event(
                            app,
                            event::WindowEvent::PinchGesture {
                                device_id,
                                delta,
                                phase: event::TouchPhase::Moved,
                            },
                        );
                    }

                    if let (Some(previous_angle), Some(angle_deg)) =
                        (self.touch_gestures.angle_deg, snapshot.angle_deg)
                    {
                        self.emit_window_event(
                            app,
                            event::WindowEvent::RotationGesture {
                                device_id,
                                delta: normalized_angle_delta_deg(previous_angle, angle_deg) as f32,
                                phase: event::TouchPhase::Moved,
                            },
                        );
                    }
                }
            }
        }

        self.touch_gestures.centroid = snapshot.map(|snapshot| snapshot.centroid);
        self.touch_gestures.span = snapshot.and_then(|snapshot| snapshot.span);
        self.touch_gestures.angle_deg = snapshot.and_then(|snapshot| snapshot.angle_deg);
    }


    fn cancel_touch_tracking<A: ApplicationHandler>(&mut self, app: &mut A) {
        self.emit_tracked_touch_cancellations(app);
        if self.touch_gestures.pan_active || self.touch_gestures.transform_active {
            self.sync_touch_gestures(None, false, true, app);
        }
        self.touch_gestures = TouchGestureState::default();
    }

    fn emit_tracked_touch_cancellations<A: ApplicationHandler>(&mut self, app: &mut A) -> bool {
        let primary_pointer = self.primary_pointer.take();
        let mut contacts = std::mem::take(&mut self.touch_contacts);
        let touched = !contacts.is_empty();
        if let Some(finger_id) = primary_pointer
            && let Some(contact) = contacts.remove(&finger_id)
        {
            self.emit_touch_cancellation(finger_id, contact, true, app);
        }
        for (finger_id, contact) in contacts {
            self.emit_touch_cancellation(finger_id, contact, false, app);
        }
        touched
    }

    fn emit_touch_cancellation<A: ApplicationHandler>(
        &self,
        finger_id: FingerId,
        contact: TouchContact,
        primary: bool,
        app: &mut A,
    ) {
        self.emit_window_event(
            app,
            event::WindowEvent::PointerLeft {
                device_id: contact.device_id,
                primary,
                position: Some(contact.position),
                kind: event::PointerKind::Touch(finger_id),
            },
        );
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        self.window_target.clear_exit();
        loop {
            match self.pump_app_events(None, &mut app) {
                PumpStatus::Exit(0) => {
                    break Ok(());
                },
                PumpStatus::Exit(code) => {
                    break Err(EventLoopError::ExitFailure(code));
                },
                _ => {
                    continue;
                },
            }
        }
    }

    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        timeout: Option<Duration>,
        mut app: A,
    ) -> PumpStatus {
        if !self.loop_running {
            self.loop_running = true;

            // Reset the internal state for the loop as we start running to
            // ensure consistent behaviour in case the loop runs and exits more
            // than once
            self.pending_redraw = false;
            self.cause = StartCause::Init;

            // run the initial loop iteration
            self.single_iteration(None, &mut app);
        }

        // Consider the possibility that the `StartCause::Init` iteration could
        // request to Exit
        if !self.exiting() {
            self.poll_events_with_timeout(timeout, &mut app);
        }
        if self.exiting() {
            self.loop_running = false;

            PumpStatus::Exit(0)
        } else {
            PumpStatus::Continue
        }
    }

    fn poll_events_with_timeout<A: ApplicationHandler>(
        &mut self,
        mut timeout: Option<Duration>,
        app: &mut A,
    ) {
        let start = Instant::now();

        self.pending_redraw |= self.redraw_flag.get_and_reset();

        // Mirrors the `PollEvent::Wake` filter below: a pending proxy wake up is work to do
        // whether or not we are running, while a pending redraw only counts while running.
        timeout = if self.window_target.event_loop_proxy.wake_up.load(Ordering::Relaxed)
            || (self.running && self.pending_redraw)
        {
            // If we already have work to do then we don't want to block on the next poll
            Some(Duration::ZERO)
        } else {
            let control_flow_timeout = match self.control_flow() {
                ControlFlow::Wait => None,
                ControlFlow::Poll => Some(Duration::ZERO),
                ControlFlow::WaitUntil(wait_deadline) => {
                    Some(wait_deadline.saturating_duration_since(start))
                },
            };

            min_timeout(control_flow_timeout, timeout)
        };

        let android_app = self.android_app.clone(); // Don't borrow self as part of poll expression
        android_app.poll_events(timeout, |poll_event| {
            let mut main_event = None;

            match poll_event {
                android_activity::PollEvent::Wake => {
                    // In the X11 backend it's noted that too many false-positive wake ups
                    // would cause the event loop to run continuously. They handle this by
                    // re-checking for pending events (assuming they cover all
                    // valid reasons for a wake up).
                    //
                    // For now, user_events and redraw_requests are the only reasons to expect
                    // a wake up here so we can ignore the wake up if there are no events/requests.
                    //
                    // Proxy wake ups are dispatched even while suspended. `single_iteration`
                    // keeps every drawing side effect behind its own `self.running` check, so
                    // running it here only delivers `proxy_wake_up` — background work such as
                    // terminal output that raises a notification must not be deferred until the
                    // app resumes. Redraw requests stay suppressed while suspended: there is no
                    // surface to draw to, and honouring them would spin, because `about_to_wait`
                    // can request another redraw on every iteration.
                    self.pending_redraw |= self.redraw_flag.get_and_reset();
                    let woken_by_proxy =
                        self.window_target.event_loop_proxy.wake_up.load(Ordering::Relaxed);
                    if !woken_by_proxy && !(self.running && self.pending_redraw) {
                        return;
                    }
                },
                android_activity::PollEvent::Timeout => {},
                android_activity::PollEvent::Main(event) => {
                    main_event = Some(event);
                },
                unknown_event => {
                    warn!("Unknown poll event {unknown_event:?} (ignored)");
                },
            }

            self.cause = match self.control_flow() {
                ControlFlow::Poll => StartCause::Poll,
                ControlFlow::Wait => StartCause::WaitCancelled { start, requested_resume: None },
                ControlFlow::WaitUntil(deadline) => {
                    if Instant::now() < deadline {
                        StartCause::WaitCancelled { start, requested_resume: Some(deadline) }
                    } else {
                        StartCause::ResumeTimeReached { start, requested_resume: deadline }
                    }
                },
            };

            self.single_iteration(main_event, app);
        });
    }

    fn control_flow(&self) -> ControlFlow {
        self.window_target.control_flow()
    }

    fn exiting(&self) -> bool {
        self.window_target.exiting()
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        EVENT_LOOP_CREATED.store(false, Ordering::Relaxed);
    }
}

pub struct EventLoopProxy {
    wake_up: AtomicBool,
    waker: AndroidAppWaker,
}

impl fmt::Debug for EventLoopProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLoopProxy").field("wake_up", &self.wake_up).finish_non_exhaustive()
    }
}

impl EventLoopProxy {
    fn new(waker: AndroidAppWaker) -> Self {
        Self { wake_up: AtomicBool::new(false), waker }
    }
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        self.wake_up.store(true, Ordering::Relaxed);
        self.waker.wake();
    }
}

#[derive(Debug)]
pub struct ActiveEventLoop {
    pub(crate) app: AndroidApp,
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    redraw_requester: RedrawRequester,
    event_loop_proxy: Arc<EventLoopProxy>,
}

impl ActiveEventLoop {
    fn clear_exit(&self) {
        self.exit.set(false);
    }
}

impl RootActiveEventLoop for ActiveEventLoop {
    fn create_proxy(&self) -> CoreEventLoopProxy {
        CoreEventLoopProxy::new(self.event_loop_proxy.clone())
    }

    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, RequestError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn create_custom_cursor(
        &self,
        _source: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError> {
        Err(NotSupportedError::new("create_custom_cursor is not supported").into())
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn system_theme(&self) -> Option<Theme> {
        None
    }

    fn listen_device_events(&self, _allowed: DeviceEvents) {}

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.control_flow.set(control_flow)
    }

    fn control_flow(&self) -> ControlFlow {
        self.control_flow.get()
    }

    fn exit(&self) {
        self.exit.set(true)
    }

    fn exiting(&self) -> bool {
        self.exit.get()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        CoreOwnedDisplayHandle::new(Arc::new(OwnedDisplayHandle))
    }

    fn rwh_06_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }
}

impl rwh_06::HasDisplayHandle for ActiveEventLoop {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::AndroidDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OwnedDisplayHandle;

impl rwh_06::HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::AndroidDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformSpecificWindowAttributes;

#[derive(Debug)]
pub struct Window {
    app: AndroidApp,
    ime_capabilities: Mutex<Option<ImeCapabilities>>,
    redraw_requester: RedrawRequester,
}

impl Window {
    pub(crate) fn new(
        el: &ActiveEventLoop,
        _window_attrs: window::WindowAttributes,
    ) -> Result<Self, RequestError> {
        // FIXME this ignores requested window attributes

        Ok(Self {
            app: el.app.clone(),
            ime_capabilities: Default::default(),
            redraw_requester: el.redraw_requester.clone(),
        })
    }

    pub(crate) fn config(&self) -> ConfigurationRef {
        self.app.config()
    }

    pub(crate) fn content_rect(&self) -> Rect {
        self.app.content_rect()
    }

    pub(crate) fn android_app(&self) -> AndroidApp {
        self.app.clone()
    }

    // Allow the usage of HasRawWindowHandle inside this function
    #[allow(deprecated)]
    fn raw_window_handle_rwh_06(&self) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        use rwh_06::HasRawWindowHandle;

        if let Some(native_window) = self.app.native_window().as_ref() {
            native_window.raw_window_handle()
        } else {
            tracing::error!(
                "Cannot get the native window, it's null and will always be null before \
                 Event::Resumed and after Event::Suspended. Make sure you only call this function \
                 between those events."
            );
            Err(rwh_06::HandleError::Unavailable)
        }
    }

    fn raw_display_handle_rwh_06(&self) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::RawDisplayHandle::Android(rwh_06::AndroidDisplayHandle::new()))
    }
}

impl rwh_06::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_display_handle_rwh_06()?;
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_window_handle_rwh_06()?;
        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw)) }
    }
}

impl CoreWindow for Window {
    fn id(&self) -> WindowId {
        GLOBAL_WINDOW
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn scale_factor(&self) -> f64 {
        scale_factor(&self.app)
    }

    fn request_redraw(&self) {
        self.redraw_requester.request_redraw()
    }

    fn pre_present_notify(&self) {}

    fn surface_position(&self) -> PhysicalPosition<i32> {
        (0, 0).into()
    }

    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        Err(NotSupportedError::new("outer_position is not supported").into())
    }

    fn set_outer_position(&self, _position: Position) {
        // no effect
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        self.outer_size()
    }

    fn request_surface_size(&self, _size: Size) -> Option<PhysicalSize<u32>> {
        Some(self.surface_size())
    }

    fn outer_size(&self) -> PhysicalSize<u32> {
        screen_size(&self.app)
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        PhysicalInsets::new(0, 0, 0, 0)
    }

    fn set_min_surface_size(&self, _: Option<Size>) {}

    fn set_max_surface_size(&self, _: Option<Size>) {}

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        None
    }

    fn set_surface_resize_increments(&self, _increments: Option<Size>) {}

    fn set_title(&self, _title: &str) {}

    fn set_transparent(&self, _transparent: bool) {}

    fn set_blur(&self, _blur: bool) {}

    fn set_visible(&self, _visibility: bool) {}

    fn is_visible(&self) -> Option<bool> {
        None
    }

    fn set_resizable(&self, _resizeable: bool) {}

    fn is_resizable(&self) -> bool {
        false
    }

    fn set_enabled_buttons(&self, _buttons: WindowButtons) {}

    fn enabled_buttons(&self) -> WindowButtons {
        WindowButtons::all()
    }

    fn set_minimized(&self, _minimized: bool) {}

    fn is_minimized(&self) -> Option<bool> {
        None
    }

    fn set_maximized(&self, _maximized: bool) {}

    fn is_maximized(&self) -> bool {
        false
    }

    fn set_fullscreen(&self, _monitor: Option<Fullscreen>) {
        warn!("Cannot set fullscreen on Android");
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        None
    }

    fn set_decorations(&self, _decorations: bool) {}

    fn is_decorated(&self) -> bool {
        true
    }

    fn set_window_level(&self, _level: WindowLevel) {}

    fn set_window_icon(&self, _window_icon: Option<winit_core::icon::Icon>) {}

    fn set_ime_cursor_area(&self, _position: Position, _size: Size) {}

    fn request_ime_update(&self, request: ImeRequest) -> Result<(), ImeRequestError> {
        let mut current_caps = self.ime_capabilities.lock().unwrap();
        match request {
            ImeRequest::Enable(enable) => {
                let (capabilities, _) = enable.into_raw();
                if current_caps.is_some() {
                    return Err(ImeRequestError::AlreadyEnabled);
                }
                *current_caps = Some(capabilities);
                #[cfg(feature = "game-activity")]
                self.app.set_text_input_state(android_activity::input::TextInputState::default());
                self.app.show_soft_input(true);
            },
            ImeRequest::Update(_) => {
                if current_caps.is_none() {
                    return Err(ImeRequestError::NotEnabled);
                }
            },
            ImeRequest::Disable => {
                *current_caps = None;
                #[cfg(feature = "game-activity")]
                self.app.set_text_input_state(android_activity::input::TextInputState::default());
                self.app.hide_soft_input(true);
            },
        }

        Ok(())
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        *self.ime_capabilities.lock().unwrap()
    }

    fn set_ime_purpose(&self, _purpose: ImePurpose) {}

    fn focus_window(&self) {}

    fn request_user_attention(&self, _request_type: Option<window::UserAttentionType>) {}

    fn set_cursor(&self, _: Cursor) {}

    fn set_cursor_position(&self, _: Position) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_position is not supported").into())
    }

    fn set_cursor_grab(&self, _: CursorGrabMode) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_grab is not supported").into())
    }

    fn set_cursor_visible(&self, _: bool) {}

    fn drag_window(&self) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_window is not supported").into())
    }

    fn drag_resize_window(&self, _direction: ResizeDirection) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_resize_window").into())
    }

    #[inline]
    fn show_window_menu(&self, _position: Position) {}

    fn set_cursor_hittest(&self, _hittest: bool) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_hittest is not supported").into())
    }

    fn set_theme(&self, _theme: Option<Theme>) {}

    fn theme(&self) -> Option<Theme> {
        None
    }

    fn set_content_protected(&self, _protected: bool) {}

    fn has_focus(&self) -> bool {
        HAS_FOCUS.load(Ordering::Relaxed)
    }

    fn title(&self) -> String {
        String::new()
    }

    fn reset_dead_keys(&self) {}

    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }
}

fn screen_size(app: &AndroidApp) -> PhysicalSize<u32> {
    if let Some(native_window) = app.native_window() {
        PhysicalSize::new(native_window.width() as _, native_window.height() as _)
    } else {
        PhysicalSize::new(0, 0)
    }
}

fn scale_factor(app: &AndroidApp) -> f64 {
    app.config().density().map(|dpi| dpi as f64 / 160.0).unwrap_or(1.0)
}

#[derive(Clone, Debug)]
struct AndroidPointerState {
    kind: event::PointerKind,
    source: AndroidPointerSource,
}

#[derive(Clone, Debug)]
enum AndroidPointerSource {
    Mouse,
    Touch { finger_id: FingerId, force: Option<Force> },
    Tablet { kind: event::TabletToolKind, data: event::TabletToolData },
    Unknown,
}

impl AndroidPointerState {
    fn pointer_source(&self) -> event::PointerSource {
        match &self.source {
            AndroidPointerSource::Mouse => event::PointerSource::Mouse,
            AndroidPointerSource::Touch { finger_id, force } => {
                event::PointerSource::Touch { finger_id: *finger_id, force: *force }
            },
            AndroidPointerSource::Tablet { kind, data } => {
                event::PointerSource::TabletTool { kind: *kind, data: data.clone() }
            },
            AndroidPointerSource::Unknown => event::PointerSource::Unknown,
        }
    }

    fn contact_button(&self) -> event::ButtonSource {
        match &self.source {
            AndroidPointerSource::Mouse => event::ButtonSource::Mouse(event::MouseButton::Left),
            AndroidPointerSource::Touch { finger_id, force } => {
                event::ButtonSource::Touch { finger_id: *finger_id, force: *force }
            },
            AndroidPointerSource::Tablet { kind, data } => event::ButtonSource::TabletTool {
                kind: *kind,
                button: event::TabletToolButton::Contact,
                data: data.clone(),
            },
            AndroidPointerSource::Unknown => event::ButtonSource::Unknown(0),
        }
    }
}

fn android_pointer_state(
    source: Source,
    tool_type: ToolType,
    finger_id: FingerId,
    pointer: &android_activity::input::Pointer<'_>,
) -> Option<AndroidPointerState> {
    let force = Some(Force::Normalized(pointer.pressure() as f64));
    let tablet_data = event::TabletToolData {
        force,
        tangential_force: None,
        twist: None,
        tilt: None,
        angle: None,
    };

    let kind = match source {
        Source::Mouse | Source::MouseRelative | Source::Touchpad | Source::Trackball => {
            AndroidPointerState {
                kind: event::PointerKind::Mouse,
                source: AndroidPointerSource::Mouse,
            }
        },
        Source::Stylus | Source::BluetoothStylus => {
            let kind = tablet_tool_kind(tool_type);
            AndroidPointerState {
                kind: event::PointerKind::TabletTool(kind),
                source: AndroidPointerSource::Tablet { kind, data: tablet_data },
            }
        },
        Source::Touchscreen => match tool_type {
            ToolType::Finger => AndroidPointerState {
                kind: event::PointerKind::Touch(finger_id),
                source: AndroidPointerSource::Touch { finger_id, force },
            },
            ToolType::Stylus | ToolType::Eraser => {
                let kind = tablet_tool_kind(tool_type);
                AndroidPointerState {
                    kind: event::PointerKind::TabletTool(kind),
                    source: AndroidPointerSource::Tablet { kind, data: tablet_data },
                }
            },
            ToolType::Mouse => AndroidPointerState {
                kind: event::PointerKind::Mouse,
                source: AndroidPointerSource::Mouse,
            },
            ToolType::Palm => return None,
            _ => AndroidPointerState {
                kind: event::PointerKind::Unknown,
                source: AndroidPointerSource::Unknown,
            },
        },
        _ => match tool_type {
            ToolType::Finger => AndroidPointerState {
                kind: event::PointerKind::Touch(finger_id),
                source: AndroidPointerSource::Touch { finger_id, force },
            },
            ToolType::Stylus | ToolType::Eraser => {
                let kind = tablet_tool_kind(tool_type);
                AndroidPointerState {
                    kind: event::PointerKind::TabletTool(kind),
                    source: AndroidPointerSource::Tablet { kind, data: tablet_data },
                }
            },
            ToolType::Mouse => AndroidPointerState {
                kind: event::PointerKind::Mouse,
                source: AndroidPointerSource::Mouse,
            },
            ToolType::Palm => return None,
            _ => AndroidPointerState {
                kind: event::PointerKind::Unknown,
                source: AndroidPointerSource::Unknown,
            },
        },
    };

    Some(kind)
}

fn action_button_source(
    button: Button,
    pointer_state: &AndroidPointerState,
) -> Option<event::ButtonSource> {
    match &pointer_state.source {
        AndroidPointerSource::Mouse => mouse_button(button).map(event::ButtonSource::Mouse),
        AndroidPointerSource::Tablet { kind, data } => tablet_tool_button(button).map(|button| {
            event::ButtonSource::TabletTool { kind: *kind, button, data: data.clone() }
        }),
        AndroidPointerSource::Touch { .. } => None,
        AndroidPointerSource::Unknown => None,
    }
}

fn mouse_button(button: Button) -> Option<event::MouseButton> {
    Some(match button {
        Button::Primary => event::MouseButton::Left,
        Button::Secondary => event::MouseButton::Right,
        Button::Tertiary => event::MouseButton::Middle,
        Button::Back => event::MouseButton::Back,
        Button::Forward => event::MouseButton::Forward,
        _ => return None,
    })
}

fn tablet_tool_button(button: Button) -> Option<event::TabletToolButton> {
    Some(match button {
        Button::Primary => event::TabletToolButton::Contact,
        Button::Secondary | Button::StylusPrimary => event::TabletToolButton::Barrel,
        Button::Tertiary => event::TabletToolButton::Other(1),
        Button::StylusSecondary => event::TabletToolButton::Other(2),
        Button::Back => event::TabletToolButton::Other(3),
        Button::Forward => event::TabletToolButton::Other(4),
        _ => return None,
    })
}

fn tablet_tool_kind(tool_type: ToolType) -> event::TabletToolKind {
    match tool_type {
        ToolType::Eraser => event::TabletToolKind::Eraser,
        _ => event::TabletToolKind::Pen,
    }
}

fn pointer_position(pointer: &android_activity::input::Pointer<'_>) -> PhysicalPosition<f64> {
    PhysicalPosition::new(pointer.x() as f64, pointer.y() as f64)
}

fn touch_snapshot(touch_contacts: &HashMap<FingerId, TouchContact>) -> Option<TouchSnapshot> {
    if touch_contacts.is_empty() {
        return None;
    }

    let count = touch_contacts.len();
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for contact in touch_contacts.values() {
        sum_x += contact.position.x;
        sum_y += contact.position.y;
    }
    let centroid = PhysicalPosition::new(sum_x / count as f64, sum_y / count as f64);

    let (span, angle_deg) = if count == 2 {
        let mut contacts: Vec<_> = touch_contacts.iter().collect();
        contacts.sort_by_key(|(finger_id, _)| finger_id.into_raw());
        let [(_, first), (_, second)] = contacts.as_slice() else { unreachable!() };
        let dx = second.position.x - first.position.x;
        let dy = second.position.y - first.position.y;
        (Some((dx * dx + dy * dy).sqrt()), Some(dy.atan2(dx).to_degrees()))
    } else {
        (None, None)
    };

    Some(TouchSnapshot { count, centroid, span, angle_deg })
}

fn normalized_angle_delta_deg(previous: f64, current: f64) -> f64 {
    let mut delta = current - previous;
    while delta > 180.0 {
        delta -= 360.0;
    }
    while delta < -180.0 {
        delta += 360.0;
    }
    delta
}
