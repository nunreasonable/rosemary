use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use corefoundation::base::CFTypeRef;

use crate::block;
use crate::boxed::{Body, contents, make, register};
use crate::define::{class_methods, methods};

/// How long a request waits when nothing said otherwise. Apple's default.
const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;

//

/// What a request carries, independent of any object holding it.
#[derive(Clone, Default)]
struct Request {
    url: String,
    method: String,
    /// In the order they were set, because a header may legitimately repeat.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: f64,
    cache_policy: usize,
}

impl Request {
    fn new(url: String, timeout: f64) -> Request {
        Request {
            url,
            method: "GET".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout,
            cache_policy: 0,
        }
    }
}

/// What came back.
struct Response {
    url: String,
    status: u16,
    headers: Vec<(String, String)>,
}

//

fn request_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        // The mutable class descends from the immutable one, which is what a
        // guest's `isKindOfClass:` asks about either way.
        let base = register::<Request>(c"NSURLRequest", None);
        let mutable = corefoundation::base::register(
            c"NSMutableURLRequest",
            Some(base),
            size_of::<Body<Request>>(),
            false,
        );
        objc::class::ptr(mutable) as usize
    });
    // SAFETY: the cell holds a class this function registered.
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("a class") }
}

fn immutable_request_class() -> &'static Class {
    request_class();
    objc::class::lookup(c"NSURLRequest").expect("registered above")
}

/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
unsafe fn request_of<'a>(receiver: Id) -> Option<&'a mut Request> {
    unsafe { contents(receiver, immutable_request_class()) }
}

/// `+requestWithURL:`, and the two-and-three-argument forms.
///
/// Always a *mutable* request object, whichever class was asked: a guest that
/// asked `NSURLRequest` for one gets something it may not mutate by the type
/// system's account, and one that asked `NSMutableURLRequest` gets what it
/// needs. The distinction Apple enforces at runtime is not one this runtime has
/// a reason to reproduce, and getting it wrong the other way would refuse a
/// `-setHTTPMethod:` the guest is entitled to send.
unsafe extern "C" fn request_with_url(_class: Id, _sel: Sel, url: Id) -> Id {
    autoreleased_request(url, DEFAULT_TIMEOUT_SECONDS)
}

unsafe extern "C" fn request_with_url_policy_timeout(
    _class: Id,
    _sel: Sel,
    url: Id,
    _policy: usize,
    timeout: f64,
) -> Id {
    // The cache policy is ignored because nothing here caches: every request
    // goes to the network, which is what `NSURLRequestReloadIgnoringCacheData`
    // asks for and what the others get anyway.
    autoreleased_request(url, timeout)
}

fn autoreleased_request(url: Id, timeout: f64) -> Id {
    // SAFETY: an `NSURL`, or nil.
    let Some(text) = (unsafe { crate::url::text_of(url) }) else {
        return std::ptr::null_mut();
    };
    let timeout = match timeout > 0.0 {
        true => timeout,
        false => DEFAULT_TIMEOUT_SECONDS,
    };
    objc::arc::autorelease(make(request_class(), Request::new(text, timeout)))
}

/// A new autoreleased request for `url`, for a framework handing a guest the
/// request something else is making.
pub fn request_for(url: &str) -> Id {
    objc::arc::autorelease(make(
        request_class(),
        Request::new(url.to_string(), DEFAULT_TIMEOUT_SECONDS),
    ))
}

/// What a framework that performs a guest's request elsewhere needs of it.
pub struct RequestParts {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
}

/// A request's URL, method and headers.
///
/// # Safety
///
/// `request` must be null or an object this runtime allocated.
pub unsafe fn request_parts(request: Id) -> Option<RequestParts> {
    let request = unsafe { request_of(request) }?;
    Some(RequestParts {
        url: request.url.clone(),
        method: request.method.clone(),
        headers: request.headers.clone(),
    })
}

/// A new autoreleased `NSHTTPURLResponse`, for a framework whose own loader
/// received it.
pub fn http_response(url: &str, status: u16, headers: Vec<(String, String)>) -> Id {
    objc::arc::autorelease(make(
        response_class(),
        Response {
            url: url.to_string(),
            status,
            headers,
        },
    ))
}

unsafe extern "C" fn set_url(receiver: Id, _sel: Sel, url: Id) {
    // SAFETY: an `NSURL`, or nil.
    let Some(text) = (unsafe { crate::url::text_of(url) }) else {
        return;
    };
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) } {
        request.url = text;
    }
}

unsafe extern "C" fn request_url(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a request this module made.
    let Some(request) = (unsafe { request_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a URL this call just made.
    unsafe { corefoundation::api::CFAutorelease(corefoundation::url::new(&request.url)) as Id }
}

unsafe extern "C" fn set_http_method(receiver: Id, _sel: Sel, method: Id) {
    // SAFETY: an `NSString`, or nil.
    let method = unsafe { text(method) };
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) } {
        request.method = method.to_ascii_uppercase();
    }
}

unsafe extern "C" fn http_method(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a request this module made.
    match unsafe { request_of(receiver) } {
        Some(request) => autoreleased_string(&request.method),
        None => std::ptr::null_mut(),
    }
}

/// `-setValue:forHTTPHeaderField:`, which *replaces* every value already set for
/// the field - unlike `-addValue:forHTTPHeaderField:`, which appends.
unsafe extern "C" fn set_value_for_header(receiver: Id, _sel: Sel, value: Id, field: Id) {
    // SAFETY: both are `NSString`s, or nil.
    let (value, field) = unsafe { (text(value), text(field)) };
    // SAFETY: the receiver is a request this module made.
    let Some(request) = (unsafe { request_of(receiver) }) else {
        return;
    };
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case(&field));
    if !field.is_empty() {
        request.headers.push((field, value));
    }
}

