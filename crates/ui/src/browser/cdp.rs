#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayElement {
    pub uid: u32,
    pub role: String,
    pub text: String,
    /// Tag HTML (`button`, `a`…) et sélecteur robuste — le badge numéroté
    /// du mode sélection s'en sert pour décrire l'élément au clic.
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub selector: String,
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

/// `{count, index}` renvoyé par `find_in_page_script`.
#[derive(Debug, Deserialize)]
pub struct FindPageResult {
    pub count: usize,
    pub index: i64,
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

/// Active les événements Network (requêtes/réponses/erreurs) pour la page.
/// Léger : pas de corps de requête capturé.
pub fn network_enable(id: u64) -> String {
    make_request(id, "Network.enable", json!({}))
}

/// Téléchargements : comportement « allow » (nom d'origine) + dossier Komet,
/// avec événements de progression. Envoyé sur la session de page — Chrome
/// l'applique au navigateur entier du profil.
pub fn set_download_behavior(id: u64, download_path: &str) -> String {
    make_request(
        id,
        "Browser.setDownloadBehavior",
        json!({
            "behavior": "allow",
            "downloadPath": download_path,
            "eventsEnabled": true
        }),
    )
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

pub fn set_device_metrics(
    id: u64,
    width: u32,
    height: u32,
    device_scale_factor: f32,
    mobile: bool,
) -> String {
    make_request(
        id,
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": device_scale_factor,
            "mobile": mobile
        }),
    )
}

/// User-Agent émulé : `None`/vide désactive l'override (retour au UA du
/// Chrome partagé du pool).
pub fn set_user_agent_override(id: u64, user_agent: Option<&str>) -> String {
    make_request(
        id,
        "Emulation.setUserAgentOverride",
        json!({ "userAgent": user_agent.unwrap_or("") }),
    )
}

