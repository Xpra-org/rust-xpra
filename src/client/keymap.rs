// src/client/keymap.rs
//! The keyboard layout the server loads for this session.
//!
//! Without one the server keeps its own - `"us"` unless it was started otherwise
//! (`parse_layout`, xpra `x11/server/keyboard_config.py`) - and then every key that layout does
//! not have simply cannot be pressed: we name keys by keysym, and a keysym the server keymap has
//! no keycode for resolves to nothing at all (`find_matching_keycode`). That is why a Spanish
//! keyboard could not type a `\u{f1}` against a default server, and an Arabic one nothing at all.
//!
//! Windows names its layouts by LANGID and X11 by the xkb names the server wants, so the table
//! below translates between the two. It is xpra's own (`WIN32_LAYOUTS`, xpra
//! `keyboard/layouts.py`), minus the few entries it has no xkb name for.

use std::env;

/// Overrides the detected layout, in the xkb names the server expects (`es`, `latam`, `de`, ...).
/// It is the only way to set one where we cannot detect it, which is everywhere but Windows.
const LAYOUT_ENV: &str = "XPRA_KEYBOARD_LAYOUT";

/// The layout to ask the server for, or `None` to leave the one it already has alone.
pub fn local_layout() -> Option<String> {
    if let Ok(name) = env::var(LAYOUT_ENV) {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    platform_layout().map(str::to_string)
}

#[cfg(windows)]
fn platform_layout() -> Option<&'static str> {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
    // A layout is per thread on Windows, and this runs on the UI thread - the one owning the
    // windows the user types into, so its layout is the one whose keys we forward. One changed
    // mid-session is not picked up: saying so needs a `keymap-changed` packet, which this client
    // does not send. The low word of the HKL is the LANGID naming the layout.
    let hkl = unsafe { GetKeyboardLayout(0) };
    layout_for_langid((hkl.0 as usize & 0xffff) as u16)
}

#[cfg(not(windows))]
fn platform_layout() -> Option<&'static str> {
    // X11 and Wayland both keep this in xkb, which winit does not expose, so the environment
    // override above is the only source here.
    None
}

fn layout_for_langid(langid: u16) -> Option<&'static str> {
    LANGID_LAYOUTS
        .binary_search_by_key(&langid, |&(id, _)| id)
        .ok()
        .map(|index| LANGID_LAYOUTS[index].1)
}

