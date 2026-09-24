//! Review units and the coverage contract (spec 14)
//!
//! A review unit is the smallest thing this crate dispatches, isolates, counts,
//! aggregates and budgets as a whole. `select` is the *single* implementation
//! of "which changed files enter the review, and why not": preview and the real
//! run must consume the same answer, because two derivations of the same
//! decision drift apart (spec 14 §2).
//!
//! This module owns selection and the ledger's vocabulary. It does not talk to
//! the network, the model or the platform: everything here is a pure function of
//! the diff text and the configuration.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use globset::GlobSet;
use sha1::{Digest, Sha1};

use crate::agent::compaction::estimate_tokens;
use crate::diff;

/// Where a review unit comes from.
///
/// v1 implements `Changeset` only. `File` is the reserved seam for later input
/// forms (whole-file or directory reviews), which must reuse this unit rather
/// than grow a parallel pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitSource {
    Changeset,
    File,
}

impl UnitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UnitSource::Changeset => "changeset",
            UnitSource::File => "file",
        }
    }
}

/// One review unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewUnit {
    /// Content-independent identity: `{source}:{primary path}`.
    pub unit_id: String,
    pub source: UnitSource,
    /// Files in this unit, in order. v1 is always a single file.
    pub files: Vec<String>,
    /// Content fingerprint of what was selected (see `content_fingerprint`).
    /// Distinct from the finding fingerprint in `crate::state`: that one tracks
    /// "which problem", this one answers "which revision of this input".
    pub unit_fp: String,
}

/// Why a changed file is not part of the selected set (spec 14 §2).
///
/// The enum is exhaustive on purpose: a new reason is a spec change. `Binary`
/// is set where the platform reports a file without a patch (a binary or a file
/// too large for the API to inline); the text-level selector never sees such a
/// file at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcludeReason {
    Binary,
    Deleted,
    Generated,
    Ignored,
    Oversized,
    SecretPath,
    Extension,
    DefaultPath,
}

impl ExcludeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExcludeReason::Binary => "binary",
            ExcludeReason::Deleted => "deleted",
            ExcludeReason::Generated => "generated",
            ExcludeReason::Ignored => "ignored",
            ExcludeReason::Oversized => "oversized",
            ExcludeReason::SecretPath => "secret-path",
            ExcludeReason::Extension => "extension",
            ExcludeReason::DefaultPath => "default-path",
        }
    }
}

/// A changed file that is not reviewed, with the reason and the note whether it
/// still travels to the model as context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedFile {
    pub path: String,
    pub reason: ExcludeReason,
    /// Deleted files carry no new content to review, but the diff still needs
    /// them for anchoring (spec 06), so they stay in the text.
    pub kept_in_text: bool,
}

/// The one selection result: what the model will receive, what the coverage
/// denominator is, and every exclusion with its reason.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Assembled diff text handed to the model.
    pub text: String,
    /// Reviewable units == the coverage denominator (spec 14 §4).
    pub units: Vec<ReviewUnit>,
    pub excluded: Vec<ExcludedFile>,
    pub estimated_tokens: u64,
}

impl Selection {
    /// Exclusions that removed the file from the model's input entirely
    /// (deleted files are excluded from review but kept in the text).
    pub fn excluded_count(&self) -> usize {
        self.excluded.iter().filter(|e| !e.kept_in_text).count()
    }

    /// Exclusions decided by the path gates only (ignore glob, generated-code
    /// heuristic, ...), excluding the ones the size budget decided later. This
    /// is the count the pre-spec-14 callers reported as "filtered out by rules".
    pub fn path_gate_excluded_count(&self) -> usize {
        self.excluded
            .iter()
            .filter(|e| !e.kept_in_text && e.reason != ExcludeReason::Oversized)
            .count()
    }

    /// Paths dropped by the size budget, in the order the budget dropped them.
    pub fn oversized_dropped(&self) -> Vec<String> {
        self.excluded
            .iter()
            .filter(|e| e.reason == ExcludeReason::Oversized)
            .map(|e| e.path.clone())
            .collect()
    }

    /// Exclusions of one reason (diagnostics, preview).
    pub fn excluded_by(&self, reason: ExcludeReason) -> Vec<&ExcludedFile> {
        self.excluded
            .iter()
            .filter(|e| e.reason == reason)
            .collect()
    }
}

/// Lifecycle of one unit inside a run (spec 14 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitState {
    Pending,
    Running,
    Covered,
    Failed(String),
    Truncated(String),
}

/// Terminal state of a run, derived from coverage alone (spec 14 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalState {
    #[default]
    Empty,
    Ok,
    Partial,
}

impl TerminalState {
    pub fn as_str(self) -> &'static str {
        match self {
            TerminalState::Ok => "ok",
            TerminalState::Partial => "partial",
            TerminalState::Empty => "empty",
        }
    }
}

/// Counts derived from a ledger: what the coverage line and the machine-readable
/// output need, in one place so they cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoverageSummary {
    pub total: usize,
    pub covered: usize,
    pub failed: usize,
    pub truncated: usize,
    pub terminal: TerminalState,
}

