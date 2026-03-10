use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// Flat translation map: "nav.dashboard" -> "Dashboard"
pub type Translations = HashMap<String, String>;

/// Info about a single locale exposed to templates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocaleInfo {
    pub code: String,
    pub name: String,
    pub native_name: String,
}

/// i18n configuration
#[derive(Debug, Clone)]
pub struct LocaleConfig {
    pub default: String,
    pub available: Vec<LocaleInfo>,
}

/// Central i18n manager.
///
/// Loads TOML translation files from a base `locales/` directory and from
/// additional addon-provided directories.  All translations are kept in
/// memory as flat `"section.key" -> "value"` maps so lookups are O(1).
pub struct I18n {
    /// lang code -> flat translation map
    translations: RwLock<HashMap<String, Translations>>,
    config: LocaleConfig,
    /// Extra locale directories registered by addons
    addon_locales_dirs: RwLock<Vec<String>>,
}

impl I18n {
    /// Create a new I18n instance and eagerly load translations from disk.
    pub fn new(config: LocaleConfig) -> Self {
        let i18n = Self {
            translations: RwLock::new(HashMap::new()),
            config,
            addon_locales_dirs: RwLock::new(vec![]),
        };
        i18n.reload();
        i18n
    }

    // ------------------------------------------------------------------
    // Loading
    // ------------------------------------------------------------------

