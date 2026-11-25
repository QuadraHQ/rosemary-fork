//! Web views shown by another process, named by `ROSEMARY_WEBKIT_BRIDGE`.
//!
//! The two sides exchange one JSON object per line over a Unix socket. This
//! module is the transport; what the messages mean to a view or a cookie store
//! lives beside the code it stands in for.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::engine::{self, Block};
use crate::{data_store, web_view};

const SOCKET_VARIABLE: &str = "ROSEMARY_WEBKIT_BRIDGE";
/// The other process is started just before this one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_RETRY: Duration = Duration::from_millis(50);

pub struct Bridge {
    outgoing: Sender<String>,
    incoming: Arc<Mutex<VecDeque<String>>>,
    closed: Arc<AtomicBool>,
    thread: u32,
}

/// A guest completion handler waiting for the other side's reply.
pub enum Pending {
    /// `void (^)(id, NSError *)`.
    Script(Block),
    /// `void (^)(NSArray *)`.
    Cookies(Block),
    /// `void (^)(void)`.
    CookieChange(Block),
}

thread_local! {
    static PENDING: RefCell<HashMap<u64, Pending>> = RefCell::new(HashMap::new());
}

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();

/// The connection, made on first use; `None` when no bridge was asked for or
/// it could not be reached, and on any thread but the one that made it.
pub fn get() -> Option<&'static Bridge> {
    let bridge = BRIDGE.get_or_init(connect).as_ref()?;
    (bridge.thread == darwin::pthread::id()).then_some(bridge)
}

fn connect() -> Option<Bridge> {
    let path = std::env::var_os(SOCKET_VARIABLE).filter(|path| !path.is_empty())?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let stream = loop {
        match UnixStream::connect(&path) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => {
                eprintln!(
                    "rosemary: webkit: {SOCKET_VARIABLE} is unreachable ({error}); \
                     web views are shown in the program's window"
                );
                return None;
            }
            Err(_) => std::thread::sleep(CONNECT_RETRY),
        }
    };
    match start(stream) {
        Ok(bridge) => {
            eprintln!("rosemary: webkit: web views are shown by the bridge");
            appkit::event::add_pump_hook(pump);
            Some(bridge)
        }
        Err(error) => {
            eprintln!("rosemary: webkit: cannot start the bridge: {error}");
            None
        }
    }
}

fn start(stream: UnixStream) -> std::io::Result<Bridge> {
    let incoming = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let (outgoing, queued) = channel::<String>();
    let mut writer = stream.try_clone()?;
    let write_closed = closed.clone();
    std::thread::Builder::new()
        .name("rosemary-webkit-bridge-write".to_string())
        .spawn(move || {
            for line in queued {
                if writer.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            write_closed.store(true, Ordering::Release);
            corefoundation::runloop::main().wake();
        })?;
    let (queue, read_closed) = (incoming.clone(), closed.clone());
    std::thread::Builder::new()
        .name("rosemary-webkit-bridge-read".to_string())
        .spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(line);
                corefoundation::runloop::main().wake();
            }
            read_closed.store(true, Ordering::Release);
            corefoundation::runloop::main().wake();
        })?;
    Ok(Bridge {
        outgoing,
        incoming,
        closed,
        thread: darwin::pthread::id(),
    })
}

impl Bridge {
    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn send(&self, message: Value) {
        if self.is_open() {
            // A closed channel is a closed bridge, which the pump reports.
            let _ = self.outgoing.send(format!("{message}\n"));
        }
    }

    /// Send `message` with a request number, and keep `pending` for its reply.
    pub fn request(&self, mut message: Value, pending: Pending) {
        if !self.is_open() {
            engine::defer(move || fail(pending));
            return;
        }
        let number = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        message["request"] = json!(number);
        PENDING.with(|all| all.borrow_mut().insert(number, pending));
        self.send(message);
    }

