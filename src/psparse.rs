//! Primary PowerShell `param()` parser, via PowerShell's own parser
//! (`[System.Management.Automation.Language.Parser]::ParseInput`).
//!
//! Far more robust than regex for real scripts: comments, multi-line attributes,
//! nested `{}` script blocks, and context-dependent parens are all handled by the
//! actual language parser — the same AST PSScriptAnalyzer is built on. Crucially
//! it **parses without executing** the script.
//!
//! This is the source of truth. [`parse`] returns `None` *only* if the PowerShell
//! subprocess fails (spawn error, non-zero exit, unreadable output) — not when a
//! script legitimately has no parameters (that's `Some(vec![])`). On `None`,
//! `main.rs` falls back to the regex parser ([`crate::parser::parse_powershell`]),
//! a deliberate backup kept in parity with this one by a test (so the two can't
//! silently drift). Both paths degrade gracefully and never panic — which matters
//! because release builds use `panic = "abort"`.

use crate::model::{Param, ParamKind};
use crate::parser::{humanize, kind_from_name, strip_quotes};
use serde::Deserialize;
use std::io::Write;
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Helper script: read the target script from stdin, parse it to an AST (without
/// executing it), and emit the *script-level* param block as a JSON array.
const PARSER_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
    $src = [Console]::In.ReadToEnd()
    $t = $null; $e = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($src, [ref]$t, [ref]$e)
    $pb = $ast.ParamBlock
    $result = @()
    if ($pb) {
        foreach ($p in $pb.Parameters) {
            $mandatory = $false
            $vs = New-Object System.Collections.Generic.List[string]
            foreach ($a in $p.Attributes) {
                if ($a -isnot [System.Management.Automation.Language.AttributeAst]) { continue }
                $n = $a.TypeName.Name
                if ($n -eq 'Parameter') {
                    foreach ($na in $a.NamedArguments) {
                        if ($na.ArgumentName -eq 'Mandatory') {
                            if ($na.ExpressionOmitted -or ("$($na.Argument.Extent.Text)" -match '\$true|\b1\b')) { $mandatory = $true }
                        }
                    }
                } elseif ($n -eq 'ValidateSet') {
                    foreach ($pa in $a.PositionalArguments) {
                        if ($pa.PSObject.Properties.Name -contains 'Value') { $vs.Add([string]$pa.Value) }
                    }
                }
            }
            $result += [PSCustomObject]@{
                name        = $p.Name.VariablePath.UserPath
                type        = if ($p.StaticType) { $p.StaticType.Name } else { 'Object' }
                mandatory   = [bool]$mandatory
                validateSet = @($vs)
                default     = if ($p.DefaultValue) { $p.DefaultValue.Extent.Text } else { $null }
            }
        }
    }
    if ($result.Count -eq 0) { [Console]::Out.Write('[]') }
    elseif ($result.Count -eq 1) { [Console]::Out.Write('[' + (ConvertTo-Json -InputObject $result[0] -Depth 6 -Compress) + ']') }
    else { [Console]::Out.Write((ConvertTo-Json -InputObject $result -Depth 6 -Compress)) }
} catch {
    exit 1
}
"#;

#[derive(Deserialize)]
struct ParamJson {
    name: String,
    #[serde(rename = "type", default)]
    type_name: String,
    #[serde(default)]
    mandatory: bool,
    #[serde(rename = "validateSet", default)]
    validate_set: Vec<String>,
    #[serde(default)]
    default: Option<String>,
}

/// Parse the PowerShell `param()` block of `source` via PowerShell's AST.
/// Returns `None` if PowerShell can't be run or its output can't be parsed, so
/// the caller can fall back to the regex parser.
pub fn parse(source: &str) -> Option<Vec<Param>> {
    let script_path = std::env::temp_dir().join("disbatch-paramparse.ps1");
    std::fs::write(&script_path, PARSER_SCRIPT).ok()?;

    let mut cmd = Command::new("powershell.exe");
    cmd.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = cmd.spawn().ok()?;
    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let json = json.trim();
    if json.is_empty() {
        return None;
    }
    let parsed: Vec<ParamJson> = serde_json::from_str(json).ok()?;
    Some(parsed.into_iter().map(json_to_param).collect())
}

