#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ScrollAxes<T> {
    pub horizontal: T,
    pub vertical: T,
}

pub(super) type PendingScroll = ScrollAxes<PendingScrollAxis>;
pub(super) type ScrollFrame = ScrollAxes<FrameAxis>;

#[derive(Default)]
pub(super) struct FrameAxis {
    awaiting_distance: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PendingScrollAxis {
    pixels: f64,
    steps: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ScrollDelta {
    pub amount: i16,
    pub pixels: Option<f32>,
}

impl FrameAxis {
    pub fn accumulate(&mut self, pending: &mut PendingScrollAxis, absolute: f64, discrete: i32) {
        if discrete != 0 {
            // SCTK combines adjacent axis events. A motion or button between
            // the step and its distance leaves the latter for another event.
            pending.steps = pending.steps.saturating_add(discrete);
            self.awaiting_distance = absolute == 0.0;
        } else if absolute != 0.0 && !std::mem::take(&mut self.awaiting_distance) {
            pending.pixels += absolute;
        }
    }
}

impl PendingScrollAxis {
    pub fn deltas(self, scale: f64, remainder: &mut f64) -> [Option<ScrollDelta>; 2] {
        let pixels = self.pixels * scale;
        let continuous = if pixels.abs() > f64::EPSILON {
            if pixels.signum() != remainder.signum() {
                *remainder = 0.0;
            }
            let accumulated = pixels + *remainder;
            *remainder = accumulated.fract();
            Some(ScrollDelta {
                amount: (-accumulated.trunc()) as i16,
                // Only the legacy integer channel uses the remainder. Fine
                // scrolling must receive each pixel of travel exactly once.
                pixels: Some(-pixels as f32),
            })
        } else {
            None
        };
        let wheel = if self.steps != 0 {
            Some(ScrollDelta {
                amount: (-(self.steps as i64)).clamp(i16::MIN as i64, i16::MAX as i64) as i16,
                pixels: None,
            })
        } else {
            None
        };
        [continuous, wheel]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_steps_do_not_depend_on_axis_distance_or_dpi() {
        for absolute in [10.0, 15.0, 30.0] {
            for scale in [1.0, 2.0] {
                let mut axis = PendingScrollAxis::default();
                FrameAxis::default().accumulate(&mut axis, absolute, 1);
                assert_eq!(
                    axis.deltas(scale, &mut 0.0),
                    [
                        None,
                        Some(ScrollDelta {
                            amount: -1,
                            pixels: None
                        })
                    ]
                );
            }
        }
    }

    #[test]
    fn wheel_distance_may_follow_an_intervening_event_in_the_same_frame() {
        let mut pending = PendingScroll::default();
        let mut frame = ScrollFrame::default();
        frame.vertical.accumulate(&mut pending.vertical, 0.0, 1);
        // Motion or a button separates SCTK's two axis events. The second
        // may also carry genuine continuous movement on the other axis.
        frame.horizontal.accumulate(&mut pending.horizontal, 4.0, 0);
        frame.vertical.accumulate(&mut pending.vertical, 10.0, 0);
        assert_eq!(
            pending.horizontal.deltas(1.0, &mut 0.0)[0].unwrap().pixels,
            Some(-4.0)
        );
        assert_eq!(
            pending.vertical.deltas(1.0, &mut 0.0),
            [
                None,
                Some(ScrollDelta {
                    amount: -1,
                    pixels: None
                }),
            ]
        );

        // Movement in another frame is independent even before dispatch.
        let mut next = ScrollFrame::default();
        next.vertical.accumulate(&mut pending.vertical, 2.0, 0);
        assert_eq!(
            pending.vertical.deltas(1.0, &mut 0.0)[0].unwrap().pixels,
            Some(-2.0)
        );
    }

    #[test]
    fn continuous_travel_is_independent_of_event_splitting() {
        for parts in [1, 4, 16, 64] {
            let mut remainder = 0.0;
            let mut pixels = 0.0;
            let mut whole = 0;
            for _ in 0..parts {
                let mut axis = PendingScrollAxis::default();
                FrameAxis::default().accumulate(&mut axis, 8.0 / parts as f64, 0);
                let [Some(delta), None] = axis.deltas(2.0, &mut remainder) else {
                    panic!("continuous movement must retain its pixel delta");
                };
                pixels += delta.pixels.unwrap();
                whole += delta.amount;
            }
            assert_eq!(pixels, -16.0);
            assert_eq!(whole, -16);
            assert_eq!(remainder, 0.0);
        }
    }

    #[test]
    fn sub_pixel_travel_is_delivered_before_an_integer_step() {
        let mut remainder = 0.0;
        for amount in [0, 0, 0, -1] {
            let mut axis = PendingScrollAxis::default();
            FrameAxis::default().accumulate(&mut axis, 0.25, 0);
            assert_eq!(
                axis.deltas(1.0, &mut remainder),
                [
                    Some(ScrollDelta {
                        amount,
                        pixels: Some(-0.25)
                    }),
                    None
                ]
            );
        }
    }

    #[test]
    fn reversal_discards_only_the_integer_remainder() {
        let mut remainder = 0.0;
        let mut axis = PendingScrollAxis::default();
        FrameAxis::default().accumulate(&mut axis, 0.75, 0);
        axis.deltas(1.0, &mut remainder);
        let mut axis = PendingScrollAxis::default();
        FrameAxis::default().accumulate(&mut axis, -0.5, 0);
        assert_eq!(
            axis.deltas(1.0, &mut remainder),
            [
                Some(ScrollDelta {
                    amount: 0,
                    pixels: Some(0.5)
                }),
                None
            ]
        );
        assert_eq!(remainder, -0.5);
    }

    #[test]
    fn coalesced_devices_keep_pixels_and_steps_separate() {
        let mut axis = PendingScrollAxis::default();
        FrameAxis::default().accumulate(&mut axis, 0.25, 0);
        FrameAxis::default().accumulate(&mut axis, 10.0, 1);
        FrameAxis::default().accumulate(&mut axis, 20.0, 2);
        assert_eq!(
            axis.deltas(2.0, &mut 0.0),
            [
                Some(ScrollDelta {
                    amount: 0,
                    pixels: Some(-0.5)
                }),
                Some(ScrollDelta {
                    amount: -3,
                    pixels: None
                }),
            ]
        );
    }

    #[test]
    fn zero_travel_does_not_reemit_the_remainder() {
        let mut remainder = 0.75;
        assert_eq!(
            PendingScrollAxis::default().deltas(1.0, &mut remainder),
            [None, None]
        );
        assert_eq!(remainder, 0.75);
    }
}
