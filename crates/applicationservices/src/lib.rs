mod colorsync;
mod printing;

#[repr(i32)]
enum CGWindowLevelKey {
    _BaseWindow = 0,
}

type CGWindowLevel = i32;

// FIXME: Window levels are not implemented; every key returns zero.
#[allow(non_snake_case)]
extern "C" fn CGWindowLevelForKey(_key: CGWindowLevelKey) -> CGWindowLevel {
    0
}

exports::exports! {
    functions {
        "CGWindowLevelForKey" => CGWindowLevelForKey,
    }
    groups { colorsync, printing }
}
