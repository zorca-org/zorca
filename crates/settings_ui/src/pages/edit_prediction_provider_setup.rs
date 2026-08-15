use copilot::GlobalCopilotAuth;
use copilot_ui::{ConfigurationMode, ConfigurationView};
use edit_prediction_ui::set_completion_provider;
use gpui::{ScrollHandle, prelude::*};
use language::language_settings::{AllLanguageSettings, EditPredictionProvider};
use settings::Settings as _;
use ui::{ContextMenu, DropdownMenu, DropdownStyle, prelude::*};
use workspace::AppState;

use crate::{SettingsWindow, components::SettingsSectionHeader};

pub(crate) fn render_edit_prediction_setup_page(
    _settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let configured_provider = AllLanguageSettings::get_global(cx)
        .edit_predictions
        .provider;
    let current_provider = match configured_provider {
        EditPredictionProvider::Copilot => EditPredictionProvider::Copilot,
        _ => EditPredictionProvider::None,
    };
    let current_provider_name = current_provider.display_name().unwrap_or("Disabled");
    let menu = ContextMenu::build(window, cx, move |menu, _, cx| {
        let fs = <dyn fs::Fs>::global(cx);
        [
            EditPredictionProvider::None,
            EditPredictionProvider::Copilot,
        ]
        .into_iter()
        .fold(menu, |menu, provider| {
            let name = provider.display_name().unwrap_or("Disabled");
            let fs = fs.clone();
            menu.toggleable_entry(
                name,
                provider == current_provider,
                IconPosition::Start,
                None,
                move |_, cx| set_completion_provider(fs.clone(), cx, provider),
            )
        })
    });
    let configuration = window.use_state(cx, |_, cx| {
        ConfigurationView::new(
            |cx| {
                let app_state = AppState::global(cx);
                GlobalCopilotAuth::try_get_or_init(app_state, cx)
                    .is_some_and(|copilot| copilot.0.read(cx).is_authenticated())
            },
            ConfigurationMode::EditPrediction,
            cx,
        )
    });

    v_flex()
        .id("copilot-setup-page")
        .size_full()
        .px_8()
        .pb_16()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            v_flex()
                .gap_1p5()
                .child(SettingsSectionHeader::new("Active Provider").no_padding(true))
                .child(
                    h_flex()
                        .pt_2p5()
                        .w_full()
                        .justify_between()
                        .child(
                            v_flex().max_w_1_2().child(Label::new("Provider")).child(
                                Label::new("Use GitHub Copilot for edit predictions.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                        .child(
                            DropdownMenu::new("provider-dropdown", current_provider_name, menu)
                                .tab_index(0)
                                .style(DropdownStyle::Outlined),
                        ),
                ),
        )
        .child(
            v_flex()
                .pt_8()
                .gap_1p5()
                .child(
                    SettingsSectionHeader::new("GitHub Copilot")
                        .icon(IconName::Copilot)
                        .no_padding(true),
                )
                .child(configuration),
        )
        .into_any_element()
}
