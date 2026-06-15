//! Regex-based parsers and the shared naming helpers.
//!
//! [`parse_powershell`] is the **fallback** `.ps1` param parser. The primary is
//! [`crate::psparse`], which drives PowerShell's real AST parser; `main.rs` only
//! falls back here if that subprocess fails. The two are kept deliberately in
//! lock-step — a parity test in `psparse` fails the build if they ever disagree
//! on a conventional `param()` block — so degrading to this never surprises the
//! user. [`parse_batch`] handles `.bat`/`.cmd` positional args, and
//! [`kind_from_name`] / [`strip_quotes`] / [`humanize`] are shared by both PS1
//! parsers.

use crate::model::{Param, ParamKind};
use regex::Regex;

/// Parse the `param(...)` block of a PowerShell script. Returns an empty vec
/// if no block is found.
pub fn parse_powershell(source: &str) -> Vec<Param> {
    let cleaned = strip_block_comments(source);
    let inner = match extract_param_block(&cleaned) {
        Some(s) => s,
        None => return Vec::new(),
    };
    split_top_level(&inner)
        .into_iter()
        .filter_map(|decl| parse_one(&decl))
        .collect()
}

/// Remove `<# ... #>` block comments so they can't confuse the scanner.
fn strip_block_comments(s: &str) -> String {
    Regex::new(r"(?s)<#.*?#>")
        .unwrap()
        .replace_all(s, " ")
        .into_owned()
}

