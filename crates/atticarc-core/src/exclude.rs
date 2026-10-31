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
//! Each rule also says what happens to the files it leaves out. *Set aside*
//! files go into a second archive beside the main one,
//! `<folder>.excluded.7z`, verified and catalogued like any other.
//! *Discard* files (junk such as shortcuts) are archived nowhere; the main
//! archive's manifest lists them with their hashes, and they are deleted
//! with the folder. If both kinds of rule match a file, it is set aside.

use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Wildcard,
    Regex,
}

/// What happens to the files a rule leaves out.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    SetAside,
    Discard,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::SetAside => "Set aside",
            Action::Discard => "Discard",
        }
    }
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
    pub action: Action,
}

impl Default for Rule {
    fn default() -> Self {
        Self { enabled: true, kind: Kind::Wildcard, pattern: String::new(), action: Action::SetAside }
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
    rules: Vec<(Regex, Action)>,
}

impl Exclusions {
    /// Compiles the enabled rules with a pattern; fails on the first that
    /// doesn't compile, so a typo never archives what it meant to leave out.
    pub fn new(rules: &[Rule]) -> Result<Self> {
        let rules = rules
            .iter()
            .filter(|r| r.enabled && !r.pattern.trim().is_empty())
            .map(|r| Ok((r.compile()?, r.action)))
            .collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    /// Whether the file at `rel` (`/` separated, inside the folder) is left
    /// out, and if so what happens to it.
    pub fn action(&self, rel: &str) -> Option<Action> {
        let mut found = None;
        for (re, action) in &self.rules {
            if re.is_match(rel) {
                if *action == Action::SetAside {
                    return Some(Action::SetAside);
                }
                found = Some(*action);
            }
        }
        found
    }

    pub fn excludes(&self, rel: &str) -> bool {
        self.action(rel).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The rule the New job page starts with: discard shortcuts, switched off.
pub fn default_rules() -> Vec<Rule> {
    vec![Rule { enabled: false, kind: Kind::Wildcard, pattern: "*.lnk".into(), action: Action::Discard }]
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
        Exclusions::new(&[Rule { enabled: true, kind, pattern: pattern.into(), action: Action::SetAside }]).unwrap()
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
        assert!(Exclusions::new(&[Rule { kind: Kind::Regex, pattern: "*.lnk".into(), ..Rule::default() }]).is_err());
        let off = Exclusions::new(&default_rules()).unwrap();
        assert!(off.is_empty() && !off.excludes("a.lnk"));
        let blank = Exclusions::new(&[Rule::default()]).unwrap();
        assert!(blank.is_empty());
    }

    #[test]
    fn set_aside_wins_over_discard() {
        let rule = |pattern: &str, action| Rule { pattern: pattern.into(), action, ..Rule::default() };
        let e = Exclusions::new(&[rule("*.lnk", Action::Discard), rule("keep/**", Action::SetAside)]).unwrap();
        assert_eq!(e.action("a/x.lnk"), Some(Action::Discard));
        assert_eq!(e.action("keep/x.lnk"), Some(Action::SetAside));
        assert_eq!(e.action("keep.txt"), None);
    }
}
