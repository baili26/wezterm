//! Keyword highlighting rules.
//!
//! This is a personal-fork feature: rules that recolor matches in the
//! terminal output as it is rendered.  It is deliberately implemented as a
//! pure data type in the `config` crate so that the GUI crate can apply the
//! rules at paint time without touching the terminal model.
use std::convert::TryFrom;

use crate::color::RgbaColor;
use fancy_regex::Regex as FancyRegex;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};

/// A compiled matcher with two engines.
///
/// `fancy-regex` treats `\b` and lookaround as "hard" expressions and runs the
/// whole pattern on its backtracking PikeVM interpreter whenever one is
/// present, which is 30-100x slower than the DFA in the `regex` crate.  Most
/// rules only use `\b` and are perfectly expressible in the `regex` crate, so
/// we compile a second engine for those and use it at paint time.  Patterns
/// that genuinely need lookaround (or backreferences) keep using fancy-regex,
/// so behaviour is unchanged for them.
#[derive(Clone, Debug)]
pub struct MatchRegex {
    /// Always present: the source of truth for serialization and the fallback.
    fancy: FancyRegex,
    /// Present when the pattern is expressible in the `regex` crate.
    fast: Option<regex::Regex>,
}

impl MatchRegex {
    pub fn new(pattern: &str) -> anyhow::Result<Self> {
        let fancy = FancyRegex::new(pattern).map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
        // `regex` rejects lookaround and backreferences itself, so a successful
        // compile means the pattern is semantically supported by the fast
        // engine.  Any syntax it accepts but fancy-regex treats as "hard" (the
        // common case being `\b`) matches identically on both engines.
        let fast = match regex::Regex::new(pattern) {
            Ok(re) => Some(re),
            Err(err) => {
                log::debug!("keyword rule {:?} stays on fancy-regex: {}", pattern, err);
                None
            }
        };
        Ok(Self { fancy, fast })
    }

    /// The pattern this matcher was built from.
    pub fn as_str(&self) -> &str {
        self.fancy.as_str()
    }

    /// Is this pattern running on the fast (DFA) engine?
    pub fn is_fast(&self) -> bool {
        self.fast.is_some()
    }

    pub fn is_match(&self, text: &str) -> anyhow::Result<bool> {
        match &self.fast {
            Some(re) => Ok(re.is_match(text)),
            None => Ok(self.fancy.is_match(text)?),
        }
    }

    /// Append the non-empty match ranges in `text` to `out`.
    ///
    /// Ranges are byte offsets.  Runtime errors from fancy-regex (e.g. the
    /// backtrack limit) end the scan for that rule rather than aborting the
    /// caller.
    pub fn find_ranges(&self, text: &str, out: &mut Vec<(usize, usize)>) {
        match &self.fast {
            Some(re) => {
                for m in re.find_iter(text) {
                    if m.start() < m.end() {
                        out.push((m.start(), m.end()));
                    }
                }
            }
            None => {
                for found in self.fancy.find_iter(text) {
                    let m = match found {
                        Ok(m) => m,
                        Err(_) => return,
                    };
                    if m.start() < m.end() {
                        out.push((m.start(), m.end()));
                    }
                }
            }
        }
    }
}

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
    pub regex: MatchRegex,
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
        let regex = MatchRegex::new(regex)?;
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

impl FromDynamic for MatchRegex {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<Self, wezterm_dynamic::Error> {
        let s = String::from_dynamic(value, options)?;
        MatchRegex::new(&s).map_err(|e| format!("{e:#}").into())
    }
}

impl ToDynamic for MatchRegex {
    fn to_dynamic(&self) -> Value {
        self.as_str().to_dynamic()
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
        assert!(
            rule.regex.is_fast(),
            "a plain \\b pattern should use the fast engine"
        );
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
    fn engine_selection() {
        // Plain word boundaries are expressible in the regex crate, so they
        // must take the fast path even though fancy-regex considers them hard.
        let plain = MatchRegex::new(r"(?i)\b(?:error|failed)\b").unwrap();
        assert!(plain.is_fast(), "\\b should not force the fancy engine");

        let unicode = MatchRegex::new(r"(?:故障|崩溃)").unwrap();
        assert!(unicode.is_fast());

        // Lookbehind cannot be delegated.
        let lookbehind = MatchRegex::new(r"(?<![\w./-])/var/log").unwrap();
        assert!(!lookbehind.is_fast(), "lookbehind must stay on fancy");

        // Backreferences too.
        let backref = MatchRegex::new(r"(foo)\1").unwrap();
        assert!(!backref.is_fast(), "backreference must stay on fancy");
    }

    #[test]
    fn both_engines_agree() {
        // The fast engine is only used when it compiled the same pattern, but
        // make sure the matched ranges really are identical for the shapes our
        // rules use.
        let patterns = [
            r"(?i)\bwarn(?:ing)?\b",
            r"\b(?:\d{1,3}\.){3}\d{1,3}\b",
            r"(?i)(?:故障|崩溃|拒绝)",
            r"\bOut of memory\b",
        ];
        let samples = [
            "WARNING: disk degraded",
            "bound to 10.0.0.12 and 192.168.1.1",
            "服务崩溃：连接拒绝",
            "kernel: Out of memory: Killed process",
            "nothing to see here",
        ];
        for pattern in patterns {
            let matcher = MatchRegex::new(pattern).unwrap();
            assert!(matcher.is_fast(), "{} should be fast", pattern);
            let fancy = FancyRegex::new(pattern).unwrap();
            for sample in samples {
                let mut mine = Vec::new();
                matcher.find_ranges(sample, &mut mine);
                let theirs: Vec<(usize, usize)> = fancy
                    .find_iter(sample)
                    .flatten()
                    .filter(|m| m.start() < m.end())
                    .map(|m| (m.start(), m.end()))
                    .collect();
                assert_eq!(mine, theirs, "pattern {pattern:?} on {sample:?}");
                mine.clear();
            }
        }
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

    #[test]
    fn default_rules_survive_a_lua_round_trip() {
        // `wezterm.default_keyword_rules()` converts the list to Lua and the
        // user assigns it back to `config.keyword_rules`; make sure that path
        // preserves names, patterns and colors.
        let rules = crate::config::default_keyword_rules();
        let lua = crate::lua::make_lua_context(std::path::Path::new("testing")).unwrap();
        let value = luahelper::to_lua(&lua, rules.clone()).unwrap();
        let round_tripped: Vec<KeywordRule> = luahelper::from_lua(value).unwrap();

        assert_eq!(round_tripped.len(), rules.len());
        for (a, b) in rules.iter().zip(round_tripped.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.regex.as_str(), b.regex.as_str());
            assert_eq!(a.fg, b.fg);
            assert_eq!(a.bg, b.bg);
            assert_eq!(a.bold, b.bold);
            assert_eq!(a.italic, b.italic);
            assert_eq!(a.underline, b.underline);
        }
    }
}
