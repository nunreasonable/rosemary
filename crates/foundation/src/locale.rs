use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The locale name this process runs under, as `language_REGION`.
fn posix() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .unwrap_or_default()
}

/// A locale name split into an ISO 639 language and an ISO 3166 country.
///
/// The encoding suffix and the `@modifier` are dropped: `en_US.UTF-8` and
/// `en_US` name the same language and the same country, and nothing here asks
/// anything else. A name with no country - or none at all, which is what the
/// `C` locale and an unset environment amount to - is `en_US`, because a
/// program uses this to pick a localisation and there has to be one.
fn split(name: &str) -> (String, String) {
    let base = name.split(['.', '@']).next().unwrap_or_default();
    let mut parts = base.split(['_', '-']);
    let language = parts.next().filter(|p| !p.is_empty()).unwrap_or("en");
    let country = parts.next().filter(|p| !p.is_empty()).unwrap_or("US");
    (language.to_ascii_lowercase(), country.to_ascii_uppercase())
}

pub(crate) fn language() -> String {
    split(&posix()).0
}

fn country() -> String {
    split(&posix()).1
}

/// The ISO 3166 country this locale names, for a caller deriving a fact about
/// the place rather than reading a preference.
pub(crate) fn country_code() -> String {
    country()
}

/// Whether this locale's language is English.
///
/// The question `crate::formatter` asks before publishing an English month name:
/// a caller told "September" for a French locale has been given a wrong answer it
/// cannot detect, where one told nothing falls back to its own CLDR tables. See
/// that module's note.
pub(crate) fn is_english() -> bool {
    language() == "en"
}

/// `language-REGION`, which is how a BCP 47 tag spells what `posix` returns.
fn tag() -> String {
    format!("{}-{}", language(), country())
}

/// `language_REGION`, the identifier `-localeIdentifier` answers with.
fn identifier() -> String {
    format!("{}_{}", language(), country())
}

/// The one locale, which every spelling of "the locale" returns.
///
/// The current locale, the system locale and the autoupdating one are the same
/// object here: Apple's differ by whether the user's preferences are applied on
/// top of the raw ones, and this host has no preferences database - the
/// environment is the only answer it has, so there is nothing for them to
/// disagree about.
static CURRENT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn shared() -> Id {
    *CURRENT.get_or_init(|| {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSLocale").expect("NSLocale is registered");
        let locale = objc::object::create_instance(class, 0);
        // Retained forever: a singleton a guest can release is one it can make
        // dangle by releasing what it never owned.
        objc::arc::make_immortal(locale);
        locale as usize
    }) as Id
}

fn identifiers() -> &'static std::sync::Mutex<std::collections::HashMap<usize, String>> {
    static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, String>>> =
        std::sync::OnceLock::new();
    IDS.get_or_init(Default::default)
}

fn identifier_of(locale: Id) -> String {
    identifiers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(locale as usize))
        .cloned()
        .unwrap_or_else(identifier)
}

unsafe extern "C" fn init_with_identifier(receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: the argument is an NSString supplied by the caller.
    let name = String::from_utf8_lossy(&unsafe { crate::string::bytes(name) }).into_owned();
    identifiers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(receiver as usize, name);
    receiver
}

unsafe extern "C" fn locale_with_identifier(_class: Id, sel: Sel, name: Id) -> Id {
    let class = objc::class::lookup(c"NSLocale").expect("NSLocale is registered");
    let locale = objc::object::create_instance(class, 0);
    // SAFETY: a fresh locale and the caller's NSString.
    unsafe {
        init_with_identifier(locale, sel, name);
    }
    objc::arc::autorelease(locale)
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    identifiers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    objc::arc::finalize(receiver);
}

fn component_of(locale: Id, key: &[u8]) -> Option<String> {
    let id = identifier_of(locale);
    let base = id.split(['.', '@']).next().unwrap_or_default();
    let parts: Vec<_> = base.split(['_', '-']).collect();
    let region = parts
        .iter()
        .skip(1)
        .find(|p| p.len() == 2 || (p.len() == 3 && p.bytes().all(|c| c.is_ascii_digit())));
    match key {
        b"kCFLocaleIdentifier" => Some(id),
        b"kCFLocaleLanguageCode" => parts
            .first()
            .filter(|p| !p.is_empty())
            .map(|p| p.to_ascii_lowercase()),
        b"kCFLocaleCountryCode" => region.map(|p| p.to_ascii_uppercase()),
        b"kCFLocaleScriptCode" => parts
            .iter()
            .skip(1)
            .find(|p| p.len() == 4)
            .map(|p| p.to_string()),
        b"kCFLocaleMeasurementSystem" => region.map(|p| {
            if matches!(p.to_ascii_uppercase().as_str(), "US" | "LR" | "MM") {
                "U.S.".into()
            } else {
                "Metric".into()
            }
        }),
        _ => None,
    }
}

