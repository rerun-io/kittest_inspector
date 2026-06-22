//! Eframe app that displays frames + accesskit trees streamed from an `egui_kittest` harness
//! or a live `eframe` app running [`egui_inspection::InspectionPlugin`], and lets the user
//! pause / resume / single-step the test and inspect individual widgets.
//!
//! The binary in `src/main.rs` is the default entry point: it picks the transport
//! (stdin/stdout for harnesses launched by `egui_kittest::InspectorPlugin`, or a unix
//! socket for live apps reading [`egui_inspection::INSPECTION_SOCKET_ENV_VAR`]), wires up a
//! single-instance lock, and runs the eframe app. This crate also exposes [`InspectorApp`]
//! and the worker-channel types as a library so integration tests can launch the same app
//! under `egui_kittest` and feed synthetic frames through the channels.

use std::sync::mpsc;

use eframe::egui;
use egui_inspection::{Frame, HarnessMessage, InspectorCommand, SourceView};

use egui::accesskit::{self, Node, NodeId, Rect as AkRect};

/// Final test outcome, cached on the [`InspectorApp`] so the status bar can render it and
/// the source panel can highlight the panic line.
#[derive(Debug, Clone)]
struct TestOutcome {
    ok: bool,
    message: Option<String>,
    /// Source view from `HarnessMessage::Finished` — overrides the latest frame's source so
    /// the panic line highlight (red) is visible even though no extra frame was sent at end.
    source: Option<SourceView>,
}

/// UI → IO-writer command channel. The writer thread drains this and forwards each command
/// to the harness over stdout.
pub type CommandTx = mpsc::Sender<InspectorCommand>;
pub type CommandRx = mpsc::Receiver<InspectorCommand>;

/// The inspector's view of the harness mode.
///
/// The harness/app itself holds the source of truth (see `crates/egui_kittest/src/inspector.rs`
/// and `crates/egui_inspection/src/plugin.rs`).
/// We mirror it here so the UI can pick the right button states — tracking what we *asked
/// for* plus whatever the latest [`Frame::blocking`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayState {
    /// We've asked the harness to play, or it's otherwise running freely (Run mode).
    Playing,
    /// Harness is blocked at a hook (confirmed by the latest frame's `blocking = true`).
    Paused,
}

pub struct InspectorApp {
    worker_rx: mpsc::Receiver<HarnessMessage>,
    command_tx: CommandTx,
    play_state: PlayState,
    /// Mirrors the harness's blocking state — updated from [`HarnessMessage::Blocked`]. Starts
    /// `true` because the harness sends its first `Blocked(true)` at the first `after_step`
    /// of its construction-time `run_ok`, but if anything races the UI, defaulting to
    /// "blocked" keeps the controls in a sensible initial position.
    harness_blocked: bool,
    /// Set by [`HarnessMessage::Finished`]. The harness is parked waiting for dismiss; the
    /// status bar shows pass/fail and the source panel switches to the outcome's source view
    /// (which carries `SourceView::panic_line` for failures).
    test_outcome: Option<TestOutcome>,
    /// Every frame the harness has ever sent, in order. Supports back/forward replay.
    history: Vec<Frame>,
    /// Index into `history` of the currently-displayed frame.
    view_index: usize,
    /// `Frame::step` currently uploaded to `current_texture` — used to decide whether the
    /// texture needs regenerating when `view_index` changes.
    textured_step: Option<u64>,
    current_texture: Option<egui::TextureHandle>,
    connected: bool,
    /// Currently hovered widget (cleared every frame, set during central-panel paint).
    hovered_node: Option<NodeId>,
    /// Last clicked widget (sticky).
    selected_node: Option<NodeId>,
    /// When on, pointer + keyboard events are forwarded to the harness.
    control_enabled: bool,
    /// Events accumulated since the last `Handle` dispatch.
    queued_events: Vec<egui::Event>,
    /// Set when the viewed frame changes; the Source section consumes it to scroll once.
    scroll_pending: bool,
    /// Screen rect of the rendered image from the previous frame. We hit-test against this
    /// at the start of the next `ui()` (before panels render) so the details tree can see
    /// `hovered_node` in the same frame as the image highlight.
    last_image_rect: Option<egui::Rect>,
    /// Display-pixel-per-physical-pixel ratio from the previous frame.
    last_image_scale: f32,
    /// Transient status line (e.g. "Copied to /tmp/...") shown next to the Copy-GIF button.
    status_message: Option<String>,
    /// Pending resize target in logical points. Initialized from the first frame so the
    /// initial value matches the peer's current size; thereafter the user owns the values
    /// (we don't track upstream changes — they'd thrash the inputs while the user is typing).
    resize_target: Option<(u32, u32)>,
    /// Set after we send an `InspectorCommand::Screenshot` to a peer that streams
    /// accesskit-only frames (live apps). Cleared as soon as a frame with pixels arrives.
    /// Prevents us from spamming screenshot requests every accesskit update.
    awaiting_screenshot: bool,
    /// Peer-supplied label from the `Hello` handshake (e.g. test name, app name). Shown in
    /// the Frame KV grid.
    peer_label: Option<String>,
}

