//! The boot-evidence signature table, compiled in from the data file that owns
//! it.
//!
//! `boot_signatures.txt` is the source of truth for what a boot log means, and
//! it has three consumers that cannot check each other at compile time: this
//! module, `scripts/boot_signatures.py`, and `crates/rudy-boot`. Before AR-15
//! the *Rust source* was the source: a Python test parsed the
//! `let patterns = [...]` literal out of `diagnostics.rs`. That caught ordinary
//! drift, at the price of pinning the shape of a Rust function against a Python
//! regular expression — and it could not see the divergence that actually
//! existed. The two scanners compared the same table with different case
//! sensitivity, because matching policy lived in neither copy.
//!
//! **A malformed table is a panic, not an empty table.** The alternative is a
//! scanner that quietly matches nothing, reports no failures, and makes every
//! boot look clean. The data is `include_str!`'d, so it cannot be missing at
//! runtime and a malformed one is an authoring error a test catches.

use std::sync::OnceLock;

/// The table itself, compiled into the binary.
const SIGNATURE_DATA: &str = include_str!("boot_signatures.txt");

/// The only schema version this parser accepts.
const SUPPORTED_SCHEMA: u32 = 1;

/// The only matching policy this parser accepts.
///
/// A pattern matches a line when it appears anywhere in it, compared with
/// case folding on both sides. Declared in the data rather than assumed by each
/// consumer, because two consumers assuming differently is exactly what
/// happened.
const SUBSTRING_CASEFOLD: &str = "substring-casefold";

/// Why a signature table could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("line {line}: {detail}")]
    Malformed { line: usize, detail: String },
    #[error(
        "the signature table declares schema {found}, and this build reads {SUPPORTED_SCHEMA}"
    )]
    UnsupportedSchema { found: String },
    #[error("the signature table is missing a required record: {0}")]
    Missing(&'static str),
    #[error("the signature table names {0:?} twice")]
    Duplicate(String),
}

/// Everything the table says, parsed.
///
/// Every string borrows the compiled-in data, so this allocates nothing beyond
/// the vectors and lives for the life of the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSignatures {
    pub ready_marker: &'static str,
    pub starting_marker: &'static str,
    pub error_prefix: &'static str,
    pub fatal: Vec<&'static str>,
    pub retryable: Vec<&'static str>,
    pub current_trace_key: &'static str,
    pub previous_trace_key: &'static str,
}

/// The table, parsed once.
///
/// Panics if the compiled-in data is malformed. That is deliberate and it is
/// the whole point of the type: the failure mode this replaces is a scanner
/// that finds no fatal patterns and calls every boot clean.
pub fn signatures() -> &'static BootSignatures {
    // `OnceLock` rather than `LazyLock`: the latter is the tidier spelling and
    // needs Rust 1.80, and this project's supported toolchain floor is an open
    // question AR-19 owns. Raising it as a side effect of a boot-evidence
    // refactor would be deciding that question by accident.
    static PARSED: OnceLock<BootSignatures> = OnceLock::new();
    PARSED.get_or_init(|| {
        parse(SIGNATURE_DATA).unwrap_or_else(|error| {
            panic!(
                "the compiled-in boot signature table is malformed: {error}. \
                 Refusing to run with an empty signature table, which would \
                 report every boot as clean."
            )
        })
    })
}

