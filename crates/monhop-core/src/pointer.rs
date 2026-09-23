//! Platform-neutral trackpad semantics: gestures and the system actions they trigger.
//!
//! Capture slots, wire frames and receivers all validate through [`PointerGesture::from_parts`].

/// Magnify deltas scale by `1 + delta`, so the lower bound is exclusive: a scale never reaches 0.
pub const MIN_MAGNIFY_DELTA: f64 = -1.0;
pub const MAX_MAGNIFY_DELTA: f64 = 4.0;
pub const MAX_ROTATE_DEGREES: f64 = 180.0;
/// Most zoom steps one pinch record may inject on any receiver. A real record stays far below one,
/// so this only bounds a hostile peer.
pub const MAX_PINCH_STEPS_PER_RECORD: u32 = 4;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GesturePhase {
    Began = 1,
    Changed = 2,
    Ended = 3,
    Cancelled = 4,
}

impl GesturePhase {
    pub const ALL: [Self; 4] = [Self::Began, Self::Changed, Self::Ended, Self::Cancelled];

    pub const fn to_wire(self) -> u8 {
        self as u8
    }

    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Began),
            2 => Some(Self::Changed),
            3 => Some(Self::Ended),
            4 => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Cancelled)
    }
}

/// A [`PointerGesture`]'s kind without its phase or value.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GestureKind {
    Magnify = 1,
    Rotate = 2,
    SmartMagnify = 3,
    ForceClick = 4,
}

impl GestureKind {
    pub const ALL: [Self; 4] = [
        Self::Magnify,
        Self::Rotate,
        Self::SmartMagnify,
        Self::ForceClick,
    ];