fn json_to_param(j: ParamJson) -> Param {
    let t = j.type_name.to_lowercase();
    let is_switch = t == "switchparameter";
    let is_bool = is_switch || t == "boolean" || t == "bool";
    let is_number = matches!(
        t.as_str(),
        "int32" | "int64" | "int16" | "long" | "int" | "double" | "single" | "float"
            | "decimal" | "byte" | "uint32" | "uint64" | "uint16"
    );

    let kind = if is_bool {
        ParamKind::Bool
    } else if !j.validate_set.is_empty() {
        ParamKind::Choice
    } else if is_number {
        ParamKind::Number
    } else {
        kind_from_name(&j.name)
    };

    let mut value = String::new();
    let mut bool_value = false;
    if let Some(d) = j.default.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            if kind == ParamKind::Bool {
                bool_value = d.eq_ignore_ascii_case("$true") || d.eq_ignore_ascii_case("true");
            } else {
                value = strip_quotes(d);
            }
        }
    }
    if kind == ParamKind::Choice && value.is_empty() {
        if let Some(first) = j.validate_set.first() {
            value = first.clone();
        }
    }

    Param {
        label: humanize(&j.name),
        name: j.name,
        kind,
        required: j.mandatory,
        is_switch,
        choices: j.validate_set,
        value,
        bool_value,
        position: None,
        custom: false,
        as_env: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_basic_param_block() {
        let src = r#"
            param(
                [Parameter(Mandatory = $true)][string]$InputFolder,
                [ValidateSet("Low", "High")][string]$Quality = "Low",
                [int]$Count = 3,
                [switch]$Force
            )
            Write-Host "hi"
        "#;
        let params = parse(src).expect("PowerShell AST parse should succeed");
        assert_eq!(params.len(), 4);
        let by = |n: &str| params.iter().find(|p| p.name == n).unwrap();
        assert!(by("InputFolder").required);
        assert_eq!(by("InputFolder").kind, ParamKind::FolderPath);
        assert_eq!(by("Quality").kind, ParamKind::Choice);
        assert_eq!(by("Quality").choices, vec!["Low", "High"]);
        assert_eq!(by("Quality").value, "Low");
        assert_eq!(by("Count").kind, ParamKind::Number);
        assert!(by("Force").is_switch);
    }

    /// The regex parser's nemesis: a comment with an unbalanced paren, a
    /// multi-line `[Parameter()]`, and a default value containing parentheses.
    /// PowerShell's real parser handles all of it.
    #[test]
    fn handles_comments_and_multiline_attributes() {
        let src = r#"
            # leading comment (with parens
            param(
                # a comment with ) an unbalanced paren
                [Parameter(
                    Mandatory
                )]
                [string]$Name = "default (with parens)"
            )
        "#;
        let params = parse(src).expect("parse should succeed");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "Name");
        assert!(params[0].required);
        assert_eq!(params[0].value, "default (with parens)");
    }

    /// A param block inside a function is NOT the script's entry params — and a
    /// gnarly/illegal one must not crash the parser.
    #[test]
    fn function_param_block_is_ignored_and_never_crashes() {
        let src = r#"Function Foo {
            [CmdletBinding()]
            param(
                [Parameter(Mandatory, ValueFromPipeline)][ValidateScript({ $false })]
                [string]$Bar = ("x")
            )
        }"#;
        let params = parse(src).expect("parse should still succeed");
        assert!(params.is_empty());
    }

    /// The reviewer's FULL pathological script — unbalanced parens, a mismatched
    /// bracket, a stray interrobang — run through the *real* AST path. PowerShell
    /// records the syntax errors internally but still returns; the param block
    /// lives inside a function, so there are no *script-level* params. The
    /// contract (priority #1): never panic, never hang, never emit garbage.
    #[test]
    fn full_adversarial_input_through_ast_does_not_crash() {
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
        // None = subprocess hiccup (acceptable degradation); Some must be empty.
        if let Some(params) = parse(gnarly) {
            assert!(
                params.is_empty(),
                "expected no script-level params, got {params:?}"
            );
        }
    }

    /// The regex fallback is a deliberate backup, not a second opinion — on the
    /// conventional param blocks users actually write it must produce the *same*
    /// controls as the AST parser, so degrading to it is invisible. (Adversarial
    /// inputs where the AST parser is strictly better are covered separately.)
    #[test]
    fn regex_fallback_matches_ast_on_conventional_blocks() {
        let corpus = [
            r#"param([Parameter(Mandatory = $true)][string]$InputFolder, [int]$Count = 3)"#,
            r#"param([ValidateSet("Low","High")][string]$Quality = "Low", [switch]$Force)"#,
            r#"param([string]$Name = "bob", [string]$LogFile, [bool]$Flag = $false)"#,
        ];
        for src in corpus {
            let ast = parse(src).expect("AST parse should succeed");
            let rgx = crate::parser::parse_powershell(src);
            assert_eq!(ast.len(), rgx.len(), "param count differs for: {src}");
            for (a, r) in ast.iter().zip(rgx.iter()) {
                assert_eq!(a.name, r.name, "name differs in: {src}");
                assert_eq!(a.kind, r.kind, "kind differs for {} in: {src}", a.name);
                assert_eq!(a.required, r.required, "required differs for {} in: {src}", a.name);
                assert_eq!(a.is_switch, r.is_switch, "is_switch differs for {} in: {src}", a.name);
                assert_eq!(a.choices, r.choices, "choices differ for {} in: {src}", a.name);
                assert_eq!(a.value, r.value, "default differs for {} in: {src}", a.name);
            }
        }
    }

    /// Why the AST parser is primary, made concrete: a `ValidateSet` whose values
    /// contain `]` is a clean dropdown under the AST parser but degrades under the
    /// regex fallback (which can't see the `]` inside the attribute string). This
    /// is the divergence `examples/parser-tricky.ps1` shows live via DISBATCH_NO_AST.
    #[test]
    fn ast_outparses_regex_on_a_bracketed_validateset() {
        let src = r#"param([ValidateSet("[1] Fast", "[2] Thorough")][string]$Mode = "[1] Fast")"#;
        let ast = parse(src).expect("AST parse should succeed");
        assert_eq!(ast.len(), 1);
        assert_eq!(ast[0].kind, ParamKind::Choice);
        assert_eq!(ast[0].choices, vec!["[1] Fast", "[2] Thorough"]);
        assert_eq!(ast[0].value, "[1] Fast");

        // The regex fallback loses the dropdown — it can't be a Choice anymore.
        let rgx = crate::parser::parse_powershell(src);
        assert_eq!(rgx.len(), 1);
        assert_ne!(rgx[0].kind, ParamKind::Choice);
    }
}
