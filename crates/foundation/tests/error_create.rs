//! `foundation::error::create` and `CFErrorCopyDescription`.

use objc::{object::Id, sel::Sel};

unsafe fn send0(receiver: Id, selector: &str) -> Id {
    let sel = objc::sel::get(selector);
    let class = unsafe { objc::object::class_of(receiver) }.expect("an object");
    let f = objc::class::method_implementation(class, sel) as usize;
    unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(Id, Sel) -> Id>(f)(receiver, sel) }
}

#[test]
fn creates_errors_with_user_info() {
    foundation::bootstrap();
    let underlying = foundation::error::make("Inner", 1, "inner");
    let error = foundation::error::create(
        "Outer",
        7,
        "outer",
        &[
            ("NSUnderlyingError", underlying),
            ("NSLocalizedDescription", underlying),
        ],
    );
    assert!(!error.is_null());
    // SAFETY: a live error made above.
    let info = unsafe { send0(error, "userInfo") };
    let count = unsafe { corefoundation::api::CFDictionaryGetCount(info as _) };
    assert_eq!(count, 2, "the description is not duplicated");
    let described = unsafe { send0(error, "localizedDescription") };
    assert_eq!(unsafe { foundation::string::bytes(described) }, b"outer");
    objc::arc::release(error);
    objc::arc::release(underlying);
}