impl InspectorApp {
    /// The frame currently being displayed. `None` only before the first frame ever arrives.
    fn view_frame(&self) -> Option<&Frame> {
        self.history.get(self.view_index)
    }

    /// True when `view_index` points at the most recent frame (so new arrivals keep scrolling
    /// the view forward).
    fn is_live_view(&self) -> bool {
        !self.history.is_empty() && self.view_index + 1 == self.history.len()
    }

    fn set_view_index(&mut self, idx: usize) {
        let idx = idx.min(self.history.len().saturating_sub(1));
        if idx != self.view_index {
            self.view_index = idx;
            self.scroll_pending = true;
        }
    }
}

impl InspectorApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        worker_rx: mpsc::Receiver<HarnessMessage>,
        command_tx: CommandTx,
    ) -> Self {
        Self {
            worker_rx,
            command_tx,
            play_state: PlayState::Paused,
            harness_blocked: true,
            test_outcome: None,
            history: Vec::new(),
            view_index: 0,
            textured_step: None,
            current_texture: None,
            connected: true,
            hovered_node: None,
            selected_node: None,
            control_enabled: false,
            queued_events: Vec::new(),
            scroll_pending: false,
            last_image_rect: None,
            last_image_scale: 1.0,
            status_message: None,
            resize_target: None,
            awaiting_screenshot: false,
            peer_label: None,
        }
    }

    /// `true` if the most recent [`HarnessMessage::Blocked`] said the harness is blocked at a
    /// hook. Used to enable the Step/Run buttons and switch the status label.
    fn harness_blocked(&self) -> bool {
        self.harness_blocked
    }

    /// Send a command to the harness; drops it on a broken channel (we can't do anything
    /// useful — the writer thread has exited).
    fn send_command(&self, cmd: InspectorCommand) {
        let _ = self.command_tx.send(cmd);
    }

    /// Hit-test the current cursor position against the cached image rect + the viewed
    /// frame's accesskit bounds and set `hovered_node`. Called at the top of `ui()` so the
    /// tree (rendered before the image) picks up the same hover state in this frame.
    fn hit_test_pointer(&mut self, ctx: &egui::Context) {
        if self.control_enabled {
            return; // In control mode we forward events, we don't inspect on hover.
        }
        let (Some(image_rect), Some(frame)) = (self.last_image_rect, self.view_frame()) else {
            return;
        };
        let Some(update) = frame.accesskit.as_ref() else {
            return;
        };
        let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if !image_rect.contains(pos) {
            return;
        }
        let f = (frame.pixels_per_point * self.last_image_scale) as f64;
        let lx = ((pos.x - image_rect.min.x) as f64) / f;
        let ly = ((pos.y - image_rect.min.y) as f64) / f;
        let mut best: Option<(NodeId, f64)> = None;
        for (id, node) in &update.nodes {
            let Some(b) = node.bounds() else { continue };
            if lx >= b.x0 && lx <= b.x1 && ly >= b.y0 && ly <= b.y1 {
                let area = (b.x1 - b.x0).max(0.0) * (b.y1 - b.y0).max(0.0);
                if best.is_none_or(|(_, a)| area < a) {
                    best = Some((*id, area));
                }
            }
        }
        self.hovered_node = best.map(|(id, _)| id);
    }

    fn pump_worker(&mut self) {
        loop {
            match self.worker_rx.try_recv() {
                Ok(HarnessMessage::Hello(hello)) => {
                    self.peer_label = hello.label;
                    // We always want every frame to carry a screenshot. If the peer isn't
                    // already streaming, ask for it. Kittest peers default `true`; live
                    // (eframe) peers default `false`.
                    if !hello.continuous_screenshots
                        && hello.capabilities.continuous_screenshots
                    {
                        let _ = self
                            .command_tx
                            .send(InspectorCommand::SetContinuousScreenshots(true));
                    }
                }
                Ok(HarnessMessage::Frame(frame)) => {
                    let was_live = self.is_live_view() || self.history.is_empty();
                    let has_image = frame.screenshot.is_some();
                    // Seed the resize input from the first frame *with an image* so the
                    // displayed values match the peer's current size. Don't overwrite later
                    // — the user owns those inputs once we have them.
                    if self.resize_target.is_none()
                        && let Some(shot) = frame.screenshot.as_ref()
                        && frame.pixels_per_point > 0.0
                    {
                        let w = (shot.width as f32 / frame.pixels_per_point).round() as u32;
                        let h = (shot.height as f32 / frame.pixels_per_point).round() as u32;
                        self.resize_target = Some((w.max(1), h.max(1)));
                    }
                    if has_image {
                        // Imaged frame back — the in-flight screenshot request is satisfied.
                        self.awaiting_screenshot = false;
                    }
                    self.history.push(*frame);
                    if was_live {
                        self.view_index = self.history.len() - 1;
                        self.scroll_pending = true;
                    }
                }
                Ok(HarnessMessage::Blocked(blocking)) => {
                    self.harness_blocked = blocking;
                    // Reconcile `play_state` with what the harness just reported.
                    if blocking {
                        self.play_state = PlayState::Paused;
                    }
                }
                Ok(HarnessMessage::Finished {
                    ok,
                    message,
                    source,
                }) => {
                    self.test_outcome = Some(TestOutcome {
                        ok,
                        message,
                        source,
                    });
                    self.play_state = PlayState::Paused;
                    // `Finished` implies the harness is blocked waiting for dismiss.
                    self.harness_blocked = true;
                    // Make the source panel re-scroll to the panic line on its next paint.
                    self.scroll_pending = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.connected = false;
                    break;
                }
            }
        }
    }

    /// (Re-)upload `view_frame()`'s pixels to `current_texture` if the texture is missing or
    /// represents a different step than what we're viewing.
    ///
    /// Skips frames without an image attached (width == 0 / height == 0). Live apps under
    /// [`egui_inspection::InspectionPlugin`] only emit pixels in response to an explicit
    /// `Screenshot` command — every other frame is accesskit-only. Keeping the previous
    /// texture gives the user something to inspect while they wait for the next screenshot.
    fn ensure_texture_uploaded(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.view_frame() else {
            return;
        };
        if self.textured_step == Some(frame.step) {
            return;
        }
        let Some(shot) = frame.screenshot.as_ref() else {
            return;
        };
        if shot.width == 0 || shot.height == 0 || shot.png.is_empty() {
            return;
        }
        let rgba = match image::load_from_memory(&shot.png) {
            Ok(img) => img.to_rgba8(),
            Err(err) => {
                log_diag(&format!("decode screenshot PNG failed: {err}"));
                return;
            }
        };
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let texture = ctx.load_texture("kittest_inspector_frame", color_image, Default::default());
        self.textured_step = Some(frame.step);
        self.current_texture = Some(texture);
    }

    /// Ship accumulated Control-mode events to the harness as a single `Handle` command, if
    /// any are queued. Does not change the harness's Play / Pause / Run mode.
    fn flush_events(&mut self) {
        if self.queued_events.is_empty() || !self.connected {
            return;
        }
        let events = std::mem::take(&mut self.queued_events);
        self.send_command(InspectorCommand::Handle { events });
    }
}

