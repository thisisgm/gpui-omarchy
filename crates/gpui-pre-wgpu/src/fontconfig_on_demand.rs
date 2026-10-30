//! Loads font files through fontconfig the first time a family or a character needs them, instead of
//! parsing every installed font at startup (800 files and ~720 ms of a cold launch on the reference box).

use anyhow::{Context as _, Result};
use fontconfig_sys as fc;
use fontconfig_sys::constants::{FC_CHARSET, FC_FAMILY, FC_FILE, FC_LANG};
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

/// Remembers what fontconfig was already asked, so each family, character and file costs one lookup.
#[derive(Default)]
pub(crate) struct OnDemandFonts {
    loaded_files: HashSet<PathBuf>,
    tried_families: HashSet<String>,
    tried_chars: HashSet<char>,
    config: Option<FontConfig>,
    locale: Option<CString>,
    frozen_names: Option<Vec<String>>,
}

impl OnDemandFonts {
    /// Reloads user rules into an owned configuration without changing process-global fontconfig state.
    pub(crate) fn reload(locale: &str, file: Option<&Path>) -> Result<Self> {
        let config = FontConfig::load(file)?;
        Ok(Self {
            config: Some(config),
            locale: Some(CString::new(locale)?),
            ..Self::default()
        })
    }

    fn config(&self) -> *mut fc::FcConfig {
        self.config
            .as_ref()
            .map_or_else(|| unsafe { fc::FcConfigGetCurrent() }, |config| config.0)
    }

    /// Retains the exact matched family, because one TTC file can contain all five CJK variants.
    pub(crate) fn covering(&mut self, family: &str, ch: char) -> Result<Option<(PathBuf, String)>> {
        anyhow::ensure!(self.frozen_names.is_none(), "fontconfig snapshot is frozen");
        file_covering(self.config(), family, self.locale.as_deref(), ch)
    }

    /// Disables all further fontconfig I/O before this snapshot reaches the UI thread.
    pub(crate) fn freeze(&mut self) {
        self.frozen_names = Some(family_names_with_config(self.config()));
    }

    pub(crate) fn family_names(&self) -> Vec<String> {
        self.frozen_names
            .clone()
            .unwrap_or_else(|| family_names_with_config(self.config()))
    }

    /// Not-yet-loaded files of the family named exactly `family`, the rule the full scan matched by.
    pub(crate) fn new_family_files(&mut self, family: &str) -> Vec<PathBuf> {
        if self.frozen_names.is_some() {
            return Vec::new();
        }
        if !self.tried_families.insert(family.to_string()) {
            return Vec::new();
        }
        let files = family_files(self.config(), family);
        self.keep_unloaded(files)
    }

    /// Not-yet-loaded files of fontconfig's preferred face for each character not tried before.
    pub(crate) fn new_covering_files(
        &mut self,
        chars: impl IntoIterator<Item = char>,
    ) -> Vec<PathBuf> {
        if self.frozen_names.is_some() {
            return Vec::new();
        }
        let mut files = Vec::new();
        for ch in chars {
            if self.tried_chars.insert(ch) {
                files.extend(
                    file_covering(self.config(), "sans-serif", self.locale.as_deref(), ch)
                        .ok()
                        .flatten()
                        .map(|(file, _)| file),
                );
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
fn family_names_with_config(config: *mut fc::FcConfig) -> Vec<String> {
    // Safety: every fontconfig object created here is destroyed before returning; strings are copied out first.
    unsafe {
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

fn family_files(config: *mut fc::FcConfig, family: &str) -> Vec<PathBuf> {
    let Ok(family) = CString::new(family) else {
        return Vec::new();
    };
    // Safety: every fontconfig object created here is destroyed before returning; strings are copied out first.
    unsafe {
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
fn file_covering(
    config: *mut fc::FcConfig,
    family: &str,
    locale: Option<&CStr>,
    ch: char,
) -> Result<Option<(PathBuf, String)>> {
    let family = CString::new(family)?;
    // Safety: all allocated patterns and charsets are destroyed and all returned strings are copied.
    unsafe {
        let pattern = fc::FcPatternCreate();
        let charset = fc::FcCharSetCreate();
        if config.is_null() || pattern.is_null() || charset.is_null() {
            if !charset.is_null() {
                fc::FcCharSetDestroy(charset);
            }
            destroy(pattern, ptr::null_mut());
            anyhow::bail!("fontconfig could not allocate a match");
        }
        fc::FcCharSetAddChar(charset, ch as fc::FcChar32);
        fc::FcPatternAddCharSet(pattern, FC_CHARSET.as_ptr(), charset);
        fc::FcCharSetDestroy(charset);
        fc::FcPatternAddString(pattern, FC_FAMILY.as_ptr(), family.as_ptr().cast());
        if let Some(locale) = locale {
            fc::FcPatternAddString(pattern, FC_LANG.as_ptr(), locale.as_ptr().cast());
        }
        if fc::FcConfigSubstitute(config, pattern, fc::FcMatchPattern) == 0 {
            fc::FcPatternDestroy(pattern);
            anyhow::bail!("fontconfig could not substitute the requested locale");
        }
        fc::FcDefaultSubstitute(pattern);
        let mut result = fc::FcResultNoMatch;
        let matched = fc::FcFontMatch(config, pattern, &mut result);
        fc::FcPatternDestroy(pattern);
        if matched.is_null() {
            return Ok(None);
        }
        let value = if pattern_has_char(matched, ch) {
            pattern_string(matched, FC_FILE)
                .zip(pattern_string(matched, FC_FAMILY))
                .map(|(path, family)| {
                    (
                        PathBuf::from(OsStr::from_bytes(&path)),
                        String::from_utf8_lossy(&family).into_owned(),
                    )
                })
        } else {
            None
        };
        fc::FcPatternDestroy(matched);
        Ok(value)
    }
}

struct FontConfig(*mut fc::FcConfig);

// Safety: this private config is never installed globally; all access is serialized by the text system lock.
unsafe impl Send for FontConfig {}
// Safety: the containing text system's lock serializes config access and destruction.
unsafe impl Sync for FontConfig {}

impl FontConfig {
    fn load(file: Option<&Path>) -> Result<Self> {
        // Safety: each successful allocation has one owner and is destroyed on all error and drop paths.
        unsafe {
            if let Some(file) = file {
                let file = CString::new(file.as_os_str().as_bytes())?;
                let config = Self(fc::FcConfigCreate());
                anyhow::ensure!(
                    !config.0.is_null(),
                    "fontconfig could not create a configuration"
                );
                anyhow::ensure!(
                    fc::FcConfigParseAndLoad(config.0, file.as_ptr().cast(), 1) != 0,
                    "fontconfig configuration could not be loaded"
                );
                anyhow::ensure!(
                    fc::FcConfigBuildFonts(config.0) != 0,
                    "fontconfig font set could not be built"
                );
                Ok(config)
            } else {
                let config = Self(fc::FcInitLoadConfigAndFonts());
                (!config.0.is_null())
                    .then_some(config)
                    .context("fontconfig configuration could not be loaded")
            }
        }
    }
}

impl Drop for FontConfig {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: this owned configuration has no remaining users when the containing snapshot is dropped.
            unsafe {
                fc::FcConfigDestroy(self.0);
            }
        }
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