    fn next(&self) -> Option<String> {
        self.incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}

fn fail(pending: Pending) {
    answer(
        pending,
        &json!({ "error": "the web view bridge is closed" }),
    );
}

fn answer(pending: Pending, reply: &Value) {
    match pending {
        Pending::Script(completion) => web_view::bridged::script_reply(&completion, reply),
        Pending::Cookies(completion) => data_store::cookies_reply(&completion, reply),
        Pending::CookieChange(completion) => {
            if let Some(error) = reply["error"].as_str() {
                engine::warn_once(&format!("changing a cookie failed: {error}"));
            }
            completion.call0();
        }
    }
}

fn pump() {
    let Some(bridge) = get() else {
        return;
    };
    let pool = objc::arc::pool_push();
    // One at a time: a delegate called from here may pump the run loop again.
    while let Some(line) = bridge.next() {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            engine::warn_once("the web view bridge sent a line that is not JSON");
            continue;
        };
        if message["event"] == "reply" {
            let pending = message["request"]
                .as_u64()
                .and_then(|number| PENDING.with(|all| all.borrow_mut().remove(&number)));
            if let Some(pending) = pending {
                answer(pending, &message);
            }
        } else {
            web_view::bridged::handle(bridge, &message);
        }
    }
    if !bridge.is_open() {
        let abandoned: Vec<Pending> =
            PENDING.with(|all| all.borrow_mut().drain().map(|(_, p)| p).collect());
        if !abandoned.is_empty() {
            engine::warn_once("the web view bridge closed with requests unanswered");
        }
        abandoned.into_iter().for_each(fail);
    }
    engine::run_deferred();
    web_view::bridged::tick(bridge);
    objc::arc::pool_pop(pool);
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use foundation::geometry::{NSPoint, NSRect, NSSize};
    use objc::object::Id;
    use objc::sel::Sel;

    use super::*;
    use crate::send;

