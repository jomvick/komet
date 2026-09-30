use crate::theme::Theme;
use gpui::{AnyElement, SharedString, div, prelude::*, px};

pub fn render_sync_settings(cx: &mut gpui::Context<crate::shell::Shell>) -> AnyElement {
    let theme = Theme::of(cx).clone();
    let edge_url = std::env::var("KOMET_EDGE_URL").unwrap_or_else(|_| "local".into());
    let has_token = std::env::var("KOMET_SYNC_TOKEN")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let status = if has_token { "Synced" } else { "Local" };
    let status_color = if has_token {
        theme.success
    } else {
        theme.text_muted
    };
    let insecure = std::env::var("KOMET_EDGE_URL").ok().is_some_and(|u| {
        let u = u.trim();
        let Some(rest) = u.split_once("://").filter(|(s, _)| s.eq_ignore_ascii_case("http")).map(|(_, r)| r) else {
            return false;
        };
        let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let host = host.rsplit('@').next().unwrap_or(host);
        let host = host.strip_prefix('[').and_then(|h| h.split_once(']')).map(|(h, _)| h).unwrap_or_else(|| host.split_once(':').map(|(h, _)| h).unwrap_or(host));
        let host = host.trim().to_lowercase();
        !(host.is_empty() || host == "localhost" || host == "127.0.0.1" || host == "::1")
    });
    div().flex().flex_col().gap(px(16.)).p(px(24.))
        .child(div().text_size(px(16.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.text).child("Sync"))
        .child(div().flex().flex_row().gap(px(8.)).child(SharedString::from(format!("Status: {status}"))).text_color(status_color))
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(SharedString::from(format!("Edge: {edge_url}"))))
        .child(div().text_size(px(12.)).text_color(theme.text_muted).child(SharedString::from(if has_token { "Token: •••• (set)" } else { "Token: not set — local only" })))
        .child(div().text_size(px(11.)).text_color(theme.text_muted.opacity(0.6)).child("Set KOMET_EDGE_URL (https://) and KOMET_SYNC_TOKEN, then restart komet. See docs/self-hosted-sync.md"))
        .child(if insecure {
            div().text_size(px(12.)).text_color(theme.warning).child("Warning: token sent over plain HTTP — use HTTPS or set KOMET_SYNC_ALLOW_INSECURE_HTTP=1 on a trusted LAN.")
        } else {
            div()
        })
        .into_any_element()
}
