//! Parses a PowerShell `param(...)` block into typed [`Param`]s.
//!
//! PowerShell is the sweet spot for auto-detection: a `param()` block is an
//! already-typed parameter spec that maps almost 1:1 onto UI controls.

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
    })
}

/// Guess a path picker from the parameter name when the type is just a string.
fn kind_from_name(name: &str) -> ParamKind {
    let n = name.to_lowercase();
    if n.contains("folder") || n.contains("dir") {
        ParamKind::FolderPath
    } else if n.contains("file") || n.contains("path") {
        ParamKind::FilePath
    } else {
        ParamKind::Text
    }
}

fn strip_quotes(s: &str) -> String {
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
fn humanize(name: &str) -> String {
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