/// Parses a signature table.
///
/// Public so a test can drive it with a deliberately broken table and see the
/// refusal, rather than only ever seeing the one table that works.
pub fn parse(text: &'static str) -> Result<BootSignatures, SignatureError> {
    let mut schema: Option<&str> = None;
    let mut policy: Option<&str> = None;
    let mut markers: Vec<(&str, &str)> = Vec::new();
    let mut traces: Vec<(&str, &str)> = Vec::new();
    let mut fatal: Vec<&str> = Vec::new();
    let mut retryable: Vec<&str> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() || raw.starts_with('#') {
            continue;
        }
        let malformed = |detail: &str| SignatureError::Malformed {
            line,
            detail: detail.to_string(),
        };
        let mut fields = raw.split('\t');
        let kind = fields.next().ok_or_else(|| malformed("no record kind"))?;
        let rest: Vec<&str> = fields.collect();

        // A pair record's value may not be empty, and neither may a pattern:
        // an empty needle matches every line, which would turn one blank field
        // into "every boot failed".
        let single = |what: &'static str| -> Result<&str, SignatureError> {
            match rest.as_slice() {
                [value] if !value.is_empty() => Ok(*value),
                [_] => Err(malformed(&format!("{what} has an empty value"))),
                _ => Err(malformed(&format!(
                    "{what} takes exactly one tab-separated value, found {}",
                    rest.len()
                ))),
            }
        };
        let pair = |what: &'static str| -> Result<(&str, &str), SignatureError> {
            match rest.as_slice() {
                [name, value] if !name.is_empty() && !value.is_empty() => Ok((*name, *value)),
                [_, _] => Err(malformed(&format!("{what} has an empty name or value"))),
                _ => Err(malformed(&format!(
                    "{what} takes a name and a value, found {} field(s)",
                    rest.len()
                ))),
            }
        };

        match kind {
            "schema" => schema = Some(single("schema")?),
            "match" => policy = Some(single("match")?),
            "marker" => markers.push(pair("marker")?),
            "trace" => traces.push(pair("trace")?),
            "fatal" => fatal.push(single("fatal")?),
            "retryable" => retryable.push(single("retryable")?),
            // Never skipped. A row this parser does not understand is a row it
            // is not scanning for, and a table that quietly loses rows scans
            // for less than it says it does.
            other => return Err(malformed(&format!("unknown record kind {other:?}"))),
        }
    }

    match schema {
        Some(value) if value == SUPPORTED_SCHEMA.to_string() => {}
        Some(found) => {
            return Err(SignatureError::UnsupportedSchema {
                found: found.to_string(),
            })
        }
        None => return Err(SignatureError::Missing("schema")),
    }
    match policy {
        Some(SUBSTRING_CASEFOLD) => {}
        Some(other) => {
            return Err(SignatureError::Malformed {
                line: 0,
                detail: format!(
                    "matching policy {other:?} is not one this build implements; \
                     it reads {SUBSTRING_CASEFOLD:?}"
                ),
            })
        }
        None => return Err(SignatureError::Missing("match")),
    }

    let named = |records: &[(&'static str, &'static str)],
                 name: &'static str,
                 what: &'static str|
     -> Result<&'static str, SignatureError> {
        let mut found = records.iter().filter(|(key, _)| *key == name);
        let first = found.next().ok_or(SignatureError::Missing(what))?.1;
        if found.next().is_some() {
            return Err(SignatureError::Duplicate(name.to_string()));
        }
        Ok(first)
    };

    let no_duplicates = |patterns: &[&'static str]| -> Result<(), SignatureError> {
        for (index, pattern) in patterns.iter().enumerate() {
            if patterns[..index].contains(pattern) {
                return Err(SignatureError::Duplicate((*pattern).to_string()));
            }
        }
        Ok(())
    };
    no_duplicates(&fatal)?;
    no_duplicates(&retryable)?;

    if fatal.is_empty() {
        return Err(SignatureError::Missing("at least one fatal pattern"));
    }

    Ok(BootSignatures {
        ready_marker: named(&markers, "ready", "marker ready")?,
        starting_marker: named(&markers, "starting", "marker starting")?,
        error_prefix: named(&markers, "error_prefix", "marker error_prefix")?,
        fatal,
        retryable,
        current_trace_key: named(&traces, "current", "trace current")?,
        previous_trace_key: named(&traces, "previous", "trace previous")?,
    })
}

/// Whether `pattern` matches `line` under the table's declared policy.
///
/// One function, so a consumer cannot implement the policy slightly
/// differently from the table that declares it.
pub fn matches(pattern: &str, line: &str) -> bool {
    line.to_lowercase().contains(&pattern.to_lowercase())
}
