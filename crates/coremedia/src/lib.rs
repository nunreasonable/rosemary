#![allow(clippy::missing_safety_doc)]

pub mod format_description;
mod h264;
pub mod sample_buffer;

/// The extension keys and values, as the constant strings a guest imports.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    corefoundation::base::string_globals(format_description::KEYS)
}

exports::exports! {
    groups {
        format_description,
    }
}