unsafe extern "C" fn add_value_for_header(receiver: Id, _sel: Sel, value: Id, field: Id) {
    // SAFETY: both are `NSString`s, or nil.
    let (value, field) = unsafe { (text(value), text(field)) };
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) }
        && !field.is_empty()
    {
        request.headers.push((field, value));
    }
}

unsafe extern "C" fn value_for_header(receiver: Id, _sel: Sel, field: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let field = unsafe { text(field) };
    // SAFETY: the receiver is a request this module made.
    let Some(request) = (unsafe { request_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    match request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&field))
    {
        Some((_, value)) => autoreleased_string(value),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn request_headers(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a request this module made.
    let Some(request) = (unsafe { request_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    headers_dictionary(&request.headers)
}

unsafe extern "C" fn set_http_body(receiver: Id, _sel: Sel, data: Id) {
    // SAFETY: an `NSData`, or nil.
    let bytes = unsafe { corefoundation::collections::data(data as CFTypeRef) }
        .map(|d| d.clone())
        .unwrap_or_default();
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) } {
        request.body = bytes;
    }
}

unsafe extern "C" fn http_body(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a request this module made.
    let Some(request) = (unsafe { request_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    if request.body.is_empty() {
        return std::ptr::null_mut();
    }
    let data = corefoundation::collections::new_data(request.body.clone());
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(data) as Id }
}

unsafe extern "C" fn set_timeout_interval(receiver: Id, _sel: Sel, timeout: f64) {
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) }
        && timeout > 0.0
    {
        request.timeout = timeout;
    }
}

unsafe extern "C" fn set_cache_policy(receiver: Id, _sel: Sel, policy: usize) {
    // SAFETY: the receiver is a request this module made.
    if let Some(request) = unsafe { request_of(receiver) } {
        request.cache_policy = policy;
    }
}

unsafe extern "C" fn cache_policy(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: the receiver is a request this module made.
    unsafe { request_of(receiver) }.map_or(0, |request| request.cache_policy)
}

unsafe extern "C" fn timeout_interval(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: the receiver is a request this module made.
    unsafe { request_of(receiver) }.map_or(DEFAULT_TIMEOUT_SECONDS, |r| r.timeout)
}

//

fn response_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let base = register::<Response>(c"NSURLResponse", None);
        let http = corefoundation::base::register(
            c"NSHTTPURLResponse",
            Some(base),
            size_of::<Body<Response>>(),
            false,
        );
        objc::class::ptr(http) as usize
    });
    // SAFETY: the cell holds a class this function registered.
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("a class") }
}

/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
unsafe fn response_of<'a>(receiver: Id) -> Option<&'a mut Response> {
    let base = objc::class::lookup(c"NSURLResponse").expect("registered by `response_class`");
    unsafe { contents(receiver, base) }
}

unsafe extern "C" fn status_code(receiver: Id, _sel: Sel) -> i64 {
    // SAFETY: the receiver is a response this module made.
    unsafe { response_of(receiver) }.map_or(0, |r| r.status as i64)
}