impl eframe::App for InspectorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump_worker();
        // Live apps stream accesskit-only frames until we explicitly ask for pixels. As
        // soon as we've seen *any* frame but still have no texture, kick off a screenshot
        // request so the central panel has something to display. Capped by
        // `awaiting_screenshot` so we don't fire one per pump.
        if self.connected
            && !self.history.is_empty()
            && self.current_texture.is_none()
            && !self.awaiting_screenshot
        {
            self.send_command(InspectorCommand::Screenshot);
            self.awaiting_screenshot = true;
        }
        self.ensure_texture_uploaded(&ctx);
        // Reset hover each frame — either the pre-hit-test below (using the cached image
        // rect from the previous frame) or the tree's own hover detection, or the central
        // panel's live hit-test will set it again.
        self.hovered_node = None;
        self.hit_test_pointer(&ctx);

        controls_panel(self, ui);
        details_panel(self, ui);
        central_panel(self, ui);

        // Forward any Control-mode events collected this frame to the harness as a single
        // `Handle` command. The harness applies them via `step_no_side_effects`, so the next
        // frame arriving via the worker channel will reflect their effect.
        self.flush_events();

        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

fn controls_panel(app: &mut InspectorApp, ui: &mut egui::Ui) {
    egui::Panel::top("controls").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            let playing = app.play_state == PlayState::Playing;
            let blocked = app.harness_blocked();
            let play_response = ui
                .add_enabled_ui(app.connected && !app.control_enabled, |ui| {
                    ui.selectable_label(playing, "▶ Play")
                })
                .inner
                .on_disabled_hover_text("Disabled while Control mode is on");
            if play_response.clicked() {
                app.play_state = PlayState::Playing;
                app.send_command(InspectorCommand::Play);
            }
            let pause_response = ui
                .add_enabled_ui(app.connected, |ui| {
                    ui.selectable_label(!playing, "⏸ Pause")
                })
                .inner
                .on_hover_text("Block the harness at its next hook");
            if pause_response.clicked() {
                app.play_state = PlayState::Paused;
                app.send_command(InspectorCommand::Pause);
            }
            let can_drive = app.connected && blocked;
            if ui
                .add_enabled(can_drive, egui::Button::new("⏩ Step"))
                .on_hover_text("Advance one frame (runs until the next after_step hook)")
                .clicked()
            {
                app.play_state = PlayState::Paused;
                app.send_command(InspectorCommand::Step);
            }
            if ui
                .add_enabled(can_drive, egui::Button::new("🏃 Run"))
                .on_hover_text("Let the harness run until the next `after_run` hook fires")
                .clicked()
            {
                app.play_state = PlayState::Playing;
                app.send_command(InspectorCommand::Run);
            }

            ui.separator();

            // History navigation.
            let total = app.history.len();
            let can_back = app.view_index > 0;
            let can_forward = app.view_index + 1 < total;
            if ui
                .add_enabled(can_back, egui::Button::new("⏴"))
                .on_hover_text("Previous frame in history")
                .clicked()
            {
                app.set_view_index(app.view_index.saturating_sub(1));
            }
            if ui
                .add_enabled(can_forward, egui::Button::new("⏵"))
                .on_hover_text("Next frame in history")
                .clicked()
            {
                app.set_view_index(app.view_index + 1);
            }
            if ui
                .add_enabled(can_forward, egui::Button::new("⏩ Live"))
                .on_hover_text("Jump to the newest frame (follow live updates)")
                .clicked()
            {
                app.set_view_index(total.saturating_sub(1));
            }
            if total > 0 {
                // Both the slider value and the label are 1-indexed for display.
                let mut scrub = app.view_index + 1;
                let response = ui.add(
                    egui::Slider::new(&mut scrub, 1..=total)
                        .text(format!("/ {total}"))
                        .clamping(egui::SliderClamping::Always),
                );
                if response.changed() {
                    app.set_view_index(scrub.saturating_sub(1));
                }
            }

            if ui
                .add_enabled(total > 0, egui::Button::new("📋 Copy as GIF"))
                .on_hover_text(
                    "Encode the whole history as a GIF and put it on the system clipboard \
                     as a file reference — paste into Slack / Discord / Finder etc.",
                )
                .clicked()
            {
                log_diag("Copy as GIF clicked");
                // Run the copy on a detached worker so a slow encode doesn't stall the UI.
                // (We can't `catch_unwind` under `panic = abort`, but the panic hook still
                // logs what happened, and the real broken-pipe cause is fixed upstream.)
                let history = app.history.clone();
                let _ = std::thread::Builder::new()
                    .name("kittest_inspector_copy_gif".into())
                    .spawn(move || match copy_history_as_gif(&history, 10.0) {
                        Ok(path) => {
                            log_diag(&format!("Copied GIF to clipboard: {}", path.display()));
                        }
                        Err(err) => log_diag(&format!("Failed to copy GIF: {err}")),
                    });
                app.status_message =
                    Some("Encoding + copying GIF on background thread — see log".into());
            }
            if let Some(msg) = app.status_message.as_deref() {
                ui.weak(msg);
            }

            ui.separator();

            let prev_control = app.control_enabled;
            if !app.connected {
                // Nothing to drive if the harness is gone.
                app.control_enabled = false;
            }
            ui.add_enabled_ui(app.connected, |ui| {
                ui.checkbox(&mut app.control_enabled, "🎮 Control")
                    .on_hover_text(
                        "Forward pointer and keyboard events on the rendered frame to the harness",
                    )
                    .on_disabled_hover_text("Harness disconnected");
            });
            if prev_control && !app.control_enabled {
                app.queued_events.clear();
            }

            ui.separator();
            if ui
                .add_enabled(app.connected, egui::Button::new("📷 Screenshot"))
                .on_hover_text(
                    "Request a fresh framebuffer screenshot from the peer. \
                     Live `eframe` apps need this to send pixels; kittest harnesses already \
                     stream them on every step.",
                )
                .clicked()
            {
                app.send_command(InspectorCommand::Screenshot);
                app.awaiting_screenshot = true;
            }

            ui.separator();
            // Resize: send a logical-point width/height to the peer. `InspectorCommand::Resize`
            // maps to `Harness::set_size` for kittest peers and a `ViewportCommand::InnerSize`
            // for live `eframe` apps.
            if let Some((w, h)) = app.resize_target.as_mut() {
                ui.add(
                    egui::DragValue::new(w)
                        .speed(1.0)
                        .range(1..=8192)
                        .prefix("w "),
                )
                .on_hover_text("Resize width (logical points)");
                ui.label("×");
                ui.add(
                    egui::DragValue::new(h)
                        .speed(1.0)
                        .range(1..=8192)
                        .prefix("h "),
                )
                .on_hover_text("Resize height (logical points)");
                let (w, h) = (*w, *h);
                if ui
                    .add_enabled(app.connected, egui::Button::new("📐 Resize"))
                    .on_hover_text("Send InspectorCommand::Resize with these dimensions")
                    .clicked()
                {
                    app.send_command(InspectorCommand::Resize {
                        width: w,
                        height: h,
                    });
                }
            }

            ui.separator();
            if let Some(outcome) = &app.test_outcome {
                if outcome.ok {
                    ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "✅ test passed");
                } else {
                    let msg = outcome.message.as_deref().unwrap_or("(no message)");
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 110, 110),
                        format!("❌ test failed: {msg}"),
                    );
                }
            } else {
                ui.label(if !app.connected {
                    "harness disconnected"
                } else if app.harness_blocked() {
                    "harness blocked"
                } else {
                    "harness running"
                });
            }
        });
    });
}