/// Coverage ledger: the frozen denominator plus one state per unit.
///
/// The denominator is frozen before the first dispatch and cannot be extended
/// afterwards, so a later arrival (or a retry) can never move the goalposts of
/// what this run promised to review (spec 14 §4).
///
/// v1 note: the pipeline dispatches the whole change set to N parallel passes
/// (spec 05), so a unit's state follows the fate of that dispatch rather than a
/// per-unit worker — see the spec's v1 note. The per-unit setters exist so the
/// grouped/per-unit dispatch that arrives with later input forms needs no new
/// vocabulary.
#[derive(Debug, Clone, Default)]
pub struct CoverageLedger {
    denominator: Vec<String>,
    states: BTreeMap<String, UnitState>,
}

impl CoverageLedger {
    /// Freeze the denominator from the selected units (duplicates collapse).
    pub fn freeze(units: &[ReviewUnit]) -> Self {
        let mut denominator = Vec::with_capacity(units.len());
        let mut states = BTreeMap::new();
        for unit in units {
            if states.contains_key(&unit.unit_id) {
                continue;
            }
            denominator.push(unit.unit_id.clone());
            states.insert(unit.unit_id.clone(), UnitState::Pending);
        }
        CoverageLedger {
            denominator,
            states,
        }
    }

    pub fn denominator(&self) -> &[String] {
        &self.denominator
    }

    pub fn state(&self, unit_id: &str) -> Option<&UnitState> {
        self.states.get(unit_id)
    }

    pub fn is_empty(&self) -> bool {
        self.denominator.is_empty()
    }

    pub fn start(&mut self, unit_id: &str) {
        self.set(unit_id, UnitState::Running);
    }

    pub fn cover(&mut self, unit_id: &str) {
        self.set(unit_id, UnitState::Covered);
    }

    pub fn fail(&mut self, unit_id: &str, reason: impl Into<String>) {
        self.set(unit_id, UnitState::Failed(reason.into()));
    }

    pub fn truncate(&mut self, unit_id: &str, reason: impl Into<String>) {
        self.set(unit_id, UnitState::Truncated(reason.into()));
    }

    pub fn start_all(&mut self) {
        self.set_all(UnitState::Running);
    }

    pub fn cover_all(&mut self) {
        self.set_all(UnitState::Covered);
    }

    pub fn fail_all(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.set_all(UnitState::Failed(reason));
    }

    pub fn truncate_all(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.set_all(UnitState::Truncated(reason));
    }

    pub fn covered_count(&self) -> usize {
        self.states
            .values()
            .filter(|s| matches!(s, UnitState::Covered))
            .count()
    }

    /// Units that are not covered, with their state (diagnostics, report line).
    pub fn uncovered(&self) -> Vec<(&str, &UnitState)> {
        self.denominator
            .iter()
            .filter_map(|id| match self.states.get(id) {
                Some(UnitState::Covered) | None => None,
                Some(state) => Some((id.as_str(), state)),
            })
            .collect()
    }

    /// Counts for the coverage line and for machine-readable output (spec 14 §4,
    /// spec 16 §2).
    pub fn summary(&self) -> CoverageSummary {
        let mut summary = CoverageSummary {
            total: self.denominator.len(),
            ..CoverageSummary::default()
        };
        for state in self.states.values() {
            match state {
                UnitState::Covered => summary.covered += 1,
                UnitState::Failed(_) => summary.failed += 1,
                UnitState::Truncated(_) => summary.truncated += 1,
                UnitState::Pending | UnitState::Running => {}
            }
        }
        summary.terminal = self.terminal();
        summary
    }

    /// Terminal state derived from coverage, never from success claims (spec 14 §4).
    pub fn terminal(&self) -> TerminalState {
        if self.denominator.is_empty() {
            TerminalState::Empty
        } else if self.covered_count() == self.denominator.len() {
            TerminalState::Ok
        } else {
            TerminalState::Partial
        }
    }

    /// Unknown ids are ignored on purpose: the denominator is frozen.
    fn set(&mut self, unit_id: &str, state: UnitState) {
        if let Some(slot) = self.states.get_mut(unit_id) {
            *slot = state;
        }
    }

    fn set_all(&mut self, state: UnitState) {
        let ids = self.denominator.clone();
        for id in ids {
            self.set(&id, state.clone());
        }
    }
}

/// Selection knobs, bundled because the set grows (strictness today, grouping
/// next) and because "the same selection" must also mean the same options
/// (spec 14 §2).
#[derive(Debug, Clone, Copy)]
pub struct SelectOptions<'a> {
    pub ignore: &'a GlobSet,
    /// `None` = no size budget (the anchoring path).
    pub max_diff_kb: Option<usize>,
    /// Enable the exclusion classes that *shrink* what is reviewed:
    /// `secret-path`, `default-path`, `extension` (spec 14 §2).
    pub strict: bool,
    /// Merge deterministic sibling files into one unit (spec 14 §1).
    pub group_units: bool,
}

