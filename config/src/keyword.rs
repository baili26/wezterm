//! Keyword highlighting rules.
//!
//! This is a personal-fork feature: rules that recolor matches in the
//! terminal output as it is rendered.  It is deliberately implemented as a
//! pure data type in the `config` crate so that the GUI crate can apply the
//! rules at paint time without touching the terminal model.
use std::convert::TryFrom;

use crate::color::RgbaColor;
use fancy_regex::Regex;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};

/// A single keyword highlighting rule.
///
/// Fields that are left unset keep whatever style the cell already had, so
/// a rule that only sets `bold` won't clobber the program's own colors.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct KeywordRule {
    /// Optional name, useful for removing/overriding built-in rules from Lua
    #[dynamic(default)]
    pub name: String,
    /// The regular expression to match against a line of terminal output
    #[dynamic(into = "RegexWrap", try_from = "RegexWrap")]
    pub regex: Regex,
    /// Foreground color; `None` keeps the cell's own color
    #[dynamic(default)]
    pub fg: Option<RgbaColor>,
    /// Background color; `None` keeps the cell's own background
    #[dynamic(default)]
    pub bg: Option<RgbaColor>,
    #[dynamic(default)]
    pub bold: bool,
    #[dynamic(default)]
    pub italic: bool,
    #[dynamic(default)]
    pub underline: bool,
}

impl KeywordRule {
    /// Convenience constructor used for the compiled-in default rules.
    pub fn new(regex: &str, fg: Option<&str>) -> anyhow::Result<Self> {
        let regex = Regex::new(regex).map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
        let fg = match fg {
            Some(s) => Some(
                RgbaColor::try_from(s.to_string())
                    .map_err(|_| anyhow::anyhow!("unknown color name: {s}"))?,
            ),
            None => None,
        };
        Ok(Self {
            name: String::new(),
            regex,
            fg,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
        })
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    pub fn with_bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn with_underline(mut self) -> Self {
        self.underline = true;
        self
    }
}

struct RegexWrap(Regex);

impl FromDynamic for RegexWrap {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<RegexWrap, wezterm_dynamic::Error> {
        let s = String::from_dynamic(value, options)?;
        Ok(RegexWrap(Regex::new(&s).map_err(|e| e.to_string())?))
    }
}

impl From<&Regex> for RegexWrap {
    fn from(regex: &Regex) -> RegexWrap {
        RegexWrap(regex.clone())
    }
}

impl Into<Regex> for RegexWrap {
    fn into(self) -> Regex {
        self.0
    }
}

impl ToDynamic for RegexWrap {
    fn to_dynamic(&self) -> Value {
        self.0.to_string().to_dynamic()
    }
}

impl_lua_conversion_dynamic!(KeywordRule);

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_rule_from_lua() {
        let value = wezterm_dynamic::Value::Object(
            vec![
                ("name".to_dynamic(), "err".to_dynamic()),
                ("regex".to_dynamic(), r"(?i)\berror\b".to_dynamic()),
                ("fg".to_dynamic(), "#ff5555".to_dynamic()),
                ("bold".to_dynamic(), true.to_dynamic()),
            ]
            .into_iter()
            .collect(),
        );

        let rule = KeywordRule::from_dynamic(&value, Default::default()).unwrap();
        assert_eq!(rule.name, "err");
        assert_eq!(rule.regex.as_str(), r"(?i)\berror\b");
        assert!(rule.fg.is_some());
        assert!(rule.bg.is_none());
        assert!(rule.bold);
        assert!(!rule.italic);
        assert!(!rule.underline);
    }

    #[test]
    fn parse_rule_rejects_bad_regex() {
        let value = wezterm_dynamic::Value::Object(
            vec![("regex".to_dynamic(), "(".to_dynamic())]
                .into_iter()
                .collect(),
        );
        assert!(KeywordRule::from_dynamic(&value, Default::default()).is_err());
    }

    #[test]
    fn round_trip() {
        let rule = KeywordRule::new(r"(?i)\bwarning\b", Some("#ffd75f"))
            .unwrap()
            .with_name("warning");
        let value = rule.to_dynamic();
        let rule2 = KeywordRule::from_dynamic(&value, Default::default()).unwrap();
        assert_eq!(rule2.name, "warning");
        assert_eq!(rule2.regex.as_str(), rule.regex.as_str());
        assert_eq!(rule2.fg, rule.fg);
    }

    #[test]
    fn builtin_defaults_are_valid() {
        let rules = crate::config::default_keyword_rules();
        assert!(!rules.is_empty(), "expected built-in keyword rules");

        for rule in &rules {
            assert!(!rule.name.is_empty(), "built-in rule without a name");
        }

        // Every built-in rule should compile (they are compiled when the
        // list is built) and match at least one representative sample.
        let samples = [
            ("error", "error: cannot connect: connection reset"),
            ("warning", "WARNING: disk usage degraded"),
            ("success", "Service started successfully"),
            ("info", "Loading configuration"),
            ("ipv4", "bound to 10.0.0.12:8080"),
            ("mac", "link up 00:1a:2b:3c:4d:5e"),
            ("url", "see https://example.com/docs"),
            ("k8s-status", "pod in CrashLoopBackOff"),
        ];
        for (name, sample) in samples {
            let rule = rules
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("missing built-in rule {name}"));
            assert!(
                rule.regex.is_match(sample).unwrap(),
                "built-in rule {name} did not match {sample:?}"
            );
        }
    }
}
