use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The process's one context, made on first use.
///
/// Apple makes one per client view; one shared object is indistinguishable to a
/// caller, because [`client`] asks which view is focused rather than
/// remembering one.
fn shared() -> Id {
    static CONTEXT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CONTEXT.get_or_init(|| {
        let class =
            objc::class::lookup(c"NSTextInputContext").expect("declared before its methods");
        objc::object::create_instance(class, 0) as usize
    }) as Id
}

/// The key window's first responder, when it is a text input client.
fn focused_client() -> Id {
    let responder = crate::window::first_responder_of(crate::window::key_window());
    match crate::text::is_input_client(responder) {
        true => responder,
        false => std::ptr::null_mut(),
    }
}

/// `+[NSTextInputContext currentInputContext]`.
///
/// Nil when nothing focused takes text, as Apple's is: a program asks this to
/// decide whether key events are text, and a context with no client would say
/// they are.
unsafe extern "C" fn current_input_context(_class: Id, _sel: Sel) -> Id {
    match focused_client().is_null() {
        true => std::ptr::null_mut(),
        false => shared(),
    }
}

/// `-[NSResponder inputContext]`.
///
/// The context text reaches the responder through, which only a responder that
/// takes text has: nil for one that does not, as Apple's is, because a program
/// asks this to decide whether the responder is a text input client at all.
/// It is the process's one context for the reason given at [`shared`], so a
/// responder that is not the focused one is handed a context whose [`client`] is.
///
/// A view overrides this to answer nil while it is not taking text and asks its
/// superclass otherwise, which is why it is on `NSResponder` and not on the
/// view.
unsafe extern "C" fn input_context(receiver: Id, _sel: Sel) -> Id {
    match crate::text::is_input_client(receiver) {
        true => shared(),
        false => std::ptr::null_mut(),
    }
}

/// `-client`: the view text is going to.
unsafe extern "C" fn client(_receiver: Id, _sel: Sel) -> Id {
    focused_client()
}

/// `-discardMarkedText`, `-invalidateCharacterCoordinates`, `-activate` and
/// `-deactivate`: there is no composition session to end, and the text input
/// area follows the first responder from [`crate::window`] already.
unsafe extern "C" fn ignored(_receiver: Id, _sel: Sel) {}

/// `-selectedKeyboardInputSource`: the one layout this host reports.
unsafe extern "C" fn selected_keyboard_input_source(_receiver: Id, _sel: Sel) -> Id {
    foundation::notification::name_object(crate::textinput::SOURCE_ID) as Id
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSTextInputContext").expect("declared before its methods");
    class_methods!(class, {
        "currentInputContext" => current_input_context, c"@16@0:8";
    });
    let responder = objc::class::lookup(c"NSResponder").expect("declared before its methods");
    methods!(responder, {
        "inputContext" => input_context, c"@16@0:8";
    });
    methods!(class, {
        "client" => client, c"@16@0:8";
        "discardMarkedText" => ignored, c"v16@0:8";
        "invalidateCharacterCoordinates" => ignored, c"v16@0:8";
        "activate" => ignored, c"v16@0:8";
        "deactivate" => ignored, c"v16@0:8";
        "selectedKeyboardInputSource" => selected_keyboard_input_source, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn insert_text(_: Id, _: Sel, _: Id, _: foundation::geometry::NSRange) {}

    /// A view that takes text, as a browser's web content view does.
    fn text_view_class() -> &'static objc::class::Class {
        static REGISTERING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        crate::bootstrap();
        let _guard = REGISTERING.lock().unwrap();
        let name = c"RosemaryTestTextInputView";
        objc::class::lookup(name).unwrap_or_else(|| {
            let base = objc::class::lookup(c"NSView").expect("NSView");
            let class = objc::class::allocate_pair(Some(base), name, 0).expect("a fresh class");
            assert!(class.add_method(
                objc::sel::get("insertText:replacementRange:"),
                insert_text as objc::class::Imp,
                c"v40@0:8@16{_NSRange=QQ}24",
            ));
            objc::class::register_pair(class);
            class
        })
    }

    fn send_input_context(receiver: Id) -> Id {
        let class = unsafe { objc::object::class_of(receiver) }.expect("an object");
        let sel = objc::sel::get("inputContext");
        let imp = objc::class::method_implementation(class, sel);
        // SAFETY: `-inputContext` is `id (id, SEL)`.
        let call: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(imp) };
        unsafe { call(receiver, sel) }
    }

    #[test]
    fn a_responder_that_takes_text_has_the_input_context() {
        let view = objc::object::create_instance(text_view_class(), 0);
        let context = send_input_context(view);
        assert!(!context.is_null());
        assert_eq!(context, shared());
        assert_eq!(send_input_context(view), context, "the same one each time");
    }

    #[test]
    fn a_responder_that_does_not_take_text_has_none() {
        crate::bootstrap();
        let plain = objc::object::create_instance(objc::class::lookup(c"NSView").unwrap(), 0);
        assert!(send_input_context(plain).is_null());
        let window = objc::object::create_instance(objc::class::lookup(c"NSResponder").unwrap(), 0);
        assert!(send_input_context(window).is_null());
    }

    #[test]
    fn a_subclass_asking_its_superclass_for_the_context_gets_it() {
        // `-[RenderWidgetHostViewCocoa inputContext]` answers nil unless text is
        // being taken, and otherwise `[super inputContext]`: the superclass is
        // `NSView`, which has to answer for it.
        crate::bootstrap();
        let view = objc::class::lookup(c"NSView").expect("NSView");
        let found = view
            .method(objc::sel::get("inputContext"))
            .expect("NSView inherits -inputContext");
        let responder = objc::class::lookup(c"NSResponder").expect("NSResponder");
        assert_eq!(
            found.implementation(),
            responder
                .method(objc::sel::get("inputContext"))
                .expect("NSResponder defines it")
                .implementation()
        );
    }
}