fn details_panel(app: &mut InspectorApp, ui: &mut egui::Ui) {
    egui::Panel::right("details")
        .resizable(true)
        .default_size(380.0)
        .show_inside(ui, |ui| {
            let Some(frame) = app.view_frame().cloned() else {
                ui.weak("Waiting for frames...");
                return;
            };

            // The Source view sits in its own resizable top panel so the user can drop it out
            // of the way when they want more room for the widget / AccessKit sections below.
            // Once the test has finished, the outcome's `SourceView` (with `panic_line`)
            // takes priority over the latest frame's so failures jump to the panic site.
            let source = app
                .test_outcome
                .as_ref()
                .and_then(|o| o.source.as_ref())
                .or(frame.source.as_ref());
            egui::Panel::top("details_source")
                .resizable(true)
                .default_size(280.0)
                .show_inside(ui, |ui| {
                    ui.heading("Source");
                    let scroll_pending = std::mem::take(&mut app.scroll_pending);
                    match source {
                        Some(source) => source_section(ui, source, scroll_pending),
                        None => {
                            ui.weak("No source location for this frame.");
                        }
                    }
                });

            egui::ScrollArea::vertical().show(ui, |ui| {
                // Make long values (file paths, labels, stringified values in the widget
                // details grid, accesskit node names…) wrap inside the fixed-width side panel
                // instead of overflowing to the right.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);

                egui::CollapsingHeader::new("Frame")
                    .default_open(true)
                    .show(ui, |ui| {
                        kv_grid(ui, "frame_grid", |ui| {
                            if let Some(label) = &app.peer_label {
                                ui.label("Peer:");
                                ui.monospace(label);
                                ui.end_row();
                            }
                            ui.label("Step:");
                            ui.monospace(frame.step.to_string());
                            ui.end_row();
                            if let Some(shot) = frame.screenshot.as_ref() {
                                ui.label("Size (px):");
                                ui.monospace(format!("{} × {}", shot.width, shot.height));
                                ui.end_row();
                            }
                            ui.label("Pixels per point:");
                            ui.monospace(format!("{:.2}", frame.pixels_per_point));
                            ui.end_row();
                            let node_count = frame.accesskit.as_ref().map_or(0, |u| u.nodes.len());
                            ui.label("AccessKit nodes:");
                            ui.monospace(node_count.to_string());
                            ui.end_row();
                        });
                    });

                let target = app.selected_node.or(app.hovered_node);
                let header = if app.selected_node.is_some() {
                    "Selected widget"
                } else if app.hovered_node.is_some() {
                    "Hovered widget"
                } else {
                    "Widget"
                };
                egui::CollapsingHeader::new(header)
                    .default_open(true)
                    .show(ui, |ui| match (target, &frame.accesskit) {
                        (Some(id), Some(update)) => {
                            if let Some((_, node)) = update.nodes.iter().find(|(nid, _)| *nid == id)
                            {
                                widget_details(ui, id, node);
                            } else {
                                ui.weak("(node not in latest tree)");
                            }
                        }
                        _ => {
                            ui.weak("Hover over the rendered frame to inspect a widget.");
                        }
                    });

                if app.selected_node.is_some()
                    && ui
                        .small_button("clear selection")
                        .on_hover_text("Stop pinning the selected widget")
                        .clicked()
                {
                    app.selected_node = None;
                }

                egui::CollapsingHeader::new("AccessKit tree")
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(update) = &frame.accesskit {
                            accesskit_tree(
                                ui,
                                update,
                                &mut app.selected_node,
                                &mut app.hovered_node,
                            );
                        } else {
                            ui.weak("(no accesskit tree)");
                        }
                    });
            });
        });
}