    /// (Re)load all translations from disk, replacing the current cache.
    pub fn reload(&self) {
        let mut cache = match self.translations.write() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("i18n: failed to acquire write lock: {}", e);
                return;
            }
        };
        cache.clear();

        // Load core locales
        let addon_dirs = self.addon_locales_dirs.read().unwrap_or_else(|e| e.into_inner());
        for info in &self.config.available {
            let mut flat = match Self::load_file("locales", &info.code) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("i18n: failed to load locales/{}.toml: {}", info.code, e);
                    Translations::new()
                }
            };
            // Merge enabled addon locales on top
            for dir in addon_dirs.iter() {
                match Self::load_file(dir, &info.code) {
                    Ok(addon_flat) => {
                        flat.extend(addon_flat);
                    }
                    Err(e) => {
                        tracing::debug!("i18n: failed to load addon locale {}/{}.toml: {}", dir, info.code, e);
                    }
                }
            }
            cache.insert(info.code.clone(), flat);
        }
    }

    /// Load a single `{base_dir}/{locale}.toml` file and flatten it.
    fn load_file(base_dir: &str, locale: &str) -> anyhow::Result<Translations> {
        let path = format!("{}/{}.toml", base_dir, locale);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("i18n: cannot read {}: {}", path, e))?;
        let value: toml::Value = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("i18n: cannot parse {}: {}", path, e))?;
        let mut flat = Translations::new();
        Self::flatten_value(&value, String::new(), &mut flat);
        Ok(flat)
    }

    /// Recursively flatten a TOML value into dot-separated keys.
    ///
    /// `{"nav": {"dashboard": "Dashboard"}}` → `"nav.dashboard" = "Dashboard"`
    fn flatten_value(value: &toml::Value, prefix: String, out: &mut Translations) {
        match value {
            toml::Value::Table(table) => {
                for (k, v) in table {
                    let new_prefix = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{}.{}", prefix, k)
                    };
                    Self::flatten_value(v, new_prefix, out);
                }
            }
            toml::Value::String(s) => {
                out.insert(prefix, s.clone());
            }
            other => {
                // Convert non-string leaves (booleans, integers, …) to strings
                out.insert(prefix, other.to_string());
            }
        }
    }

    // ------------------------------------------------------------------
    // Addon support
    // ------------------------------------------------------------------

    /// Register an additional locale directory (provided by an addon) and
    /// immediately reload all translations so the new strings are available.
    pub fn register_addon_locales(&self, addon_id: &str, locales_dir: String) {
        {
            let mut dirs = self.addon_locales_dirs.write().unwrap_or_else(|e| e.into_inner());
            tracing::debug!("i18n: registering addon '{}' locales at '{}'", addon_id, locales_dir);
            dirs.push(locales_dir);
        }
        self.reload();
    }

    // ------------------------------------------------------------------
    // Translation lookups
    // ------------------------------------------------------------------

    /// Look up a translation key for the given locale.
    ///
    /// Falls back to `self.config.default` if the key is missing in the
    /// requested locale, and finally returns the key itself as a last resort.
    pub fn t(&self, locale: &str, key: &str) -> String {
        let cache = self.translations.read().unwrap_or_else(|e| e.into_inner());

        // 1. Requested locale
        if let Some(flat) = cache.get(locale) {
            if let Some(v) = flat.get(key) {
                return v.clone();
            }
        }

        // 2. Default locale fallback
        if locale != self.config.default {
            if let Some(flat) = cache.get(&self.config.default) {
                if let Some(v) = flat.get(key) {
                    return v.clone();
                }
            }
        }

        // 3. Return the key itself so templates always render something
        key.to_string()
    }

    /// Look up a key and replace `{{var}}` placeholders with values from `vars`.
    pub fn t_with_vars(&self, locale: &str, key: &str, vars: &HashMap<String, String>) -> String {
        let mut text = self.t(locale, key);
        for (k, v) in vars {
            text = text.replace(&format!("{{{{{}}}}}", k), v);
        }
        text
    }

    /// Return the full translation map for a locale as a nested
    /// `serde_json::Value` object suitable for injection into template context.
    ///
    /// The flat `"nav.dashboard"` key is re-hydrated into
    /// `{"nav": {"dashboard": "…"}}`.
    pub fn get_translations_json(&self, locale: &str) -> serde_json::Value {
        let cache = self.translations.read().unwrap_or_else(|e| e.into_inner());

        let flat = cache
            .get(locale)
            .or_else(|| cache.get(&self.config.default));

        let mut root = serde_json::Map::new();
        if let Some(flat) = flat {
            for (dotted_key, value) in flat {
                Self::set_nested(&mut root, dotted_key, serde_json::Value::String(value.clone()));
            }
        }
        serde_json::Value::Object(root)
    }

    /// Insert a value at a dot-separated path inside a JSON map.
    fn set_nested(
        map: &mut serde_json::Map<String, serde_json::Value>,
        key: &str,
        value: serde_json::Value,
    ) {
        let mut parts = key.splitn(2, '.');
        let head = match parts.next() {
            Some(h) => h,
            None => return,
        };

        if let Some(tail) = parts.next() {
            // Recurse into (or create) the nested object
            let child = map
                .entry(head.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(ref mut m) = *child {
                Self::set_nested(m, tail, value);
            }
        } else {
            map.insert(head.to_string(), value);
        }
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn available_locales(&self) -> &[LocaleInfo] {
        &self.config.available
    }

    pub fn default_locale(&self) -> &str {
        &self.config.default
    }
}

// ------------------------------------------------------------------
// Request-level locale detection
// ------------------------------------------------------------------

/// Detect the user's preferred locale from (in order of priority):
/// 1. A `locale` cookie
/// 2. The `Accept-Language` HTTP header
/// 3. The configured default
pub fn detect_locale(
    headers: &axum::http::HeaderMap,
    available: &[LocaleInfo],
    default: &str,
) -> String {
    use axum::http::header::COOKIE;

    // 1. Cookie takes highest priority
    for cookie_header in headers.get_all(COOKIE) {
        if let Ok(s) = cookie_header.to_str() {
            for pair in s.split(';') {
                let mut kv = pair.trim().splitn(2, '=');
                if kv.next().map(str::trim) == Some("locale") {
                    let val = kv.next().unwrap_or("").trim().to_string();
                    if !val.is_empty() && is_available(&val, available) {
                        return val;
                    }
                }
            }
        }
    }

    // 2. Accept-Language header
    if let Some(accept) = headers.get(axum::http::header::ACCEPT_LANGUAGE) {
        if let Ok(s) = accept.to_str() {
            // Parse "zh-CN,zh;q=0.9,en;q=0.8" style values
            let mut candidates: Vec<(&str, f32)> = s
                .split(',')
                .filter_map(|item| {
                    let mut parts = item.trim().splitn(2, ';');
                    let tag = parts.next()?.trim();
                    let q: f32 = parts
                        .next()
                        .and_then(|p| p.trim().strip_prefix("q="))
                        .and_then(|q| q.parse().ok())
                        .unwrap_or(1.0);
                    Some((tag, q))
                })
                .collect();
            candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            for (tag, _) in candidates {
                // Match full tag first ("zh-CN"), then base language ("zh")
                if is_available(tag, available) {
                    return tag.to_string();
                }
                let base = tag.split('-').next().unwrap_or(tag);
                if is_available(base, available) {
                    return base.to_string();
                }
            }
        }
    }

    // 3. Default
    default.to_string()
}

fn is_available(code: &str, available: &[LocaleInfo]) -> bool {
    available.iter().any(|l| l.code == code)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Build a minimal I18n with two in-memory locale files written to a tmpdir.
    fn make_i18n(tmp: &TempDir) -> I18n {
        let dir = tmp.path().join("locales");
        std::fs::create_dir_all(&dir).unwrap();

        // English
        let mut f = std::fs::File::create(dir.join("en.toml")).unwrap();
        writeln!(
            f,
            r#"
[common]
save = "Save"
cancel = "Cancel"

[messages]
saved = "{{{{name}}}} saved."
"#
        )
        .unwrap();

        // Chinese
        let mut f = std::fs::File::create(dir.join("zh.toml")).unwrap();
        writeln!(
            f,
            r#"
[common]
save = "保存"
"#
        )
        .unwrap();

        let config = LocaleConfig {
            default: "en".to_string(),
            available: vec![
                LocaleInfo { code: "en".into(), name: "English".into(), native_name: "English".into() },
                LocaleInfo { code: "zh".into(), name: "Chinese".into(), native_name: "中文".into() },
            ],
        };

        // Override the locales dir for this test
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let i18n = I18n::new(config);
        std::env::set_current_dir(original).unwrap();
        i18n
    }

    #[test]
    fn lookup_existing_key() {
        let tmp = TempDir::new().unwrap();
        let i18n = make_i18n(&tmp);
        assert_eq!(i18n.t("en", "common.save"), "Save");
        assert_eq!(i18n.t("zh", "common.save"), "保存");
    }

    #[test]
    fn fallback_to_default_locale() {
        let tmp = TempDir::new().unwrap();
        let i18n = make_i18n(&tmp);
        // "common.cancel" is only in English
        assert_eq!(i18n.t("zh", "common.cancel"), "Cancel");
    }

    #[test]
    fn fallback_to_key_when_missing() {
        let tmp = TempDir::new().unwrap();
        let i18n = make_i18n(&tmp);
        assert_eq!(i18n.t("en", "nonexistent.key"), "nonexistent.key");
    }

    #[test]
    fn variable_interpolation() {
        let tmp = TempDir::new().unwrap();
        let i18n = make_i18n(&tmp);
        let mut vars = std::collections::HashMap::new();
        vars.insert("name".to_string(), "Extension 100".to_string());
        assert_eq!(
            i18n.t_with_vars("en", "messages.saved", &vars),
            "Extension 100 saved."
        );
    }

    #[test]
    fn get_translations_json_is_nested() {
        let tmp = TempDir::new().unwrap();
        let i18n = make_i18n(&tmp);
        let json = i18n.get_translations_json("en");
        let save = &json["common"]["save"];
        assert_eq!(save.as_str().unwrap(), "Save");
    }

    #[test]
    fn detect_locale_from_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            "locale=zh; rustpbx_session=abc".parse().unwrap(),
        );
        let available = vec![
            LocaleInfo { code: "en".into(), name: "English".into(), native_name: "English".into() },
            LocaleInfo { code: "zh".into(), name: "Chinese".into(), native_name: "中文".into() },
        ];
        assert_eq!(detect_locale(&headers, &available, "en"), "zh");
    }

    #[test]
    fn detect_locale_from_accept_language() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT_LANGUAGE,
            "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap(),
        );
        let available = vec![
            LocaleInfo { code: "en".into(), name: "English".into(), native_name: "English".into() },
            LocaleInfo { code: "zh".into(), name: "Chinese".into(), native_name: "中文".into() },
        ];
        assert_eq!(detect_locale(&headers, &available, "en"), "zh");
    }

    #[test]
    fn detect_locale_defaults_when_unsupported() {
        let headers = axum::http::HeaderMap::new();
        let available = vec![
            LocaleInfo { code: "en".into(), name: "English".into(), native_name: "English".into() },
        ];
        assert_eq!(detect_locale(&headers, &available, "en"), "en");
    }
}
