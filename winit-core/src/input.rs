use std::collections::HashMap;

use dpi::PhysicalPosition;

use crate::event::{
    ButtonSource, DeviceId, ElementState, FingerId, MouseScrollDelta, PointerKind, PointerSource,
    TouchPhase, WindowEvent,
};

#[derive(Clone, Debug, PartialEq)]
pub enum WindowInputEvent {
    Pointer(PointerInputEvent),
    Scroll(ScrollInputEvent),
    Gesture(GestureInputEvent),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PointerInputEvent {
    pub device_id: Option<DeviceId>,
    pub pointer_id: PointerId,
    pub position: Option<PhysicalPosition<f64>>,
    pub primary: bool,
    pub kind: PointerKind,
    pub action: PointerInputAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PointerInputAction {
    Entered,
    Moved { source: PointerSource },
    Pressed { button: ButtonSource },
    Released { button: ButtonSource },
    Left,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollInputEvent {
    pub device_id: Option<DeviceId>,
    pub position: Option<PhysicalPosition<f64>>,
    pub delta: MouseScrollDelta,
    pub phase: TouchPhase,
    pub source: ScrollSource,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GestureInputEvent {
    Pan { device_id: Option<DeviceId>, delta: PhysicalPosition<f32>, phase: TouchPhase },
    Pinch { device_id: Option<DeviceId>, delta: f64, phase: TouchPhase },
    Rotation { device_id: Option<DeviceId>, delta: f32, phase: TouchPhase },
    DoubleTap { device_id: Option<DeviceId> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerId {
    Mouse,
    Touch(FingerId),
    TabletTool(PointerKind),
    Unknown { device_id: Option<DeviceId>, primary: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollSource {
    Wheel,
    Gesture,
    Unknown,
}

#[derive(Default)]
pub struct WindowInputMapper {
    pointers: HashMap<PointerId, PointerState>,
}

impl WindowInputMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map_window_event(
        &mut self,
        event: &WindowEvent,
        mut emit: impl FnMut(WindowInputEvent),
    ) {
        match event {
            WindowEvent::Focused(false) | WindowEvent::Destroyed => {
                self.cancel_all(emit);
            },
            WindowEvent::PointerEntered { device_id, position, primary, kind } => {
                let pointer_id = PointerId::from_kind(*kind, *device_id, *primary);
                self.pointers.insert(
                    pointer_id,
                    PointerState {
                        device_id: *device_id,
                        kind: *kind,
                        primary: *primary,
                        pressed_buttons: Vec::new(),
                        last_position: Some(*position),
                    },
                );
                emit(WindowInputEvent::Pointer(PointerInputEvent {
                    device_id: *device_id,
                    pointer_id,
                    position: Some(*position),
                    primary: *primary,
                    kind: *kind,
                    action: PointerInputAction::Entered,
                }));
            },
            WindowEvent::PointerMoved { device_id, position, primary, source } => {
                let pointer_id = PointerId::from_source(source, *device_id, *primary);
                let kind = PointerKind::from(source.clone());
                self.pointers
                    .entry(pointer_id)
                    .and_modify(|state| {
                        state.device_id = *device_id;
                        state.kind = kind;
                        state.primary = *primary;
                        state.last_position = Some(*position);
                    })
                    .or_insert(PointerState {
                        device_id: *device_id,
                        kind,
                        primary: *primary,
                        pressed_buttons: Vec::new(),
                        last_position: Some(*position),
                    });
                emit(WindowInputEvent::Pointer(PointerInputEvent {
                    device_id: *device_id,
                    pointer_id,
                    position: Some(*position),
                    primary: *primary,
                    kind,
                    action: PointerInputAction::Moved { source: source.clone() },
                }));
            },
            WindowEvent::PointerButton { device_id, state, position, primary, button } => {
                let pointer_id = PointerId::from_button(button, *device_id, *primary);
                let kind = button.pointer_kind();
                let button_id = ButtonId::from_button_source(button);
                self.pointers
                    .entry(pointer_id)
                    .and_modify(|pointer_state| {
                        pointer_state.device_id = *device_id;
                        pointer_state.kind = kind;
                        pointer_state.primary = *primary;
                        pointer_state.last_position = Some(*position);
                        match state {
                            ElementState::Pressed => {
                                if !pointer_state.pressed_buttons.contains(&button_id) {
                                    pointer_state.pressed_buttons.push(button_id);
                                }
                            },
                            ElementState::Released => {
                                pointer_state
                                    .pressed_buttons
                                    .retain(|pressed| pressed != &button_id);
                            },
                        }
                    })
                    .or_insert(PointerState {
                        device_id: *device_id,
                        kind,
                        primary: *primary,
                        pressed_buttons: match state {
                            ElementState::Pressed => vec![button_id],
                            ElementState::Released => Vec::new(),
                        },
                        last_position: Some(*position),
                    });
                emit(WindowInputEvent::Pointer(PointerInputEvent {
                    device_id: *device_id,
                    pointer_id,
                    position: Some(*position),
                    primary: *primary,
                    kind,
                    action: match state {
                        ElementState::Pressed => {
                            PointerInputAction::Pressed { button: button.clone() }
                        },
                        ElementState::Released => {
                            PointerInputAction::Released { button: button.clone() }
                        },
                    },
                }));
            },
            WindowEvent::PointerLeft { device_id, position, primary, kind } => {
                let pointer_id = PointerId::from_kind(*kind, *device_id, *primary);
                let state = self.pointers.remove(&pointer_id);
                let action = if state.as_ref().is_some_and(|state| {
                    !state.pressed_buttons.is_empty()
                        && matches!(
                            kind,
                            PointerKind::Touch(_)
                                | PointerKind::TabletTool(_)
                                | PointerKind::Unknown
                        )
                }) {
                    PointerInputAction::Cancelled
                } else {
                    PointerInputAction::Left
                };
                self.shrink_pointer_storage_if_sparse();
                emit(WindowInputEvent::Pointer(PointerInputEvent {
                    device_id: state.as_ref().map(|state| state.device_id).unwrap_or(*device_id),
                    pointer_id,
                    position: position.or_else(|| state.and_then(|state| state.last_position)),
                    primary: *primary,
                    kind: *kind,
                    action,
                }));
            },
            WindowEvent::MouseWheel { device_id, delta, phase } => {
                emit(WindowInputEvent::Scroll(ScrollInputEvent {
                    device_id: *device_id,
                    position: self.mouse_position(),
                    delta: *delta,
                    phase: *phase,
                    source: ScrollSource::Wheel,
                }));
            },
            WindowEvent::PanGesture { device_id, delta, phase } => {
                emit(WindowInputEvent::Gesture(GestureInputEvent::Pan {
                    device_id: *device_id,
                    delta: *delta,
                    phase: *phase,
                }));
            },
            WindowEvent::PinchGesture { device_id, delta, phase } => {
                emit(WindowInputEvent::Gesture(GestureInputEvent::Pinch {
                    device_id: *device_id,
                    delta: *delta,
                    phase: *phase,
                }));
            },
            WindowEvent::RotationGesture { device_id, delta, phase } => {
                emit(WindowInputEvent::Gesture(GestureInputEvent::Rotation {
                    device_id: *device_id,
                    delta: *delta,
                    phase: *phase,
                }));
            },
            WindowEvent::DoubleTapGesture { device_id } => {
                emit(WindowInputEvent::Gesture(GestureInputEvent::DoubleTap {
                    device_id: *device_id,
                }));
            },
            _ => {},
        }
    }

    pub fn reset(&mut self) {
        self.pointers.clear();
        self.shrink_pointer_storage_if_sparse();
    }

    pub fn cancel_all(&mut self, mut emit: impl FnMut(WindowInputEvent)) {
        let pointers = std::mem::take(&mut self.pointers);
        for (pointer_id, state) in pointers {
            let action = if state.pressed_buttons.is_empty() {
                PointerInputAction::Left
            } else {
                PointerInputAction::Cancelled
            };
            emit(WindowInputEvent::Pointer(PointerInputEvent {
                device_id: state.device_id,
                pointer_id,
                position: state.last_position,
                primary: state.primary,
                kind: state.kind,
                action,
            }));
        }
        self.shrink_pointer_storage_if_sparse();
    }

    pub fn touch_contact_count(&self) -> usize {
        self.pointers.values().filter(|state| matches!(state.kind, PointerKind::Touch(_))).count()
    }

    fn shrink_pointer_storage_if_sparse(&mut self) {
        if self.pointers.is_empty() && self.pointers.capacity() > 16 {
            self.pointers.shrink_to(8);
        }
    }

    fn mouse_position(&self) -> Option<PhysicalPosition<f64>> {
        self.pointers.get(&PointerId::Mouse).and_then(|state| state.last_position)
    }
}

#[derive(Clone)]
struct PointerState {
    device_id: Option<DeviceId>,
    kind: PointerKind,
    primary: bool,
    pressed_buttons: Vec<ButtonId>,
    last_position: Option<PhysicalPosition<f64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ButtonId {
    Mouse(crate::event::MouseButton),
    Touch(FingerId),
    TabletTool { kind: crate::event::TabletToolKind, button: crate::event::TabletToolButton },
    Unknown(u16),
}

impl PointerId {
    pub fn from_kind(kind: PointerKind, device_id: Option<DeviceId>, primary: bool) -> Self {
        match kind {
            PointerKind::Mouse => Self::Mouse,
            PointerKind::Touch(finger_id) => Self::Touch(finger_id),
            PointerKind::TabletTool(_) => Self::TabletTool(kind),
            PointerKind::Unknown => Self::Unknown { device_id, primary },
        }
    }

    pub fn from_source(source: &PointerSource, device_id: Option<DeviceId>, primary: bool) -> Self {
        match source {
            PointerSource::Mouse => Self::Mouse,
            PointerSource::Touch { finger_id, .. } => Self::Touch(*finger_id),
            PointerSource::TabletTool { kind, .. } => {
                Self::TabletTool(PointerKind::TabletTool(*kind))
            },
            PointerSource::Unknown => Self::Unknown { device_id, primary },
        }
    }

    pub fn from_button(button: &ButtonSource, device_id: Option<DeviceId>, primary: bool) -> Self {
        match button {
            ButtonSource::Mouse(_) => Self::Mouse,
            ButtonSource::Touch { finger_id, .. } => Self::Touch(*finger_id),
            ButtonSource::TabletTool { kind, .. } => {
                Self::TabletTool(PointerKind::TabletTool(*kind))
            },
            ButtonSource::Unknown(_) => Self::Unknown { device_id, primary },
        }
    }
}

impl ButtonSource {
    pub fn pointer_kind(&self) -> PointerKind {
        match self {
            ButtonSource::Mouse(_) => PointerKind::Mouse,
            ButtonSource::Touch { finger_id, .. } => PointerKind::Touch(*finger_id),
            ButtonSource::TabletTool { kind, .. } => PointerKind::TabletTool(*kind),
            ButtonSource::Unknown(_) => PointerKind::Unknown,
        }
    }
}

impl ButtonId {
    fn from_button_source(button: &ButtonSource) -> Self {
        match button {
            ButtonSource::Mouse(mouse_button) => Self::Mouse(*mouse_button),
            ButtonSource::Touch { finger_id, .. } => Self::Touch(*finger_id),
            ButtonSource::TabletTool { kind, button, .. } => {
                Self::TabletTool { kind: *kind, button: *button }
            },
            ButtonSource::Unknown(button) => Self::Unknown(*button),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ElementState, Force, MouseButton};

    fn collect(mapper: &mut WindowInputMapper, event: &WindowEvent) -> Vec<WindowInputEvent> {
        let mut events = Vec::new();
        mapper.map_window_event(event, |input| events.push(input));
        events
    }

    #[test]
    fn touch_pointer_left_without_release_becomes_cancelled() {
        let mut mapper = WindowInputMapper::new();
        let finger_id = FingerId::from_raw(7);

        collect(
            &mut mapper,
            &WindowEvent::PointerEntered {
                device_id: None,
                position: PhysicalPosition::new(10.0, 20.0),
                primary: true,
                kind: PointerKind::Touch(finger_id),
            },
        );
        collect(
            &mut mapper,
            &WindowEvent::PointerButton {
                device_id: None,
                state: ElementState::Pressed,
                position: PhysicalPosition::new(10.0, 20.0),
                primary: true,
                button: ButtonSource::Touch { finger_id, force: Some(Force::Normalized(0.5)) },
            },
        );

        let events = collect(
            &mut mapper,
            &WindowEvent::PointerLeft {
                device_id: None,
                position: None,
                primary: true,
                kind: PointerKind::Touch(finger_id),
            },
        );

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            WindowInputEvent::Pointer(PointerInputEvent {
                pointer_id: PointerId::Touch(id),
                action: PointerInputAction::Cancelled,
                ..
            }) if *id == finger_id
        ));
    }

    #[test]
    fn mouse_pointer_left_stays_left() {
        let mut mapper = WindowInputMapper::new();
        collect(
            &mut mapper,
            &WindowEvent::PointerEntered {
                device_id: None,
                position: PhysicalPosition::new(1.0, 2.0),
                primary: true,
                kind: PointerKind::Mouse,
            },
        );
        collect(
            &mut mapper,
            &WindowEvent::PointerButton {
                device_id: None,
                state: ElementState::Pressed,
                position: PhysicalPosition::new(1.0, 2.0),
                primary: true,
                button: ButtonSource::Mouse(MouseButton::Left),
            },
        );

        let events = collect(
            &mut mapper,
            &WindowEvent::PointerLeft {
                device_id: None,
                position: Some(PhysicalPosition::new(3.0, 4.0)),
                primary: true,
                kind: PointerKind::Mouse,
            },
        );

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            WindowInputEvent::Pointer(PointerInputEvent {
                pointer_id: PointerId::Mouse,
                action: PointerInputAction::Left,
                ..
            })
        ));
    }

    #[test]
    fn pan_gesture_emits_only_pan_gesture() {
        let mut mapper = WindowInputMapper::new();
        let first = FingerId::from_raw(1);
        let second = FingerId::from_raw(2);

        collect(
            &mut mapper,
            &WindowEvent::PointerEntered {
                device_id: None,
                position: PhysicalPosition::new(10.0, 20.0),
                primary: true,
                kind: PointerKind::Touch(first),
            },
        );
        collect(
            &mut mapper,
            &WindowEvent::PointerEntered {
                device_id: None,
                position: PhysicalPosition::new(30.0, 40.0),
                primary: false,
                kind: PointerKind::Touch(second),
            },
        );

        let events = collect(
            &mut mapper,
            &WindowEvent::PanGesture {
                device_id: None,
                delta: PhysicalPosition::new(0.0, 12.0),
                phase: TouchPhase::Moved,
            },
        );

        assert!(matches!(
            &events[0],
            WindowInputEvent::Gesture(GestureInputEvent::Pan {
                delta,
                phase: TouchPhase::Moved,
                ..
            }) if *delta == PhysicalPosition::new(0.0, 12.0)
        ));
        assert_eq!(events.len(), 1);
    }
}