/// `-allHeaderFields`, keyed by the header name as it arrived.
unsafe extern "C" fn all_header_fields(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a response this module made.
    let Some(response) = (unsafe { response_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    headers_dictionary(&response.headers)
}

fn headers_dictionary(headers: &[(String, String)]) -> Id {
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = headers
        .iter()
        .map(|(name, value)| {
            (
                crate::string::from_str(name) as CFTypeRef,
                crate::string::from_str(value) as CFTypeRef,
            )
        })
        .collect();
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: the dictionary retained each; this call owns the ones it made.
    unsafe {
        for (name, value) in pairs {
            corefoundation::api::CFRelease(name);
            corefoundation::api::CFRelease(value);
        }
        corefoundation::api::CFAutorelease(dictionary) as Id
    }
}

unsafe extern "C" fn response_url(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a response this module made.
    let Some(response) = (unsafe { response_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a URL this call just made.
    unsafe { corefoundation::api::CFAutorelease(corefoundation::url::new(&response.url)) as Id }
}

unsafe extern "C" fn mime_type(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a response this module made.
    let Some(response) = (unsafe { response_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    let content_type = response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    // The MIME type is the media type alone; the charset and any other
    // parameter belong to `-textEncodingName`, which this does not answer.
    match content_type {
        Some((_, value)) => autoreleased_string(value.split(';').next().unwrap_or("").trim()),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn text_encoding_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a response this module made.
    let Some(response) = (unsafe { response_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, value)| {
            value.split(';').skip(1).find_map(|part| {
                let (name, value) = part.trim().split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("charset")
                    .then(|| autoreleased_string(value.trim().trim_matches('"')))
            })
        })
        .unwrap_or(std::ptr::null_mut())
}

/// `-expectedContentLength`, or `NSURLResponseUnknownLength` when the response
/// did not say - which is every chunked one.
unsafe extern "C" fn expected_content_length(receiver: Id, _sel: Sel) -> i64 {
    // SAFETY: the receiver is a response this module made.
    let Some(response) = (unsafe { response_of(receiver) }) else {
        return -1;
    };
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .unwrap_or(-1)
}

//

/// The one shared session, and the connection pool behind it.
///
/// An `Agent` rather than a call per request, because it is what keeps
/// connections alive between them and holds the cookie jar - which is the
/// behaviour `+sharedSession` is documented to have.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(concat!("rosemary/", env!("CARGO_PKG_VERSION")))
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// Perform `request`, returning the response and its body or a message.
fn perform(request: &Request) -> Result<(Response, Vec<u8>), String> {
    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(request.url.as_str());
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let timeout = std::time::Duration::from_secs_f64(request.timeout);
    let built = builder
        .body(())
        .map_err(|e| format!("the request could not be built: {e}"))?;
    let (mut parts, ()) = built.into_parts();
    parts.extensions.insert(
        ureq::config::Config::builder()
            .timeout_global(Some(timeout))
            .build(),
    );
    // A body is sent for every method that has one; `SendBody::none()` is what
    // makes a GET a GET rather than a POST with nothing in it.
    let body = match request.body.is_empty() {
        true => ureq::SendBody::none(),
        false => ureq::SendBody::from_owned_reader(std::io::Cursor::new(request.body.clone())),
    };
    let mut response = agent()
        .run(ureq::http::Request::from_parts(parts, body))
        .map_err(|e| e.to_string())?;

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    // No cap on the body: a caller asked for the whole resource, and a limit
    // here would hand it a truncated one it had no way to notice.
    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::MAX)
        .read_to_vec()
        .map_err(|e| format!("the response body could not be read: {e}"))?;
    crate::urlsession_trace::response(&request.url, status, &bytes);
    Ok((
        Response {
            url: request.url.clone(),
            status,
            headers,
        },
        bytes,
    ))
}

/// The three objects a completion handler is passed.
fn objects(result: Result<(Response, Vec<u8>), String>) -> (Id, Id, Id) {
    match result {
        Ok((response, bytes)) => {
            let data = corefoundation::collections::new_data(bytes);
            // SAFETY: two fresh objects, handed on autoreleased as Apple's are.
            let data = unsafe { corefoundation::api::CFAutorelease(data) as Id };
            let response = objc::arc::autorelease(make(response_class(), response));
            (data, response, std::ptr::null_mut())
        }
        Err(message) => (
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            crate::error::make(
                crate::error::NSURL_ERROR_DOMAIN,
                crate::error::NSURL_ERROR_UNKNOWN,
                &message,
            ),
        ),
    }
}

/// Call a `void (^)(NSData *, NSURLResponse *, NSError *)` block.
///
/// # Safety
///
/// `block` must be null or a block literal, whose `invoke` has that prototype.
unsafe fn call_completion(block: Id, data: Id, response: Id, error: Id) {
    if block.is_null() {
        return;
    }
    // SAFETY: a block literal, whose `invoke` is the
    // third word. Under the emulated backend the pointer found there is already
    // a thunk, because the method's `@?` encoding had `rosemary::shims::block`
    // rewrite it on the way in.
    unsafe {
        let invoke = *((block as *const u8).add(block::INVOKE_OFFSET) as *const usize);
        if invoke == 0 {
            return;
        }
        let f: unsafe extern "C" fn(Id, Id, Id, Id) = std::mem::transmute(invoke);
        f(block, data, response, error);
    }
}

//

/// A task belongs to its session until its final callback has returned.
struct Task {
    request: Request,
    completion: usize,
    session: std::sync::atomic::AtomicUsize,
    owner: std::sync::Arc<SessionState>,
    download: bool,
    description: std::sync::Mutex<usize>,
    response: std::sync::atomic::AtomicUsize,
    started: std::sync::atomic::AtomicBool,
    /// NSURLSessionTaskState: 0 running, 1 suspended, 2 canceling, 3 completed.
    state: std::sync::atomic::AtomicUsize,
    completion_claimed: std::sync::atomic::AtomicBool,
}

impl Drop for Task {
    fn drop(&mut self) {
        objc::arc::release(
            *self
                .description
                .get_mut()
                .unwrap_or_else(|e| e.into_inner()) as Id,
        );
        objc::arc::release(self.response.load(Ordering::Relaxed) as Id);
        objc::block::release(self.completion as Id);
        objc::arc::release(self.session.load(Ordering::Relaxed) as Id);
    }
}

// Only immutable data and synchronized ownership cross the worker boundary.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

fn task_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let base = corefoundation::base::abstract_class(c"NSURLSessionTask", None);
        objc::class::ptr(register::<Task>(c"NSURLSessionDataTask", Some(base))) as usize
    });
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("registered") }
}

fn download_task_class() -> &'static Class {
    corefoundation::base::register(
        c"NSURLSessionDownloadTask",
        Some(task_class()),
        size_of::<Body<Task>>(),
        false,
    )
}

/// Borrow shared state without manufacturing aliased mutable references.
unsafe fn shared_contents<'a, T>(object: Id, class: &'static Class) -> Option<&'a T> {
    if !unsafe { corefoundation::base::is_kind_of(object as CFTypeRef, class) } {
        return None;
    }
    unsafe { (*(object as *const Body<T>)).contents.as_ref() }
}

unsafe fn task_of<'a>(object: Id) -> Option<&'a Task> {
    unsafe { shared_contents(object, task_class()) }
}

fn enqueue(queue: usize, work: impl FnOnce() + Send + 'static) {
    let op = crate::operation::with_work(work);
    crate::operation::add_to_queue(queue as Id, op);
    objc::arc::release(op);
}

unsafe extern "C" fn resume(receiver: Id, _sel: Sel) {
    let Some(task) = (unsafe { task_of(receiver) }) else {
        return;
    };
    if task.completion_claimed.load(Ordering::Acquire) || task.started.swap(true, Ordering::AcqRel)
    {
        return;
    }
    if task
        .state
        .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let request = task.request.clone();
    let task_id = objc::arc::retain(receiver) as usize;
    // The synchronous transport can finish after cancellation. Its result is
    // discarded if cancellation already queued the one terminal callback.
    let spawned = std::thread::Builder::new()
        .name("urlsession".into())
        .spawn(move || {
            let result = perform(&request);
            complete_task(task_id, Some(result));
            objc::arc::release(task_id as Id);
        });
    if spawned.is_err() {
        complete_task(task_id, Some(Err("could not start network worker".into())));
        objc::arc::release(task_id as Id);
    }
}

