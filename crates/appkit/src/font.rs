use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// `+[NSFontManager sharedFontManager]`.
unsafe extern "C" fn shared_font_manager(class: Id, _sel: Sel) -> Id {
    *SHARED.get_or_init(|| {
        // SAFETY: the receiver is the NSFontManager class declared by AppKit.
        let manager = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // AppKit owns the process-wide singleton.
        objc::arc::retain(manager);
        manager as usize
    }) as Id
}

/// `-[NSFontManager localizedNameForFamily:face:]`.
///
/// The fallback family exposed by CoreText has no separate localized names.
unsafe extern "C" fn localized_name(_receiver: Id, _sel: Sel, family: Id, face: Id) -> Id {
    if family.is_null() { face } else { family }
}

/// `-[NSFontManager availableMembersOfFontFamily:]`: one member per family
/// that resolves - `[PostScript name, face, weight, traits]` - or nil.
/// FIXME (ACCURACY): only the face the family resolves to, as a regular one.
unsafe extern "C" fn available_members(_receiver: Id, sel: Sel, family: Id) -> Id {
    if family.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `family` is an NSString as the selector requires.
    let font = unsafe { font_with_name(std::ptr::null_mut(), sel, family, 12.0) };
    if font.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a font just resolved.
    let name = unsafe { crate::coretext::CTFontCopyPostScriptName(font as *const _) };
    let face = corefoundation::base::interned_name("Regular");
    let weight: i64 = 5;
    let traits: i64 = 0;
    // SAFETY: plain number constructors over locals.
    let (weight, traits) = unsafe {
        (
            corefoundation::api::CFNumberCreate(std::ptr::null(), 4, (&raw const weight).cast()),
            corefoundation::api::CFNumberCreate(std::ptr::null(), 4, (&raw const traits).cast()),
        )
    };
    let callbacks = corefoundation::callbacks::Policy::from_type_callbacks;
    let member = corefoundation::collections::new_array(
        &[name as _, face as _, weight, traits],
        callbacks(),
        false,
    );
    let members = corefoundation::collections::new_array(&[member], callbacks(), false);
    // SAFETY: the arrays hold their own references now.
    unsafe {
        for object in [name as _, weight, traits, member] {
            corefoundation::api::CFRelease(object);
        }
        corefoundation::api::CFAutorelease(members) as Id
    }
}

/// `-[NSFontManager weightOfFont:]`: the regular weight on AppKit's 0-15
/// scale, which every face here is taken as.
unsafe extern "C" fn weight_of_font(_receiver: Id, _sel: Sel, _font: Id) -> isize {
    5
}

/// `-[NSFontManager traitsOfFont:]`: none.
unsafe extern "C" fn traits_of_font(_receiver: Id, _sel: Sel, _font: Id) -> usize {
    0
}

/// `-[NSFontManager fontWithFamily:traits:weight:size:]`.
///
/// The named family is resolved by CoreText. Trait and legacy integer weight
/// selection can refine a family containing several faces; the current font
/// facade resolves the family's regular face, which is still preferable to
/// returning nil for UI text that can use it.
unsafe extern "C" fn font_with_family(
    _receiver: Id,
    _sel: Sel,
    family: Id,
    _traits: usize,
    _weight: isize,
    size: f64,
) -> Id {
    if family.is_null() {
        // SAFETY: the system-font constructor's declared arguments.
        return unsafe { system_font_of_size(std::ptr::null_mut(), _sel, size) };
    }
    // SAFETY: `family` is an NSString as required by the selector.
    let font = unsafe { font_with_name(std::ptr::null_mut(), _sel, family, size) };
    if font.is_null() {
        // A Mac-only family such as Helvetica is routinely requested by old
        // games. Use the platform's system face when fontconfig has no exact
        // family rather than making otherwise renderable UI text disappear.
        unsafe { system_font_of_size(std::ptr::null_mut(), _sel, size) }
    } else {
        font
    }
}

/// `+[NSFont fontWithName:size:]`.
///
/// `NSFont` is toll-free bridged to `CTFont`, so the font is the Core Text one.
unsafe extern "C" fn font_with_name(_class: Id, _sel: Sel, name: Id, size: f64) -> Id {
    // SAFETY: null or a string, as the selector requires.
    let font =
        unsafe { crate::coretext::CTFontCreateWithName(name as *const _, size, std::ptr::null()) };
    // SAFETY: a fresh object, or null.
    unsafe { autorelease(font) }
}

/// `+systemFontOfSize:`, and its bold twin, which is the same face here: the
/// fallback family is the only one there is to pick from.
unsafe extern "C" fn system_font_of_size(_class: Id, _sel: Sel, size: f64) -> Id {
    // SAFETY: `kCTFontUIFontSystem` with no language.
    let font = unsafe { crate::coretext::CTFontCreateUIFontForLanguage(2, size, std::ptr::null()) };
    // SAFETY: a fresh object, or null.
    unsafe { autorelease(font) }
}