impl<'a> SelectOptions<'a> {
    pub fn new(ignore: &'a GlobSet, max_diff_kb: usize) -> Self {
        Self {
            ignore,
            max_diff_kb: Some(max_diff_kb),
            strict: false,
            group_units: false,
        }
    }

    /// No size budget: every file that survives the path gates.
    pub fn unbounded(ignore: &'a GlobSet) -> Self {
        Self {
            ignore,
            max_diff_kb: None,
            strict: false,
            group_units: false,
        }
    }

    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    pub fn group_units(mut self, group_units: bool) -> Self {
        self.group_units = group_units;
        self
    }
}

/// The one selection implementation (spec 14 §2).
pub fn select(input: &str, opts: &SelectOptions<'_>) -> Selection {
    select_impl(input, opts)
}

/// Paths whose content must not travel to a model provider (spec 14 §2).
///
/// Both the bare and the `**/`-prefixed form are listed for the same name: the
/// bare form catches a repository root, the prefixed one any depth, and the glob
/// semantics of "zero or more directories" are easy to get subtly wrong.
const SECRET_PATH_PATTERNS: &[&str] = &[
    ".env",
    "**/.env",
    ".env.*",
    "**/.env.*",
    "*.env",
    "**/*.env",
    "*.pem",
    "**/*.pem",
    "*.key",
    "**/*.key",
    "*.p12",
    "**/*.p12",
    "*.pfx",
    "**/*.pfx",
    "*.jks",
    "**/*.jks",
    "*.keystore",
    "**/*.keystore",
    "id_rsa*",
    "**/id_rsa*",
    "id_ed25519*",
    "**/id_ed25519*",
    "id_ecdsa*",
    "**/id_ecdsa*",
    "id_dsa*",
    "**/id_dsa*",
    ".netrc",
    "**/.netrc",
    ".npmrc",
    "**/.npmrc",
    ".pypirc",
    "**/.pypirc",
    ".htpasswd",
    "**/.htpasswd",
    "credentials",
    "**/credentials",
    "credentials.json",
    "**/credentials.json",
    "*service-account*.json",
    "**/*service-account*.json",
    "secrets.yml",
    "**/secrets.yml",
    "secrets.yaml",
    "**/secrets.yaml",
    "*.tfstate",
    "**/*.tfstate",
    "*.tfstate.backup",
    "**/*.tfstate.backup",
    "kubeconfig",
    "**/kubeconfig",
    ".ssh/**",
    "**/.ssh/**",
    ".docker/config.json",
    "**/.docker/config.json",
];

/// Built-in "not worth a model's attention" paths (spec 14 §2): dependency trees,
/// build output, lockfiles, generated code.
const DEFAULT_PATH_PATTERNS: &[&str] = &[
    "**/node_modules/**",
    "**/vendor/**",
    "**/dist/**",
    "**/build/**",
    "**/target/**",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
    "**/*.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/go.sum",
    "**/*.generated.*",
    "**/*.pb.go",
    "**/*.g.dart",
];

fn build_globset(patterns: &[&str]) -> GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        // A bad built-in pattern would silently disable a gate, so fail loudly;
        // `built_in_catalogs_compile` keeps this unreachable in practice.
        builder.add(globset::Glob::new(pattern).expect("built-in exclusion pattern must compile"));
    }
    builder
        .build()
        .expect("built-in exclusion globset must build")
}

fn secret_globset() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| build_globset(SECRET_PATH_PATTERNS))
}

fn default_path_globset() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| build_globset(DEFAULT_PATH_PATTERNS))
}

fn select_impl(input: &str, opts: &SelectOptions<'_>) -> Selection {
    let ignore = opts.ignore;
    let mut excluded = Vec::new();
    let mut filtered = String::with_capacity(input.len());

    for section in diff::split_sections(input) {
        // Header lines and sections whose path is unrecognized have no gates to
        // apply and are simply kept.
        if let Some(path) = diff::section_path(section) {
            // Fixed order (spec 14 §2) so that one file has exactly one reason.
            if ignore.is_match(path) {
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::Ignored,
                    kept_in_text: false,
                });
                continue;
            }
            if opts.strict && secret_globset().is_match(path) {
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::SecretPath,
                    kept_in_text: false,
                });
                continue;
            }
            if opts.strict && default_path_globset().is_match(path) {
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::DefaultPath,
                    kept_in_text: false,
                });
                continue;
            }
            if diff::looks_generated(section) {
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::Generated,
                    kept_in_text: false,
                });
                continue;
            }
            if opts.strict && !diff::is_reviewable_path(path) {
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::Extension,
                    kept_in_text: false,
                });
                continue;
            }
            if is_deleted(section) {
                // Not reviewable, but the anchoring pass needs it.
                excluded.push(ExcludedFile {
                    path: path.to_string(),
                    reason: ExcludeReason::Deleted,
                    kept_in_text: true,
                });
            }
        }
        filtered.push_str(section);
        if !section.ends_with('\n') {
            filtered.push('\n');
        }
    }

    let text = match opts.max_diff_kb {
        None => filtered,
        Some(kb) => {
            let truncation = diff::truncate_text(&filtered, kb);
            for path in &truncation.truncated_files {
                excluded.push(ExcludedFile {
                    path: path.clone(),
                    reason: ExcludeReason::Oversized,
                    kept_in_text: false,
                });
            }
            truncation.text
        }
    };

    // Units are derived from the text that is actually dispatched, so the
    // denominator can never claim more than the model received.
    let mut members: Vec<(&str, &str)> = Vec::new();
    for section in diff::split_sections(&text) {
        let Some(path) = diff::section_path(section) else {
            continue;
        };
        if is_deleted(section) {
            continue;
        }
        if members.iter().any(|(p, _)| *p == path) {
            continue; // first wins, matching the documented dedup rule
        }
        members.push((path, section));
    }
    let units = build_units(&members, opts.group_units);

    let estimated_tokens = estimate_tokens(&text);

    Selection {
        text,
        units,
        excluded,
        estimated_tokens,
    }
}