unsafe extern "C" fn cancel(receiver: Id, _sel: Sel) {
    complete_task(receiver as usize, None);
}

/// Claim the final callback once, including cancellation before first resume.
fn complete_task(id: usize, result: Option<Result<(Response, Vec<u8>), String>>) {
    let Some(task) = (unsafe { task_of(id as Id) }) else {
        return;
    };
    if task.completion_claimed.swap(true, Ordering::AcqRel) {
        return;
    }
    if result.is_none() {
        task.state.store(2, Ordering::Release);
    }
    objc::arc::retain(id as Id);
    enqueue(task.owner.queue, move || {
        let pool = objc::arc::pool_push();
        let task = unsafe { task_of(id as Id) }.expect("retained task");
        let session = task.session.load(Ordering::Acquire);
        let mut temporary = None;
        let (data, response, error) = match result {
            None => (
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                crate::error::make(crate::error::NSURL_ERROR_DOMAIN, -999, "cancelled"),
            ),
            Some(Err(message)) => objects(Err(message)),
            Some(Ok((response, bytes))) => {
                let response = objc::arc::autorelease(make(response_class(), response));
                if task.download {
                    static NEXT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let path = format!(
                        "/tmp/rosemary-download-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    );
                    match std::fs::write(prefix::ns::host_path(&path), bytes) {
                        Ok(()) => {
                            let url = objc::arc::autorelease(corefoundation::url::new_file_url(
                                &path, false,
                            ) as Id);
                            temporary = Some(path);
                            (url, response, std::ptr::null_mut())
                        }
                        Err(e) => (
                            std::ptr::null_mut(),
                            response,
                            crate::error::make(
                                crate::error::NSURL_ERROR_DOMAIN,
                                -1,
                                &e.to_string(),
                            ),
                        ),
                    }
                } else {
                    let data = corefoundation::collections::new_data(bytes);
                    (
                        unsafe { corefoundation::api::CFAutorelease(data) as Id },
                        response,
                        std::ptr::null_mut(),
                    )
                }
            }
        };
        task.response
            .store(objc::arc::retain(response) as usize, Ordering::Release);
        task.state.store(3, Ordering::Release);
        if task.completion != 0 {
            unsafe { call_completion(task.completion as Id, data, response, error) };
        } else {
            let delegate = task.owner.delegate.load(Ordering::Acquire) as Id;
            if task.download && error.is_null() {
                send_optional(
                    delegate,
                    "URLSession:downloadTask:didFinishDownloadingToURL:",
                    session as Id,
                    id as Id,
                    data,
                );
            }
            send_optional(
                delegate,
                "URLSession:task:didCompleteWithError:",
                session as Id,
                id as Id,
                error,
            );
        }
        if let Some(path) = temporary {
            let _ = std::fs::remove_file(prefix::ns::host_path(&path));
        }
        let owner = task.owner.clone();
        let held = {
            let mut progress = owner.progress.lock().unwrap_or_else(|e| e.into_inner());
            progress.tasks.remove(&id)
        };
        if held.is_some() {
            objc::arc::release(id as Id);
        }
        maybe_invalidated(session, &owner);
        let session = task.session.swap(0, Ordering::AcqRel);
        objc::arc::release(session as Id);
        objc::arc::pool_pop(pool);
        objc::arc::release(id as Id);
    });
}

fn send_optional(delegate: Id, name: &str, session: Id, task: Id, value: Id) {
    let sel = objc::sel::get(name);
    if unsafe { objc::object::class_of(delegate) }
        .and_then(|c| c.method(sel))
        .is_some()
    {
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, Id, Id, Id) =
                std::mem::transmute(objc::msg::entry());
            f(delegate, sel, session, task, value);
        }
    }
}

unsafe extern "C" fn task_state(receiver: Id, _sel: Sel) -> usize {
    unsafe { task_of(receiver) }.map_or(3, |t| t.state.load(Ordering::Acquire))
}

unsafe extern "C" fn set_task_description(receiver: Id, _sel: Sel, description: Id) {
    if let Some(task) = unsafe { task_of(receiver) } {
        let old = std::mem::replace(
            &mut *task.description.lock().unwrap_or_else(|e| e.into_inner()),
            objc::arc::retain(description) as usize,
        );
        objc::arc::release(old as Id);
    }
}

unsafe extern "C" fn task_description(receiver: Id, _sel: Sel) -> Id {
    unsafe { task_of(receiver) }.map_or(std::ptr::null_mut(), |task| {
        let value = task.description.lock().unwrap_or_else(|e| e.into_inner());
        objc::arc::autorelease(objc::arc::retain(*value as Id))
    })
}

unsafe extern "C" fn task_current_request(receiver: Id, _sel: Sel) -> Id {
    unsafe { task_of(receiver) }.map_or(std::ptr::null_mut(), |task| {
        objc::arc::autorelease(make(immutable_request_class(), task.request.clone()))
    })
}

unsafe extern "C" fn task_response(receiver: Id, _sel: Sel) -> Id {
    unsafe { task_of(receiver) }.map_or(std::ptr::null_mut(), |task| {
        task.response.load(Ordering::Acquire) as Id
    })
}