fn central_panel(app: &mut InspectorApp, ui: &mut egui::Ui) {
    egui::CentralPanel::default().show_inside(ui, |ui| {
        let Some(tex) = app.current_texture.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label("Waiting for harness to connect...");
            });
            return;
        };
        let Some(frame) = app.view_frame().cloned() else {
            return;
        };

        let physical = tex.size_vec2(); // physical pixels of the rendered frame
        let avail = ui.available_size();
        let scale = (avail.x / physical.x)
            .min(avail.y / physical.y)
            .clamp(0.05, 1.0);
        let display_size = physical * scale;

        let (image_rect, response) = ui.allocate_exact_size(
            display_size,
            egui::Sense::click().union(egui::Sense::hover()),
        );
        ui.painter().image(
            tex.id(),
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        // Cache the image placement so the next frame's `hit_test_pointer` can run before
        // the tree is rendered and keep the two in sync.
        app.last_image_rect = Some(image_rect);
        app.last_image_scale = scale;

        // logical_point → screen_position:
        //     screen = image_rect.min + ak_rect * pixels_per_point * scale
        let logical_to_screen = |r: AkRect| -> egui::Rect {
            let f = frame.pixels_per_point * scale;
            egui::Rect::from_min_max(
                image_rect.min + egui::vec2(r.x0 as f32 * f, r.y0 as f32 * f),
                image_rect.min + egui::vec2(r.x1 as f32 * f, r.y1 as f32 * f),
            )
        };

        if app.control_enabled {
            // In Control mode clicks/hovers drive the harness, not the inspector.
            forward_events(
                app,
                ui,
                image_rect,
                frame.pixels_per_point,
                scale,
                &response,
            );
        } else {
            // Inspection mode: hover was already resolved in `hit_test_pointer` at the top
            // of `ui()` so the tree and the image stay in sync — we only need to handle the
            // click here.
            if response.clicked() {
                app.selected_node = app.hovered_node;
            }

            let painter = ui.painter_at(image_rect);
            if let Some(update) = &frame.accesskit {
                let draw = |id: NodeId, color: egui::Color32| {
                    if let Some((_, node)) = update.nodes.iter().find(|(nid, _)| *nid == id)
                        && let Some(b) = node.bounds()
                    {
                        painter.rect_stroke(
                            logical_to_screen(b),
                            2.0,
                            egui::Stroke::new(1.5, color),
                            egui::StrokeKind::Outside,
                        );
                    }
                };
                if let Some(id) = app.selected_node {
                    draw(id, egui::Color32::from_rgb(80, 180, 255));
                }
                if let Some(id) = app.hovered_node
                    && app.hovered_node != app.selected_node
                {
                    draw(id, egui::Color32::from_rgb(255, 220, 90));
                }
            }
        }
    });
}

