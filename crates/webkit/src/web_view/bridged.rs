//! `WKWebView` over the bridge: what [`super`] asks of WebKitGTK, asked of
//! the process on the other end instead.

use std::sync::atomic::{AtomicBool, Ordering};

use objc::object::Id;
use serde_json::{Value, json};

use super::*;
use crate::bridge::{Bridge, Pending};
use crate::configuration;

/// What is known of a page the other side holds.
#[derive(Default)]
pub(super) struct Page {
    url: String,
    can_go_back: bool,
    can_go_forward: bool,
    shown: bool,
    sent: Option<Sent>,
}

/// What the other side was last configured from.
#[derive(Clone, PartialEq)]
struct Sent {
    revision: u64,
    custom_user_agent: Option<String>,
    zoom: f64,
    tab_focuses_links: bool,
}

/// The bridge and view id behind a bridged `WKWebView`.
pub(super) fn of(object: Id) -> Option<(&'static Bridge, usize)> {
    let id = with_object(object, |v| v.page.as_ref().map(|_| v.id)).flatten()?;
    Some((crate::bridge::get()?, id))
}

fn with_page<R>(id: usize, body: impl FnOnce(&mut Page) -> R) -> Option<R> {
    with_view(id, |v| v.page.as_mut().map(body)).flatten()
}

pub(super) fn url_of(id: usize) -> String {
    with_page(id, |p| p.url.clone()).unwrap_or_default()
}

pub(super) fn can_go_back(id: usize) -> bool {
    with_page(id, |p| p.can_go_back).unwrap_or_default()
}

pub(super) fn can_go_forward(id: usize) -> bool {
    with_page(id, |p| p.can_go_forward).unwrap_or_default()
}

/// Send the view's scripts, handlers and settings if any of them changed.
pub(super) fn configure(bridge: &Bridge, id: usize) {
    let Some((configuration, custom, zoom, sent)) = with_view(id, |v| {
        (
            v.configuration as Id,
            v.custom_user_agent.clone(),
            v.page_zoom,
            v.page.as_ref().and_then(|p| p.sent.clone()),
        )
    }) else {
        return;
    };
    // SAFETY: the view's retained configuration.
    let controller = unsafe { configuration::controller_of(configuration) };
    // SAFETY: a configuration's preferences.
    let preferences =
        unsafe { configuration::preferences(configuration::preferences_of(configuration)) };
    let developer = preferences.as_ref().is_some_and(|p| p.developer_extras);
    let current = Sent {
        // SAFETY: a content controller.
        revision: unsafe { configuration::revision(controller) },
        custom_user_agent: custom.clone(),
        zoom,
        tab_focuses_links: preferences.as_ref().is_some_and(|p| p.tab_focuses_links),
    };
    if sent.as_ref() == Some(&current) {
        return;
    }
    // SAFETY: as above.
    let agent = custom.unwrap_or_else(|| unsafe { user_agent(configuration) });
    let header = header_user_agent(&agent);
    let mut scripts = Vec::new();
    if header != agent {
        scripts.push(configuration::user_agent_override(&agent));
    }
    // SAFETY: as above.
    scripts.extend(unsafe { configuration::scripts(controller) });
    let scripts: Vec<Value> = scripts
        .into_iter()
        .map(|s| {
            json!({
                "source": s.source,
                "atStart": s.at_document_start,
                "mainOnly": s.main_frame_only,
            })
        })
        .collect();
    bridge.send(json!({
        "op": "configure",
        "view": id,
        "userAgent": header,
        "developerExtras": developer,
        "zoom": current.zoom,
        "tabFocusesLinks": current.tab_focuses_links,
        "scripts": scripts,
        // SAFETY: as above.
        "handlers": unsafe { configuration::handler_names(controller) },
    }));
    with_page(id, |p| p.sent = Some(current));
}

pub(super) fn load(
    bridge: &Bridge,
    receiver: Id,
    id: usize,
    url: &str,
    headers: &[(String, String)],
) -> Id {
    configure(bridge, id);
    let navigation = start_navigation(receiver);
    let headers: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(name, value)| (name.clone(), Value::from(value.as_str())))
        .collect();
    bridge.send(json!({
        "op": "load",
        "view": id,
        "url": host_url(url),
        "headers": headers,
    }));
    navigation
}

pub(super) fn evaluate(bridge: &Bridge, id: usize, script: &str, completion: Option<Block>) {
    configure(bridge, id);
    let message = json!({ "op": "eval", "view": id, "script": script });
    match completion {
        Some(completion) => bridge.request(message, Pending::Script(completion)),
        None => bridge.send(message),
    }
}