/// Turn selected sections into units, optionally merging deterministic siblings
/// (spec 14 §1). Grouping changes dispatch/accounting granularity only: findings
/// always hang off a concrete file.
fn build_units(members: &[(&str, &str)], group: bool) -> Vec<ReviewUnit> {
    if !group {
        return members
            .iter()
            .map(|(path, section)| single_unit(path, section))
            .collect();
    }

    let mut keys: Vec<(GroupKey, Vec<usize>)> = Vec::new();
    for (index, (path, _)) in members.iter().enumerate() {
        let key = group_key(path);
        match keys.iter_mut().find(|(k, _)| *k == key) {
            Some((_, indexes)) => indexes.push(index),
            None => keys.push((key, vec![index])),
        }
    }

    let mut units = Vec::with_capacity(keys.len());
    for (key, indexes) in keys {
        if indexes.len() == 1 {
            let (path, section) = members[indexes[0]];
            units.push(single_unit(path, section));
            continue;
        }
        let mut group_members: Vec<(&str, &str)> = indexes.iter().map(|i| members[*i]).collect();
        group_members.sort_by(|a, b| a.0.cmp(b.0));
        units.push(ReviewUnit {
            unit_id: group_unit_id(&key),
            source: UnitSource::Changeset,
            files: group_members
                .iter()
                .map(|(p, _)| (*p).to_string())
                .collect(),
            unit_fp: group_content_fingerprint(&group_members),
        });
    }
    units
}

/// Identity of a grouped unit, derived from the group key so that adding or
/// removing a variant never changes it (spec 14 §1): deriving it from a member
/// path made the group change identity as soon as a variant sorted earlier
/// (`README.ja.md` before `README.md`), which a unit test caught.
fn group_unit_id(key: &GroupKey) -> String {
    let name = if key.ext.is_empty() {
        key.stem.clone()
    } else {
        format!("{}.{}", key.stem, key.ext)
    };
    let id = if key.dir.is_empty() {
        name
    } else {
        format!("{}/{name}", key.dir)
    };
    format!("{}:group:{id}", UnitSource::Changeset.as_str())
}

fn single_unit(path: &str, section: &str) -> ReviewUnit {
    ReviewUnit {
        unit_id: format!("{}:{}", UnitSource::Changeset.as_str(), path),
        source: UnitSource::Changeset,
        files: vec![path.to_string()],
        unit_fp: content_fingerprint(path, section),
    }
}

/// Deterministic pairing key: (directory, stem with the variant suffix removed,
/// extension). See spec 14 §1 for the three rules this encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupKey {
    dir: String,
    stem: String,
    ext: String,
}

fn group_key(path: &str) -> GroupKey {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name),
        None => (String::new(), path),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), ext.to_ascii_lowercase()),
        None => (name.to_string(), String::new()),
    };
    let stem = strip_test_suffix(&stem);
    let stem = strip_direction_suffix(&stem);
    let stem = strip_locale_suffix(&stem);
    GroupKey { dir, stem, ext }
}

fn strip_test_suffix(stem: &str) -> String {
    for suffix in ["_test", ".test", "_spec", ".spec"] {
        if let Some(base) = stem.strip_suffix(suffix).filter(|b| !b.is_empty()) {
            return base.to_string();
        }
    }
    stem.to_string()
}

fn strip_direction_suffix(stem: &str) -> String {
    for suffix in [".up", "_up", ".down", "_down"] {
        if let Some(base) = stem.strip_suffix(suffix).filter(|b| !b.is_empty()) {
            return base.to_string();
        }
    }
    stem.to_string()
}

fn strip_locale_suffix(stem: &str) -> String {
    // Longest locale-looking tail first (`zh-CN` must resolve to `zh-CN`, not to
    // a bare `zh` plus a leftover `CN`), which means walking separators left to
    // right and taking the first tail that is a recognised language tag.
    for (index, _) in stem.match_indices(['.', '_', '-']) {
        let (base, tail) = stem.split_at(index);
        let tail = &tail[1..];
        if !base.is_empty() && is_locale(tail) {
            return base.to_string();
        }
    }
    stem.to_string()
}

