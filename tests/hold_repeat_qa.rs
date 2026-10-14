//! Headless behavioral reference and source-contract checks for Inkpad's Slint
//! press-and-hold size buttons.
//!
//! Slint owns the actual timers and pointer routing. These tests make the
//! intended cancellation and rate limits explicit without launching a window.
//! They do not replace a physical mouse/focus test of the compiled GUI.

const INITIAL_DELAY_MS: u64 = 360;
const REPEAT_INTERVAL_MS: u64 = 75;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    MouseUp,
    PointerCancel,
    PointerLeft,
    Disabled,
    WindowFocusLost,
}

#[derive(Debug)]
struct HoldStepper {
    value: i32,
    min: i32,
    max: i32,
    direction: i32,
    held: bool,
    next_step_ms: Option<u64>,
}

impl HoldStepper {
    fn new(value: i32, min: i32, max: i32, direction: i32) -> Self {
        assert!(min <= value && value <= max);
        assert!(direction == -1 || direction == 1);
        Self { value, min, max, direction, held: false, next_step_ms: None }
    }

    fn enabled(&self) -> bool {
        if self.direction == 1 { self.value < self.max } else { self.value > self.min }
    }

    fn pointer_down(&mut self, now_ms: u64) -> bool {
        // A repeated DOWN during an existing grab must not create a second step.
        if self.held || !self.enabled() {
            return false;
        }
        self.held = true;
        self.step();
        if self.enabled() {
            // The 360 ms arming timer switches on a *separate* 75 ms
            // repeating timer in Slint. Its first callback is at ~435 ms.
            self.next_step_ms = Some(now_ms.saturating_add(INITIAL_DELAY_MS + REPEAT_INTERVAL_MS));
        } else {
            self.stop(StopReason::Disabled);
        }
        true
    }

    fn timer_callback(&mut self, now_ms: u64) -> bool {
        let Some(next_ms) = self.next_step_ms else { return false; };
        if !self.held || !self.enabled() || now_ms < next_ms {
            return false;
        }
        self.step();
        if self.enabled() {
            // One step per timer callback even if the UI thread was stalled.
            // Do not enqueue a large backlog of missed repeats.
            self.next_step_ms = Some(now_ms.saturating_add(REPEAT_INTERVAL_MS));
        } else {
            self.stop(StopReason::Disabled);
        }
        true
    }

    fn stop(&mut self, _reason: StopReason) {
        self.held = false;
        self.next_step_ms = None;
    }

    fn step(&mut self) {
        self.value = (self.value + self.direction).clamp(self.min, self.max);
    }
}

fn step_button_source() -> String {
    let ui = include_str!("../ui/main.slint");
    let start = ui.find("component StepButton ").expect("StepButton still exists");
    let end = ui[start..].find("component DeleteRowButton ")
        .map(|i| start + i).expect("StepButton has a stable end boundary");
    ui[start..end].to_owned()
}

#[test]
fn initial_click_is_single_and_repeat_is_bounded() {
    let mut stepper = HoldStepper::new(18, 12, 40, 1);
    assert!(stepper.pointer_down(1_000));
    assert_eq!(stepper.value, 19);
    assert!(!stepper.pointer_down(1_002), "duplicate DOWN must not double-step");
    for time in [1_100, 1_359, 1_360, 1_434] {
        assert!(!stepper.timer_callback(time), "repeat fired prematurely at {time}");
    }
    assert!(stepper.timer_callback(1_435));
    assert_eq!(stepper.value, 20);
    assert!(!stepper.timer_callback(1_499));
    assert!(stepper.timer_callback(1_510));
    assert_eq!(stepper.value, 21);
    stepper.stop(StopReason::MouseUp);
    assert!(!stepper.timer_callback(50_000));
    assert_eq!(stepper.value, 21);
}

#[test]
fn cancellation_modes_prevent_stray_repeat_and_reentry() {
    for reason in [
        StopReason::MouseUp,
        StopReason::PointerCancel,
        StopReason::PointerLeft,
        StopReason::Disabled,
        StopReason::WindowFocusLost,
    ] {
        let mut stepper = HoldStepper::new(20, 12, 40, -1);
        assert!(stepper.pointer_down(0));
        assert_eq!(stepper.value, 19);
        stepper.stop(reason);
        assert!(!stepper.timer_callback(10_000), "{reason:?} leaked repeat");
        assert_eq!(stepper.value, 19);
        // Moving back inside or regaining focus is not a new pointer DOWN.
        assert!(!stepper.timer_callback(10_075), "{reason:?} rearmed");
        assert!(stepper.pointer_down(10_100), "a fresh press should work");
        assert_eq!(stepper.value, 18);
    }
}

