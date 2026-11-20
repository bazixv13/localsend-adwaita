//! Translation support.
//!
//! Messages are looked up with GNU gettext, which glibc already provides, so
//! no additional dependency is needed. Translations live in `po/` and are
//! compiled to `.mo` files under `share/locale/<lang>/LC_MESSAGES`.
//!
//! Every user-facing string is passed through [`t`]; untranslated strings fall
//! back to the English msgid, so a missing translation never leaves a blank
//! label.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_ulong};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Matches the gettext domain installed by `po/*.po` and the Flatpak manifest.
const DOMAIN: &str = "localsend-adwaita";
/// `LC_ALL` on glibc. Only Linux is supported, like the rest of the app.
const LC_ALL: c_int = 6;

extern "C" {
    fn bindtextdomain(domainname: *const c_char, dirname: *const c_char) -> *mut c_char;
    fn textdomain(domainname: *const c_char) -> *mut c_char;
    fn gettext(msgid: *const c_char) -> *mut c_char;
    fn ngettext(msgid1: *const c_char, msgid2: *const c_char, n: c_ulong) -> *mut c_char;
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
}

static INIT: OnceLock<()> = OnceLock::new();

/// Prepares gettext. Must run before the first translated string is built,
/// and before GTK reads any of its own settings.
pub fn init() {
    INIT.get_or_init(|| {
        unsafe {
            // An empty locale means "use LANG/LC_*". Without this gettext
            // always answers with the msgid.
            let applied = setlocale(LC_ALL, c"".as_ptr());
            // A minimal sandbox image may not have the requested locale
            // generated. glibc still honours LANGUAGE in C.UTF-8, so fall back
            // to it rather than staying in the C locale.
            if applied.is_null() || locale_is_c() {
                setlocale(LC_ALL, c"C.UTF-8".as_ptr());
            }
        }
        let Some(dir) = locale_dir() else {
            return;
        };
        let Ok(domain) = CString::new(DOMAIN) else {
            return;
        };
        let Ok(path) = CString::new(dir.to_string_lossy().into_owned()) else {
            return;
        };
        unsafe {
            bindtextdomain(domain.as_ptr(), path.as_ptr());
            textdomain(domain.as_ptr());
        }
    });
}

/// Whether the current locale is plain C or POSIX, where gettext ignores
/// `LANGUAGE` and never translates.
fn locale_is_c() -> bool {
    // SAFETY: passing a null pointer only reads the current locale name.
    unsafe {
        let current = setlocale(LC_ALL, std::ptr::null());
        if current.is_null() {
            return true;
        }
        matches!(CStr::from_ptr(current).to_bytes(), b"C" | b"POSIX")
    }
}

/// Where the compiled catalogues live. The application's own directory comes
/// before the system one so a release tarball installed into `~/.local` and a
/// Flatpak under `/app` both find their own translations. Flatpak installations
/// use a private directory because `/app/share/locale` is split into a locale
/// extension that is only downloaded for languages the user selected.
fn locale_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LOCALSEND_LOCALEDIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin) = exe.parent() {
            candidates.push(bin.join("../share/localsend-adwaita/locale"));
            candidates.push(bin.join("../share/locale"));
        }
    }
    candidates.push(PathBuf::from("/app/share/localsend-adwaita/locale"));
    candidates.push(PathBuf::from("/app/share/locale"));
    candidates.push(PathBuf::from("/usr/local/share/locale"));
    candidates.push(PathBuf::from("/usr/share/locale"));
    candidates
        .into_iter()
        .find(|dir| dir.is_dir())
        .and_then(|dir| dir.canonicalize().ok().or(Some(dir)))
}

/// Translates one message. The return value is a `String` because gettext
/// owns its storage; callers pass it as `&str`.
pub fn t(message: &str) -> String {
    let Ok(text) = CString::new(message) else {
        return message.to_string();
    };
    unsafe {
        let translated = gettext(text.as_ptr());
        if translated.is_null() {
            message.to_string()
        } else {
            CStr::from_ptr(translated).to_string_lossy().into_owned()
        }
    }
}

/// Translates a template and fills its `{}` placeholders in order.
///
/// `format!` requires a literal format string, so a translated template cannot
/// be passed to it. This walks the template instead, leaving text after the
/// last placeholder untouched.
pub fn fmt(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    fill(&t(template), args)
}

/// Chooses the singular or plural form and fills the placeholders, with the
/// first `{}` receiving `number`. Languages with more than two plural forms
/// get the right one from gettext instead of an English "s".
pub fn np(number: u64, singular: &str, plural: &str, args: &[&dyn std::fmt::Display]) -> String {
    let number_text = number.to_string();
    let (Ok(one), Ok(many)) = (CString::new(singular), CString::new(plural)) else {
        return fill(plural, args).replacen("{}", &number_text, 1);
    };
    let count = c_ulong::try_from(number).unwrap_or(c_ulong::MAX);
    // SAFETY: both msgids are valid for the duration of the call.
    let translated = unsafe { ngettext(one.as_ptr(), many.as_ptr(), count) };
    let template = if translated.is_null() {
        plural.to_string()
    } else {
        unsafe { CStr::from_ptr(translated) }
            .to_string_lossy()
            .into_owned()
    };
    // The count goes into the first placeholder; the rest belong to `args`.
    let with_number = fill(&template, &[&number_text]);
    fill(&with_number, args)
}

fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    for arg in args {
        let Some(index) = rest.find("{}") else {
            break;
        };
        out.push_str(&rest[..index]);
        out.push_str(&arg.to_string());
        rest = &rest[index + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untranslated_messages_keep_their_text() {
        // The test harness runs in the C locale, so gettext returns the msgid.
        assert_eq!(t("Cancel"), "Cancel");
    }

    #[test]
    fn plurals_pick_the_right_form() {
        assert_eq!(np(1, "{} file", "{} files", &[]), "1 file");
        assert_eq!(np(4, "{} file", "{} files", &[]), "4 files");
        assert_eq!(
            np(2, "{} file ({})", "{} files ({})", &[&"12 MB"]),
            "2 files (12 MB)"
        );
    }

    #[test]
    fn fills_placeholders_in_order() {
        assert_eq!(
            fmt("{} files, {}", &[&3, &"two folders"]),
            "3 files, two folders"
        );
        assert_eq!(fmt("no placeholders", &[&1]), "no placeholders");
        assert_eq!(fmt("{} and {}", &[&1]), "1 and {}");
    }
}