/// `+userFixedPitchFontOfSize:`, the monospaced `kCTFontUIFontUserFixedPitch`.
unsafe extern "C" fn user_fixed_pitch_font_of_size(_class: Id, _sel: Sel, size: f64) -> Id {
    // SAFETY: `kCTFontUIFontUserFixedPitch` with no language.
    let font = unsafe { crate::coretext::CTFontCreateUIFontForLanguage(1, size, std::ptr::null()) };
    // SAFETY: a fresh object, or null.
    unsafe { autorelease(font) }
}

/// `+monospacedSystemFontOfSize:weight:`; the weight picks nothing here, as
/// there is one face per family.
unsafe extern "C" fn monospaced_system_font_of_size(
    class: Id,
    sel: Sel,
    size: f64,
    _weight: f64,
) -> Id {
    // SAFETY: the same arguments the one-size form takes.
    unsafe { user_fixed_pitch_font_of_size(class, sel, size) }
}

unsafe extern "C" fn system_font_size(_class: Id, _sel: Sel) -> f64 {
    13.0
}

/// `+[NSFont systemFontSizeForControlSize:]`, in points.
///
/// The legacy small and mini controls have their own metrics; regular and
/// unhandled sizes use the standard system font size.
unsafe extern "C" fn system_font_size_for_control_size(
    class: Id,
    sel: Sel,
    control_size: usize,
) -> f64 {
    match control_size {
        1 => 11.0,
        2 => 9.0,
        // SAFETY: the class-method receiver and selector have the same types.
        _ => unsafe { system_font_size(class, sel) },
    }
}

unsafe fn autorelease(font: *const std::ffi::c_void) -> Id {
    if font.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { corefoundation::api::CFAutorelease(font) as Id }
}

/// The `NSFont` instance accessors.
///
/// A font here is the Core Text one - a `CFDictionary` of attributes, see
/// `coretext` - so the receiver is an `NSDictionary` and these are installed
/// there. There is no `NSFont` class of its own to put them on until fonts stop
/// being dictionaries.
unsafe fn autoreleased(string: *const std::ffi::c_void) -> Id {
    unsafe { autorelease(string) }
}

unsafe extern "C" fn font_family_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a font this runtime made; the copy is +1.
    unsafe { autoreleased(crate::coretext::CTFontCopyFamilyName(receiver as *const _)) }
}

unsafe extern "C" fn font_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    unsafe {
        autoreleased(crate::coretext::CTFontCopyPostScriptName(
            receiver as *const _,
        ))
    }
}

unsafe extern "C" fn font_point_size(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: as above.
    unsafe { crate::coretext::CTFontGetSize(receiver as *const _) }
}

/// `-printerFont` and `-screenFont`: the font itself, as there is one
/// rendering of it.
unsafe extern "C" fn same_font(receiver: Id, _sel: Sel) -> Id {
    receiver
}

unsafe extern "C" fn font_ascender(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: a font, as the selector's receiver.
    unsafe { crate::coretext::CTFontGetAscent(receiver as _) }
}

/// `-descender`: negative below the baseline, where Core Text's is positive.
unsafe extern "C" fn font_descender(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: as above.
    -unsafe { crate::coretext::CTFontGetDescent(receiver as _) }
}

unsafe extern "C" fn font_leading(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: as above.
    unsafe { crate::coretext::CTFontGetLeading(receiver as _) }
}

unsafe extern "C" fn font_cap_height(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: as above.
    unsafe { crate::coretext::CTFontGetCapHeight(receiver as _) }
}

unsafe extern "C" fn font_x_height(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: as above.
    unsafe { crate::coretext::CTFontGetXHeight(receiver as _) }
}

/// `+[NSFont findFontLike:forString:withRange:inLanguage:]`: the font asked
/// about, which fontconfig's fallback already backs for characters it lacks.
unsafe extern "C" fn find_font_like(
    _class: Id,
    _sel: Sel,
    font: Id,
    _string: Id,
    _location: usize,
    _length: usize,
    _language: Id,
) -> Id {
    font
}

/// `-symbolicTraits` of a font descriptor: none, a regular upright face.
unsafe extern "C" fn symbolic_traits(_receiver: Id, _sel: Sel) -> u32 {
    0
}

/// `-fontDescriptor`: a font and its descriptor are the same attributes
/// here.
unsafe extern "C" fn font_descriptor(receiver: Id, _sel: Sel) -> Id {
    receiver
}

/// `-[NSFontManager convertFont:toHaveTrait:]` and its `toNotHaveTrait:` twin:
/// the font as it is, since one face per family is all there is.
unsafe extern "C" fn convert_font_trait(_receiver: Id, _sel: Sel, font: Id, _trait: usize) -> Id {
    font
}

/// `+systemFontOfSize:weight:`: the system face, whatever the weight.
unsafe extern "C" fn system_font_of_size_weight(class: Id, sel: Sel, size: f64, _weight: f64) -> Id {
    // SAFETY: the same receiver and size.
    unsafe { system_font_of_size(class, sel, size) }
}

