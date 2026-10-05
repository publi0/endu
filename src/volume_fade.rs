//! Short, interruptible fades that relinquish ownership after manual changes.

use std::time::{Duration, Instant};

pub const FADE_DURATION: Duration = Duration::from_millis(120);
pub const FADE_TICK: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug)]
pub struct VolumeState {
    pub volume: f32,
    pub muted: Option<bool>,
}

impl VolumeState {
    fn matches(self, other: Self) -> bool {
        (self.volume - other.volume).abs() <= 0.001 && self.muted == other.muted
    }
}

pub trait VolumeDevice {
    fn read(&mut self) -> Option<VolumeState>;
    fn write(&mut self, volume: f32) -> bool;
    fn is_current_output(&mut self) -> bool {
        true
    }
}

struct Ramp {
    from: f32,
    to: f32,
    started: Instant,
}

pub struct VolumeFade<D: VolumeDevice> {
    device: D,
    original: f32,
    lowered: f32,
    last: VolumeState,
    owns_volume: bool,
    active: bool,
    ramp: Option<Ramp>,
}

impl<D: VolumeDevice> VolumeFade<D> {
    pub fn new(device: D, now: Instant) -> Option<Self> {
        Self::with_remaining_volume(device, 0.0, now)
    }

    /// Keep a fraction of the original volume, never an absolute system level.
    pub fn with_remaining_volume(mut device: D, remaining: f32, now: Instant) -> Option<Self> {
        if !remaining.is_finite() || !(0.0..1.0).contains(&remaining) {
            return None;
        }
        let original = device.read()?;
        if !original.volume.is_finite()
            || !(0.0..=1.0).contains(&original.volume)
            || original.volume == 0.0
            || original.muted == Some(true)
        {
            return None;
        }
        Some(Self {
            device,
            original: original.volume,
            lowered: original.volume * remaining,
            last: original,
            owns_volume: true,
            active: true,
            ramp: Some(Ramp {
                from: original.volume,
                to: original.volume * remaining,
                started: now,
            }),
        })
    }

    fn still_owned(&mut self) -> bool {
        if !self.owns_volume {
            return false;
        }
        if !self
            .device
            .read()
            .is_some_and(|current| current.matches(self.last))
        {
            self.owns_volume = false;
            self.ramp = None;
        }
        self.owns_volume
    }

    pub fn can_reactivate(&mut self) -> bool {
        self.still_owned() && self.device.is_current_output()
    }

    pub fn set_active(&mut self, active: bool, now: Instant) {
        if self.active == active || !self.still_owned() {
            return;
        }
        self.active = active;
        self.ramp = Some(Ramp {
            from: self.last.volume,
            to: if active { self.lowered } else { self.original },
            started: now,
        });
    }