#[test]
fn disabled_edges_never_overshoot_for_text_or_brush() {
    for (min, max) in [(12, 40), (1, 40)] {
        for (at_edge, direction) in [(max, 1), (min, -1)] {
            let mut stepper = HoldStepper::new(at_edge, min, max, direction);
            assert!(!stepper.pointer_down(0));
            assert!(!stepper.timer_callback(30_000));
            assert_eq!(stepper.value, at_edge);
        }
        let mut stepper = HoldStepper::new(max - 1, min, max, 1);
        assert!(stepper.pointer_down(0));
        assert_eq!(stepper.value, max);
        assert!(!stepper.held, "reaching a limit should end hold");
        assert!(!stepper.timer_callback(5_000));
    }
}

#[test]
fn stalled_ui_does_not_emit_backlog_burst() {
    let mut stepper = HoldStepper::new(12, 12, 40, 1);
    assert!(stepper.pointer_down(0));
    assert!(stepper.timer_callback(60_000));
    assert_eq!(stepper.value, 14, "large elapsed time emits only one callback");
    assert!(!stepper.timer_callback(60_000));
    assert!(stepper.timer_callback(60_075));
    assert_eq!(stepper.value, 15);
}

#[test]
fn slint_wiring_preserves_rate_limits_and_cancel_paths() {
    let ui = step_button_source();
    assert!(ui.contains("interval: 360ms;"), "missing arming delay");
    assert!(ui.contains("interval: 75ms;"), "missing repeat interval");
    assert!(ui.contains("root.held && !root.repeating"), "arming timer must stop on release");
    assert!(ui.contains("root.held && root.repeating"), "repeat timer must stop on release");
    assert!(ui.contains("PointerEventKind.down"), "size step should fire on initial DOWN");
    assert!(ui.contains("PointerEventKind.up") && ui.contains("PointerEventKind.cancel"),
        "release and capture cancellation must stop repeat");
    assert!(ui.contains("changed enabled"), "becoming disabled must stop repeat");
    let window = include_str!("../ui/main.slint");
    for bounds in [
        "root.font-size > 12", "root.font-size < 40",
        "root.brush-size > 1", "root.brush-size < 40",
    ] {
        assert!(window.contains(bounds), "size limit must disable stepper: {bounds}");
    }
}

#[test]
fn slint_drag_outside_grab_must_stop_hold() {
    // Slint 1.18.1 delivers PointerEventKind.move to a grabbed TouchArea even
    // when coordinates leave it. has-hover stays true during that grab, so
    // checking hover alone does not meet this contract.
    let ui = step_button_source();
    assert!(ui.contains("PointerEventKind.move"),
        "StepButton needs a move handler to stop repeating when dragged outside");
    assert!(ui.contains("mouse-x") && ui.contains("mouse-y"),
        "move handler needs grabbed pointer coordinates for bounds cancellation");
}

#[test]
fn slint_focus_loss_must_stop_hold() {
    let ui = step_button_source();
    assert!(ui.contains("hit.focus-keyboard();"),
        "mouse down must give the stepper a FocusScope for WindowActivation focus loss");
    assert!(ui.contains("focus-active: hit.has-focus"),
        "FocusScope.has-focus reflects native WindowActivation FocusOut");
    assert!(ui.contains("changed focus-active"),
        "loss of keyboard/window focus must clear any active hold");
    let cancel_on_blur = ui.split("changed focus-active").nth(1).unwrap();
    let handler = cancel_on_blur.split("width: 32px;").next().unwrap();
    assert!(handler.contains("!root.focus-active")
        && handler.contains("root.held = false")
        && handler.contains("root.repeating = false"),
        "blur handler must stop both the arming and repeating timers");
}

#[test]
fn slint_duplicate_primary_down_does_not_double_step() {
    // A held TouchArea can receive more than one DOWN (e.g. additional touch
    // contacts). An immediate click must be guarded by the same hold latch.
    let ui = step_button_source();
    let down = ui.split("PointerEventKind.down").nth(1)
        .expect("initial pointer-down handler must exist");
    let initial_step = down.split("root.clicked();").next()
        .expect("initial click should be in the DOWN handler");
    assert!(initial_step.contains("!root.held"),
        "initial DOWN needs !root.held guard before its immediate step");
}