fn session_class() -> &'static Class {
    register::<Session>(c"NSURLSession", None)
}
struct Session {
    state: std::sync::Arc<SessionState>,
}
struct SessionState {
    delegate: std::sync::atomic::AtomicUsize,
    queue: usize,
    shared: bool,
    progress: std::sync::Mutex<SessionProgress>,
}
#[derive(Default)]
struct SessionProgress {
    tasks: std::collections::HashMap<usize, bool>,
    invalidating: bool,
    notified: bool,
}
impl Drop for SessionState {
    fn drop(&mut self) {
        objc::arc::release(self.delegate.load(Ordering::Relaxed) as Id);
        objc::arc::release(self.queue as Id);
    }
}
unsafe fn session_of<'a>(receiver: Id) -> Option<&'a Session> {
    unsafe { shared_contents(receiver, session_class()) }
}

fn new_session(delegate: Id, queue: Id, shared: bool) -> Id {
    let queue = if queue.is_null() {
        crate::operation::serial_queue()
    } else {
        objc::arc::retain(queue)
    };
    make(
        session_class(),
        Session {
            state: std::sync::Arc::new(SessionState {
                delegate: std::sync::atomic::AtomicUsize::new(objc::arc::retain(delegate) as usize),
                queue: queue as usize,
                shared,
                progress: Default::default(),
            }),
        },
    )
}
unsafe extern "C" fn shared_session(_class: Id, _sel: Sel) -> Id {
    static SESSION: OnceLock<usize> = OnceLock::new();
    *SESSION.get_or_init(|| {
        let session = new_session(std::ptr::null_mut(), std::ptr::null_mut(), true);
        objc::arc::make_immortal(session);
        session as usize
    }) as Id
}
unsafe extern "C" fn session_with_configuration(_class: Id, _sel: Sel, _configuration: Id) -> Id {
    objc::arc::autorelease(new_session(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        false,
    ))
}
unsafe extern "C" fn session_with_configuration_delegate_queue(
    _class: Id,
    _sel: Sel,
    _configuration: Id,
    delegate: Id,
    queue: Id,
) -> Id {
    objc::arc::autorelease(new_session(delegate, queue, false))
}
unsafe extern "C" fn delegate_queue(receiver: Id, _sel: Sel) -> Id {
    unsafe { session_of(receiver) }.map_or(std::ptr::null_mut(), |s| s.state.queue as Id)
}

