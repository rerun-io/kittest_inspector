//! Example test that intentionally fails — useful for exercising the inspector's
//! end-of-test flow: the status bar turns red with the panic message, the source panel
//! highlights the panic line, and the harness stays parked so you can poke at the final
//! UI state via Control mode / Handle events before dismissing with Play / Step / Run.
//!
//! Runs normally (marked `#[should_panic]` so `cargo test` passes):
//!
//! ```sh
//! cargo test --test failing_example
//! ```
//!
//! Under the inspector:
//!
//! ```sh
//! cargo build && KITTEST_INSPECTOR=1 KITTEST_INSPECTOR_PATH=target/debug/kittest_inspector cargo test --test example_app
//! ```

use egui_kittest::kittest::Queryable as _;
use egui_kittest::{Harness, install_panic_hook};

#[derive(Default)]
struct Counter {
    count: u32,
}

fn build_ui(ui: &mut egui::Ui, state: &mut Counter) {
    ui.heading("Counter");
    if ui.button("increment").clicked() {
        state.count += 1;
    }
    ui.label(format!("count = {}", state.count));
}

#[test]
#[should_panic(expected = "counter should be at least 5")]
fn counter_never_reaches_five() {
    // Capture the panic message + location so the inspector can display them.
    install_panic_hook();

    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(240.0, 120.0))
        .build_ui_state(build_ui, Counter::default());

    // Click twice so the inspector history has a few distinct frames to scrub through
    // before reaching the failure.
    for _ in 0..2 {
        harness.get_by_label("increment").click();
        harness.run();
    }

    // Intentional failure — the panic line (this assert) will be highlighted in red in
    // the inspector's source panel.
    assert!(
        harness.state().count >= 5,
        "counter should be at least 5, got {}",
        harness.state().count,
    );
}