    pub fn tick(&mut self, now: Instant) {
        if self.ramp.is_none() || !self.still_owned() {
            return;
        }
        let ramp = self.ramp.as_ref().unwrap();
        let progress = (now.saturating_duration_since(ramp.started).as_secs_f32()
            / FADE_DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        let next = if progress >= 1.0 {
            ramp.to
        } else {
            ramp.from + (ramp.to - ramp.from) * eased
        };
        if (next - self.last.volume).abs() > f32::EPSILON {
            if !self.device.write(next) {
                self.restore_now();
                return;
            }
            // Read back the hardware value, which may be quantized. Ownership
            // detection is best effort: CoreAudio has no atomic compare-and-set.
            let Some(actual) = self
                .device
                .read()
                .filter(|state| state.volume.is_finite() && (0.0..=1.0).contains(&state.volume))
            else {
                self.owns_volume = false;
                self.ramp = None;
                return;
            };
            self.last = actual;
        }
        if progress >= 1.0 {
            self.ramp = None;
        }
    }

    pub fn is_animating(&self) -> bool {
        self.ramp.is_some()
    }

    pub fn is_restoring(&self) -> bool {
        !self.active && self.is_animating()
    }

    pub fn restore_now(&mut self) {
        if self.still_owned() && (self.last.volume - self.original).abs() > f32::EPSILON {
            let _ = self.device.write(self.original);
        }
        self.ramp = None;
        self.owns_volume = false;
    }
}

impl<D: VolumeDevice> Drop for VolumeFade<D> {
    fn drop(&mut self) {
        self.restore_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct FakeState {
        volume: f32,
        muted: bool,
        writes: Vec<f32>,
        fail_write: bool,
        fail_read: bool,
        quantize: bool,
        current_output: bool,
    }
    struct Fake(Rc<RefCell<FakeState>>);
    impl VolumeDevice for Fake {
        fn is_current_output(&mut self) -> bool {
            self.0.borrow().current_output
        }
        fn read(&mut self) -> Option<VolumeState> {
            let state = self.0.borrow();
            (!state.fail_read).then_some(VolumeState {
                volume: state.volume,
                muted: Some(state.muted),
            })
        }
        fn write(&mut self, value: f32) -> bool {
            let mut state = self.0.borrow_mut();
            if state.fail_write {
                state.fail_write = false;
                return false;
            }
            state.volume = if state.quantize {
                (value * 20.0).round() / 20.0
            } else {
                value
            };
            let written = state.volume;
            state.writes.push(written);
            true
        }
    }
    fn fake() -> Rc<RefCell<FakeState>> {
        Rc::new(RefCell::new(FakeState {
            volume: 0.8,
            muted: false,
            writes: Vec::new(),
            fail_write: false,
            fail_read: false,
            quantize: false,
            current_output: true,
        }))
    }

    #[test]
    fn lowering_is_relative_and_quick_restarts_keep_the_original_baseline() {
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::with_remaining_volume(Fake(device.clone()), 0.8, now).unwrap();
        fade.tick(now + Duration::from_millis(60));
        assert!((device.borrow().volume - 0.72).abs() < 0.001);
        fade.set_active(false, now + Duration::from_millis(60));
        fade.tick(now + Duration::from_millis(120));
        assert!((device.borrow().volume - 0.76).abs() < 0.001);
        fade.set_active(true, now + Duration::from_millis(120));
        fade.tick(now + Duration::from_millis(240));
        assert!((device.borrow().volume - 0.64).abs() < 0.001);
        fade.set_active(false, now + Duration::from_millis(240));
        fade.tick(now + Duration::from_millis(360));
        assert_eq!(device.borrow().volume, 0.8);
    }

    #[test]
    fn lowering_preserves_manual_changes_and_rejects_invalid_or_unchanged_levels() {
        let now = Instant::now();
        for remaining in [-0.1, 1.0, 1.1, f32::NAN] {
            let device = fake();
            assert!(
                VolumeFade::with_remaining_volume(Fake(device.clone()), remaining, now).is_none()
            );
            assert!(device.borrow().writes.is_empty());
        }
        let device = fake();
        let mut fade = VolumeFade::with_remaining_volume(Fake(device.clone()), 0.8, now).unwrap();
        fade.tick(now + FADE_DURATION);
        device.borrow_mut().volume = 0.3;
        fade.set_active(false, now + FADE_DURATION);
        fade.tick(now + FADE_DURATION * 2);
        drop(fade);
        assert_eq!(device.borrow().volume, 0.3);
    }

    #[test]
    fn ramps_are_smooth_and_end_at_the_original_level() {
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        for step in 1..=12 {
            fade.tick(now + FADE_TICK * step);
        }
        assert_eq!(device.borrow().volume, 0.0);
        assert!(
            device
                .borrow()
                .writes
                .windows(2)
                .all(|pair| pair[1] <= pair[0])
        );
        device.borrow_mut().writes.clear();
        let stop = now + FADE_DURATION;
        fade.set_active(false, stop);
        for step in 1..=12 {
            fade.tick(stop + FADE_TICK * step);
        }
        assert_eq!(device.borrow().volume, 0.8);
        assert!(
            device
                .borrow()
                .writes
                .windows(2)
                .all(|pair| pair[1] >= pair[0])
        );
        assert!(!fade.is_restoring());
    }

    #[test]
    fn quick_stop_and_restart_preserve_the_original_baseline() {
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        fade.tick(now + Duration::from_millis(60));
        assert!((device.borrow().volume - 0.4).abs() < 0.001);
        fade.set_active(false, now + Duration::from_millis(60));
        fade.tick(now + Duration::from_millis(120));
        assert!((device.borrow().volume - 0.6).abs() < 0.001);
        fade.set_active(true, now + Duration::from_millis(120));
        fade.tick(now + Duration::from_millis(240));
        assert_eq!(device.borrow().volume, 0.0);
        fade.set_active(false, now + Duration::from_millis(240));
        fade.tick(now + Duration::from_millis(360));
        assert_eq!(device.borrow().volume, 0.8);
    }

    #[test]
    fn manual_changes_during_either_direction_are_never_restored() {
        for restoring in [false, true] {
            let device = fake();
            let now = Instant::now();
            let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
            fade.tick(now + FADE_DURATION);
            if restoring {
                fade.set_active(false, now + FADE_DURATION);
            }
            device.borrow_mut().volume = 0.3;
            fade.tick(now + FADE_DURATION * 2);
            drop(fade);
            assert_eq!(device.borrow().volume, 0.3);
        }
    }

    #[test]
    fn muted_outputs_are_untouched_and_manual_mute_is_preserved() {
        for volume in [0.0, 0.8] {
            let device = fake();
            device.borrow_mut().volume = volume;
            device.borrow_mut().muted = true;
            assert!(VolumeFade::new(Fake(device.clone()), Instant::now()).is_none());
            assert!(device.borrow().writes.is_empty());
        }
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        fade.tick(now + Duration::from_millis(60));
        let held = device.borrow().volume;
        device.borrow_mut().muted = true;
        fade.set_active(false, now + FADE_DURATION);
        drop(fade);
        assert_eq!(device.borrow().volume, held);
        assert!(device.borrow().muted);
    }

    #[test]
    fn quantization_and_shutdown_keep_the_original_volume() {
        let device = fake();
        device.borrow_mut().quantize = true;
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        for step in 1..=12 {
            fade.tick(now + FADE_TICK * step);
        }
        assert_eq!(device.borrow().volume, 0.0);
        drop(fade);
        assert_eq!(device.borrow().volume, 0.8);
    }

    #[test]
    fn unavailable_device_never_triggers_an_unconditional_restore() {
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        fade.tick(now + Duration::from_millis(60));
        let held = device.borrow().volume;
        device.borrow_mut().fail_read = true;
        fade.tick(now + FADE_DURATION);
        drop(fade);
        assert_eq!(device.borrow().volume, held);
    }

    #[test]
    fn a_failed_write_restores_only_the_still_owned_level_and_stops() {
        let device = fake();
        let now = Instant::now();
        let mut fade = VolumeFade::new(Fake(device.clone()), now).unwrap();
        fade.tick(now + Duration::from_millis(60));
        device.borrow_mut().fail_write = true;
        fade.tick(now + FADE_DURATION);
        assert_eq!(device.borrow().volume, 0.8);
        assert!(!fade.is_animating());
        let writes = device.borrow().writes.len();
        fade.tick(now + FADE_DURATION * 2);
        drop(fade);
        assert_eq!(device.borrow().writes.len(), writes);
    }

    #[test]
    fn reactivation_rejects_manual_changes_and_a_different_output() {
        for change_output in [false, true] {
            let device = fake();
            let now = Instant::now();
            let mut old = VolumeFade::new(Fake(device.clone()), now).unwrap();
            old.tick(now + FADE_DURATION);
            old.set_active(false, now + FADE_DURATION);
            if change_output {
                device.borrow_mut().current_output = false;
            } else {
                device.borrow_mut().volume = 0.3;
            }
            assert!(!old.can_reactivate());
            drop(old);
            assert_eq!(
                device.borrow().volume,
                if change_output { 0.8 } else { 0.3 }
            );
            if !change_output {
                let mut fresh = VolumeFade::new(Fake(device.clone()), now).unwrap();
                fresh.tick(now + FADE_DURATION);
                fresh.set_active(false, now + FADE_DURATION);
                fresh.tick(now + FADE_DURATION * 2);
                assert_eq!(device.borrow().volume, 0.3);
            }
        }
    }
}
