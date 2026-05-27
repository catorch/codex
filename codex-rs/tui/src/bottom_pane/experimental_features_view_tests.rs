use super::*;
use codex_features::Stage;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::unbounded_channel;

#[test]
fn js_repl_toggle_renders_and_saves() {
    let Stage::Experimental {
        name,
        menu_description,
        ..
    } = Feature::JsRepl.stage()
    else {
        panic!("js_repl must be available in the experimental menu");
    };
    let mut frames = Vec::new();
    for width in [80, 40] {
        let (tx, mut rx) = unbounded_channel();
        let mut view = ExperimentalFeaturesView::new(
            vec![ExperimentalFeatureItem {
                feature: Feature::JsRepl,
                name: name.to_string(),
                description: menu_description.to_string(),
                enabled: false,
            }],
            AppEventSender::new(tx),
            crate::keymap::RuntimeKeymap::defaults().list,
        );
        for state in ["disabled", "enabled"] {
            if state == "enabled" {
                view.handle_key_event(KeyEvent::from(KeyCode::Char(' ')));
            }
            let area = Rect::new(
                /*x*/ 0,
                /*y*/ 0,
                width,
                view.desired_height(width),
            );
            let mut buffer = Buffer::empty(area);
            view.render(area, &mut buffer);
            let rendered = buffer
                .content
                .chunks(usize::from(width))
                .map(|row| {
                    row.iter()
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            frames.push(format!("width {width}, {state}:\n{rendered}"));
        }
        view.handle_key_event(KeyEvent::from(KeyCode::Enter));
        let AppEvent::UpdateFeatureFlags { updates } = rx.try_recv().expect("saved feature flags")
        else {
            panic!("expected feature flag update");
        };
        assert_eq!(updates, vec![(Feature::JsRepl, true)]);
        assert!(view.is_complete());
    }
    insta::assert_snapshot!("js_repl_experimental_toggle", frames.join("\n\n"));
}
