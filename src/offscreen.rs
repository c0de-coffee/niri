//! Outputs that nobody looks at: the headless backend's outputs and virtual outputs.
//!
//! They aren't rendered. We still compute the render element states, so that surfaces get their
//! primary output and frame callbacks; the damage tracker does that without drawing anything.
//! Frames are paced with a timer at the output refresh rate, like the TTY backend does when there's
//! no damage. Screencasts of these outputs are rendered as usual after `redraw()` calls us.

use std::collections::HashMap;
use std::mem;
use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::Refresh;

use crate::backend::RenderResult;
use crate::niri::{Niri, RedrawState};
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::get_monotonic_time;

/// Refresh interval for outputs that don't know better.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_micros(16_667);

#[derive(Default)]
pub struct OffscreenState {
    /// Used to compute render element states without rendering.
    damage_trackers: HashMap<Output, OutputDamageTracker>,
}

impl OffscreenState {
    pub fn remove_output(&mut self, output: &Output) {
        self.damage_trackers.remove(output);
    }
}

impl Niri {
    /// "Renders" an output that nobody looks at. See the module docs.
    pub fn render_offscreen(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        let _span = tracy_client::span!("Niri::render_offscreen");

        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::Output,
            xray: None,
        };
        let elements = self.render_to_vec(ctx, output, true);

        let damage_tracker = self
            .offscreen
            .damage_trackers
            .entry(output.clone())
            .or_insert_with(|| OutputDamageTracker::from_output(output));
        let (damage, states) = match damage_tracker.damage_output(1, &elements) {
            Ok((damage, states)) => (damage.is_some(), states),
            Err(err) => {
                warn!("error computing damage: {err:?}");
                (false, RenderElementStates::default())
            }
        };

        self.update_primary_scanout_output(output, &states);

        let rv = if damage {
            let mut presentation_feedbacks = self.take_presentation_feedbacks(output, &states);
            presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
                target_presentation_time,
                Refresh::Unknown,
                0,
                wp_presentation_feedback::Kind::empty(),
            );
            RenderResult::Submitted
        } else {
            RenderResult::NoDamage
        };

        self.queue_offscreen_vblank_timer(output.clone(), target_presentation_time);

        rv
    }

    /// Same as the TTY backend: wake up at the predicted presentation time.
    fn queue_offscreen_vblank_timer(&mut self, output: Output, target_presentation_time: Duration) {
        let output_state = self.output_state.get_mut(&output).unwrap();
        match mem::take(&mut output_state.redraw_state) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
                return;
            }
        }

        let now = get_monotonic_time();
        let mut duration = target_presentation_time.saturating_sub(now);
        if duration.is_zero() {
            duration += output_state
                .frame_clock
                .refresh_interval()
                .unwrap_or(DEFAULT_REFRESH_INTERVAL);
        }

        // When the timer is supposed to fire, which is when we consider the frame presented.
        let presentation_time = now + duration;

        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state
                    .niri
                    .on_offscreen_vblank_timer(&output, presentation_time);
                TimeoutAction::Drop
            })
            .unwrap();
        output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
    }

    fn on_offscreen_vblank_timer(&mut self, output: &Output, presentation_time: Duration) {
        let _span = tracy_client::span!("Niri::on_offscreen_vblank_timer");

        // The output may have been removed in the meantime.
        let Some(output_state) = self.output_state.get_mut(output) else {
            return;
        };

        // Use the predicted time rather than when the timer actually fired: timer jitter would
        // otherwise make consecutive target times go slightly backwards.
        output_state.frame_clock.presented(presentation_time);

        // We waited for the timer, now we can send frame callbacks again.
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => unreachable!(),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => (),
            // The timer fired just in front of a redraw.
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                output_state.redraw_state = RedrawState::Queued;
                return;
            }
        }

        if output_state.unfinished_animations_remain {
            self.queue_redraw(output);
        } else {
            self.send_frame_callbacks(output);
        }
    }
}
