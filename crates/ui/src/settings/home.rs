//! Settings → Home: which panels show under the new-chat composer, and what
//! Running services lists. Device-local UI settings; nothing here talks RPC.

use gpui::{Context, SharedString, Window, div, prelude::*, px};

use crate::settings::widgets;
use crate::shell::home_stats::{HomePanel, panel_shown, set_panel_shown};
use crate::theme::Theme;

#[derive(Default)]
pub struct HomeSettingsPage;

/// A switch sized to its 56×40 activation target, as on Notifications.
fn switch(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: &'static str,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let accent = theme.accent;
    div()
        .id(id.clone())
        .flex_none()
        .w(px(widgets::SWITCH_WIDTH))
        .h(px(40.0))
        .flex()
        .items_center()
        .justify_center()
        .role(gpui::Role::Switch)
        .aria_label(label)
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .tab_index(0)
        .focus_visible(move |style| style.border_2().border_color(accent))
        .cursor_pointer()
        .child(widgets::toggle_switch(theme, on, id))
}

fn row(theme: &Theme, first: bool, title: &'static str, description: &'static str) -> gpui::Div {
    widgets::card_row(theme, first).child(
        div()
            .flex_1()
            .min_w(px(160.0))
            .flex()
            .flex_col()
            .child(widgets::row_title(theme, title))
            .child(widgets::meta_line(
                theme,
                vec![div().child(description).into_any_element()],
            )),
    )
}

impl Render for HomeSettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let panels = HomePanel::ALL.into_iter().map(|panel| {
            let shown = panel_shown(panel, cx);
            row(&theme, false, panel.label(), panel.description()).child(
                switch(
                    &theme,
                    format!("home-panel-{}", panel.label()),
                    panel.label(),
                    shown,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    set_panel_shown(panel, !shown, cx);
                    cx.notify();
                })),
            )
        });
        let panels_card = widgets::section_card(&theme)
            .mt_0()
            .child(row(
                &theme,
                true,
                "Working now",
                "Running and finished threads across your devices. Always shown.",
            ))
            .children(panels);
        let all_ports = crate::settings::home_services_all_ports(cx);
        let services_card = widgets::section_card(&theme).mt_0().child(
            row(
                &theme,
                true,
                "Show all ports",
                "List every port your processes listen on, not only those started in a project.",
            )
            .child(
                switch(
                    &theme,
                    "home-services-all-ports",
                    "Show all ports",
                    all_ports,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    crate::settings::set_home_services_all_ports(!all_ports, cx);
                    cx.notify();
                })),
            ),
        );
        div()
            .id("home-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Home", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Panels under the new-chat composer. Customize on Home toggles them too.",
                    ))
                    .child(widgets::section(&theme, "Panels", panels_card))
                    .child(widgets::section(&theme, "Running services", services_card)),
            )
    }
}