/// A command that carries nothing but the view: `back`, `forward` or `stop`.
pub(super) fn command(bridge: &Bridge, id: usize, op: &str) {
    bridge.send(json!({ "op": op, "view": id }));
}

/// A value from a page as the Objective-C object WebKit bridges it to, nil for
/// `null` and `undefined`.
fn object_from(value: &Value) -> Id {
    let number = || objc::class::lookup(c"NSNumber").expect("registered by Foundation");
    match value {
        Value::Null => std::ptr::null_mut(),
        // SAFETY: `+numberWithBool:` is `id (Class, SEL, BOOL)`.
        Value::Bool(flag) => unsafe {
            send!(objc::class::ptr(number()) as Id, "numberWithBool:", *flag => bool; Id)
        },
        // SAFETY: `+numberWithDouble:` is `id (Class, SEL, double)`.
        Value::Number(n) => unsafe {
            send!(objc::class::ptr(number()) as Id, "numberWithDouble:",
                n.as_f64().unwrap_or_default() => f64; Id)
        },
        Value::String(text) => engine::string(text),
        Value::Array(_) | Value::Object(_) => {
            let data = objc::arc::autorelease(corefoundation::collections::new_data(
                value.to_string().into_bytes(),
            ) as Id);
            let class =
                objc::class::lookup(c"NSJSONSerialization").expect("registered by Foundation");
            // SAFETY: `+JSONObjectWithData:options:error:` has this prototype.
            unsafe {
                send!(objc::class::ptr(class) as Id, "JSONObjectWithData:options:error:",
                    data => Id, 0usize => usize, std::ptr::null_mut::<Id>() => *mut Id; Id)
            }
        }
    }
}

/// The reply to an `eval`.
pub(crate) fn script_reply(completion: &Block, reply: &Value) {
    match reply["error"].as_str() {
        Some(message) => {
            // `WKErrorJavaScriptExceptionOccurred`.
            let error = foundation::error::make("WKErrorDomain", 4, message);
            completion.call2(std::ptr::null_mut(), error);
        }
        None => completion.call2(object_from(&reply["value"]), std::ptr::null_mut()),
    }
}

struct RemoteDecision {
    decision: u64,
    taken: AtomicBool,
}

unsafe extern "C" fn decide_remote(block: *mut HostBlock<RemoteDecision>, policy: isize) {
    // SAFETY: the block `remote_handler` made.
    let context = unsafe { &(*block).context };
    if context.taken.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(bridge) = crate::bridge::get() {
        allow(bridge, context.decision, policy == 1);
    }
}

fn remote_handler(decision: u64) -> Id {
    engine::host_block(
        decide_remote as *const std::ffi::c_void,
        RemoteDecision {
            decision,
            taken: AtomicBool::new(false),
        },
    )
}

fn allow(bridge: &Bridge, decision: u64, allowed: bool) {
    bridge.send(json!({ "op": "policy", "decision": decision, "allow": allowed }));
}

fn text<'a>(message: &'a Value, key: &str) -> &'a str {
    message[key].as_str().unwrap_or_default()
}