/// Language / region tags accepted as a translatable variant. Kept explicit so a
/// file called `report-final.md` is never grouped by accident.
fn is_locale(tag: &str) -> bool {
    const LANGUAGES: &[&str] = &[
        "en", "zh", "ja", "ko", "ru", "fr", "de", "es", "pt", "it", "nl", "pl", "tr", "ar", "hi",
        "vi", "th", "id", "uk", "cs", "sv", "da", "fi", "no", "nb", "el", "he", "hu", "ro", "sk",
        "bg", "hr", "sl", "lt", "lv", "et", "ca", "eu", "gl", "sr", "ms", "fil", "sw",
    ];
    let lower = tag.to_ascii_lowercase();
    let (language, region) = match lower.split_once('-') {
        Some((language, region)) => (language, Some(region)),
        None => (lower.as_str(), None),
    };
    if !LANGUAGES.contains(&language) {
        return false;
    }
    match region {
        None => true,
        // `zh-cn`, `pt-br`, `en-us`: region subtags are two letters or a script tag.
        Some(region) => {
            region.len() == 2 || region.len() == 4 || region == "hans" || region == "hant"
        }
    }
}

/// Fingerprint of a group: members sorted by path, each folded in with its path.
fn group_content_fingerprint(members: &[(&str, &str)]) -> String {
    let mut h = Sha1::new();
    for (path, section) in members {
        h.update(path.as_bytes());
        h.update(b"\n");
        h.update(section.as_bytes());
        h.update(b"\n");
    }
    let digest = h.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// A whole-file deletion: no new content to review.
pub(crate) fn is_deleted(section: &str) -> bool {
    section.lines().any(|l| l == "+++ /dev/null")
}

/// Content fingerprint of one unit: sha1 over path + section text, first 8 bytes
/// as hex. Raw bytes (not normalized) on purpose: a whitespace change in the
/// diff is a change of what is being reviewed.
fn content_fingerprint(path: &str, section: &str) -> String {
    let mut h = Sha1::new();
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(section.as_bytes());
    let digest = h.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::GlobSetBuilder;

    fn ignore_set(patterns: &[&str]) -> GlobSet {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(globset::Glob::new(p).unwrap());
        }
        b.build().unwrap()
    }

    fn no_ignore() -> GlobSet {
        ignore_set(&[])
    }

    /// A section big enough that the kilobyte-granular budget actually bites.
    fn padded(path: &str, added_lines: usize) -> String {
        let mut body = String::new();
        for i in 0..added_lines {
            body.push_str(&format!("+line {i} of {path}\n"));
        }
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,0 +1,{added_lines} @@\n{body}"
        )
    }

    const SRC: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,6 +10,7 @@ fn main() {
 context
-old
+new
";

    const LOCK: &str = "\
diff --git a/Cargo.lock b/Cargo.lock
index 3333333..4444444 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,3 +1,4 @@
 name = \"x\"
+version = \"1\"
";

    const GENERATED: &str = "\
diff --git a/gen.rs b/gen.rs
--- a/gen.rs
+++ b/gen.rs
@@ -1,2 +1,3 @@
+// Code generated by tool. DO NOT EDIT.
 fn x() {}
";

    const ENV_FILE: &str = "\
diff --git a/.env b/.env
index 7777777..8888888 100644
--- a/.env
+++ b/.env
@@ -1,0 +1,1 @@
+SECRET=1
";

    const VENDORED: &str = "\
diff --git a/node_modules/pkg/index.js b/node_modules/pkg/index.js
--- a/node_modules/pkg/index.js
+++ b/node_modules/pkg/index.js
@@ -1,0 +1,1 @@
+module.exports = 1;
";

    const UNKNOWN_EXT: &str = "\
diff --git a/assets/logo.psd b/assets/logo.psd
--- a/assets/logo.psd
+++ b/assets/logo.psd
@@ -1,0 +1,1 @@
+binary-ish
";

    const DELETED: &str = "\
diff --git a/gone.rs b/gone.rs
index 5555555..6666666 100644
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn gone() {}
";

    #[test]
    fn select_is_pure() {
        let a = select(
            &format!("{SRC}{LOCK}"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        let b = select(
            &format!("{SRC}{LOCK}"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        assert_eq!(a.units, b.units);
        assert_eq!(a.excluded, b.excluded);
        assert_eq!(a.text, b.text);
        assert_eq!(a.estimated_tokens, b.estimated_tokens);
    }

    #[test]
    fn ignored_path_is_excluded_and_absent_from_text() {
        let sel = select(
            &format!("{SRC}{LOCK}"),
            &SelectOptions::unbounded(&ignore_set(&["Cargo.lock"])),
        );
        assert_eq!(sel.units.len(), 1);
        assert_eq!(sel.units[0].files, vec!["src/main.rs"]);
        assert_eq!(sel.excluded_count(), 1);
        assert_eq!(sel.excluded_by(ExcludeReason::Ignored).len(), 1);
        assert!(!sel.text.contains("Cargo.lock"));
    }

    #[test]
    fn generated_content_is_excluded() {
        let sel = select(GENERATED, &SelectOptions::unbounded(&no_ignore()));
        assert!(sel.units.is_empty());
        assert_eq!(sel.excluded_by(ExcludeReason::Generated).len(), 1);
    }

    #[test]
    fn deleted_file_is_excluded_from_units_but_kept_in_text() {
        let sel = select(
            &format!("{SRC}{DELETED}"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        assert_eq!(sel.units.len(), 1, "deleted files are not reviewable");
        let deleted = sel.excluded_by(ExcludeReason::Deleted);
        assert_eq!(deleted.len(), 1);
        assert!(deleted[0].kept_in_text);
        assert_eq!(
            sel.excluded_count(),
            0,
            "kept in text != removed from input"
        );
        assert!(sel.text.contains("gone.rs"), "anchoring needs the deletion");
    }

    #[test]
    fn budget_drops_the_lowest_priority_file_first() {
        // Both sections are ~700 bytes, so a 1 KB budget fits one of them. The
        // lock file is config priority (3) and src/main.rs is source priority
        // (0), so the lock file goes first while the first section is always
        // kept (floor guarantee, spec 03).
        let src = padded("src/main.rs", 60);
        let lock = padded("Cargo.lock", 60);
        assert!(
            src.len() + lock.len() > 1024,
            "fixture must exceed the budget"
        );
        let sel = select(
            &format!("{src}{lock}"),
            &SelectOptions::new(&no_ignore(), 1),
        );
        assert_eq!(sel.oversized_dropped(), vec!["Cargo.lock".to_string()]);
        assert_eq!(sel.units.len(), 1);
        assert_eq!(sel.units[0].files, vec!["src/main.rs"]);
        assert!(!sel.text.contains("Cargo.lock"));
        assert_eq!(sel.excluded_by(ExcludeReason::Oversized).len(), 1);
    }

    #[test]
    fn first_file_survives_an_impossible_budget() {
        let sel = select(
            &format!("{SRC}{LOCK}"),
            &SelectOptions::new(&no_ignore(), 0),
        );
        assert!(!sel.units.is_empty(), "floor guarantee: first file is kept");
        assert!(sel.text.contains("src/main.rs"));
    }

    #[test]
    fn duplicate_paths_yield_one_unit() {
        let sel = select(
            &format!("{SRC}{SRC}"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        assert_eq!(sel.units.len(), 1);
    }

    #[test]
    fn unit_fingerprint_tracks_content_not_identity() {
        let a = select(SRC, &SelectOptions::unbounded(&no_ignore()));
        let b = select(
            &SRC.replace("+new", "+newer"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        assert_eq!(a.units[0].unit_id, b.units[0].unit_id);
        assert_ne!(a.units[0].unit_fp, b.units[0].unit_fp);
    }

    #[test]
    fn unit_id_is_content_independent() {
        let a = select(SRC, &SelectOptions::unbounded(&no_ignore()));
        let b = select(
            &SRC.replace("index 1111111..2222222", "index aaaaaaa..bbbbbbb"),
            &SelectOptions::unbounded(&no_ignore()),
        );
        assert_eq!(a.units[0].unit_id, b.units[0].unit_id);
    }

    fn unit(id: &str) -> ReviewUnit {
        ReviewUnit {
            unit_id: id.to_string(),
            source: UnitSource::Changeset,
            files: vec![id.to_string()],
            unit_fp: "fp".to_string(),
        }
    }

    #[test]
    fn ledger_freezes_the_denominator() {
        let mut ledger = CoverageLedger::freeze(&[unit("a"), unit("b"), unit("a")]);
        assert_eq!(ledger.denominator(), ["a", "b"], "duplicates collapse");
        // A unit that was never frozen cannot enter the ledger afterwards.
        ledger.cover("c");
        assert_eq!(ledger.state("c"), None);
        assert_eq!(ledger.covered_count(), 0);
    }

    #[test]
    fn ledger_terminal_state_comes_from_coverage() {
        let mut empty = CoverageLedger::freeze(&[]);
        empty.cover_all();
        assert_eq!(empty.terminal(), TerminalState::Empty);

        let units = [unit("a"), unit("b")];
        let mut ok = CoverageLedger::freeze(&units);
        ok.start_all();
        ok.cover_all();
        assert_eq!(ok.terminal(), TerminalState::Ok);

        let mut partial = CoverageLedger::freeze(&units);
        partial.start_all();
        partial.cover("a");
        partial.fail("b", "provider 500");
        assert_eq!(partial.terminal(), TerminalState::Partial);
        let uncovered = partial.uncovered();
        assert_eq!(uncovered.len(), 1);
        assert_eq!(uncovered[0].0, "b");
        assert_eq!(
            uncovered[0].1,
            &UnitState::Failed("provider 500".to_string())
        );
    }

    #[test]
    fn ledger_truncation_is_a_distinct_uncovered_state() {
        let mut ledger = CoverageLedger::freeze(&[unit("a")]);
        ledger.start_all();
        ledger.truncate_all("diff over budget");
        assert_eq!(ledger.terminal(), TerminalState::Partial);
        assert_eq!(
            ledger.state("a"),
            Some(&UnitState::Truncated("diff over budget".to_string()))
        );
    }

    #[test]
    fn summary_counts_states_and_terminal() {
        let units = [unit("a"), unit("b"), unit("c")];
        let mut ledger = CoverageLedger::freeze(&units);
        ledger.start_all();
        ledger.cover("a");
        ledger.fail("b", "boom");
        ledger.truncate("c", "budget");
        let summary = ledger.summary();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.covered, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.truncated, 1);
        assert_eq!(summary.terminal, TerminalState::Partial);

        let empty = CoverageLedger::freeze(&[]).summary();
        assert_eq!(empty.terminal, TerminalState::Empty);
    }

    #[test]
    fn built_in_catalogs_compile_and_cover_root_and_nested_forms() {
        for path in [
            ".env",
            "sub/.env",
            "deep/nested/.env.local",
            "id_rsa",
            "keys/id_rsa.pub",
            "svc/service-account-prod.json",
        ] {
            assert!(
                secret_globset().is_match(path),
                "{path} must be treated as a secret path"
            );
        }
        for path in [
            "node_modules/a/b.js",
            "web/dist/x.js",
            "Cargo.lock",
            "app.min.js",
            "api.pb.go",
        ] {
            assert!(
                default_path_globset().is_match(path),
                "{path} must be a built-in default exclusion"
            );
        }
    }

    #[test]
    fn strict_gates_are_off_by_default() {
        let sel = select(
            &format!("{ENV_FILE}{VENDORED}{UNKNOWN_EXT}"),
            &SelectOptions::new(&no_ignore(), 400),
        );
        assert_eq!(sel.units.len(), 3, "default behaviour must not shrink");
        assert!(sel.excluded_by(ExcludeReason::SecretPath).is_empty());
        assert!(sel.excluded_by(ExcludeReason::DefaultPath).is_empty());
        assert!(sel.excluded_by(ExcludeReason::Extension).is_empty());
    }

    #[test]
    fn strict_gates_exclude_and_name_the_reason() {
        let sel = select(
            &format!("{ENV_FILE}{VENDORED}{UNKNOWN_EXT}{SRC}"),
            &SelectOptions::new(&no_ignore(), 400).strict(true),
        );
        assert_eq!(sel.units.len(), 1);
        assert_eq!(sel.units[0].files, vec!["src/main.rs"]);
        assert_eq!(sel.excluded_by(ExcludeReason::SecretPath).len(), 1);
        assert_eq!(sel.excluded_by(ExcludeReason::DefaultPath).len(), 1);
        assert_eq!(sel.excluded_by(ExcludeReason::Extension).len(), 1);
        // Nothing is dropped silently: every exclusion is in the one list.
        assert_eq!(sel.excluded_count(), 3);
    }

    #[test]
    fn strict_order_decides_the_reason_for_a_multi_hit_file() {
        // node_modules/service-account.json hits secret-path and default-path;
        // the fixed order attributes it to secret-path (spec 14 §2).
        let input = "\
diff --git a/node_modules/service-account.json b/node_modules/service-account.json
--- a/node_modules/service-account.json
+++ b/node_modules/service-account.json
@@ -1,0 +1,1 @@
+{}
";
        let sel = select(input, &SelectOptions::new(&no_ignore(), 400).strict(true));
        assert_eq!(sel.excluded_by(ExcludeReason::SecretPath).len(), 1);
        assert!(sel.excluded_by(ExcludeReason::DefaultPath).is_empty());
    }

    #[test]
    fn extension_gate_keeps_extensionless_build_files() {
        assert!(diff::is_reviewable_path("Dockerfile"));
        assert!(diff::is_reviewable_path("src/main.rs"));
        assert!(
            diff::is_reviewable_path("deploy/Dockerfile.prod"),
            "build-definition variants carry a suffix"
        );
        // Kinds that are reviewable but absent from the spec 03 priority table.
        // `lustre/go.mod` is a real case: it was the single file the strict gate
        // mis-classified while running against a real pull request.
        assert!(diff::is_reviewable_path("lustre/go.mod"));
        assert!(diff::is_reviewable_path("api/service.proto"));
        assert!(diff::is_reviewable_path("infra/main.tf"));
        assert!(!diff::is_reviewable_path("assets/logo.psd"));

        let input = "\
diff --git a/Dockerfile b/Dockerfile
--- a/Dockerfile
+++ b/Dockerfile
@@ -1,0 +1,1 @@
+FROM scratch
";
        let sel = select(input, &SelectOptions::new(&no_ignore(), 400).strict(true));
        assert_eq!(
            sel.units.len(),
            1,
            "Dockerfile stays reviewable under strict"
        );
    }

    fn section(path: &str, body: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,0 +1,1 @@\n+{body}\n"
        )
    }

    #[test]
    fn grouping_is_off_by_default() {
        let input = format!(
            "{}{}",
            section("README.md", "en"),
            section("README.zh-CN.md", "zh")
        );
        let sel = select(&input, &SelectOptions::new(&no_ignore(), 4000));
        assert_eq!(sel.units.len(), 2);
    }

    #[test]
    fn grouping_merges_locale_variants_with_a_stable_id() {
        let input = format!(
            "{}{}{}",
            section("README.md", "en"),
            section("README.zh-CN.md", "zh"),
            section("README.ru.md", "ru")
        );
        let sel = select(
            &input,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(sel.units.len(), 1);
        assert_eq!(
            sel.units[0].files,
            vec!["README.md", "README.ru.md", "README.zh-CN.md"],
            "members are sorted by path"
        );
        assert_eq!(sel.units[0].unit_id, "changeset:group:README.md");

        // Adding another variant must not change the identity of the group.
        let more = format!("{input}{}", section("README.ja.md", "ja"));
        let sel_more = select(
            &more,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(sel_more.units.len(), 1);
        assert_eq!(sel_more.units[0].unit_id, sel.units[0].unit_id);
    }

    #[test]
    fn grouping_merges_migration_direction_pairs_and_test_siblings() {
        let migrations = format!(
            "{}{}",
            section("db/001_init.up.sql", "create table"),
            section("db/001_init.down.sql", "drop table")
        );
        let sel = select(
            &migrations,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(sel.units.len(), 1);
        assert_eq!(
            sel.units[0].files,
            vec!["db/001_init.down.sql", "db/001_init.up.sql"]
        );
        assert_eq!(sel.units[0].unit_id, "changeset:group:db/001_init.sql");

        let source = format!(
            "{}{}",
            section("src/store.rs", "impl"),
            section("src/store_test.rs", "test")
        );
        let sel = select(
            &source,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(sel.units.len(), 1);
        assert_eq!(
            sel.units[0].files,
            vec!["src/store.rs", "src/store_test.rs"]
        );
        assert_eq!(sel.units[0].unit_id, "changeset:group:src/store.rs");
    }

    #[test]
    fn grouping_leaves_unrelated_files_alone() {
        let input = format!(
            "{}{}{}",
            section("src/a.rs", "a"),
            section("src/b.rs", "b"),
            section("report-final.md", "x")
        );
        let sel = select(
            &input,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(sel.units.len(), 3, "different names must not group");
        assert!(sel.units.iter().all(|u| u.files.len() == 1));
    }

    #[test]
    fn grouping_changes_the_coverage_denominator_only() {
        let input = format!(
            "{}{}",
            section("README.md", "en"),
            section("README.zh-CN.md", "zh")
        );
        let plain = select(&input, &SelectOptions::new(&no_ignore(), 4000));
        let grouped = select(
            &input,
            &SelectOptions::new(&no_ignore(), 4000).group_units(true),
        );
        assert_eq!(plain.units.len(), 2);
        assert_eq!(grouped.units.len(), 1);
        // The dispatched text is identical either way: grouping is accounting,
        // not dispatch, in v1 (spec 14 §1).
        assert_eq!(plain.text, grouped.text);
        assert_eq!(
            grouped.units[0].unit_fp.len(),
            16,
            "group fingerprint is well formed"
        );
    }

    #[test]
    fn wrapper_parity_with_the_legacy_helpers() {
        // The selection must stay a superset-free replacement for
        // filter_text + truncate_text: same text, same excluded/were-dropped sets.
        // 60 padded lines per file keeps the fixture comfortably above the 1 KB
        // budget, so the truncation path is genuinely exercised on both sides.
        let input = format!(
            "{}{}{GENERATED}{DELETED}",
            padded("src/main.rs", 60),
            padded("Cargo.lock", 60)
        );
        let ignore = ignore_set(&["Cargo.lock"]);
        let (legacy_text, legacy_excluded) = diff::filter_text(&input, &ignore);
        let legacy_trunc = diff::truncate_text(&legacy_text, 1);

        let sel = select(&input, &SelectOptions::new(&ignore, 1));
        assert!(
            !legacy_trunc.truncated_files.is_empty(),
            "parity must be exercised with a real truncation"
        );
        assert_eq!(sel.text, legacy_trunc.text);
        // `Selection.excluded` is the union of the two legacy answers: the path
        // gates (ignore glob, generated heuristic) and the size budget.
        assert_eq!(sel.path_gate_excluded_count(), legacy_excluded);
        assert_eq!(
            sel.excluded_count(),
            legacy_excluded + legacy_trunc.truncated_files.len()
        );
        assert_eq!(sel.oversized_dropped(), legacy_trunc.truncated_files);
    }
}
