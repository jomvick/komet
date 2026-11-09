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

pub fn page_enable(id: u64) -> String {
    make_request(id, "Page.enable", json!({}))
}

pub fn runtime_enable(id: u64) -> String {
    make_request(id, "Runtime.enable", json!({}))
}

pub fn page_navigate(id: u64, url: &str) -> String {
    make_request(id, "Page.navigate", json!({ "url": url }))
}

pub fn start_screencast(id: u64, max_width: u32, max_height: u32, format: &str, quality: u32) -> String {
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
    make_request(id, "Page.screencastFrameAck", json!({ "sessionId": session_id }))
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

pub fn dispatch_mouse_click(id: u64, event_type: &str, x: f32, y: f32, button: &str) -> String {
    make_request(
        id,
        "Input.dispatchMouseEvent",
        json!({
            "type": event_type, // "mousePressed" or "mouseReleased"
            "x": x,
            "y": y,
            "button": button,
            "clickCount": 1
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

pub fn dispatch_key_down(id: u64, key: &str, code: &str, windows_virtual_key_code: u32) -> String {
    make_request(
        id,
        "Input.dispatchKeyEvent",
        json!({
            "type": "rawKeyDown",
            "key": key,
            "code": code,
            "windowsVirtualKeyCode": windows_virtual_key_code
        }),
    )
}

pub fn dispatch_key_up(id: u64, key: &str, code: &str) -> String {
    make_request(
        id,
        "Input.dispatchKeyEvent",
        json!({
            "type": "keyUp",
            "key": key,
            "code": code
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