    pub const fn to_wire(self) -> u8 {
        self as u8
    }

    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Magnify),
            2 => Some(Self::Rotate),
            3 => Some(Self::SmartMagnify),
            4 => Some(Self::ForceClick),
            _ => None,
        }
    }

    /// Discrete kinds happen once and carry neither a phase nor a value.
    pub const fn is_discrete(self) -> bool {
        matches!(self, Self::SmartMagnify | Self::ForceClick)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerGesture {
    /// NSEvent magnification: the view's scale multiplies by `1 + delta`.
    Magnify {
        phase: GesturePhase,
        delta: f64,
    },
    /// Counterclockwise positive.
    Rotate {
        phase: GesturePhase,
        degrees: f64,
    },
    SmartMagnify,
    /// Look Up.
    ForceClick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureError {
    /// A continuous kind without a phase, or a discrete kind with a phase or a nonzero value.
    Shape,
    NonFinite,
    OutOfRange,
}

impl PointerGesture {
    /// The one validation rule for a gesture in any representation. Discrete kinds need `phase`
    /// `None` and `value` exactly `+0.0`.
    pub const fn from_parts(
        kind: GestureKind,
        phase: Option<GesturePhase>,
        value: f64,
    ) -> Result<Self, GestureError> {
        if kind.is_discrete() {
            if phase.is_some() || value.to_bits() != 0 {
                return Err(GestureError::Shape);
            }
            return Ok(match kind {
                GestureKind::SmartMagnify => Self::SmartMagnify,
                _ => Self::ForceClick,
            });
        }
        let Some(phase) = phase else {
            return Err(GestureError::Shape);
        };
        if !value.is_finite() {
            return Err(GestureError::NonFinite);
        }
        match kind {
            GestureKind::Magnify => {
                if value <= MIN_MAGNIFY_DELTA || value > MAX_MAGNIFY_DELTA {
                    return Err(GestureError::OutOfRange);
                }
                Ok(Self::Magnify {
                    phase,
                    delta: value,
                })
            }
            _ => {
                if value.abs() > MAX_ROTATE_DEGREES {
                    return Err(GestureError::OutOfRange);
                }
                Ok(Self::Rotate {
                    phase,
                    degrees: value,
                })
            }
        }
    }

    pub const fn validate(self) -> Result<(), GestureError> {
        match Self::from_parts(self.kind(), self.phase(), self.value()) {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub const fn kind(self) -> GestureKind {
        match self {
            Self::Magnify { .. } => GestureKind::Magnify,
            Self::Rotate { .. } => GestureKind::Rotate,
            Self::SmartMagnify => GestureKind::SmartMagnify,
            Self::ForceClick => GestureKind::ForceClick,
        }
    }

    pub const fn phase(self) -> Option<GesturePhase> {
        match self {
            Self::Magnify { phase, .. } | Self::Rotate { phase, .. } => Some(phase),
            Self::SmartMagnify | Self::ForceClick => None,
        }
    }

    /// The magnify delta or rotation degrees; 0 for discrete kinds.
    pub const fn value(self) -> f64 {
        match self {
            Self::Magnify { delta, .. } => delta,
            Self::Rotate { degrees, .. } => degrees,
            Self::SmartMagnify | Self::ForceClick => 0.0,
        }
    }
}

/// A system action named by its result, not by the fingers that trigger it on either platform.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SystemGesture {
    Overview = 1,
    AppWindows = 2,
    /// The desktop or Space to the left, whichever way the fingers moved.
    DesktopPrevious = 3,
    DesktopNext = 4,
    ShowDesktop = 5,
    Launcher = 6,
    NavigateBack = 7,
    NavigateForward = 8,
    NotificationCenter = 9,
    Search = 10,
    SwitchAppPrevious = 11,
    SwitchAppNext = 12,
}

impl SystemGesture {
    pub const ALL: [Self; 12] = [
        Self::Overview,
        Self::AppWindows,
        Self::DesktopPrevious,
        Self::DesktopNext,
        Self::ShowDesktop,
        Self::Launcher,
        Self::NavigateBack,
        Self::NavigateForward,
        Self::NotificationCenter,
        Self::Search,
        Self::SwitchAppPrevious,
        Self::SwitchAppNext,
    ];

    pub const fn to_wire(self) -> u8 {
        self as u8
    }

    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Overview),
            2 => Some(Self::AppWindows),
            3 => Some(Self::DesktopPrevious),
            4 => Some(Self::DesktopNext),
            5 => Some(Self::ShowDesktop),
            6 => Some(Self::Launcher),
            7 => Some(Self::NavigateBack),
            8 => Some(Self::NavigateForward),
            9 => Some(Self::NotificationCenter),
            10 => Some(Self::Search),
            11 => Some(Self::SwitchAppPrevious),
            12 => Some(Self::SwitchAppNext),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn magnify(value: f64) -> Result<PointerGesture, GestureError> {
        PointerGesture::from_parts(GestureKind::Magnify, Some(GesturePhase::Changed), value)
    }

    fn rotate(value: f64) -> Result<PointerGesture, GestureError> {
        PointerGesture::from_parts(GestureKind::Rotate, Some(GesturePhase::Changed), value)
    }

    #[test]
    fn every_enum_value_round_trips_its_wire_byte_and_nothing_else_decodes() {
        for phase in GesturePhase::ALL {
            assert_eq!(GesturePhase::from_wire(phase.to_wire()), Some(phase));
        }
        for kind in GestureKind::ALL {
            assert_eq!(GestureKind::from_wire(kind.to_wire()), Some(kind));
        }
        for gesture in SystemGesture::ALL {
            assert_eq!(SystemGesture::from_wire(gesture.to_wire()), Some(gesture));
        }
        for byte in [0, 5, u8::MAX] {
            assert_eq!(GesturePhase::from_wire(byte), None);
            assert_eq!(GestureKind::from_wire(byte), None);
        }
        for byte in [0, 13, u8::MAX] {
            assert_eq!(SystemGesture::from_wire(byte), None);
        }
    }

    #[test]
    fn every_valid_gesture_round_trips_its_parts() {
        let mut gestures = vec![PointerGesture::SmartMagnify, PointerGesture::ForceClick];
        for phase in GesturePhase::ALL {
            gestures.push(PointerGesture::Magnify { phase, delta: 0.0 });
            gestures.push(PointerGesture::Rotate {
                phase,
                degrees: -12.5,
            });
        }
        for gesture in gestures {
            assert_eq!(gesture.validate(), Ok(()));
            assert_eq!(
                PointerGesture::from_parts(gesture.kind(), gesture.phase(), gesture.value()),
                Ok(gesture)
            );
        }
    }

    #[test]
    fn magnify_keeps_the_scale_positive_and_bounded() {
        assert_eq!(magnify(MIN_MAGNIFY_DELTA), Err(GestureError::OutOfRange));
        assert!(magnify(-0.999).is_ok());
        assert!(magnify(MAX_MAGNIFY_DELTA).is_ok());
        assert_eq!(magnify(4.000_001), Err(GestureError::OutOfRange));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(magnify(value), Err(GestureError::NonFinite));
        }
    }

    #[test]
    fn rotation_stays_within_half_a_turn_either_way() {
        assert!(rotate(MAX_ROTATE_DEGREES).is_ok());
        assert!(rotate(-MAX_ROTATE_DEGREES).is_ok());
        assert_eq!(rotate(180.001), Err(GestureError::OutOfRange));
        assert_eq!(rotate(-180.001), Err(GestureError::OutOfRange));
        assert_eq!(rotate(f64::NAN), Err(GestureError::NonFinite));
    }

    #[test]
    fn discrete_kinds_carry_no_phase_or_value_and_continuous_kinds_need_a_phase() {
        for kind in [GestureKind::SmartMagnify, GestureKind::ForceClick] {
            assert!(PointerGesture::from_parts(kind, None, 0.0).is_ok());
            for (phase, value) in [
                (Some(GesturePhase::Began), 0.0),
                (None, 1.0),
                (None, -0.0),
                (None, f64::NAN),
            ] {
                assert_eq!(
                    PointerGesture::from_parts(kind, phase, value),
                    Err(GestureError::Shape)
                );
            }
        }
        for kind in [GestureKind::Magnify, GestureKind::Rotate] {
            assert_eq!(
                PointerGesture::from_parts(kind, None, 0.0),
                Err(GestureError::Shape)
            );
        }
        assert_eq!(
            PointerGesture::Magnify {
                phase: GesturePhase::Began,
                delta: 9.0
            }
            .validate(),
            Err(GestureError::OutOfRange)
        );
    }
}