pub(crate) fn install() {
    let dictionary = objc::class::lookup(c"NSDictionary").expect("declared before its methods");
    methods!(dictionary, {
        "familyName" => font_family_name, c"@16@0:8";
        "fontName" => font_name, c"@16@0:8";
        "displayName" => font_family_name, c"@16@0:8";
        "pointSize" => font_point_size, c"d16@0:8";
        "printerFont" => same_font, c"@16@0:8";
        "fontDescriptor" => font_descriptor, c"@16@0:8";
        "symbolicTraits" => symbolic_traits, c"I16@0:8";
        "screenFont" => same_font, c"@16@0:8";
        "ascender" => font_ascender, c"d16@0:8";
        "descender" => font_descender, c"d16@0:8";
        "leading" => font_leading, c"d16@0:8";
        "capHeight" => font_cap_height, c"d16@0:8";
        "xHeight" => font_x_height, c"d16@0:8";
    });

    let class = objc::class::lookup(c"NSFont").expect("declared before its methods");
    class_methods!(class, {
        "fontWithName:size:" => font_with_name, c"@32@0:8@16d24";
        "systemFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "boldSystemFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "userFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "messageFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "menuFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "labelFontOfSize:" => system_font_of_size, c"@24@0:8d16";
        "systemFontOfSize:weight:" => system_font_of_size_weight, c"@32@0:8d16d24";
        "findFontLike:forString:withRange:inLanguage:" => find_font_like,
            c"@56@0:8@16@24{_NSRange=QQ}32@48";
        "boldSystemFontOfSize:weight:" => system_font_of_size_weight, c"@32@0:8d16d24";
        "monospacedSystemFontOfSize:weight:" => monospaced_system_font_of_size, c"@32@0:8d16d24";
        "userFixedPitchFontOfSize:" => user_fixed_pitch_font_of_size, c"@24@0:8d16";
        "systemFontSize" => system_font_size, c"d16@0:8";
        "systemFontSizeForControlSize:" => system_font_size_for_control_size, c"d24@0:8Q16";
    });

    let class = objc::class::lookup(c"NSFontManager").expect("declared before its methods");
    class_methods!(class, {
        "sharedFontManager" => shared_font_manager, c"@16@0:8";
    });
    methods!(class, {
        "localizedNameForFamily:face:" => localized_name, c"@32@0:8@16@24";
        "fontWithFamily:traits:weight:size:" => font_with_family, c"@48@0:8@16Q24q32d40";
        "availableMembersOfFontFamily:" => available_members, c"@24@0:8@16";
        "weightOfFont:" => weight_of_font, c"q24@0:8@16";
        "convertFont:toHaveTrait:" => convert_font_trait, c"@32@0:8@16Q24";
        "convertFont:toNotHaveTrait:" => convert_font_trait, c"@32@0:8@16Q24";
        "traitsOfFont:" => traits_of_font, c"Q24@0:8@16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_font_manager_is_stable() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSFontManager").unwrap();
        let class = objc::class::ptr(class).cast();
        // SAFETY: direct calls with the Objective-C signature installed above.
        let first = unsafe { shared_font_manager(class, objc::sel::get("sharedFontManager")) };
        let second = unsafe { shared_font_manager(class, objc::sel::get("sharedFontManager")) };
        assert!(!first.is_null());
        assert_eq!(first, second);
    }

    #[test]
    fn localized_font_name_uses_the_family() {
        crate::bootstrap();
        let family = foundation::string::from_str("Sans") as Id;
        let face = foundation::string::from_str("Regular") as Id;
        // SAFETY: direct call with the Objective-C signature installed above.
        let result = unsafe {
            localized_name(
                std::ptr::null_mut(),
                objc::sel::get("localizedNameForFamily:face:"),
                family,
                face,
            )
        };
        assert_eq!(result, family);
        unsafe {
            corefoundation::api::CFRelease(family.cast());
            corefoundation::api::CFRelease(face.cast());
        }
    }

    #[test]
    fn system_font_size_for_control_size_uses_the_class_message_abi() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSFont").unwrap();
        let selector = objc::sel::get("systemFontSizeForControlSize:");
        let method = class
            .metaclass()
            .method(selector)
            .expect("NSFont must implement +systemFontSizeForControlSize:");
        // SAFETY: the registered method owns its NUL-terminated type encoding.
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(method.types()) },
            c"d24@0:8Q16"
        );
        let receiver = objc::class::ptr(class).cast();
        // SAFETY: these are the installed class-method signatures. Dispatching
        // through objc_msgSend also checks the unsigned NSControlSize argument
        // and floating-point return ABI used by a guest application.
        unsafe {
            let size: unsafe extern "C" fn(Id, Sel, usize) -> f64 =
                std::mem::transmute(objc::msg::entry());
            let standard: unsafe extern "C" fn(Id, Sel) -> f64 =
                std::mem::transmute(objc::msg::entry());
            assert_eq!(size(receiver, selector, 0), 13.0);
            assert_eq!(size(receiver, selector, 1), 11.0);
            assert_eq!(size(receiver, selector, 2), 9.0);
            let fallback = standard(receiver, objc::sel::get("systemFontSize"));
            assert_eq!(size(receiver, selector, 99), fallback);
            assert_eq!(size(receiver, selector, usize::MAX), fallback);
        }
    }
}
