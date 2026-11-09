#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayElement {
    pub uid: u32,
    pub role: String,
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone)]
pub struct ConsoleLog {
    pub level: String,
    pub text: String,
}

pub fn make_request(id: u64, method: &str, params: Value) -> String {
    json!({
        "id": id,
        "method": method,
        "params": params
    })
    .to_string()
}

/// Bits CDP `modifiers` : Alt=1, Ctrl=2, Meta=4, Shift=8.
pub const CDP_MOD_ALT: u32 = 1;
pub const CDP_MOD_CTRL: u32 = 2;
pub const CDP_MOD_META: u32 = 4;
pub const CDP_MOD_SHIFT: u32 = 8;

/// (alt, ctrl, shift, meta) → bitmask CDP
pub fn cdp_modifiers(alt: bool, ctrl: bool, shift: bool, meta: bool) -> u32 {
    ((alt as u32) * CDP_MOD_ALT)
        | ((ctrl as u32) * CDP_MOD_CTRL)
        | ((shift as u32) * CDP_MOD_SHIFT)
        | ((meta as u32) * CDP_MOD_META)
}

pub fn page_enable(id: u64) -> String {
    make_request(id, "Page.enable", json!({}))
}

pub fn runtime_enable(id: u64) -> String {
    make_request(id, "Runtime.enable", json!({}))
}

pub fn page_navigate(id: u64, url: &str) -> String {
    make_request(id, "Page.navigate", json!({ "url": url }))
}

pub fn start_screencast(
    id: u64,
    max_width: u32,
    max_height: u32,
    format: &str,
    quality: u32,
) -> String {
    make_request(
        id,
        "Page.startScreencast",
        json!({
            "format": format,
            "quality": quality,
            "maxWidth": max_width,
            "maxHeight": max_height,
            "everyNthFrame": 1
        }),
    )
}

pub fn stop_screencast(id: u64) -> String {
    make_request(id, "Page.stopScreencast", json!({}))
}

pub fn screencast_frame_ack(id: u64, session_id: u64) -> String {
    make_request(
        id,
        "Page.screencastFrameAck",
        json!({ "sessionId": session_id }),
    )
}

pub fn capture_screenshot(id: u64) -> String {
    make_request(id, "Page.captureScreenshot", json!({ "format": "png" }))
}

pub fn set_device_metrics(id: u64, width: u32, height: u32, device_scale_factor: f32) -> String {
    make_request(
        id,
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": device_scale_factor,
            "mobile": false
        }),
    )
}

pub fn dispatch_mouse_move(id: u64, x: f32, y: f32) -> String {
    make_request(
        id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseMoved",
            "x": x,
            "y": y
        }),
    )
}

pub fn dispatch_mouse_click(
    id: u64,
    event_type: &str,
    x: f32,
    y: f32,
    button: &str,
    click_count: u32,
) -> String {
    make_request(
        id,
        "Input.dispatchMouseEvent",
        json!({
            "type": event_type, // "mousePressed" or "mouseReleased"
            "x": x,
            "y": y,
            "button": button,
            "clickCount": click_count
        }),
    )
}

pub fn dispatch_mouse_wheel(id: u64, x: f32, y: f32, delta_x: f64, delta_y: f64) -> String {
    make_request(
        id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseWheel",
            "x": x,
            "y": y,
            "deltaX": delta_x,
            "deltaY": delta_y
        }),
    )
}

pub fn dispatch_char_event(id: u64, text: &str) -> String {
    make_request(
        id,
        "Input.dispatchKeyEvent",
        json!({
            "type": "char",
            "text": text
        }),
    )
}

pub fn dispatch_key_event(
    id: u64,
    event_type: &str,
    key: &str,
    code: &str,
    windows_virtual_key_code: Option<u32>,
    modifiers: u32,
) -> String {
    let mut params = json!({
        "type": event_type, // "rawKeyDown" or "keyUp"
        "key": key,
        "code": code,
        "modifiers": modifiers
    });
    if let Some(vk) = windows_virtual_key_code {
        params["windowsVirtualKeyCode"] = json!(vk);
    }
    make_request(id, "Input.dispatchKeyEvent", params)
}

pub fn insert_text(id: u64, text: &str) -> String {
    make_request(id, "Input.insertText", json!({ "text": text }))
}

pub fn page_reload(id: u64) -> String {
    make_request(id, "Page.reload", json!({ "ignoreCache": false }))
}

pub fn get_navigation_history(id: u64) -> String {
    make_request(id, "Page.getNavigationHistory", json!({}))
}

pub fn navigate_to_history_entry(id: u64, entry_id: u64) -> String {
    make_request(
        id,
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
    )
}

pub fn handle_java_script_dialog(id: u64, accept: bool, prompt_text: Option<&str>) -> String {
    let mut params = json!({ "accept": accept });
    if let Some(text) = prompt_text {
        params["promptText"] = json!(text);
    }
    make_request(id, "Page.handleJavaScriptDialog", params)
}

