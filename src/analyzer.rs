//! Local, offline heuristic risk analyzer for .ps1 / .bat scripts.
//!
//! This is NOT antivirus. It performs a static, pattern-based scan and surfaces
//! risky *capabilities* (download-and-run, encoded commands, keyboard hooks,
//! persistence, shadow-copy deletion, etc.) so the user can make an informed
//! choice before running a script. It catches obvious and lightly-obfuscated
//! patterns; determined obfuscation can evade it, and it will produce false
//! positives (plenty of legitimate scripts download files or touch the registry).

use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Caution,
    Warning,
}

impl Severity {
    pub fn rank(self) -> u8 {
        match self {
            Severity::Warning => 1,
            Severity::Caution => 0,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Severity::Warning => "WARNING",
            Severity::Caution => "CAUTION",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub category: &'static str,
    pub title: &'static str,
    pub line: usize, // 1-based
    pub snippet: String,
}

struct Rule {
    severity: Severity,
    category: &'static str,
    title: &'static str,
    re: Regex,
}

fn rule(severity: Severity, category: &'static str, title: &'static str, pattern: &str) -> Rule {
    Rule {
        severity,
        category,
        title,
        re: Regex::new(pattern).expect("valid analyzer regex"),
    }
}

fn rules() -> &'static Vec<Rule> {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(build_rules)
}

#[rustfmt::skip]
fn build_rules() -> Vec<Rule> {
    use Severity::*;
    vec![
        // --- Download & remote code execution --------------------------------
        rule(Warning, "Download & execute", "Downloads code straight into memory",          r"(?i)downloadstring|downloaddata"),
        rule(Caution, "Network",            "Downloads a file from the internet",            r"(?i)downloadfile|start-bitstransfer"),
        rule(Caution, "Network",            "Makes an HTTP request",                         r"(?i)\b(invoke-webrequest|iwr|invoke-restmethod|irm|wget|curl)\b"),
        rule(Caution, "Network",            "Uses the .NET WebClient",                       r"(?i)net\.webclient"),
        rule(Warning, "LOLBin",             "certutil used to download/decode",              r"(?i)certutil(\.exe)?.*(-urlcache|-decode|-f\s+http)"),
        rule(Warning, "LOLBin",             "bitsadmin file transfer",                       r"(?i)bitsadmin(\.exe)?.*/transfer"),
        // --- Code from strings / obfuscation ---------------------------------
        rule(Warning, "Code execution",     "Executes a string as code (Invoke-Expression)", r"(?i)\b(invoke-expression|iex)\b"),
        rule(Warning, "Obfuscation",        "Runs a Base64-encoded command (-EncodedCommand)", r"(?i)(-encodedcommand|-enc)\b"),
        rule(Caution, "Obfuscation",        "Decodes Base64 data",                           r"(?i)frombase64string"),
        rule(Warning, "Defense evasion",    "Contains an AMSI-bypass marker",                r"(?i)amsiutils|amsiinitfailed|amsicontext"),
        rule(Caution, "Reflection",         "Loads a .NET assembly reflectively",            r"(?i)reflection\.assembly"),
        // --- Native API / keylogging -----------------------------------------
        rule(Warning, "Keylogger",          "Keyboard hook / keystroke capture API",         r"(?i)setwindowshookex|wh_keyboard_ll|getasynckeystate|getkeyboardstate|registerrawinputdevices"),
        rule(Caution, "Native code",        "Compiles or loads inline native code (Add-Type)", r"(?i)add-type"),
        rule(Caution, "Native code",        "Imports a native Win32 API (P/Invoke)",         r"(?i)dllimport|\buser32\.dll|\bkernel32\.dll"),
        rule(Caution, "Window",             "Reads the foreground/active window",            r"(?i)getforegroundwindow|getwindowtext"),
        // --- Persistence -----------------------------------------------------
        rule(Warning, "Persistence",        "Registry Run-key autostart",                    r"(?i)currentversion\\run"),
        rule(Caution, "Registry",           "Modifies the registry",                         r"(?i)\breg(\.exe)?\s+add|new-itemproperty|set-itemproperty"),
        rule(Warning, "Persistence",        "Creates a scheduled task",                      r"(?i)schtasks(\.exe)?\s+/create|register-scheduledtask"),
        rule(Warning, "Persistence",        "Writes to the Startup folder",                  r"(?i)\\start menu\\programs\\startup"),
        // --- Destructive -----------------------------------------------------
        rule(Warning, "Ransomware",         "Deletes volume shadow copies",                  r"(?i)vssadmin(\.exe)?\s+delete\s+shadows"),
        rule(Warning, "Anti-forensics",     "Wipes free disk space (cipher /w)",             r"(?i)cipher(\.exe)?\s+/w"),
        rule(Warning, "Destructive",        "Formats a drive",                               r"(?i)\bformat\s+[a-z]:"),
        rule(Caution, "Destructive",        "Recursive force delete",                        r"(?i)remove-item\b.*-recurse.*-force|\brd\s+/s\s+/q|\bdel\s+/[a-z]"),
        rule(Caution, "Boot",               "Edits boot configuration (bcdedit)",            r"(?i)\bbcdedit\b"),
        // --- Stealth / living-off-the-land -----------------------------------
        rule(Caution, "Stealth",            "Runs with a hidden window",                     r"(?i)-windowstyle\s+hidden|\s-w\s+hidden\b"),
        rule(Caution, "LOLBin",             "Indirect execution (rundll32/mshta/regsvr32/wscript/cscript)", r"(?i)\b(rundll32|mshta|regsvr32|wscript|cscript)\b"),
        rule(Caution, "Stealth",            "Hides files (attrib +h)",                       r"(?i)attrib\s+.*\+h"),
        // --- Credentials / exfiltration --------------------------------------
        rule(Warning, "Credential theft",   "Credential-dumping tooling or keywords",        r"(?i)\bmimikatz\b|sekurlsa|\blsass\b"),
        rule(Caution, "Credentials",        "Handles credentials in plaintext",              r"(?i)convertto-securestring.*-asplaintext|get-credential"),
        rule(Caution, "Clipboard",          "Reads the clipboard",                           r"(?i)get-clipboard"),
        // --- Privilege / policy / security toggles ---------------------------
        rule(Caution, "Elevation",          "Requests administrator elevation",              r"(?i)#requires\s+-runasadministrator|-verb\s+runas"),
        rule(Caution, "Policy",             "Weakens the PowerShell execution policy",       r"(?i)set-executionpolicy\s+(bypass|unrestricted)"),
        rule(Warning, "Defense evasion",    "Disables Defender or the firewall",             r"(?i)disablerealtimemonitoring|-exclusionpath|netsh\s+(advfirewall|firewall)|set-mppreference"),
    ]
}

/// Scan a script and return findings, most severe first.
pub fn analyze(source: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        for rule in rules() {
            if rule.re.is_match(line) {
                findings.push(Finding {
                    severity: rule.severity,
                    category: rule.category,
                    title: rule.title,
                    line: idx + 1,
                    snippet: line.trim().chars().take(160).collect(),
                });
            }
        }
    }
    findings.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then(a.line.cmp(&b.line))
    });
    findings
}

/// True if any finding is warning-level (used to gate the Run button).
pub fn has_warning(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Warning)
}

/// (warning, caution) counts.
pub fn counts(findings: &[Finding]) -> (usize, usize) {
    let mut c = (0usize, 0usize);
    for f in findings {
        match f.severity {
            Severity::Warning => c.0 += 1,
            Severity::Caution => c.1 += 1,
        }
    }
    c
}
