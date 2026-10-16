//! Loads font files through fontconfig the first time a family or a character needs them, instead of
//! parsing every installed font at startup (800 files and ~720 ms of a cold launch on the reference box).

use fontconfig_sys as fc;
use fontconfig_sys::constants::{FC_CHARSET, FC_FAMILY, FC_FILE};
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

/// Remembers what fontconfig was already asked, so each family, character and file costs one lookup.
#[derive(Default)]
pub(crate) struct OnDemandFonts {
    loaded_files: HashSet<PathBuf>,
    tried_families: HashSet<String>,
    tried_chars: HashSet<char>,
}

impl OnDemandFonts {
    /// Not-yet-loaded files of the family named exactly `family`, the rule the full scan matched by.
    pub(crate) fn new_family_files(&mut self, family: &str) -> Vec<PathBuf> {
        if !self.tried_families.insert(family.to_string()) {
            return Vec::new();
        }
        let files = family_files(family);
        self.keep_unloaded(files)
    }

    /// Not-yet-loaded files of fontconfig's preferred face for each character not tried before.
    pub(crate) fn new_covering_files(&mut self, chars: impl IntoIterator<Item = char>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for ch in chars {
            if self.tried_chars.insert(ch) {
                files.extend(file_covering(ch));
            }
        }
        self.keep_unloaded(files)
    }

    fn keep_unloaded(&mut self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        files
            .into_iter()
            .filter(|path| self.loaded_files.insert(path.clone()))
            .collect()
    }
}

/// Every installed family name, read from fontconfig's cache rather than from the font files.
pub(crate) fn family_names() -> Vec<String> {
    // Safety: every fontconfig object created here is destroyed before returning; strings are copied out first.
    unsafe {
        let config = fc::FcConfigGetCurrent();
        let pattern = fc::FcPatternCreate();
        let objects = fc::FcObjectSetCreate();
        if config.is_null() || pattern.is_null() || objects.is_null() {
            destroy(pattern, objects);
            return Vec::new();
        }
        fc::FcObjectSetAdd(objects, FC_FAMILY.as_ptr());
        let set = fc::FcFontList(config, pattern, objects);
        let names = font_set_strings(set, FC_FAMILY)
            .into_iter()
            .map(|name| String::from_utf8_lossy(&name).into_owned())
            .collect();
        if !set.is_null() {
            fc::FcFontSetDestroy(set);
        }
        destroy(pattern, objects);
        names
    }
}

fn family_files(family: &str) -> Vec<PathBuf> {
    let Ok(family) = CString::new(family) else {
        return Vec::new();
    };
    // Safety: every fontconfig object created here is destroyed before returning; strings are copied out first.
    unsafe {
        let config = fc::FcConfigGetCurrent();
        let pattern = fc::FcPatternCreate();
        let objects = fc::FcObjectSetCreate();
        if config.is_null() || pattern.is_null() || objects.is_null() {
            destroy(pattern, objects);
            return Vec::new();
        }
        fc::FcPatternAddString(pattern, FC_FAMILY.as_ptr(), family.as_ptr().cast());
        fc::FcObjectSetAdd(objects, FC_FILE.as_ptr());
        let set = fc::FcFontList(config, pattern, objects);
        let files = font_set_strings(set, FC_FILE)
            .into_iter()
            .map(|bytes| PathBuf::from(OsStr::from_bytes(&bytes)))
            .collect();
        if !set.is_null() {
            fc::FcFontSetDestroy(set);
        }
        destroy(pattern, objects);
        files
    }
}

/// The file of fontconfig's best match for `ch`, only when that face really carries the glyph.
fn file_covering(ch: char) -> Option<PathBuf> {
    // Safety: every fontconfig object created here is destroyed before returning; strings are copied out first.
    unsafe {
        let config = fc::FcConfigGetCurrent();
        let pattern = fc::FcPatternCreate();
        let charset = fc::FcCharSetCreate();
        if config.is_null() || pattern.is_null() || charset.is_null() {
            if !charset.is_null() {
                fc::FcCharSetDestroy(charset);
            }
            destroy(pattern, ptr::null_mut());
            return None;
        }
        fc::FcCharSetAddChar(charset, ch as fc::FcChar32);
        fc::FcPatternAddCharSet(pattern, FC_CHARSET.as_ptr(), charset);
        fc::FcCharSetDestroy(charset);
        fc::FcConfigSubstitute(config, pattern, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pattern);
        let mut result = fc::FcResultNoMatch;
        let matched = fc::FcFontMatch(config, pattern, &mut result);
        fc::FcPatternDestroy(pattern);
        if matched.is_null() {
            return None;
        }
        let file = if pattern_has_char(matched, ch) {
            pattern_string(matched, FC_FILE).map(|bytes| PathBuf::from(OsStr::from_bytes(&bytes)))
        } else {
            None
        };
        fc::FcPatternDestroy(matched);
        file
    }
}

unsafe fn pattern_has_char(pattern: *mut fc::FcPattern, ch: char) -> bool {
    let mut charset: *mut fc::FcCharSet = ptr::null_mut();
    // Safety: `pattern` is a live fontconfig pattern; the charset it returns is borrowed, not owned.
    unsafe {
        fc::FcPatternGetCharSet(pattern, FC_CHARSET.as_ptr(), 0, &mut charset) == fc::FcResultMatch
            && !charset.is_null()
            && fc::FcCharSetHasChar(charset, ch as fc::FcChar32) != 0
    }
}

unsafe fn pattern_string(pattern: *mut fc::FcPattern, object: &CStr) -> Option<Vec<u8>> {
    let mut value: *mut fc::FcChar8 = ptr::null_mut();
    // Safety: `pattern` is a live fontconfig pattern; the string it returns is copied before the pattern dies.
    unsafe {
        if fc::FcPatternGetString(pattern, object.as_ptr(), 0, &mut value) != fc::FcResultMatch
            || value.is_null()
        {
            return None;
        }
        Some(CStr::from_ptr(value.cast::<c_char>()).to_bytes().to_vec())
    }
}

unsafe fn font_set_strings(set: *mut fc::FcFontSet, object: &CStr) -> Vec<Vec<u8>> {
    if set.is_null() {
        return Vec::new();
    }
    // Safety: `set` is a live font set whose `fonts` array holds `nfont` patterns.
    unsafe {
        let set = &*set;
        (0..set.nfont.max(0) as usize)
            .filter_map(|ix| pattern_string(*set.fonts.add(ix), object))
            .collect()
    }
}

unsafe fn destroy(pattern: *mut fc::FcPattern, objects: *mut fc::FcObjectSet) {
    // Safety: both pointers are either null or owned by the caller and not used afterwards.
    unsafe {
        if !pattern.is_null() {
            fc::FcPatternDestroy(pattern);
        }
        if !objects.is_null() {
            fc::FcObjectSetDestroy(objects);
        }
    }
}