/// LANGID -> xkb layout, sorted so it can be searched. From xpra `keyboard/layouts.py`.
static LANGID_LAYOUTS: &[(u16, &str)] = &[
    (0x401, "ar"),
    (0x402, "bg"),
    (0x403, "ad"),
    (0x404, "tw"),
    (0x405, "cz"),
    (0x406, "dk"),
    (0x407, "de"),
    (0x408, "gr"),
    (0x409, "us"),
    (0x40a, "es"),
    (0x40b, "fi"),
    (0x40c, "fr"),
    (0x40d, "il"),
    (0x40e, "hu"),
    (0x40f, "is"),
    (0x410, "it"),
    (0x411, "jp"),
    (0x412, "kr"),
    (0x413, "nl"),
    (0x414, "no"),
    (0x415, "pl"),
    (0x416, "br"),
    (0x418, "ro"),
    (0x419, "ru"),
    (0x41a, "hr"),
    (0x41b, "sk"),
    (0x41c, "al"),
    (0x41d, "se"),
    (0x41e, "th"),
    (0x41f, "tr"),
    (0x420, "pk"),
    (0x422, "ua"),
    (0x423, "by"),
    (0x424, "si"),
    (0x425, "ee"),
    (0x426, "lv"),
    (0x427, "lt"),
    (0x429, "ir"),
    (0x42a, "vn"),
    (0x42b, "am"),
    (0x42c, "az"),
    (0x42d, "es"),
    (0x42f, "mk"),
    (0x437, "ge"),
    (0x438, "fo"),
    (0x439, "in"),
    (0x43e, "in"),
    (0x43f, "kz"),
    (0x440, "kg"),
    (0x441, "ke"),
    (0x443, "uz"),
    (0x444, "ru"),
    (0x446, "in"),
    (0x447, "in"),
    (0x449, "in"),
    (0x44a, "in"),
    (0x44b, "in"),
    (0x44e, "in"),
    (0x44f, "in"),
    (0x450, "mn"),
    (0x456, "es"),
    (0x457, "in"),
    (0x45a, "sy"),
    (0x801, "iq"),
    (0x804, "cn"),
    (0x807, "de"),
    (0x809, "gb"),
    (0x80a, "es"),
    (0x80c, "be"),
    (0x810, "it"),
    (0x813, "nl"),
    (0x814, "no"),
    (0x816, "pt"),
    (0x81a, "rs"),
    (0x81d, "se"),
    (0x82c, "az"),
    (0x83e, "in"),
    (0x843, "uz"),
    (0xc01, "ara"),
    (0xc04, "cn"),
    (0xc07, "at"),
    (0xc09, "us"),
    (0xc0a, "es"),
    (0xc0c, "ca"),
    (0x1001, "ara"),
    (0x1004, "cn"),
    (0x1007, "de"),
    (0x1009, "ca"),
    (0x100a, "latam"),
    (0x100c, "ch"),
    (0x1401, "ara"),
    (0x1404, "cn"),
    (0x1407, "de"),
    (0x1409, "us"),
    (0x140a, "latam"),
    (0x140c, "fr"),
    (0x1801, "ara"),
    (0x1809, "en"),
    (0x180a, "latam"),
    (0x180c, "fr"),
    (0x1c01, "ara"),
    (0x1c09, "en"),
    (0x1c0a, "latam"),
    (0x2001, "ara"),
    (0x2009, "en"),
    (0x200a, "latam"),
    (0x2401, "ara"),
    (0x2409, "en"),
    (0x240a, "latam"),
    (0x2801, "sy"),
    (0x2809, "us"),
    (0x280a, "latam"),
    (0x2c01, "ara"),
    (0x2c09, "us"),
    (0x2c0a, "latam"),
    (0x3001, "ara"),
    (0x3009, "us"),
    (0x300a, "latam"),
    (0x3401, "ara"),
    (0x3409, "us"),
    (0x340a, "latam"),
    (0x3801, "ara"),
    (0x3809, "us"),
    (0x380a, "latam"),
    (0x3c01, "ara"),
    (0x3c09, "us"),
    (0x3c0a, "latam"),
    (0x4001, "ara"),
    (0x4009, "us"),
    (0x400a, "latam"),
    (0x4409, "us"),
    (0x440a, "latam"),
    (0x4809, "us"),
    (0x480a, "latam"),
    (0x4c0a, "latam"),
    (0x500a, "latam"),
    (0xe40a, "latam"),
    (0xe40c, "fr"),
];


#[cfg(test)]
mod tests {
    use super::{layout_for_langid, LANGID_LAYOUTS};

    #[test]
    fn langids_map_to_the_xkb_layout_names_the_server_wants() {
        assert_eq!(layout_for_langid(0x0409), Some("us"));
        // the two Spanish ones differ, which is why the table is keyed on the whole LANGID and
        // not on the primary language: Spain is `es`, Argentina is `latam`
        assert_eq!(layout_for_langid(0x040a), Some("es"));
        assert_eq!(layout_for_langid(0x2c0a), Some("latam"));
        assert_eq!(layout_for_langid(0x0401), Some("ar"));
        // an unknown one leaves the server's layout alone rather than guessing at it
        assert_eq!(layout_for_langid(0xffff), None);
    }

    #[test]
    fn the_table_is_sorted_so_the_search_works() {
        assert!(LANGID_LAYOUTS.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }
}