/// Active/désactive l'émulation tactile (appuis multiples pour les devices
/// mobiles émulés).
pub fn set_touch_emulation(id: u64, enabled: bool) -> String {
    make_request(
        id,
        "Emulation.setTouchEmulationEnabled",
        json!({
            "enabled": enabled,
            "maxTouchPoints": if enabled { 5 } else { 1 }
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

/// Helper JS partagé par les scripts d'inspection : génère un sélecteur
/// robuste pour `el` — `#id` s'il est unique, puis `[data-testid]`, sinon un
/// chemin `:nth-of-type` limité à 5 niveaux. Injecté tel quel dans les
/// templates via le marqueur `/*__SELECTOR_HELPER__*/`.
const SELECTOR_HELPER_JS: &str = r#"
    const __cssesc = (s) => (window.CSS && CSS.escape ? CSS.escape(String(s)) : String(s));
    const __uniq = (sel) => { try { return document.querySelectorAll(sel).length === 1; } catch (e) { return false; } };
    const __selectorOf = (el) => {
        if (el.id) {
            const s = '#' + __cssesc(el.id);
            if (__uniq(s)) return s;
        }
        const tid = el.getAttribute('data-testid');
        if (tid) {
            const s = '[data-testid="' + String(tid).replace(/(["\\])/g, '\\$1') + '"]';
            if (__uniq(s)) return s;
        }
        const parts = [];
        let cur = el;
        for (let depth = 0; depth < 5 && cur && cur.nodeType === 1; depth++) {
            let part = cur.tagName.toLowerCase();
            const parent = cur.parentElement;
            if (parent) {
                const same = Array.prototype.filter.call(parent.children, c => c.tagName === cur.tagName);
                if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(cur) + 1) + ')';
            }
            parts.unshift(part);
            cur = parent;
            if (cur === document.body || cur === document.documentElement) break;
        }
        return parts.join(' > ');
    };
"#;

const EXTRACT_ELEMENTS_TEMPLATE: &str = r#"
(() => {
/*__SELECTOR_HELPER__*/
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
            tag: el.tagName.toLowerCase(),
            selector: __selectorOf(el),
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

/// JS payload that extracts interactive element bounding boxes and labels for
/// the numbered selection overlay (mode « Add element », touche N).
pub fn extract_elements_script() -> String {
    EXTRACT_ELEMENTS_TEMPLATE.replace("/*__SELECTOR_HELPER__*/", SELECTOR_HELPER_JS)
}

const PROBE_TEMPLATE: &str = r#"
(() => {
    const el = document.elementFromPoint(@X@, @Y@);
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    const tag = el.tagName.toLowerCase();
    const id = el.id ? '#' + (window.CSS && CSS.escape ? CSS.escape(el.id) : el.id) : '';
    return {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        label: tag + id + ' · ' + Math.round(rect.width) + '×' + Math.round(rect.height)
    };
})()
"#;

/// Sonde de survol du mode sélection : rectangle + pastille sous le curseur.
/// `x`/`y` sont des pixels CSS dans la page (viewport).
pub fn probe_at_script(x: f32, y: f32) -> String {
    PROBE_TEMPLATE
        .replace("@X@", &x.to_string())
        .replace("@Y@", &y.to_string())
}

const DESCRIBE_TEMPLATE: &str = r#"
(() => {
/*__SELECTOR_HELPER__*/
    const el = document.elementFromPoint(@X@, @Y@);
    if (!el || el === document.body || el === document.documentElement) return null;
    const tag = el.tagName.toLowerCase();
    const attrs = {};
    for (const name of ['id', 'class', 'name', 'href', 'aria-label', 'data-testid', 'placeholder', 'type']) {
        const v = el.getAttribute(name);
        if (v != null && v !== '') attrs[name] = String(v).slice(0, 200);
    }
    const rect = el.getBoundingClientRect();
    const clone = el.cloneNode(true);
    clone.querySelectorAll('script,style,noscript').forEach(n => n.remove());
    let html = clone.outerHTML || '';
    if (html.length > 1500) html = html.slice(0, 1500);
    const text = (el.innerText || el.value || el.getAttribute('aria-label') || el.title || el.getAttribute('placeholder') || '')
        .replace(/\s+/g, ' ').trim().slice(0, 200);
    return {
        url: location.href,
        title: document.title || '',
        tag: tag,
        role: el.getAttribute('role') || tag,
        text: text,
        selector: __selectorOf(el),
        attributes: attrs,
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        outer_html: html
    };
})()
"#;

/// Description complète d'un élément pour « Add element » : `elementFromPoint`
/// + sélecteur robuste + attributs + `outerHTML` tronqué (scripts/styles
///   retirés). Retourne `null` sur `body`/`html` pour ne pas indexer la page.
pub fn describe_at_script(x: f32, y: f32) -> String {
    DESCRIBE_TEMPLATE
        .replace("@X@", &x.to_string())
        .replace("@Y@", &y.to_string())
        .replace("/*__SELECTOR_HELPER__*/", SELECTOR_HELPER_JS)
}

/// Find in Page sans toucher au DOM : parcours des nœuds texte → `Range`s
/// affichés via l'API CSS Custom Highlight (`CSS.highlights`). `step` 0
/// rescanne, ±1 avance/recule sur la même requête sans rescanner. Le style
/// `::highlight()` vit dans une balise `<style>` retirée au nettoyage.
/// Retourne `{count, index}` (`index` -1 = aucune occurrence).
const FIND_TEMPLATE: &str = r#"
(() => {
    const QUERY = @QUERY@;
    const STEP = @STEP@;
    const ALL = 'komet-find-all';
    const CURRENT = 'komet-find-current';
    const STYLE_ID = 'komet-find-style';
    const state = window.__kometFind || (window.__kometFind = { q: null, ranges: [], index: -1 });
    const unsupported = () => !window.CSS || !CSS.highlights || typeof Highlight !== 'function';

    const cleanup = () => {
        if (!unsupported()) {
            CSS.highlights.delete(ALL);
            CSS.highlights.delete(CURRENT);
        }
        const style = document.getElementById(STYLE_ID);
        if (style) style.remove();
    };
    const paint = () => {
        if (!state.ranges.length || unsupported()) return;
        if (!document.getElementById(STYLE_ID)) {
            const style = document.createElement('style');
            style.id = STYLE_ID;
            style.textContent =
                '::highlight(komet-find-all){background-color:rgba(255,213,79,.35)}' +
                '::highlight(komet-find-current){background-color:rgba(255,145,0,.65)}';
            (document.head || document.documentElement).appendChild(style);
        }
        CSS.highlights.set(ALL, new Highlight(...state.ranges));
        CSS.highlights.set(CURRENT, new Highlight(state.ranges[state.index]));
        const range = state.ranges[state.index];
        const el = range.startContainer.parentElement || range.startContainer;
        if (el && el.scrollIntoView) {
            el.scrollIntoView({ block: 'center', inline: 'nearest' });
        }
    };

    if (unsupported()) return { count: 0, index: -1, unsupported: true };
    if (!QUERY) {
        state.q = null; state.ranges = []; state.index = -1;
        cleanup();
        return { count: 0, index: -1 };
    }
    // Même requête + step ±1 : on avance sans rescanner le DOM.
    if (STEP !== 0 && state.q === QUERY && state.ranges.length) {
        state.index = (state.index + STEP + state.ranges.length) % state.ranges.length;
        paint();
        return { count: state.ranges.length, index: state.index };
    }
    // Nouvelle recherche : re-parcours des nœuds texte.
    cleanup();
    state.q = QUERY; state.ranges = []; state.index = 0;
    const needle = QUERY.toLowerCase();
    const root = document.body || document.documentElement;
    if (!root) return { count: 0, index: -1 };
    const SKIP = /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/;
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
        acceptNode(node) {
            const value = node.nodeValue;
            if (!value) return NodeFilter.FILTER_REJECT;
            const parent = node.parentElement;
            if (parent && SKIP.test(parent.tagName)) return NodeFilter.FILTER_REJECT;
            return value.toLowerCase().includes(needle)
                ? NodeFilter.FILTER_ACCEPT
                : NodeFilter.FILTER_REJECT;
        }
    });
    const LIMIT = 2000;
    let node;
    while ((node = walker.nextNode()) && state.ranges.length < LIMIT) {
        const text = node.nodeValue;
        const lower = text.toLowerCase();
        let i = lower.indexOf(needle);
        while (i !== -1 && state.ranges.length < LIMIT) {
            try {
                const range = document.createRange();
                range.setStart(node, i);
                range.setEnd(node, Math.min(i + QUERY.length, text.length));
                state.ranges.push(range);
            } catch (e) { /* nœud détaché en cours de scan */ }
            i = lower.indexOf(needle, i + 1);
        }
    }
    if (!state.ranges.length) {
        state.index = -1;
        return { count: 0, index: -1 };
    }
    paint();
    return { count: state.ranges.length, index: state.index };
})()
"#;

/// Expression JS pour `Runtime.evaluate` : recherche/surlignage dans la page.
/// `query` vide → nettoie les surlignages ; `step` ±1 navigue les occurrences.
pub fn find_in_page_script(query: &str, step: i32) -> String {
    let query_json = serde_json::to_string(query).unwrap_or_else(|_| "null".into());
    FIND_TEMPLATE
        .replace("@QUERY@", &query_json)
        .replace("@STEP@", &step.to_string())
}

/// Retire surlignages, style injecté et état — appelé à la fermeture de la
/// barre Find in Page.
pub fn find_clear_script() -> String {
    r#"
(() => {
    const state = window.__kometFind;
    if (state) { state.q = null; state.ranges = []; state.index = -1; }
    if (window.CSS && CSS.highlights) {
        CSS.highlights.delete('komet-find-all');
        CSS.highlights.delete('komet-find-current');
    }
    const style = document.getElementById('komet-find-style');
    if (style) style.remove();
    return true;
})()
"#
    .to_string()
}

/// Clear Storage : cookies de tout le navigateur (partagés entre les
/// onglets de ce profil Chrome).
pub fn clear_browser_cookies(id: u64) -> String {
    make_request(id, "Network.clearBrowserCookies", json!({}))
}

/// Clear Storage : cache HTTP/navigateur.
pub fn clear_browser_cache(id: u64) -> String {
    make_request(id, "Network.clearBrowserCache", json!({}))
}

/// Clear Storage : données d'une origine (localStorage, IndexedDB,
/// cookies de l'origine, service workers…) — `storageTypes: "all"`.
pub fn clear_data_for_origin(id: u64, origin: &str) -> String {
    make_request(
        id,
        "Storage.clearDataForOrigin",
        json!({
            "origin": origin,
            "storageTypes": "all"
        }),
    )
}

// Ferme un page target (une surface qui se ferme libère sa cible ; le Chrome
// partagé du pool survit si d'autres surfaces l'utilisent).
pub fn close_target(id: u64, target_id: &str) -> String {
    json!({
        "id": id,
        "method": "Target.closeTarget",
        "params": { "targetId": target_id }
    })
    .to_string()
}

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
    fn builds_clear_storage_requests() {
        let cookies = parsed(&clear_browser_cookies(21));
        assert_eq!(cookies["method"], "Network.clearBrowserCookies");
        assert_eq!(cookies["id"], 21);

        let cache = parsed(&clear_browser_cache(22));
        assert_eq!(cache["method"], "Network.clearBrowserCache");

        let origin = parsed(&clear_data_for_origin(23, "https://example.com"));
        assert_eq!(origin["method"], "Storage.clearDataForOrigin");
        assert_eq!(origin["params"]["origin"], "https://example.com");
        assert_eq!(origin["params"]["storageTypes"], "all");
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

    #[test]
    fn builds_close_target_request() {
        let parsed_value = parsed(&close_target(7, "ABC123"));
        assert_eq!(parsed_value["id"], 7);
        assert_eq!(parsed_value["method"], "Target.closeTarget");
        assert_eq!(parsed_value["params"]["targetId"], "ABC123");
    }

    #[test]
    fn probe_script_interpolates_coordinates() {
        let script = probe_at_script(120.5, 40.0);
        assert!(script.contains("elementFromPoint(120.5, 40)"));
        assert!(script.contains("label:"));
        assert!(script.contains("width:"));
        // Pas de helper de sélecteur dans la sonde (aller simple).
        assert!(!script.contains("__selectorOf"));
    }

    #[test]
    fn describe_script_returns_element_context_shape() {
        let script = describe_at_script(10.0, 20.0);
        assert!(script.contains("elementFromPoint(10, 20)"));
        for key in [
            "url:",
            "title:",
            "tag:",
            "role:",
            "text:",
            "selector:",
            "attributes:",
            "outer_html:",
        ] {
            assert!(script.contains(key), "missing key {key}");
        }
        // Le sélecteur est validé par unicité et borné à 5 niveaux.
        assert!(script.contains("__selectorOf"));
        assert!(script.contains("depth < 5"));
        assert!(script.contains("querySelectorAll(sel).length === 1"));
        // Scripts/styles retirés du outerHTML.
        assert!(script.contains("script,style,noscript"));
    }

    #[test]
    fn find_script_embeds_escaped_query_and_step() {
        let script = find_in_page_script("héllo \"x\"", 1);
        // La requête est injectée en JSON : guillemets et retours échappés.
        assert!(script.contains("const QUERY = \"héllo \\\"x\\\"\";"));
        assert!(script.contains("const STEP = 1;"));
        assert!(script.contains("CSS.highlights"));
        assert!(script.contains("scrollIntoView"));
        // Requête vide → nettoyage (QUERY falsy → branche !QUERY).
        let cleared = find_in_page_script("", 0);
        assert!(cleared.contains("const QUERY = \"\";"));
        assert!(cleared.contains("const STEP = 0;"));
        // Le script de fermeture supprime surlignages et style.
        let clear = find_clear_script();
        assert!(clear.contains("komet-find-all"));
        assert!(clear.contains("komet-find-style"));
    }

    #[test]
    fn extract_script_emits_tag_and_selector() {
        let script = extract_elements_script();
        assert!(script.contains("tag: el.tagName.toLowerCase()"));
        assert!(script.contains("selector: __selectorOf(el)"));
        assert!(script.contains("uid: uid++"));
    }
}
