//! What the Rust tests share with `scripts/suite_cases.py`.

/// The file-name prefixes `suite_cases.py`'s `ISO_FAMILIES` lists for `family`,
/// each pattern with its `*.iso` taken off.
///
/// The Rust tests that read staged images keep their own family tables, and
/// those drifted from the suite's (PRV-11, ST-B12). Each table is checked
/// against this, so a pattern changed in one place fails until it is changed in
/// the other.
pub fn suite_patterns(family: &str) -> Vec<String> {
    let suite = include_str!("../../../../scripts/suite_cases.py");
    let table = &suite[suite
        .find("ISO_FAMILIES = {")
        .expect("suite_cases.py declares ISO_FAMILIES")..];
    let table = &table[..table.find("\n}").expect("ISO_FAMILIES closes")];
    let line = table
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&format!("\"{family}\":")))
        .unwrap_or_else(|| panic!("ISO_FAMILIES has no {family:?}"));
    line.split('"')
        .skip(3)
        .step_by(2)
        .map(|pattern| {
            pattern
                .strip_suffix("*.iso")
                .unwrap_or_else(|| panic!("{family}: {pattern:?} does not end in *.iso"))
                .to_string()
        })
        .collect()
}
