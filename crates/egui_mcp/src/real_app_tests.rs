//! `query_tree` run against a real egui frame.
//!
//! The hand-built trees in [`crate::tree`]'s tests pin one rule each, on a tree of five nodes.
//! This module builds a small but ordinary app — panels, a heading, buttons, a text field, a
//! check box, a slider, a collapsing section — lays out an actual frame with `AccessKit`
//! enabled, and snapshots what an agent would receive. That is where the rules meet egui's real
//! output: the containers it emits, where it puts a widget's text, and how deep the nesting
//! gets.
//!
//! The snapshots carry no positions (a [`TreeNode`] has no bounds), so they don't move with
//! fonts or platform.

use accesskit_consumer::Tree;

use crate::tree::{Query, QueryFilter, TreeNode, query};

/// Logical size of the frame the tests lay out. Wide enough that nothing is clipped, which
/// would otherwise show up as a `hidden` flag that differs between platforms.
const SCREEN_SIZE: egui::Vec2 = egui::vec2(1280.0, 900.0);

/// A label long enough to wrap over several lines, to show whether an agent is handed the whole
/// text or only what fits.
const LONG_LABEL: &str = "This paragraph is here to be long: it wraps over several lines in the central panel, so the frame has to lay it out as more than one run of text. An agent reading the tree should still get every word of it, because the text it can read is the only thing telling it what this part of the app is for.";

/// The demo app's mutable state, so its widgets report real values rather than defaults.
struct DemoApp {
    search: String,
    wrap_lines: bool,
    volume: f32,
    project: usize,

    /// egui's own gallery, for coverage of the widget kinds we wouldn't think to write out:
    /// combo box, radio group, colour picker, progress bar, hyperlink, and the rest.
    gallery: egui_demo_lib::WidgetGallery,
}

impl Default for DemoApp {
    fn default() -> Self {
        Self {
            search: "tree".to_owned(),
            wrap_lines: true,
            volume: 42.0,
            project: 1,
            gallery: egui_demo_lib::WidgetGallery::default(),
        }
    }
}

impl DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui) {
        // The gallery shows an image, which needs a loader to be anything but a broken icon.
        egui_extras::install_image_loaders(ui.ctx());

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Search");
                ui.text_edit_singleline(&mut self.search);
                ui.button("Run").clicked();
            });
        });

        egui::Panel::left("projects").show(ui, |ui| {
            ui.heading("Projects");
            for (i, name) in ["egui", "kittest", "rerun"].iter().enumerate() {
                ui.selectable_value(&mut self.project, i, *name);
            }
        });

        // An agent panel living inside the app it drives, as a host embedding this server has.
        // It echoes the user's own words, so a text query hits it before it hits the app.
        egui::Panel::right("agent chat").show(ui, |ui| {
            ui.heading("Chat");
            ui.label("where is the volume slider?");
            ui.label("Second row of the settings panel.");
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.label("Settings");
            ui.label(LONG_LABEL);
            ui.checkbox(&mut self.wrap_lines, "Wrap long lines");
            ui.add(egui::Slider::new(&mut self.volume, 0.0..=100.0).text("Volume"));
            egui::CollapsingHeader::new("Advanced")
                .default_open(true)
                .show(ui, |ui| {
                    ui.label("Nothing to see here");
                    ui.add_enabled(false, egui::Button::new("Reset"));
                });

            // Neither of these is a widget an agent can act on, so neither should reach it.
            ui.separator();
            ui.add_space(16.0);

            egui_demo_lib::View::ui(&mut self.gallery, ui);
        });
    }
}

/// Lay out two frames of the demo app and hand back the `AccessKit` tree of the second.
///
/// Two, because egui sizes several containers from what the previous frame measured — one pass
/// alone would snapshot a half-laid-out app.
fn demo_app_tree() -> Tree {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN_SIZE)),
        ..Default::default()
    };

    let mut app = DemoApp::default();
    // `drop_without_applying_deltas` because these frames are never painted: nothing here
    // consumes the texture deltas an output carries, and dropping one that still holds them
    // panics.
    ctx.run_ui(input.clone(), |ui| app.ui(ui))
        .drop_without_applying_deltas();
    let mut output = ctx.run_ui(input, |ui| app.ui(ui));
    let update = output
        .platform_output
        .accesskit_update
        .take()
        .expect("accesskit is enabled, so every frame carries an update");
    output.drop_without_applying_deltas();
    // `false` — the same as the bridge passes for an app whose window isn't focused.
    Tree::new(update, false)
}

fn query_app(filter: &QueryFilter) -> String {
    let nodes = query(&demo_app_tree(), filter, 1.0);
    serde_json::to_string_pretty(&nodes).expect("serialize")
}

/// The whole app, the way `query_tree` with no filter hands it to an agent.
///
/// Without a `limit`, so the snapshot stays the whole picture — what `limit` does to it is its
/// own test.
#[test]
fn the_whole_app() {
    insta::assert_snapshot!(query_app(&QueryFilter {
        limit: usize::MAX,
        ..Default::default()
    }));
}

/// Every button, lifted out of the panels and rows that hold them.
#[test]
fn every_button() {
    insta::assert_snapshot!(query_app(&QueryFilter {
        query: Query {
            role: Some("Button".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }));
}

/// A text match, the way an agent looks for one widget it means to act on. `content_contains`
/// because a `Label`'s text is in `value`, while a slider's is in `label`.
#[test]
fn one_widget_by_its_text() {
    insta::assert_snapshot!(query_app(&QueryFilter {
        query: Query {
            content_contains: Some("volume".to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }));
}

/// A wrapped label reaches the agent whole: egui splits it into one `TextRun` per line, but the
/// `Label` above them still carries the entire string, so nothing is truncated on the way out.
#[test]
fn a_long_label_arrives_whole() {
    fn find<'a>(nodes: &'a [TreeNode], value: &str) -> Option<&'a TreeNode> {
        nodes.iter().find_map(|node| {
            (node.value.as_deref() == Some(value))
                .then_some(node)
                .or_else(|| find(&node.children, value))
        })
    }

    let whole_tree = query(&demo_app_tree(), &QueryFilter::default(), 1.0);
    let label = find(&whole_tree, LONG_LABEL).expect("the long label is in the tree, in full");
    assert!(
        label.children.len() > 1,
        "the text wrapped, so it arrives as several runs below the label"
    );

    // And a phrase from the middle of it finds that same label, not just the run it landed in.
    let matched = query(
        &demo_app_tree(),
        &QueryFilter {
            query: Query {
                content_contains: Some("every word of it".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        },
        1.0,
    );
    assert_eq!(
        matched.first().and_then(|node| node.value.as_deref()),
        Some(LONG_LABEL)
    );
}

/// Nothing an agent can act on should reach it without an `id` to act on it with, and the
/// pruning rules shouldn't leave a node that carries nothing at all.
#[test]
fn every_returned_node_is_actionable() {
    fn check(nodes: &[TreeNode]) {
        for node in nodes {
            assert!(!node.id.is_empty(), "every node is addressable");
            assert!(
                node.role.is_some()
                    || node.label.is_some()
                    || node.value.is_some()
                    || !node.children.is_empty(),
                "a node that says nothing should have been pruned: {node:?}"
            );
            check(&node.children);
        }
    }

    let nodes = query(&demo_app_tree(), &QueryFilter::default(), 1.0);
    assert!(!nodes.is_empty(), "the app has widgets");
    check(&nodes);
}
