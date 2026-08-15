use collections::HashMap;
use copilot::{Copilot, CopilotEditPredictionDelegate};
use editor::{EditPredictionRequestTrigger, Editor};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, WeakEntity};
use language::language_settings::{EditPredictionProvider, all_language_settings};
use settings::SettingsStore;
use std::{cell::RefCell, rc::Rc};
use ui::Window;

pub fn init(cx: &mut App) {
    let editors: Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>> = Rc::default();
    cx.observe_new({
        let editors = editors.clone();
        move |editor: &mut Editor, window, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }

            register_backward_compatible_actions(editor, cx);

            let Some(window) = window else {
                return;
            };

            let editor_handle = cx.entity().downgrade();
            cx.on_release({
                let editor_handle = editor_handle.clone();
                let editors = editors.clone();
                move |_, _| {
                    editors.borrow_mut().remove(&editor_handle);
                }
            })
            .detach();

            editors
                .borrow_mut()
                .insert(editor_handle, window.window_handle());
            assign_edit_prediction_provider(
                editor,
                copilot_enabled(cx),
                EditPredictionRequestTrigger::EditorCreated,
                window,
                cx,
            );
        }
    })
    .detach();

    cx.observe_global::<SettingsStore>({
        let mut was_enabled = copilot_enabled(cx);
        move |cx| {
            let is_enabled = copilot_enabled(cx);
            if is_enabled == was_enabled {
                return;
            }

            telemetry::event!(
                "Edit Prediction Provider Changed",
                from = provider_name(was_enabled),
                to = provider_name(is_enabled)
            );
            was_enabled = is_enabled;

            for (editor, window) in editors.borrow().iter() {
                _ = window.update(cx, |_window, window, cx| {
                    _ = editor.update(cx, |editor, cx| {
                        assign_edit_prediction_provider(
                            editor,
                            is_enabled,
                            EditPredictionRequestTrigger::ProviderChanged,
                            window,
                            cx,
                        );
                    })
                });
            }
        }
    })
    .detach();
}

fn copilot_enabled(cx: &App) -> bool {
    all_language_settings(None, cx).edit_predictions.provider == EditPredictionProvider::Copilot
}

fn provider_name(enabled: bool) -> &'static str {
    if enabled { "Copilot" } else { "None" }
}

fn register_backward_compatible_actions(editor: &mut Editor, cx: &mut Context<Editor>) {
    // We renamed some of these actions to not be copilot-specific, but that
    // would have not been backwards-compatible. So here we are re-registering
    // the actions with the old names to not break people's keymaps.
    editor
        .register_action(cx.listener(
            |editor, _: &copilot::Suggest, window: &mut Window, cx: &mut Context<Editor>| {
                editor.show_edit_prediction(&Default::default(), window, cx);
            },
        ))
        .detach();
}

fn assign_edit_prediction_provider(
    editor: &mut Editor,
    enabled: bool,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if !enabled {
        editor.set_edit_prediction_provider::<CopilotEditPredictionDelegate>(
            None, trigger, window, cx,
        );
        return;
    }

    let Some(project) = editor.project().cloned() else {
        return;
    };
    let Some(copilot) = Copilot::start_for_project(&project, cx) else {
        return;
    };

    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
        copilot.update(cx, |copilot, cx| {
            copilot.register_buffer(&buffer, cx);
        });
    }

    let provider = cx.new(|_| CopilotEditPredictionDelegate::new(copilot));
    editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
}
