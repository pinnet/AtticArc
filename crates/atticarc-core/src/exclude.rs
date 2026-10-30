//! Exclusions: patterns for files to leave out of archives.
//!
//! A rule is either a wildcard or a regular expression, and is matched
//! against a file's path inside the folder being archived, `/` separated,
//! ignoring case. A wildcard without a `/` matches the file name alone, so
//! `*.lnk` leaves out shortcuts in every subfolder; with a `/` it matches the
//! whole path. `*` matches within one folder name, `**` across folders, `?`
//! one character. A regular expression matches anywhere in the path unless
//! anchored with `^` or `$`.
//!
//! Excluded files are not in the archive, so a folder that still holds one
//! can't be deleted: the check before deleting finds a file the archive
//! doesn't have.

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Wildcard,
    Regex,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Wildcard => "Wildcard",
            Kind::Regex => "Regex",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Rule {
    pub enabled: bool,
    pub kind: Kind,
    pub pattern: String,
}

impl Default for Rule {
    fn default() -> Self {
        Self { enabled: true, kind: Kind::Wildcard, pattern: String::new() }
    }
}

impl Rule {
    /// The rule as a regular expression, or why it isn't one.
    pub fn compile(&self) -> Result<Regex> {
        let source = match self.kind {
            Kind::Regex => self.pattern.clone(),
            Kind::Wildcard => wildcard_regex(&self.pattern),
        };
        RegexBuilder::new(&source)
            .case_insensitive(true)
            .build()
            .with_context(|| format!("'{}' isn't a valid {}", self.pattern, self.kind.label().to_lowercase()))
    }
}

/// The enabled rules, ready to match paths.
#[derive(Clone, Debug, Default)]
pub struct Exclusions {
    rules: Vec<Regex>,
}

impl Exclusions {
    /// Compiles the enabled rules with a pattern; fails on the first that
    /// doesn't compile, so a typo never archives what it meant to leave out.
    pub fn new(rules: &[Rule]) -> Result<Self> {
        let rules = rules
            .iter()
            .filter(|r| r.enabled && !r.pattern.trim().is_empty())
            .map(Rule::compile)
            .collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    /// Whether the file at `rel` (`/` separated, inside the folder) is left out.
    pub fn excludes(&self, rel: &str) -> bool {
        self.rules.iter().any(|r| r.is_match(rel))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The rule the New job page starts with: shortcuts, switched off.
pub fn default_rules() -> Vec<Rule> {
    vec![Rule { enabled: false, kind: Kind::Wildcard, pattern: "*.lnk".into() }]
}

fn wildcard_regex(pattern: &str) -> String {
    let pattern = pattern.trim().replace('\\', "/");
    let mut re = String::from(if pattern.contains('/') { "^" } else { "(^|/)" });
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                re.push_str(".*");
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(kind: Kind, pattern: &str) -> Exclusions {
        Exclusions::new(&[Rule { enabled: true, kind, pattern: pattern.into() }]).unwrap()
    }

    #[test]
    fn wildcards() {
        let lnk = ex(Kind::Wildcard, "*.lnk");
        assert!(lnk.excludes("Desktop.lnk"));
        assert!(lnk.excludes("sub/deeper/Shortcut to x.LNK"));
        assert!(!lnk.excludes("notes.lnk.txt"));
        assert!(!lnk.excludes("lnk"));
        let thumbs = ex(Kind::Wildcard, "Thumbs.db");
        assert!(thumbs.excludes("a/b/thumbs.db"));
        assert!(!thumbs.excludes("a/b/myThumbs.db"));
        let top = ex(Kind::Wildcard, "cache/*");
        assert!(top.excludes("cache/x.bin"));
        assert!(!top.excludes("cache/sub/x.bin"));
        assert!(!top.excludes("a/cache/x.bin"));
        let deep = ex(Kind::Wildcard, "**/node_modules/**");
        assert!(deep.excludes("app/node_modules/x/y.js"));
        assert!(ex(Kind::Wildcard, "IMG_00?.jpg").excludes("IMG_001.JPG"));
        assert!(ex(Kind::Wildcard, "a+b (1).txt").excludes("x/a+b (1).txt"));
    }

    #[test]
    fn regexes_and_switches() {
        let r = ex(Kind::Regex, r"\.(tmp|bak)$");
        assert!(r.excludes("x/file.TMP"));
        assert!(!r.excludes("x/file.tmpl"));
        assert!(Exclusions::new(&[Rule { enabled: true, kind: Kind::Regex, pattern: "*.lnk".into() }]).is_err());
        let off = Exclusions::new(&default_rules()).unwrap();
        assert!(off.is_empty() && !off.excludes("a.lnk"));
        let blank = Exclusions::new(&[Rule::default()]).unwrap();
        assert!(blank.is_empty());
    }
}