fn maybe_invalidated(receiver: usize, state: &std::sync::Arc<SessionState>) {
    let notify = {
        let mut p = state.progress.lock().unwrap_or_else(|e| e.into_inner());
        if !p.invalidating || !p.tasks.is_empty() || p.notified {
            false
        } else {
            p.notified = true;
            true
        }
    };
    if !notify {
        return;
    }
    let state = state.clone();
    objc::arc::retain(receiver as Id);
    enqueue(state.queue, move || {
        let pool = objc::arc::pool_push();
        let delegate = state.delegate.swap(0, Ordering::AcqRel) as Id;
        let sel = objc::sel::get("URLSession:didBecomeInvalidWithError:");
        if unsafe { objc::object::class_of(delegate) }
            .and_then(|c| c.method(sel))
            .is_some()
        {
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel, Id, Id) =
                    std::mem::transmute(objc::msg::entry());
                f(delegate, sel, receiver as Id, std::ptr::null_mut());
            }
        }
        objc::arc::release(delegate);
        objc::arc::release(receiver as Id);
        objc::arc::pool_pop(pool);
    });
}
unsafe extern "C" fn invalidate_session(receiver: Id, sel: Sel) {
    let Some(session) = (unsafe { session_of(receiver) }) else {
        return;
    };
    let state = session.state.clone();
    if state.shared {
        return;
    }
    let tasks = {
        let mut p = state.progress.lock().unwrap_or_else(|e| e.into_inner());
        p.invalidating = true;
        if sel == objc::sel::get("invalidateAndCancel") {
            p.tasks
                .keys()
                .map(|id| objc::arc::retain(*id as Id) as usize)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };
    for id in tasks {
        complete_task(id, None);
        objc::arc::release(id as Id);
    }
    maybe_invalidated(receiver as usize, &state);
}

unsafe extern "C" fn get_tasks(receiver: Id, _sel: Sel, block: Id) {
    let Some(session) = (unsafe { session_of(receiver) }) else {
        return;
    };
    let block = objc::block::copy(block) as usize;
    let tasks: Vec<_> = session
        .state
        .progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tasks
        .iter()
        .map(|(&id, &download)| (objc::arc::retain(id as Id) as usize, download))
        .collect();
    enqueue(session.state.queue, move || {
        let pool = objc::arc::pool_push();
        let array = |download: Option<bool>| {
            let values: Vec<_> = tasks
                .iter()
                .filter(|(_, d)| download == Some(*d))
                .map(|(id, _)| *id as CFTypeRef)
                .collect();
            let a = corefoundation::collections::new_array(
                &values,
                corefoundation::callbacks::Policy::from_type_callbacks(),
                false,
            );
            unsafe { corefoundation::api::CFAutorelease(a) as Id }
        };
        unsafe {
            call_completion(
                block as Id,
                array(Some(false)),
                array(None),
                array(Some(true)),
            )
        };
        for (id, _) in tasks {
            objc::arc::release(id as Id);
        }
        objc::block::release(block as Id);
        objc::arc::pool_pop(pool);
    });
}

fn make_task(receiver: Id, request: Request, completion: Id, download: bool) -> Id {
    let Some(session) = (unsafe { session_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    let mut progress = session
        .state
        .progress
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if progress.invalidating {
        return std::ptr::null_mut();
    }
    let task = make(
        if download {
            download_task_class()
        } else {
            task_class()
        },
        Task {
            request,
            completion: objc::block::copy(completion) as usize,
            session: std::sync::atomic::AtomicUsize::new(objc::arc::retain(receiver) as usize),
            owner: session.state.clone(),
            download,
            description: Default::default(),
            response: Default::default(),
            started: Default::default(),
            state: std::sync::atomic::AtomicUsize::new(1),
            completion_claimed: Default::default(),
        },
    );
    progress
        .tasks
        .insert(objc::arc::retain(task) as usize, download);
    objc::arc::autorelease(task)
}
unsafe extern "C" fn data_task_with_request(
    receiver: Id,
    _sel: Sel,
    request: Id,
    completion: Id,
) -> Id {
    let Some(request) = (unsafe { request_of(request) }) else {
        return std::ptr::null_mut();
    };
    make_task(receiver, request.clone(), completion, false)
}
unsafe extern "C" fn data_task_with_url(receiver: Id, _sel: Sel, url: Id, completion: Id) -> Id {
    let Some(text) = (unsafe { crate::url::text_of(url) }) else {
        return std::ptr::null_mut();
    };
    make_task(
        receiver,
        Request::new(text, DEFAULT_TIMEOUT_SECONDS),
        completion,
        false,
    )
}
unsafe extern "C" fn download_task_with_request(receiver: Id, _sel: Sel, request: Id) -> Id {
    download_task(receiver, request, std::ptr::null_mut())
}
unsafe extern "C" fn download_task_with_request_handler(
    receiver: Id,
    _sel: Sel,
    request: Id,
    completion: Id,
) -> Id {
    download_task(receiver, request, completion)
}
fn download_task(receiver: Id, request: Id, completion: Id) -> Id {
    let Some(request) = (unsafe { request_of(request) }) else {
        return std::ptr::null_mut();
    };
    make_task(receiver, request.clone(), completion, true)
}

/// `+[NSURLConnection sendSynchronousRequest:returningResponse:error:]`, which
/// is the whole of `NSURLConnection` a program still uses.
unsafe extern "C" fn send_synchronous_request(
    _class: Id,
    _sel: Sel,
    request: Id,
    response_out: *mut Id,
    error_out: *mut Id,
) -> Id {
    // SAFETY: a request this module made, or nil.
    let Some(request) = (unsafe { request_of(request) }) else {
        return std::ptr::null_mut();
    };
    let (data, response, error) = objects(perform(&request.clone()));
    // Both out-parameters are optional, which is why each is checked: a caller
    // that wants only the data passes NULL for the others.
    // SAFETY: each is null or a writable `NSURLResponse *` / `NSError *` the
    // caller owns.
    unsafe {
        if !response_out.is_null() {
            *response_out = response;
        }
        if !error_out.is_null() {
            *error_out = error;
        }
    }
    data
}

//

/// # Safety
///
/// `string` must be null or a string object.
unsafe fn text(string: Id) -> String {
    if string.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(&unsafe { crate::string::bytes(string) }).into_owned()
}

fn autoreleased_string(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a string this call just created.
    unsafe { corefoundation::api::CFAutorelease(string as CFTypeRef) as Id }
}

/// Every class this module answers for.
pub const CLASSES: &[&str] = &[
    "NSURLRequest",
    "NSMutableURLRequest",
    "NSURLResponse",
    "NSHTTPURLResponse",
    "NSURLSession",
    "NSURLSessionTask",
    "NSURLSessionDataTask",
    "NSURLSessionDownloadTask",
    "NSURLConnection",
    "NSError",
];

pub(crate) fn install() {
    let mutable = request_class();
    let immutable = immutable_request_class();
    // The accessors go on the immutable class, which the mutable one inherits;
    // the setters would too, and are on it only because that is where a reader
    // looks for them.
    class_methods!(immutable, {
        "requestWithURL:" => request_with_url, c"@24@0:8@16";
        "requestWithURL:cachePolicy:timeoutInterval:" => request_with_url_policy_timeout, c"@40@0:8@16Q24d32";
    });
    class_methods!(mutable, {
        "requestWithURL:" => request_with_url, c"@24@0:8@16";
        "requestWithURL:cachePolicy:timeoutInterval:" => request_with_url_policy_timeout, c"@40@0:8@16Q24d32";
    });
    methods!(immutable, {
        "URL" => request_url, c"@16@0:8";
        "HTTPMethod" => http_method, c"@16@0:8";
        "HTTPBody" => http_body, c"@16@0:8";
        "valueForHTTPHeaderField:" => value_for_header, c"@24@0:8@16";
        "allHTTPHeaderFields" => request_headers, c"@16@0:8";
        "timeoutInterval" => timeout_interval, c"d16@0:8";
        "cachePolicy" => cache_policy, c"Q16@0:8";
        "setURL:" => set_url, c"v24@0:8@16";
        "setHTTPMethod:" => set_http_method, c"v24@0:8@16";
        "setHTTPBody:" => set_http_body, c"v24@0:8@16";
        "setValue:forHTTPHeaderField:" => set_value_for_header, c"v32@0:8@16@24";
        "addValue:forHTTPHeaderField:" => add_value_for_header, c"v32@0:8@16@24";
        "setTimeoutInterval:" => set_timeout_interval, c"v24@0:8d16";
        "setCachePolicy:" => set_cache_policy, c"v24@0:8Q16";
    });

    let response = response_class();
    let base = objc::class::lookup(c"NSURLResponse").expect("registered by `response_class`");
    methods!(base, {
        "URL" => response_url, c"@16@0:8";
        "MIMEType" => mime_type, c"@16@0:8";
        "textEncodingName" => text_encoding_name, c"@16@0:8";
        "expectedContentLength" => expected_content_length, c"q16@0:8";
    });
    methods!(response, {
        "statusCode" => status_code, c"q16@0:8";
        "allHeaderFields" => all_header_fields, c"@16@0:8";
    });

    let task = task_class();
    download_task_class();
    methods!(task, {
        "state" => task_state, c"Q16@0:8";
        "resume" => resume, c"v16@0:8";
        "cancel" => cancel, c"v16@0:8";
        "setTaskDescription:" => set_task_description, c"v24@0:8@16";
        "taskDescription" => task_description, c"@16@0:8";
        "currentRequest" => task_current_request, c"@16@0:8";
        "originalRequest" => task_current_request, c"@16@0:8";
        "response" => task_response, c"@16@0:8";
    });

    let session = session_class();
    class_methods!(session, {
        "sharedSession" => shared_session, c"@16@0:8";
        "sessionWithConfiguration:" => session_with_configuration, c"@24@0:8@16";
        "sessionWithConfiguration:delegate:delegateQueue:"
            => session_with_configuration_delegate_queue, c"@40@0:8@16@24@32";
    });
    // `@?` for the handler, which is what tells the emulated backend's argument
    // conversion that the parameter is a block whose `invoke` needs a thunk.
    methods!(session, {
        "dataTaskWithRequest:completionHandler:" => data_task_with_request, c"@32@0:8@16@?24";
        "dataTaskWithURL:completionHandler:" => data_task_with_url, c"@32@0:8@16@?24";
        "downloadTaskWithRequest:" => download_task_with_request, c"@24@0:8@16";
        "downloadTaskWithRequest:completionHandler:"
            => download_task_with_request_handler, c"@32@0:8@16@?24";
        "delegateQueue" => delegate_queue, c"@16@0:8";
        "getTasksWithCompletionHandler:" => get_tasks, c"v24@0:8@?16";
        "invalidateAndCancel" => invalidate_session, c"v16@0:8";
        "finishTasksAndInvalidate" => invalidate_session, c"v16@0:8";
    });

    let connection = corefoundation::base::abstract_class(c"NSURLConnection", None);
    class_methods!(connection, {
        "sendSynchronousRequest:returningResponse:error:" => send_synchronous_request, c"@40@0:8@16^@24^@32";
    });

    crate::error::install();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request built the way a guest builds one, run the way
    /// `+sendSynchronousRequest:` runs it. Against a real server, because what
    /// is being asserted is that this tree's TLS stack completes a handshake -
    /// which is the whole reason the transport is here.
    #[test]
    fn a_https_request_comes_back_with_a_status_and_a_body() {
        crate::bootstrap();
        let request = Request::new("https://example.com/".to_string(), 30.0);
        let (response, body) = match perform(&request) {
            Ok(pair) => pair,
            // A machine with no network is not a failing build. The refusal is
            // still reported, because a silent skip here would let a broken
            // transport look like an offline laptop.
            Err(e) => {
                eprintln!("skipping: no network ({e})");
                return;
            }
        };
        assert_eq!(response.status, 200);
        assert!(!body.is_empty(), "a body came back");
        assert!(
            response
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type")),
            "the headers survived"
        );
    }

    /// The method and the headers a caller set are the ones that go out, which
    /// is what a server echoing them back proves.
    #[test]
    fn a_post_sends_its_method_body_and_headers() {
        crate::bootstrap();
        let mut request = Request::new("https://postman-echo.com/post".to_string(), 30.0);
        request.method = "POST".to_string();
        request
            .headers
            .push(("x-rosemary".to_string(), "1".to_string()));
        request.body = b"a body".to_vec();
        let (response, body) = match perform(&request) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("skipping: no network ({e})");
                return;
            }
        };
        assert_eq!(response.status, 200);
        let echoed = String::from_utf8_lossy(&body);
        assert!(echoed.contains("a body"), "the body was sent: {echoed}");
        assert!(echoed.contains("x-rosemary"), "the header was sent");
    }

    /// A URL nothing serves is an error rather than a response, and the message
    /// reaches the `NSError` a completion handler is given.
    #[test]
    fn a_failure_becomes_an_error_with_a_message() {
        crate::bootstrap();
        let request = Request::new("https://invalid.invalid/".to_string(), 10.0);
        let Err(message) = perform(&request) else {
            panic!("a name that cannot resolve is not a response");
        };
        assert!(!message.is_empty(), "the failure says something");

        let (data, response, error) = objects(Err(message));
        assert!(data.is_null() && response.is_null());
        assert!(!error.is_null(), "the handler gets an NSError");
        // SAFETY: `-code` is `NSInteger (id, SEL)` on the error just made.
        let sel = objc::sel::get("code");
        let method = unsafe { objc::object::class_of(error) }
            .and_then(|c| c.method(sel))
            .expect("NSError answers -code");
        let code: i64 = unsafe {
            let f: unsafe extern "C" fn(Id, Sel) -> i64 =
                std::mem::transmute(method.implementation());
            f(error, sel)
        };
        assert_eq!(code, crate::error::NSURL_ERROR_UNKNOWN);
    }
    static LIFECYCLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static EVENTS: std::sync::Mutex<Vec<(usize, i64, usize)>> = std::sync::Mutex::new(Vec::new());
    unsafe extern "C" fn record_completion(_block: Id, data: Id, response: Id, error: Id) {
        let code = if error.is_null() {
            0
        } else {
            unsafe {
                let f: unsafe extern "C" fn(Id, Sel) -> i64 =
                    std::mem::transmute(objc::msg::entry());
                f(error, objc::sel::get("code"))
            }
        };
        let queue = unsafe {
            let class = objc::class::lookup(c"NSOperationQueue").unwrap();
            let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
            f(
                objc::class::ptr(class) as Id,
                objc::sel::get("currentQueue"),
            )
        };
        let status = if response.is_null() {
            0
        } else {
            (unsafe { status_code(response, objc::sel::get("statusCode")) }) as usize
        };
        let _ = data;
        EVENTS.lock().unwrap().push((status, code, queue as usize));
    }
    unsafe extern "C" fn record_invalidation(_delegate: Id, _sel: Sel, _session: Id, _error: Id) {
        EVENTS.lock().unwrap().push((999, 0, 0));
    }
    unsafe extern "C" fn record_tasks(_block: Id, data: Id, _upload: Id, download: Id) {
        let n = unsafe {
            corefoundation::api::CFArrayGetCount(data as CFTypeRef)
                + corefoundation::api::CFArrayGetCount(download as CFTypeRef)
        };
        EVENTS.lock().unwrap().push((998, n as i64, 0));
    }
    fn wait_events(n: usize) {
        for _ in 0..500 {
            if EVENTS.lock().unwrap().len() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("callback deadline expired");
    }
    fn suspend_queue(queue: Id, suspended: bool) {
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
            f(queue, objc::sel::get("setSuspended:"), suspended as u8);
        }
    }
    fn test_session() -> Id {
        let class = corefoundation::base::register(c"RosemarySessionTestDelegate", None, 0, false);
        class.replace_method(
            objc::sel::get("URLSession:didBecomeInvalidWithError:"),
            record_invalidation as *const _,
            c"v32@0:8@16@24",
        );
        let delegate = objc::object::create_instance(class, 0);
        let session = new_session(delegate, std::ptr::null_mut(), false);
        objc::arc::release(delegate);
        session
    }

    #[test]
    fn cancellation_before_resume_uses_delegate_queue_and_precedes_invalidation() {
        let _lock = LIFECYCLE_LOCK.lock().unwrap();
        crate::bootstrap();
        EVENTS.lock().unwrap().clear();
        let pool = objc::arc::pool_push();
        let session = test_session();
        let queue = unsafe { delegate_queue(session, objc::sel::get("delegateQueue")) };
        suspend_queue(queue, true);
        let block = crate::block::fake::block(record_completion as *const _);
        let task = make_task(
            session,
            Request::new("http://127.0.0.1:1/".into(), 1.0),
            block.id(),
            false,
        );
        assert_eq!(unsafe { task_state(task, objc::sel::get("state")) }, 1);
        unsafe {
            cancel(task, objc::sel::get("cancel"));
            resume(task, objc::sel::get("resume"));
            invalidate_session(session, objc::sel::get("invalidateAndCancel"));
        }
        assert!(EVENTS.lock().unwrap().is_empty());
        suspend_queue(queue, false);
        wait_events(2);
        assert_eq!(
            *EVENTS.lock().unwrap(),
            vec![(0, -999, queue as usize), (999, 0, 0)]
        );
        assert_eq!(unsafe { task_state(task, objc::sel::get("state")) }, 3);
        assert!(make_task(session, Request::default(), block.id(), false).is_null());
        objc::arc::release(session);
        objc::arc::pool_pop(pool);
    }

    #[test]
    fn an_http_error_keeps_its_response_and_finishes_before_invalidation() {
        use std::io::{Read, Write};
        let _lock = LIFECYCLE_LOCK.lock().unwrap();
        crate::bootstrap();
        EVENTS.lock().unwrap().clear();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback required");
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut b = [0; 4096];
            let _ = stream.read(&mut b).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .unwrap();
        });
        let pool = objc::arc::pool_push();
        let session = test_session();
        let queue = unsafe { delegate_queue(session, objc::sel::get("delegateQueue")) };
        let block = crate::block::fake::block(record_completion as *const _);
        let enumeration = crate::block::fake::block(record_tasks as *const _);
        let task = make_task(session, Request::new(url, 5.0), block.id(), false);
        unsafe {
            get_tasks(
                session,
                objc::sel::get("getTasksWithCompletionHandler:"),
                enumeration.id(),
            );
        }
        wait_events(1);
        assert_eq!(EVENTS.lock().unwrap()[0], (998, 1, 0));
        unsafe {
            resume(task, objc::sel::get("resume"));
            invalidate_session(session, objc::sel::get("finishTasksAndInvalidate"));
        }
        wait_events(3);
        server.join().unwrap();
        assert_eq!(
            *EVENTS.lock().unwrap(),
            vec![(998, 1, 0), (400, 0, queue as usize), (999, 0, 0)]
        );
        let response = unsafe { task_response(task, objc::sel::get("response")) };
        assert_eq!(
            unsafe { status_code(response, objc::sel::get("statusCode")) },
            400
        );
        assert!(
            unsafe { session_of(session) }
                .unwrap()
                .state
                .progress
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
        objc::arc::release(session);
        objc::arc::pool_pop(pool);
    }

    #[test]
    fn shared_session_cannot_be_invalidated_and_configured_sessions_are_distinct() {
        crate::bootstrap();
        let pool = objc::arc::pool_push();
        let sel = objc::sel::get("sharedSession");
        let shared = unsafe { shared_session(std::ptr::null_mut(), sel) };
        unsafe {
            invalidate_session(shared, objc::sel::get("invalidateAndCancel"));
        }
        assert!(
            !unsafe { session_of(shared) }
                .unwrap()
                .state
                .progress
                .lock()
                .unwrap()
                .invalidating
        );
        let a =
            unsafe { session_with_configuration(std::ptr::null_mut(), sel, std::ptr::null_mut()) };
        let b =
            unsafe { session_with_configuration(std::ptr::null_mut(), sel, std::ptr::null_mut()) };
        assert_ne!(a, b);
        assert_ne!(a, shared);
        objc::arc::pool_pop(pool);
    }
}
