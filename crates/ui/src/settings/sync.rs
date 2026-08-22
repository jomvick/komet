use gpui::{div, prelude::*, px, AnyElement, SharedString};
use crate::theme::Theme;

pub fn render_sync_settings(cx: &mut gpui::Context<crate::shell::Shell>) -> AnyElement {
    let theme = Theme::of(cx).clone();
    let edge_url = std::env::var("KOMET_EDGE_URL").unwrap_or_else(|_| "local".into());
    let has_token = std::env::var("KOMET_SYNC_TOKEN").map(|v| !v.is_empty()).unwrap_or(false);
    let status = if has_token { "Synced" } else { "Local" };
    let status_color = if has_token { theme.success } else { theme.text_muted };
    div().flex().flex_col().gap(px(16.)).p(px(24.))
        .child(div().text_size(px(16.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.text).child("Sync"))
        .child(div().flex().flex_row().gap(px(8.)).child(SharedString::from(format!("Status: {status}"))).text_color(status_color))
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(SharedString::from(format!("Edge: {edge_url}"))))
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(SharedString::from(if has_token { "Token: •••• (set)" } else { "Token: not set — local only" })))
        .child(div().text_size(px(11.)).text_color(theme.text_muted.opacity(0.6)).child("Set KOMET_EDGE_URL and KOMET_SYNC_TOKEN, then restart komet. See docs/self-hosted-sync.md"))
        .into_any_element()
}
