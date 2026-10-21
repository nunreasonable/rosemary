//! Bounded transport diagnostics. Never log URLs, headers or response text.
use corefoundation::base::CFTypeRef;
use objc::object::Id;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

fn known_code(body: &[u8]) -> &'static str {
    if body.len() > 65536 {
        return "omitted";
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return "omitted";
    };
    let pool = objc::arc::pool_push();
    let code = crate::json::from_text(text)
        .ok()
        .map_or("omitted", |object| {
            for key in ["errorCode", "code"] {
                let key = crate::string::from_str(key);
                let value = unsafe {
                    corefoundation::api::CFDictionaryGetValue(object as CFTypeRef, key as CFTypeRef)
                } as Id;
                objc::arc::release(key);
                if value.is_null() {
                    continue;
                }
                // Compare to fixed literals; even a value under "errorCode" might
                // contain an identifier or a credential supplied by a server.
                for allowed in [
                    "GAME_START_ERROR",
                    "INVALID_REQUEST",
                    "BAD_REQUEST",
                    "NOT_FOUND",
                    "FORBIDDEN",
                    "UNAUTHORIZED",
                    "QUEUE_ENTRY_NOT_ENTITLED",
                    "FAILED_TUTORIAL",
                    "INVALID_STATE",
                    "SERVICE_UNAVAILABLE",
                ] {
                    let expected = crate::string::from_str(allowed);
                    let equal = unsafe {
                        corefoundation::api::CFEqual(value as CFTypeRef, expected as CFTypeRef)
                    } != 0;
                    objc::arc::release(expected);
                    if equal {
                        return allowed;
                    }
                }
            }
            "omitted"
        });
    objc::arc::pool_pop(pool);
    code
}

pub(crate) fn response(path: &str, status: u16, body: &[u8]) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if !*ENABLED.get_or_init(|| std::env::var_os("ROSEMARY_TRACE_URLSESSION").is_some()) {
        return;
    }
    #[allow(deprecated)] // try_update is not available on older supported Rust.
    let n = COUNT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < 128).then_some(n + 1)
    });
    let Ok(n) = n else {
        return;
    };
    // Match only the route shape. Never print party/member IDs or query data.
    let route = if path
        .split(['?', '#'])
        .next()
        .is_some_and(|p| p.ends_with("/startAction"))
    {
        "start-action"
    } else {
        "other"
    };
    let code = if status >= 400 {
        known_code(body)
    } else {
        "none"
    };
    eprintln!(
        "rosemary: urlsession response sequence={} route={route} status={status} bytes={} code={code}",
        n + 1,
        body.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_codes_are_allowlisted_not_arbitrary_server_text() {
        crate::bootstrap();
        assert_eq!(
            known_code(br#"{"errorCode":"GAME_START_ERROR","token":"secret"}"#),
            "GAME_START_ERROR"
        );
        assert_eq!(
            known_code(br#"{"errorCode":"SECRET_TOKEN_VALUE","code":123456,"message":"secret"}"#),
            "omitted"
        );
        assert_eq!(known_code(br#"{"message":"GAME_START_ERROR"}"#), "omitted");
        assert_eq!(known_code(b"not JSON"), "omitted");
        assert_eq!(known_code(br#"["GAME_START_ERROR"]"#), "omitted");
        assert_eq!(known_code(&vec![b' '; 65537]), "omitted");
    }
}