/// An event about a view.
pub(crate) fn handle(bridge: &Bridge, message: &Value) {
    let id = message["view"].as_u64().unwrap_or_default() as usize;
    let decision = message["decision"].as_u64();
    let Some(view) = delegates(id) else {
        if let Some(decision) = decision {
            allow(bridge, decision, false);
        }
        return;
    };
    if let Some(url) = message["currentUrl"].as_str() {
        with_page(id, |p| {
            p.url = url.to_string();
            p.can_go_back = message["back"].as_bool().unwrap_or_default();
            p.can_go_forward = message["forward"].as_bool().unwrap_or_default();
        });
    }
    match text(message, "event") {
        "navigation" => {
            const SELECTOR: &str = "webView:decidePolicyForNavigationAction:decisionHandler:";
            let Some(decision) = decision else { return };
            if !engine::responds(view.navigation, SELECTOR) {
                allow(bridge, decision, true);
                return;
            }
            let action = remote_action(
                text(message, "url"),
                &url_of(id),
                message["type"].as_i64().unwrap_or(-1),
                message["newWindow"].as_bool().unwrap_or_default(),
            );
            // SAFETY: the delegate answers the selector, whose prototype this is.
            unsafe {
                send!(view.navigation, "webView:decidePolicyForNavigationAction:decisionHandler:",
                    view.object => Id, action => Id, remote_handler(decision) => Id; ())
            };
        }
        "response" => {
            const SELECTOR: &str = "webView:decidePolicyForNavigationResponse:decisionHandler:";
            let Some(decision) = decision else { return };
            if !engine::responds(view.navigation, SELECTOR) {
                allow(bridge, decision, true);
                return;
            }
            let response = foundation::urlsession::http_response(
                &guest_url(text(message, "url")),
                message["status"].as_u64().unwrap_or_default() as u16,
                Vec::new(),
            );
            let object = autoreleased(
                response_class(),
                NavigationResponse {
                    main: message["main"].as_bool().unwrap_or_default(),
                    objects: Owned::new(&[response]),
                },
            );
            // SAFETY: as above.
            unsafe {
                send!(view.navigation, "webView:decidePolicyForNavigationResponse:decisionHandler:",
                    view.object => Id, object => Id, remote_handler(decision) => Id; ())
            };
        }
        "load" => load_event(&view, id, message["stage"].as_i64().unwrap_or(-1) as c_int),
        "failed" => report_load_failure(
            &view,
            id,
            message["provisional"].as_bool().unwrap_or_default(),
            text(message, "domain"),
            message["code"].as_i64().unwrap_or_default() as c_int,
            text(message, "message"),
        ),
        "terminated" => content_process_terminated(&view),
        "new-window" => {
            let action = remote_action(text(message, "url"), &url_of(id), -1, true);
            ask_for_new_window(&view, action);
        }
        "message" => {
            let name = text(message, "name");
            // SAFETY: the view's retained configuration.
            let controller = unsafe { configuration::controller_of(view.configuration) };
            // SAFETY: a content controller.
            let Some(handler) = (unsafe { configuration::handler_named(controller, name) }) else {
                return;
            };
            let object = message_object(
                view.object,
                &url_of(id),
                name,
                object_from(&message["body"]),
            );
            // SAFETY: a handler answers the `WKScriptMessageHandler` method.
            unsafe {
                send!(handler, "userContentController:didReceiveScriptMessage:",
                    controller => Id, object => Id; ())
            };
        }
        "closed" => closed(&view),
        // Only the address and history changed, which is taken above.
        "state" => {}
        other => engine::warn_once(&format!("unknown web view bridge event {other}")),
    }
}

fn remote_action(url: &str, page: &str, kind: i64, new_window: bool) -> Id {
    let url = guest_url(url);
    let source_url = match page.is_empty() || page == "about:blank" {
        true => url.clone(),
        false => guest_url(page),
    };
    let request = foundation::urlsession::request_for(&url);
    let mut objects = vec![request, frame_info(&source_url, true)];
    if !new_window {
        objects.push(frame_info(&url, true));
    }
    autoreleased(
        action_class(),
        NavigationAction {
            navigation_type: navigation_type(kind),
            objects: Owned::new(&objects),
        },
    )
}

/// The person closed the page's window, which on a Mac they could only do
/// through the program's own control. `-webViewDidClose:` is how WebKit says a
/// page is finished; Roblox's controller answers `-closeButtonAction:`.
fn closed(view: &Delegates) {
    if engine::responds(view.ui, "webViewDidClose:") {
        // SAFETY: the delegate answers the selector.
        unsafe { send!(view.ui, "webViewDidClose:", view.object => Id; ()) };
        return;
    }
    for delegate in [view.navigation, view.ui] {
        if engine::responds(delegate, "closeButtonAction:") {
            // SAFETY: an action method, `void (id, SEL, id)`.
            unsafe { send!(delegate, "closeButtonAction:", std::ptr::null_mut() => Id; ()) };
            return;
        }
    }
    engine::warn_once("the program has no way to be told its web view's window was closed");
}

/// Tell the other side which pages are on screen, and of configuration changes
/// made since the last load.
pub(crate) fn tick(bridge: &Bridge) {
    let views: Vec<(usize, usize, bool, bool)> = VIEWS.with(|views| {
        views
            .borrow()
            .iter()
            .filter_map(|v| {
                let page = v.page.as_ref()?;
                Some((v.id, v.object, page.shown, page.sent.is_some()))
            })
            .collect()
    });
    for (id, object, was_shown, loaded) in views {
        let object = object as Id;
        // SAFETY: a live view, registered while it is.
        let window = unsafe { appkit::view::window_of(object) };
        // SAFETY: as above.
        let on_screen = unsafe { appkit::view::presentation_frame(object) }.is_some();
        let shown = on_screen
            && !appkit::view::is_hidden_or_has_hidden_ancestor(object)
            && appkit::window::is_shown(window);
        if loaded {
            configure(bridge, id);
        }
        if shown != was_shown {
            with_page(id, |p| p.shown = shown);
            bridge.send(json!({ "op": "visible", "view": id, "visible": shown }));
        }
    }
}