/// Inspect the inspector's own input events and forward those relevant to the harness.
///
/// Pointer events only forward when their position is inside the rendered-image rect and their
/// coordinates are translated to harness logical space. Keyboard / text events always forward.
fn forward_events(
    app: &mut InspectorApp,
    ui: &egui::Ui,
    image_rect: egui::Rect,
    pixels_per_point: f32,
    scale: f32,
    image_response: &egui::Response,
) {
    let to_logical = |pos: egui::Pos2| -> egui::Pos2 {
        let f = pixels_per_point * scale;
        egui::pos2(
            (pos.x - image_rect.min.x) / f,
            (pos.y - image_rect.min.y) / f,
        )
    };

    let input_events = ui.ctx().input(|i| i.events.clone());
    for ev in input_events {
        match ev {
            egui::Event::PointerMoved(pos) if image_rect.contains(pos) => {
                app.queued_events
                    .push(egui::Event::PointerMoved(to_logical(pos)));
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            } if image_rect.contains(pos) => {
                app.queued_events.push(egui::Event::PointerButton {
                    pos: to_logical(pos),
                    button,
                    pressed,
                    modifiers,
                });
            }
            egui::Event::PointerGone => {
                app.queued_events.push(egui::Event::PointerGone);
            }
            mw @ egui::Event::MouseWheel { .. } if image_response.hovered() => {
                app.queued_events.push(mw);
            }
            ev @ (egui::Event::Text(_)
            | egui::Event::Key { .. }
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::Paste(_)
            | egui::Event::Ime(_)) => {
                app.queued_events.push(ev);
            }
            _ => {}
        }
    }
}

fn kv_grid(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .striped(true)
        .show(ui, body);
}