fn metric_of(locale: Id) -> bool {
    !matches!(
        component_of(locale, b"kCFLocaleCountryCode").as_deref(),
        Some("US" | "LR" | "MM")
    )
}

unsafe extern "C" fn current_locale(_class: Id, _sel: Sel) -> Id {
    shared()
}

/// `+preferredLanguages`, which is the one language this process has.
///
/// One element rather than a guessed-at fallback chain: a program walks the
/// list looking for a localisation it has, and an invented second entry would
/// send it to a language the user never asked for.
unsafe extern "C" fn preferred_languages(_class: Id, _sel: Sel) -> Id {
    let tag = crate::string::from_str(&tag());
    let array = corefoundation::collections::new_array(
        &[tag as corefoundation::base::CFTypeRef],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: both are fresh objects this call owns.
    unsafe {
        corefoundation::api::CFRelease(tag as corefoundation::base::CFTypeRef);
        corefoundation::api::CFAutorelease(array) as Id
    }
}

/// ISO 639-1, which `+ISOLanguageCodes` lists.
const ISO_LANGUAGE_CODES: &[&str] = &[
    "aa", "ab", "ae", "af", "ak", "am", "an", "ar", "as", "av", "ay", "az", "ba", "be", "bg",
    "bh", "bi", "bm", "bn", "bo", "br", "bs", "ca", "ce", "ch", "co", "cr", "cs", "cu", "cv",
    "cy", "da", "de", "dv", "dz", "ee", "el", "en", "eo", "es", "et", "eu", "fa", "ff", "fi",
    "fj", "fo", "fr", "fy", "ga", "gd", "gl", "gn", "gu", "gv", "ha", "he", "hi", "ho", "hr",
    "ht", "hu", "hy", "hz", "ia", "id", "ie", "ig", "ii", "ik", "io", "is", "it", "iu", "ja",
    "jv", "ka", "kg", "ki", "kj", "kk", "kl", "km", "kn", "ko", "kr", "ks", "ku", "kv", "kw",
    "ky", "la", "lb", "lg", "li", "ln", "lo", "lt", "lu", "lv", "mg", "mh", "mi", "mk", "ml",
    "mn", "mr", "ms", "mt", "my", "na", "nb", "nd", "ne", "ng", "nl", "nn", "no", "nr", "nv",
    "ny", "oc", "oj", "om", "or", "os", "pa", "pi", "pl", "ps", "pt", "qu", "rm", "rn", "ro",
    "ru", "rw", "sa", "sc", "sd", "se", "sg", "si", "sk", "sl", "sm", "sn", "so", "sq", "sr",
    "ss", "st", "su", "sv", "sw", "ta", "te", "tg", "th", "ti", "tk", "tl", "tn", "to", "tr",
    "ts", "tt", "tw", "ty", "ug", "uk", "ur", "uz", "ve", "vi", "vo", "wa", "wo", "xh", "yi",
    "yo", "za", "zh", "zu",
];

/// `+ISOLanguageCodes`: every two-letter language code.
unsafe extern "C" fn iso_language_codes(_class: Id, _sel: Sel) -> Id {
    let codes: Vec<_> = ISO_LANGUAGE_CODES
        .iter()
        .map(|code| crate::string::from_str(code) as corefoundation::base::CFTypeRef)
        .collect();
    let array = corefoundation::collections::new_array(
        &codes,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: fresh objects this call owns; the array holds its own references.
    unsafe {
        for code in codes {
            corefoundation::api::CFRelease(code);
        }
        corefoundation::api::CFAutorelease(array) as Id
    }
}

/// The locale components, by the key the constants below name.
///
/// A key this does not know answers nil, which is what Apple's locale does for
/// a component it has no value for.
unsafe extern "C" fn object_for_key(receiver: Id, _sel: Sel, key: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let key = unsafe { crate::string::bytes(key) };
    // `kCFLocaleUsesMetricSystem` is a `CFBoolean`, not a string, so it is
    // answered before the ones that are.
    if key.as_slice() == b"kCFLocaleUsesMetricSystem" {
        return corefoundation::collections::boolean(metric_of(receiver)) as Id;
    }
    match component_of(receiver, key.as_slice()) {
        Some(value) => autoreleased(&value),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn locale_identifier(receiver: Id, _sel: Sel) -> Id {
    autoreleased(&identifier_of(receiver))
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

/// The `NSString *` globals naming a locale component, and their documented
/// values - which are the CoreFoundation spellings, because Foundation's
/// constants *are* CoreFoundation's.
///
/// Published whether or not [`component_of`] answers for them, and that is the
/// point: an unresolved *data* import binds to zero, so a guest asking for the
/// currency code with a key nothing defines passes address zero rather than
/// getting the nil this answers.
pub const KEYS: &[(&str, &str)] = &[
    ("_NSLocaleIdentifier", "kCFLocaleIdentifier"),
    ("_NSLocaleLanguageCode", "kCFLocaleLanguageCode"),
    ("_NSLocaleCountryCode", "kCFLocaleCountryCode"),
    ("_NSLocaleScriptCode", "kCFLocaleScriptCode"),
    ("_NSLocaleVariantCode", "kCFLocaleVariantCode"),
    ("_NSLocaleUsesMetricSystem", "kCFLocaleUsesMetricSystem"),
    ("_NSLocaleMeasurementSystem", "kCFLocaleMeasurementSystem"),
    ("_NSLocaleDecimalSeparator", "kCFLocaleDecimalSeparator"),
    ("_NSLocaleGroupingSeparator", "kCFLocaleGroupingSeparator"),
    ("_NSLocaleCurrencySymbol", "kCFLocaleCurrencySymbol"),
    ("_NSLocaleCurrencyCode", "kCFLocaleCurrencyCode"),
    ("_NSLocaleCalendarIdentifier", "kCFLocaleCalendarIdentifier"),
    (
        "_NSLocaleCollationIdentifier",
        "kCFLocaleCollationIdentifier",
    ),
    // The quotation delimiters, published so the symbol resolves. `component`
    // answers nil for them, which is the point: which quotation marks a language
    // uses is CLDR data this tree does not carry, and a caller told nothing falls
    // back to its own - see `crate::formatter`.
    (
        "_kCFLocaleQuotationBeginDelimiterKey",
        "kCFLocaleQuotationBeginDelimiterKey",
    ),
    (
        "_kCFLocaleQuotationEndDelimiterKey",
        "kCFLocaleQuotationEndDelimiterKey",
    ),
    (
        "_kCFLocaleAlternateQuotationBeginDelimiterKey",
        "kCFLocaleAlternateQuotationBeginDelimiterKey",
    ),
    (
        "_kCFLocaleAlternateQuotationEndDelimiterKey",
        "kCFLocaleAlternateQuotationEndDelimiterKey",
    ),
    ("_kCFLocaleIdentifier", "kCFLocaleIdentifier"),
    ("_kCFLocaleLanguageCode", "kCFLocaleLanguageCode"),
    ("_kCFLocaleCountryCode", "kCFLocaleCountryCode"),
    ("_kCFLocaleScriptCode", "kCFLocaleScriptCode"),
    ("_kCFLocaleVariantCode", "kCFLocaleVariantCode"),
    ("_kCFLocaleUsesMetricSystem", "kCFLocaleUsesMetricSystem"),
    ("_kCFLocaleMeasurementSystem", "kCFLocaleMeasurementSystem"),
    ("_kCFLocaleDecimalSeparator", "kCFLocaleDecimalSeparator"),
    ("_kCFLocaleGroupingSeparator", "kCFLocaleGroupingSeparator"),
    ("_kCFLocaleCurrencySymbol", "kCFLocaleCurrencySymbol"),
    ("_kCFLocaleCurrencyCode", "kCFLocaleCurrencyCode"),
    (
        "_kCFLocaleCalendarIdentifier",
        "kCFLocaleCalendarIdentifier",
    ),
    (
        "_kCFLocaleCollationIdentifier",
        "kCFLocaleCollationIdentifier",
    ),
];

/// The CoreFoundation spellings, which name the same locale as the class above.
pub mod cf {
    // The C spellings are the interface; renaming them would only hide it.
    #![allow(non_snake_case)]

    use objc::object::Id;

    /// `CFLocaleRef CFLocaleCopyCurrent(void)`.
    ///
    /// A `Copy`, so the caller owns a reference and will release it - which is
    /// the only difference between this and [`CFLocaleGetSystem`] here.
    unsafe extern "C" fn CFLocaleCopyCurrent() -> Id {
        let locale = super::shared();
        objc::arc::retain(locale);
        locale
    }

    /// `CFStringRef CFLocaleGetIdentifier(CFLocaleRef)`.
    ///
    /// A `Get`, so the string has to outlive the call without the caller holding
    /// a reference: it is interned rather than made fresh.
    unsafe extern "C" fn CFLocaleGetIdentifier(locale: Id) -> Id {
        interned(&super::identifier_of(locale))
    }

    /// `CFTypeRef CFLocaleGetValue(CFLocaleRef, CFLocaleKey key)`.
    ///
    /// The same components `-objectForKey:` answers, by the same key text - a
    /// guest that reads `kCFLocaleLanguageCode` and one that reads
    /// `NSLocaleLanguageCode` are naming one string, so they cannot disagree.
    /// A `Get` again, so the value is interned.
    unsafe extern "C" fn CFLocaleGetValue(locale: Id, key: Id) -> Id {
        // SAFETY: an `NSString`, or nil.
        let key = unsafe { crate::string::bytes(key) };
        if key.as_slice() == b"kCFLocaleUsesMetricSystem" {
            return corefoundation::collections::boolean(super::metric_of(locale)) as Id;
        }
        match super::component_of(locale, key.as_slice()) {
            Some(value) => interned(&value),
            None => std::ptr::null_mut(),
        }
    }

    /// A string that outlives the call, one object per distinct text.
    ///
    /// Retained forever, which is what a `Get` requires and what makes handing
    /// the same object out twice safe: a guest that released one would make
    /// every later read of it dangle.
    fn interned(text: &str) -> Id {
        static POOL: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, object)) = pool.iter().find(|(t, _)| t == text) {
            return *object as Id;
        }
        let object = crate::string::from_str(text);
        objc::arc::make_immortal(object);
        pool.push((text.to_string(), object as usize));
        object
    }

    /// `CFLocaleRef CFLocaleGetSystem(void)`.
    ///
    /// A `Get`, so the caller does not own it and will not release it - which
    /// the singleton already is: retained forever, and handed out without a
    /// reference. It is the same object `+[NSLocale systemLocale]` answers, for
    /// the reason [`super::CURRENT`] records.
    unsafe extern "C" fn CFLocaleGetSystem() -> Id {
        super::shared()
    }

    /// `CFArrayRef CFLocaleCopyPreferredLanguages(void)`: the languages
    /// `+[NSLocale preferredLanguages]` answers, owned by the caller.
    unsafe extern "C" fn CFLocaleCopyPreferredLanguages() -> Id {
        // SAFETY: a class method that ignores its receiver and selector, and
        // answers an autoreleased array this call takes a reference to.
        unsafe {
            let array = super::preferred_languages(
                std::ptr::null_mut(),
                objc::sel::get("preferredLanguages"),
            );
            objc::arc::retain(array)
        }
    }

    exports::exports! {
        functions {
            "CFLocaleCopyPreferredLanguages" => CFLocaleCopyPreferredLanguages,
            "CFLocaleGetSystem" => CFLocaleGetSystem,
            "CFLocaleCopyCurrent" => CFLocaleCopyCurrent,
            "CFLocaleGetIdentifier" => CFLocaleGetIdentifier,
            "CFLocaleGetValue" => CFLocaleGetValue,
        }
    }
}

pub(crate) fn install() {
    let class =
        corefoundation::base::register(c"NSLocale", None, std::mem::size_of::<usize>(), false);
    class_methods!(class, {
        "localeWithLocaleIdentifier:" => locale_with_identifier, c"@24@0:8@16";
        "currentLocale" => current_locale, c"@16@0:8";
        "autoupdatingCurrentLocale" => current_locale, c"@16@0:8";
        "systemLocale" => current_locale, c"@16@0:8";
        "preferredLanguages" => preferred_languages, c"@16@0:8";
        "ISOLanguageCodes" => iso_language_codes, c"@16@0:8";
    });
    methods!(class, {
        "initWithLocaleIdentifier:" => init_with_identifier, c"@24@0:8@16";
        "dealloc" => dealloc, c"v16@0:8";
        "objectForKey:" => object_for_key, c"@24@0:8@16";
        "localeIdentifier" => locale_identifier, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_locales_keep_their_identifiers_and_components() {
        crate::bootstrap();
        let pool = objc::arc::pool_push();
        for (name, language, country) in [("fr_CA", "fr", "CA"), ("zh_Hant_TW", "zh", "TW")] {
            let string = crate::string::from_str(name);
            // SAFETY: the argument is a live NSString and the result is a locale.
            let locale = unsafe {
                locale_with_identifier(
                    std::ptr::null_mut(),
                    objc::sel::get("localeWithLocaleIdentifier:"),
                    string,
                )
            };
            objc::arc::release(string);
            assert_eq!(identifier_of(locale), name);
            assert_eq!(
                component_of(locale, b"kCFLocaleLanguageCode").as_deref(),
                Some(language)
            );
            assert_eq!(
                component_of(locale, b"kCFLocaleCountryCode").as_deref(),
                Some(country)
            );
            let get: unsafe extern "C" fn(Id) -> Id = unsafe {
                std::mem::transmute(cf::symbol("CFLocaleGetIdentifier").unwrap() as usize)
            };
            assert_eq!(
                unsafe { crate::string::bytes(get(locale)) },
                name.as_bytes()
            );
        }
        objc::arc::pool_pop(pool);
    }

    /// A `Get` hands back the same object every time and does not hand over a
    /// reference with it - so the one thing that must be true is that it is the
    /// singleton, and not a fresh locale the caller would be expected to free.
    #[test]
    fn the_system_locale_is_the_one_locale() {
        crate::bootstrap();
        let address = cf::symbol("CFLocaleGetSystem").expect("the name has to resolve");
        // SAFETY: the address of `CFLocaleGetSystem`, which takes no argument
        // and returns an object.
        let system: unsafe extern "C" fn() -> Id = unsafe { std::mem::transmute(address as usize) };
        let (first, second) = unsafe { (system(), system()) };
        assert!(!first.is_null());
        assert_eq!(first, second, "a Get answers with one object");
        assert_eq!(
            first,
            shared(),
            "and it is the locale every other call gets"
        );
    }

    /// The components a caller reads through the CoreFoundation spelling are
    /// the ones the class answers, and a component this host has no data for is
    /// nil rather than an invented one.
    #[test]
    fn the_locale_answers_the_components_it_can_derive() {
        crate::bootstrap();
        let address = cf::symbol("CFLocaleGetValue").expect("the name has to resolve");
        // SAFETY: the address of `CFLocaleGetValue`, which takes a locale and a
        // key and returns an object.
        let value: unsafe extern "C" fn(Id, Id) -> Id =
            unsafe { std::mem::transmute(address as usize) };
        let ask = |key: &str| {
            let key = crate::string::from_str(key);
            // SAFETY: a live locale and a string this test made.
            unsafe { value(shared(), key) }
        };
        let text = |object: Id| {
            // SAFETY: an `NSString` this runtime made, or nil.
            String::from_utf8(unsafe { crate::string::bytes(object) }).expect("utf-8")
        };
        assert_eq!(text(ask("kCFLocaleIdentifier")), identifier());
        assert_eq!(text(ask("kCFLocaleLanguageCode")), language());
        assert_eq!(text(ask("kCFLocaleCountryCode")), country());
        assert!(
            ask("kCFLocaleCurrencyCode").is_null(),
            "a currency is locale data this tree does not carry"
        );
        // A `Get`, so the same object each time rather than one per call.
        assert_eq!(ask("kCFLocaleIdentifier"), ask("kCFLocaleIdentifier"));
    }

    /// Whether the country measures in metres is a fact about the country, and
    /// the answer is a boolean object rather than a string.
    /// Whether the country measures in metres is a fact about the country, and
    /// the two answers derived from it cannot disagree.
    #[test]
    fn the_measurement_system_follows_the_country() {
        crate::bootstrap();
        let system =
            component_of(shared(), b"kCFLocaleMeasurementSystem").expect("every country has one");
        assert_eq!(system == "Metric", metric_of(shared()));
        assert_eq!(system == "U.S.", country() == "US");
        assert!(
            component_of(shared(), b"kCFLocaleDecimalSeparator").is_none(),
            "a separator is locale data this tree does not carry"
        );
    }

    #[test]
    fn a_posix_name_splits_into_a_language_and_a_country() {
        assert_eq!(split("fr_CA.UTF-8"), ("fr".into(), "CA".into()));
        assert_eq!(split("en_GB@euro"), ("en".into(), "GB".into()));
        assert_eq!(split("de"), ("de".into(), "US".into()));
        assert_eq!(split(""), ("en".into(), "US".into()));
    }
}