    thread_local! {
        static SEEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn see(what: String) {
        SEEN.with(|seen| seen.borrow_mut().push(what));
    }

    fn seen(prefix: &str) -> Option<String> {
        SEEN.with(|seen| {
            seen.borrow()
                .iter()
                .find(|s| s.starts_with(prefix))
                .cloned()
        })
    }

    fn wait_for(prefix: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            pump();
            if let Some(found) = seen(prefix) {
                SEEN.with(|seen| seen.borrow_mut().retain(|s| *s != found));
                return found;
            }
            assert!(Instant::now() < deadline, "never saw {prefix}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn description(object: Id) -> String {
        if object.is_null() {
            return "nil".to_string();
        }
        // SAFETY: a live object; `-description` is `id (id, SEL)`.
        engine::text_of(unsafe { send!(object, "description"; Id) })
    }

    unsafe extern "C" fn decide(_: Id, _: Sel, _: Id, action: Id, handler: Id) {
        // SAFETY: an action and its request.
        let url = unsafe { send!(send!(action, "request"; Id), "URL"; Id) };
        see(format!("decide {}", description(url)));
        Block::copy(handler).unwrap().call1(1usize as Id);
    }

    unsafe extern "C" fn finished(_: Id, _: Sel, view: Id, _: Id) {
        // SAFETY: a web view.
        let url = unsafe { send!(view, "URL"; Id) };
        see(format!("finished {}", description(url)));
    }

    unsafe extern "C" fn failed(_: Id, _: Sel, _: Id, _: Id, error: Id) {
        see(format!("failed {}", description(error)));
    }

    unsafe extern "C" fn message(_: Id, _: Sel, _: Id, message: Id) {
        // SAFETY: a script message with a dictionary body.
        unsafe {
            let body = send!(message, "body"; Id);
            let sum = send!(body, "objectForKey:", engine::string("sum") => Id; Id);
            let agent = send!(body, "objectForKey:", engine::string("agent") => Id; Id);
            see(format!(
                "message {} {}",
                description(sum),
                description(agent)
            ));
        }
    }

    unsafe extern "C" fn evaluated(_: *mut engine::HostBlock<()>, value: Id, error: Id) {
        match error.is_null() {
            true => see(format!("value {}", description(value))),
            false => see("error".to_string()),
        }
    }

    unsafe extern "C" fn listed(_: *mut engine::HostBlock<()>, cookies: Id) {
        // SAFETY: an array of cookies.
        unsafe {
            let count = send!(cookies, "count"; usize);
            let all: Vec<String> = (0..count)
                .map(|at| {
                    let cookie = send!(cookies, "objectAtIndex:", at => usize; Id);
                    format!(
                        "{}={}",
                        engine::text_of(send!(cookie, "name"; Id)),
                        engine::text_of(send!(cookie, "value"; Id))
                    )
                })
                .collect();
            see(format!("cookies {}", all.join(";")));
        }
    }

    unsafe extern "C" fn changed(_: *mut engine::HostBlock<()>) {
        see("changed".to_string());
    }

    fn serve() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request);
                let page = "<title>Bridge</title><h1>Quadra web window test</h1><script>\
                    window.webkit.messageHandlers.probe.postMessage(\
                    {sum: injected + 1, agent: navigator.userAgent})</script>";
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Set-Cookie: served=yes; Path=/\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{page}",
                    page.len()
                );
            }
        });
        address
    }

    #[test]
    #[ignore = "requires a running web window named by ROSEMARY_WEBKIT_BRIDGE; run alone"]
    fn a_page_in_the_bridge_answers_as_an_embedded_one_does() {
        crate::bootstrap();
        get().expect("the bridge must be reachable");
        let delegate_class = corefoundation::base::abstract_class(c"BridgeTestDelegate", None);
        crate::methods!(delegate_class, {
            "webView:decidePolicyForNavigationAction:decisionHandler:" => decide, c"v40@0:8@16@24@?32";
            "webView:didFinishNavigation:" => finished, c"v32@0:8@16@24";
            "webView:didFailProvisionalNavigation:withError:" => failed, c"v40@0:8@16@24@32";
            "userContentController:didReceiveScriptMessage:" => message, c"v32@0:8@16@24";
        });
        let delegate = objc::object::create_instance(delegate_class, 0);
        let address = serve();
        let new = |name: &std::ffi::CStr| {
            let class = objc::class::lookup(name).unwrap();
            // SAFETY: `+new` is `id (Class, SEL)`.
            unsafe { send!(objc::class::ptr(class) as Id, "new"; Id) }
        };
        // SAFETY: each message is sent with its method's prototype.
        unsafe {
            let configuration = new(c"WKWebViewConfiguration");
            let controller = send!(configuration, "userContentController"; Id);
            send!(controller, "addScriptMessageHandler:name:",
                delegate => Id, engine::string("probe") => Id; ());
            let script =
                objc::object::create_instance(objc::class::lookup(c"WKUserScript").unwrap(), 0);
            let script = send!(script, "initWithSource:injectionTime:forMainFrameOnly:",
                engine::string("window.injected = 41;") => Id, 0isize => isize, 1u8 => u8; Id);
            send!(controller, "addUserScript:", script => Id; ());
            let frame = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: 640.0,
                    height: 480.0,
                },
            };
            let view = objc::object::create_instance(objc::class::lookup(c"WKWebView").unwrap(), 0);
            let view = send!(view, "initWithFrame:configuration:",
                frame => NSRect, configuration => Id; Id);
            send!(view, "setNavigationDelegate:", delegate => Id; ());
            let request = foundation::urlsession::request_for(&address);
            assert!(!send!(view, "loadRequest:", request => Id; Id).is_null());
            assert_eq!(wait_for("decide"), format!("decide {address}"));
            let posted = wait_for("message");
            assert!(
                posted.starts_with("message 42 Mozilla/5.0 (Macintosh"),
                "{posted}"
            );
            assert_eq!(wait_for("finished"), format!("finished {address}"));

            let handler = engine::host_block(evaluated as *const std::ffi::c_void, ());
            let evaluate = |source: &str| {
                send!(view, "evaluateJavaScript:completionHandler:",
                    engine::string(source) => Id, handler => Id; ());
            };
            evaluate("document.title");
            assert_eq!(wait_for("value"), "value Bridge");
            evaluate("6 * 7");
            assert_eq!(wait_for("value"), "value 42");
            evaluate("null");
            assert_eq!(wait_for("value"), "value nil");
            evaluate("throw new Error('expected')");
            wait_for("error");

            let store = send!(send!(configuration, "websiteDataStore"; Id), "httpCookieStore"; Id);
            let list = engine::host_block(listed as *const std::ffi::c_void, ());
            send!(store, "getAllCookies:", list => Id; ());
            let cookies = wait_for("cookies");
            assert!(
                cookies.contains("served=yes"),
                "the served cookie is missing"
            );

            let properties = new(c"NSMutableDictionary");
            for (key, value) in [("Name", "added"), ("Value", "1"), ("Domain", "example.com")] {
                send!(properties, "setObject:forKey:",
                    engine::string(value) => Id, engine::string(key) => Id; ());
            }
            let cookie_class = objc::class::lookup(c"NSHTTPCookie").unwrap();
            let cookie = send!(objc::class::ptr(cookie_class) as Id, "cookieWithProperties:",
                properties => Id; Id);
            let done = engine::host_block(changed as *const std::ffi::c_void, ());
            send!(store, "setCookie:completionHandler:", cookie => Id, done => Id; ());
            wait_for("changed");
            send!(store, "getAllCookies:", list => Id; ());
            assert!(wait_for("cookies").contains("added=1"));
            send!(store, "deleteCookie:completionHandler:", cookie => Id, done => Id; ());
            wait_for("changed");
            send!(store, "getAllCookies:", list => Id; ());
            assert!(!wait_for("cookies").contains("added=1"));

            let gone = foundation::urlsession::request_for("http://127.0.0.1:1/");
            send!(view, "loadRequest:", gone => Id; Id);
            wait_for("failed");
        }
    }
}