/// Render the "Source" section: the test file (topmost common ancestor across the call and
/// its events), with the relevant lines highlighted and (once per new frame) the view
/// scrolled to them.
fn source_section(ui: &mut egui::Ui, source: &SourceView, scroll_pending: bool) {
    ui.horizontal(|ui| {
        ui.monospace(shorten_path(&source.path));
        if let Some(line) = source.call_site_line {
            ui.weak(format!("(producer: line {line})"));
        }
    });

    let Some(contents) = source.contents.as_deref() else {
        ui.weak(format!("(couldn't read {})", source.path));
        return;
    };

    let call_site_line = source.call_site_line;
    let panic_line = source.panic_line;
    let event_lines: std::collections::HashSet<u32> = source.event_lines.iter().copied().collect();
    // Panic takes precedence for scroll focus so failed tests jump straight to the panic line.
    let focus_line = panic_line
        .or(call_site_line)
        .or_else(|| source.event_lines.first().copied());

    // Semi-transparent tints so the highlight works in both light and dark themes without
    // darkening the text. Alpha ~72/255 keeps the underlying text fully legible.
    let call_bg = egui::Color32::from_rgba_unmultiplied(80, 160, 255, 72);
    let event_bg = egui::Color32::from_rgba_unmultiplied(255, 180, 60, 72);
    let panic_bg = egui::Color32::from_rgba_unmultiplied(240, 80, 80, 96);

    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    let lines: Vec<&str> = contents.lines().collect();
    let total_height = lines.len() as f32 * row_height;

    // Estimated monospace advance width. For fixed-pitch fonts (like Hack) the ratio between
    // character height and advance is ~0.55; being slightly generous avoids clipping.
    let char_width = row_height * 0.6_f32;
    let longest_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32;
    let gutter_width = char_width * 5.0 + ui.spacing().item_spacing.x; // "{:>4} " column
    let content_width: f32 = gutter_width + char_width * longest_chars + 16.0;

    // Expand to fill the enclosing (resizable) panel — the user's drag on the panel handle
    // determines how tall the source view is.
    let scroll_area = egui::ScrollArea::both().auto_shrink([false, false]);
    // `show_viewport` lets us decide ourselves which rows to render + lets us reason in the
    // content's *virtual* coordinate space. That means we can build a target rect for the
    // focus line whether or not it's currently visible, and `scroll_to_rect` will animate
    // the scroll area towards it smoothly.
    scroll_area.show_viewport(ui, |ui, viewport| {
        let row_width = content_width.max(viewport.width());
        ui.set_height(total_height);
        ui.set_width(row_width);
        let content_top = ui.min_rect().top();
        let content_left = ui.min_rect().left();
        let start = (viewport.min.y / row_height).floor().max(0.0) as usize;
        let end = ((viewport.max.y / row_height).ceil() as usize)
            .min(lines.len())
            .max(start);

        for (idx, line) in lines.iter().enumerate().take(end).skip(start) {
            let line_no = idx as u32 + 1;
            let y = idx as f32 * row_height;
            let row_rect = egui::Rect::from_min_size(
                egui::pos2(content_left, content_top + y),
                egui::vec2(row_width, row_height),
            );
            let is_panic = Some(line_no) == panic_line;
            let is_call = Some(line_no) == call_site_line;
            let is_event = event_lines.contains(&line_no);
            let bg = if is_panic {
                Some(panic_bg)
            } else if is_call {
                Some(call_bg)
            } else if is_event {
                Some(event_bg)
            } else {
                None
            };
            let mut row_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(row_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            source_line_row(&mut row_ui, line_no, line, bg, row_rect);
        }

        if scroll_pending && let Some(focus) = focus_line {
            let y = focus.saturating_sub(1) as f32 * row_height;
            let target = egui::Rect::from_min_size(
                egui::pos2(content_left, content_top + y),
                egui::vec2(1.0, row_height),
            );
            ui.scroll_to_rect(target, Some(egui::Align::Center));
        }
    });
}

fn source_line_row(
    ui: &mut egui::Ui,
    line_no: u32,
    text: &str,
    bg: Option<egui::Color32>,
    row_rect: egui::Rect,
) {
    if let Some(color) = bg {
        ui.painter().rect_filled(row_rect, 2.0, color);
    }
    ui.add(egui::Label::new(
        egui::RichText::new(format!("{line_no:>4} "))
            .monospace()
            .weak(),
    ));
    ui.add(
        egui::Label::new(egui::RichText::new(text).monospace())
            .wrap_mode(egui::TextWrapMode::Extend),
    );
}

/// Shorten a `rustc`-reported path for display — keep the last two components so we show
/// `tests/menu.rs` instead of a long absolute path, while still disambiguating.
fn shorten_path(path: &str) -> String {
    let components: Vec<&str> = path.split(['/', '\\']).collect();
    if components.len() <= 2 {
        path.to_owned()
    } else {
        let n = components.len();
        format!("{}/{}", components[n - 2], components[n - 1])
    }
}

/// Render the accesskit tree recursively, similar in style to the egui demo's `inspection_ui`
/// — collapsible parents with their children indented below, leaves as selectable labels.
fn accesskit_tree(
    ui: &mut egui::Ui,
    update: &accesskit::TreeUpdate,
    selected: &mut Option<NodeId>,
    hovered: &mut Option<NodeId>,
) {
    use std::collections::{HashMap, HashSet};

    let nodes: HashMap<NodeId, &Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();

    // Prefer the tree's declared root. If this update doesn't carry tree-level info (diff-only
    // updates can omit it), fall back to any node that no other node lists as a child.
    let root = update.tree.as_ref().map(|t| t.root).or_else(|| {
        let mut children: HashSet<NodeId> = HashSet::new();
        for (_, node) in &update.nodes {
            for c in node.children() {
                children.insert(*c);
            }
        }
        update
            .nodes
            .iter()
            .map(|(id, _)| *id)
            .find(|id| !children.contains(id))
    });

    match root {
        Some(root_id) => render_ak_node(ui, root_id, &nodes, selected, hovered),
        None => {
            // Shouldn't normally happen; degrade to a flat list.
            for (id, _) in &update.nodes {
                render_ak_node(ui, *id, &nodes, selected, hovered);
            }
        }
    }
}

fn render_ak_node(
    ui: &mut egui::Ui,
    id: NodeId,
    nodes: &std::collections::HashMap<NodeId, &Node>,
    selected: &mut Option<NodeId>,
    hovered: &mut Option<NodeId>,
) {
    let Some(node) = nodes.get(&id).copied() else {
        ui.weak(format!("(missing {:?})", id.0));
        return;
    };
    let role = format!("{:?}", node.role());
    let text = match node.label().or_else(|| node.value()) {
        Some(label) if !label.is_empty() => format!("{role}  {label:?}"),
        _ => role,
    };
    // Both the image's hovered state and the tree's selection light up the same row — a row
    // shown highlighted in the tree corresponds to the rect drawn on the image.
    let highlight = *selected == Some(id) || *hovered == Some(id);
    let children = node.children();

    if children.is_empty() {
        let response = ui.selectable_label(highlight, text);
        if response.clicked() {
            *selected = Some(id);
        }
        if response.hovered() {
            *hovered = Some(id);
        }
        return;
    }

    let header_id = ui.make_persistent_id(("ak_node", id.0));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), header_id, true)
        .show_header(ui, |ui| {
            let response = ui.selectable_label(highlight, text);
            if response.clicked() {
                *selected = Some(id);
            }
            if response.hovered() {
                *hovered = Some(id);
            }
        })
        .body(|ui| {
            for child_id in children {
                render_ak_node(ui, *child_id, nodes, selected, hovered);
            }
        });
}