/// Find `param ( ... )` and return the text between the outermost parens,
/// respecting strings, line comments and nested brackets.
fn extract_param_block(s: &str) -> Option<String> {
    let m = Regex::new(r"(?i)\bparam\b\s*\(").unwrap().find(s)?;
    let start = m.end(); // just past the opening '('
    let mut depth = 1i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_comment = false;
    let mut prev = '\0';
    let mut out = String::new();
    for c in s[start..].chars() {
        if in_comment {
            out.push(c);
            if c == '\n' {
                in_comment = false;
            }
        } else if in_single {
            out.push(c);
            if c == '\'' {
                in_single = false;
            }
        } else if in_double {
            out.push(c);
            if c == '"' && prev != '`' {
                in_double = false;
            }
        } else {
            match c {
                '#' => {
                    in_comment = true;
                    out.push(c);
                }
                '\'' => {
                    in_single = true;
                    out.push(c);
                }
                '"' => {
                    in_double = true;
                    out.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    out.push(c);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(out);
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        prev = c;
    }
    None
}

/// Split the param block into individual declarations on top-level commas,
/// ignoring commas inside strings, comments or brackets. Comments are dropped.
fn split_top_level(inner: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_comment = false;
    let mut prev = '\0';
    for c in inner.chars() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
        } else if in_single {
            cur.push(c);
            if c == '\'' {
                in_single = false;
            }
        } else if in_double {
            cur.push(c);
            if c == '"' && prev != '`' {
                in_double = false;
            }
        } else {
            match c {
                '#' => in_comment = true,
                '\'' => {
                    in_single = true;
                    cur.push(c);
                }
                '"' => {
                    in_double = true;
                    cur.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    parts.push(cur.trim().to_string());
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        prev = c;
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// Parse one parameter declaration, e.g.
/// `[Parameter(Mandatory=$true)][string]$InputFolder = "."`.
fn parse_one(decl: &str) -> Option<Param> {
    let attr_re = Regex::new(r"\[([^\]]*)\]").unwrap();
    let attrs: Vec<String> = attr_re
        .captures_iter(decl)
        .map(|c| c[1].trim().to_string())
        .collect();

    // Strip attributes, then the first `$name` is the parameter variable.
    let without_attrs = attr_re.replace_all(decl, " ");
    let name = Regex::new(r"\$(\w+)")
        .unwrap()
        .captures(&without_attrs)?
        .get(1)?
        .as_str()
        .to_string();

    let default_raw = Regex::new(r"(?s)\$\w+\s*=\s*(.+)")
        .unwrap()
        .captures(&without_attrs)
        .map(|c| c[1].trim().to_string());

    let lower: Vec<String> = attrs.iter().map(|a| a.to_lowercase()).collect();
    let is_switch = lower.iter().any(|a| a == "switch");
    let is_bool = lower
        .iter()
        .any(|a| a == "bool" || a == "boolean" || a == "system.boolean");
    let is_number = lower.iter().any(|a| {
        matches!(
            a.as_str(),
            "int" | "int16" | "int32" | "int64" | "long" | "double" | "single"
                | "float" | "decimal" | "byte" | "uint16" | "uint32" | "uint64"
        )
    });

    let mut choices = Vec::new();
    for a in &attrs {
        if a.to_lowercase().starts_with("validateset") {
            choices = Regex::new(r#"["']([^"']*)["']"#)
                .unwrap()
                .captures_iter(a)
                .map(|c| c[1].to_string())
                .collect();
        }
    }

    let required = lower.iter().any(|a| {
        if let Some(idx) = a.find("mandatory") {
            let rest = a[idx + "mandatory".len()..]
                .trim_start_matches([' ', '=']);
            !rest.starts_with("$false")
        } else {
            false
        }
    });

    let kind = if is_switch || is_bool {
        ParamKind::Bool
    } else if !choices.is_empty() {
        ParamKind::Choice
    } else if is_number {
        ParamKind::Number
    } else {
        kind_from_name(&name)
    };

    let mut value = String::new();
    let mut bool_value = false;
    if let Some(d) = &default_raw {
        let d = d.trim();
        if kind == ParamKind::Bool {
            bool_value = d.eq_ignore_ascii_case("$true") || d.eq_ignore_ascii_case("true");
        } else {
            value = strip_quotes(d);
        }
    }
    if kind == ParamKind::Choice && value.is_empty() {
        if let Some(first) = choices.first() {
            value = first.clone();
        }
    }

    Some(Param {
        label: humanize(&name),
        name,
        kind,
        required,
        is_switch,
        choices,
        value,
        bool_value,
        position: None,
        custom: false,
        as_env: false,
    })
}

/// Detect positional arguments (`%1`..`%9`, including `%~dp1` style modifiers)
/// in a batch/cmd file and turn each into an ordered Text control.
pub fn parse_batch(source: &str) -> Vec<Param> {
    let re = Regex::new(r"%(?:~[a-zA-Z]*)?([1-9])").unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for cap in re.captures_iter(source) {
        if let Ok(n) = cap[1].parse::<u32>() {
            seen.insert(n);
        }
    }
    seen.into_iter()
        .map(|n| Param {
            name: format!("arg{n}"),
            label: format!("Argument {n}"),
            kind: ParamKind::Text,
            required: false,
            is_switch: false,
            choices: Vec::new(),
            value: String::new(),
            bool_value: false,
            position: Some(n),
            custom: false,
            as_env: false,
        })
        .collect()
}

/// Guess a path picker from the parameter name when the type is just a string.
pub fn kind_from_name(name: &str) -> ParamKind {
    let n = name.to_lowercase();
    if n.contains("folder") || n.contains("dir") {
        ParamKind::FolderPath
    } else if n.contains("file") || n.contains("path") {
        ParamKind::FilePath
    } else {
        ParamKind::Text
    }
}

pub fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// "InputFolder" -> "Input Folder", "OutFile" -> "Out File", "log_path" -> "Log path".
pub fn humanize(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower_or_digit = false;
    for c in name.chars() {
        if c == '_' || c == '-' {
            out.push(' ');
            prev_lower_or_digit = false;
            continue;
        }
        if c.is_uppercase() && prev_lower_or_digit {
            out.push(' ');
        }
        out.push(c);
        prev_lower_or_digit = c.is_lowercase() || c.is_numeric();
    }
    let mut chars = out.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Graceful-degradation guarantee (the Reddit reviewer's #1 priority): the
    /// regex parser is the pure-Rust fallback and MUST NOT panic on adversarial
    /// input — release builds use `panic = "abort"`, so a panic hard-kills the
    /// app. Correctness of the *result* is deliberately NOT asserted here;
    /// survival is.
    #[test]
    fn regex_parser_never_panics_on_adversarial_input() {
        let gnarly = r##"Function Foo {
[CmdletBinding()]
param (
  # This is a comment (and this part is in parentheses)
  [Parameter(Mandatory,
    ValueFromPipeline
  )][ValidateScript({
$false
}
)]
 [string]$Bar = ("`'())$($PWD.Trim()))`'")
) #end block (and some unbalanced parentheses(and sub-parenthetical stuff with a mismatched bracket, too!‽])

# Some comment (with
# parentheses)

}"##;
        // Must return without panicking; the result may be empty or wrong.
        let _ = parse_powershell(gnarly);
        let _ = parse_powershell("");
        let _ = parse_powershell("param(");
        let _ = parse_powershell("param(   \n  ");
        let _ = parse_powershell("param($a,, $b, [int],)");
        let _ = parse_powershell("param([ValidateSet(]$x = \"unterminated");
        let _ = parse_powershell(&"param(".repeat(2000));
        let _ = parse_powershell("param([string]$x = '日本語‽')");
    }

    /// The regex fallback must also be *correct* on a realistic block, not just
    /// non-crashing — it's what ships when the AST path is unavailable.
    #[test]
    fn regex_parser_is_correct_on_a_realistic_block() {
        let src = r#"
            param(
                [Parameter(Mandatory = $true)][string]$InputFolder,
                [ValidateSet("Low", "High")][string]$Quality = "Low",
                [int]$Count = 3,
                [switch]$Force
            )
        "#;
        let p = parse_powershell(src);
        assert_eq!(p.len(), 4);
        let by = |n: &str| p.iter().find(|x| x.name == n).unwrap();
        assert!(by("InputFolder").required);
        assert_eq!(by("InputFolder").kind, ParamKind::FolderPath);
        assert_eq!(by("Quality").kind, ParamKind::Choice);
        assert_eq!(by("Quality").choices, vec!["Low", "High"]);
        assert_eq!(by("Quality").value, "Low");
        assert_eq!(by("Count").kind, ParamKind::Number);
        assert_eq!(by("Count").value, "3");
        assert!(by("Force").is_switch);
        assert!(!by("InputFolder").is_switch);
    }

    #[test]
    fn strip_quotes_handles_edges() {
        assert_eq!(strip_quotes("\"x\""), "x");
        assert_eq!(strip_quotes("'x'"), "x");
        assert_eq!(strip_quotes("x"), "x");
        assert_eq!(strip_quotes("\""), "\""); // one quote: too short to strip
        assert_eq!(strip_quotes("\"\""), ""); // empty quoted string
        assert_eq!(strip_quotes("\"unbalanced"), "\"unbalanced");
        assert_eq!(strip_quotes("\"x'"), "\"x'"); // mismatched quote chars
        assert_eq!(strip_quotes("'日本語'"), "日本語"); // multibyte between ASCII quotes
        assert_eq!(strip_quotes("  \"x\"  "), "x"); // trims before stripping
    }

    #[test]
    fn kind_from_name_picks_the_right_picker() {
        assert_eq!(kind_from_name("InputFolder"), ParamKind::FolderPath);
        assert_eq!(kind_from_name("WorkDir"), ParamKind::FolderPath);
        assert_eq!(kind_from_name("LogFile"), ParamKind::FilePath);
        assert_eq!(kind_from_name("ConfigPath"), ParamKind::FilePath);
        assert_eq!(kind_from_name("UserName"), ParamKind::Text);
    }

    #[test]
    fn humanize_splits_camel_case_and_separators() {
        assert_eq!(humanize("InputFolder"), "Input Folder");
        assert_eq!(humanize("OutFile"), "Out File");
        assert_eq!(humanize("log_path"), "Log path");
    }

    #[test]
    fn parse_batch_finds_ordered_positional_args() {
        let p = parse_batch("@echo off\ncopy %1 %2\necho %~dpnx3 and %1 again\n");
        let names: Vec<&str> = p.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["arg1", "arg2", "arg3"]); // de-duped, in order
        assert_eq!(p[0].position, Some(1));
        assert!(p.iter().all(|x| x.kind == ParamKind::Text));
    }
}