pub fn set_auto_attach(id: u64) -> String {
    make_request(
        id,
        "Target.setAutoAttach",
        json!({
            "autoAttach": true,
            "flatten": true,
            "waitForDebuggerOnStart": false
        }),
    )
}

pub fn evaluate(id: u64, expression: &str) -> String {
    make_request(
        id,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true
        }),
    )
}

/// JS payload that extracts interactive element bounding boxes and labels for the Obscura overlay.
pub const EXTRACT_ELEMENTS_SCRIPT: &str = r#"
(() => {
    const selectors = 'a[href], button, input, select, textarea, [role="button"], [role="link"], [role="tab"], [role="checkbox"], [onclick]';
    const nodes = Array.from(document.querySelectorAll(selectors));
    let uid = 1;
    const items = [];
    for (const el of nodes) {
        const style = window.getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') continue;
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) continue;
        const text = (el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || el.title || '').slice(0, 40).trim();
        const role = el.getAttribute('role') || el.tagName.toLowerCase();
        items.push({
            uid: uid++,
            role: role,
            text: text,
            x: Math.round(rect.x),
            y: Math.round(rect.y),
            width: Math.round(rect.width),
            height: Math.round(rect.height)
        });
        if (items.length >= 60) break;
    }
    return items;
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parsed(request: &str) -> Value {
        serde_json::from_str(request).unwrap()
    }

    #[test]
    fn builds_page_reload() {
        let v = parsed(&page_reload(3));
        assert_eq!(v["method"], "Page.reload");
        assert_eq!(v["params"]["ignoreCache"], false);
    }

    #[test]
    fn builds_navigation_history_requests() {
        let history = parsed(&get_navigation_history(7));
        assert_eq!(history["method"], "Page.getNavigationHistory");
        assert_eq!(history["params"].as_object().map(|o| o.len()), Some(0));

        let jump = parsed(&navigate_to_history_entry(8, 12));
        assert_eq!(jump["method"], "Page.navigateToHistoryEntry");
        assert_eq!(jump["params"]["entryId"], 12);
    }

    #[test]
    fn builds_javascript_dialog_responses() {
        let accept = parsed(&handle_java_script_dialog(4, true, None));
        assert_eq!(accept["method"], "Page.handleJavaScriptDialog");
        assert_eq!(accept["params"]["accept"], true);
        assert!(accept["params"].get("promptText").is_none());

        let dismiss_prompt = parsed(&handle_java_script_dialog(5, false, Some("hello")));
        assert_eq!(dismiss_prompt["params"]["accept"], false);
        assert_eq!(dismiss_prompt["params"]["promptText"], "hello");
    }

    #[test]
    fn builds_insert_text_request() {
        let v = parsed(&insert_text(6, "collé ✓"));
        assert_eq!(v["method"], "Input.insertText");
        assert_eq!(v["params"]["text"], "collé ✓");
    }

    #[test]
    fn builds_auto_attach_request() {
        let v = parsed(&set_auto_attach(9));
        assert_eq!(v["method"], "Target.setAutoAttach");
        assert_eq!(v["params"]["autoAttach"], true);
        assert_eq!(v["params"]["flatten"], true);
        assert_eq!(v["params"]["waitForDebuggerOnStart"], false);
    }

    #[test]
    fn builds_key_event_with_modifiers() {
        let v = parsed(&dispatch_key_event(
            10,
            "rawKeyDown",
            "Backspace",
            "Backspace",
            Some(8),
            cdp_modifiers(false, false, false, false),
        ));
        assert_eq!(v["method"], "Input.dispatchKeyEvent");
        assert_eq!(v["params"]["type"], "rawKeyDown");
        assert_eq!(v["params"]["key"], "Backspace");
        assert_eq!(v["params"]["code"], "Backspace");
        assert_eq!(v["params"]["windowsVirtualKeyCode"], 8);
        assert_eq!(v["params"]["modifiers"], 0);

        let chord = parsed(&dispatch_key_event(
            11,
            "rawKeyDown",
            "c",
            "KeyC",
            Some(67),
            cdp_modifiers(false, true, false, true),
        ));
        assert_eq!(chord["params"]["modifiers"], 2 | 4); // ctrl + meta
    }

    #[test]
    fn builds_mouse_click_with_button_and_count() {
        let right_double = parsed(&dispatch_mouse_click(
            12,
            "mousePressed",
            10.0,
            20.0,
            "right",
            2,
        ));
        assert_eq!(right_double["params"]["type"], "mousePressed");
        assert_eq!(right_double["params"]["button"], "right");
        assert_eq!(right_double["params"]["clickCount"], 2);

        let left = parsed(&dispatch_mouse_click(
            13,
            "mouseReleased",
            0.0,
            0.0,
            "left",
            1,
        ));
        assert_eq!(left["params"]["clickCount"], 1);
    }
}