/// Render the inspector grid for a single accesskit node, mimicking egui's `inspection_ui`.
fn widget_details(ui: &mut egui::Ui, id: NodeId, node: &Node) {
    kv_grid(ui, "widget_grid", |ui| {
        ui.label("ID:");
        ui.monospace(format!("{:?}", id.0));
        ui.end_row();

        ui.label("Role:");
        ui.monospace(format!("{:?}", node.role()));
        ui.end_row();

        if let Some(b) = node.bounds() {
            ui.label("Bounds:");
            ui.monospace(format!(
                "({:.1}, {:.1}) → ({:.1}, {:.1})  [{:.1} × {:.1}]",
                b.x0,
                b.y0,
                b.x1,
                b.y1,
                b.x1 - b.x0,
                b.y1 - b.y0,
            ));
            ui.end_row();
        }

        for (label, value) in [
            ("Label:", node.label()),
            ("Value:", node.value()),
            ("Description:", node.description()),
            ("Placeholder:", node.placeholder()),
            ("Tooltip:", node.tooltip()),
            ("Class:", node.class_name()),
            ("Author ID:", node.author_id()),
            ("Keyboard:", node.keyboard_shortcut()),
        ] {
            if let Some(v) = value
                && !v.is_empty()
            {
                ui.label(label);
                ui.monospace(v);
                ui.end_row();
            }
        }

        let flags = [
            ("Disabled", node.is_disabled()),
            ("Hidden", node.is_hidden()),
            ("Read-only", node.is_read_only()),
        ];
        let mut on_flags: Vec<&str> = flags
            .iter()
            .filter(|(_, on)| *on)
            .map(|(n, _)| *n)
            .collect();
        if let Some(sel) = node.is_selected() {
            on_flags.push(if sel { "Selected" } else { "Unselected" });
        }
        if !on_flags.is_empty() {
            ui.label("Flags:");
            ui.monospace(on_flags.join(", "));
            ui.end_row();
        }

        if let Some(t) = node.toggled() {
            ui.label("Toggled:");
            ui.monospace(format!("{t:?}"));
            ui.end_row();
        }

        let child_count = node.children().len();
        if child_count > 0 {
            ui.label("Children:");
            ui.monospace(child_count.to_string());
            ui.end_row();
        }
    });
}

/// Encode the entire history as a looping GIF, write it to a timestamped file in the system
/// temp dir, and put a *file reference* for that path onto the system clipboard via arboard.
/// Pasting into Slack / Discord / GitHub / Finder etc. attaches the GIF with animation intact.
/// Mirrors the recorder's GIF behaviour: animation plays at `frame_rate`, last frame held
/// for one second so the loop point is obvious.
fn copy_history_as_gif(history: &[Frame], frame_rate: f32) -> Result<std::path::PathBuf, String> {
    use image::codecs::gif::{GifEncoder, Repeat};

    if history.is_empty() {
        return Err("history is empty".into());
    }
    log_diag(&format!(
        "encoding {} frame(s) @ {frame_rate} fps",
        history.len()
    ));

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // Stable-across-processes temp path is fine here: each invocation wants a fresh file.
    #[expect(clippy::disallowed_methods)]
    let path = std::env::temp_dir().join(format!("kittest_inspector_{ts}.gif"));
    log_diag(&format!("writing to {}", path.display()));

    let file = std::fs::File::create(&path)
        .map_err(|err| format!("couldn't create {}: {err}", path.display()))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = GifEncoder::new(writer);
    encoder
        .set_repeat(Repeat::Infinite)
        .map_err(|err| format!("set_repeat: {err}"))?;

    let denom = frame_rate.max(0.1).round().clamp(1.0, u32::MAX as f32) as u32;
    let frame_delay = image::Delay::from_numer_denom_ms(1000, denom);
    let hold_delay = image::Delay::from_numer_denom_ms(1000, 1);

    let last_idx = history.len() - 1;
    for (i, frame) in history.iter().enumerate() {
        let Some(shot) = frame.screenshot.as_ref() else {
            continue;
        };
        let buffer = image::load_from_memory(&shot.png)
            .map_err(|err| format!("frame {i}: decode screenshot PNG failed: {err}"))?
            .to_rgba8();
        let delay = if i == last_idx {
            hold_delay
        } else {
            frame_delay
        };
        let anim_frame = image::Frame::from_parts(buffer, 0, 0, delay);
        encoder
            .encode_frame(anim_frame)
            .map_err(|err| format!("encode frame {i}: {err}"))?;
    }
    // Finalise the GIF write before handing the path to the clipboard.
    drop(encoder);
    log_diag("GIF encoded, opening clipboard…");

    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| format!("open clipboard: {err}"))?;
    log_diag("clipboard opened, setting file_list…");
    clipboard
        .set()
        .file_list(&[&path])
        .map_err(|err| format!("set clipboard file list: {err}"))?;
    log_diag("clipboard file_list set");

    Ok(path)
}

/// Append a diagnostic line to `{temp}/kittest_inspector.log`.
///
/// We do NOT write to stderr — when the harness's captured stderr pipe closes mid-run,
/// `eprintln!` panics with "failed printing to stderr: Broken pipe" and kills the window.
pub fn log_diag(msg: &str) {
    use std::io::Write as _;

    #[expect(clippy::disallowed_methods)]
    let path = std::env::temp_dir().join("kittest_inspector.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}
