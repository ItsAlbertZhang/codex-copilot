//! `override` / `unoverride`: merge the installed profile into the shared
//! `config.toml`, keeping one whole-file backup of what was there before.
//!
//! The backup is the entire previous file, not a per-key diff: restoring is
//! then a plain overwrite that cannot half-apply, and nothing has to be
//! un-merged in reverse.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Key, Table, TableLike, Value};

use crate::catalog;
use crate::commands::{read_state, Ctx};

/// Lives next to `config.toml`, and is also the marker that an override is on.
const BACKUP_FILE: &str = "codex-copilot.override-backup.json";

#[derive(Serialize, Deserialize)]
struct Backup {
    version: u32,
    profile: String,
    /// `null` means there was no `config.toml` before the override. Required:
    /// a backup that merely omitted it must not read as "there was no file",
    /// which would delete the user's config on restore.
    #[serde(deserialize_with = "explicit_option")]
    original: Option<String>,
    /// Exactly what the override wrote, so later edits can be reported.
    applied: String,
}

/// serde treats a missing `Option` field as `None`; `deserialize_with` turns
/// that off, leaving `"original": null` as the only way to say "did not exist".
fn explicit_option<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

fn backup_path(ctx: &Ctx) -> PathBuf {
    ctx.codex_home.join(BACKUP_FILE)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// `Ok(None)`: no override. `Err`: the file is there but says nothing usable,
/// which every caller has to treat as "an override may be active".
fn read_backup(ctx: &Ctx) -> Result<Option<Backup>> {
    let path = backup_path(ctx);
    let Some(text) = read_optional(&path)? else {
        return Ok(None);
    };
    let unusable = || {
        format!(
            "could not read the override backup {}; delete it to abandon the override, or restore \
             config.toml by hand",
            path.display()
        )
    };
    let backup: Backup = serde_json::from_str(&text).with_context(unusable)?;
    if backup.version != 1 || backup.profile.is_empty() {
        Err(anyhow::anyhow!("unsupported backup version")).with_context(unusable)?;
    }
    Ok(Some(backup))
}

/// Blocks `install` / `uninstall` while this profile owns config.toml. An
/// unreadable backup is an unknown owner, so it blocks too.
pub fn ensure_not_owner(ctx: &Ctx) -> Result<()> {
    if let Some(backup) = read_backup(ctx)? {
        if backup.profile == ctx.profile {
            bail!(
                "profile `{}` is overridden; run unoverride first",
                ctx.profile
            );
        }
    }
    Ok(())
}

/// One line for `status`, plus one more when an unoverride left edits behind.
/// Never fails: a broken backup is itself the news.
pub fn print_status(ctx: &Ctx) {
    let path = backup_path(ctx);
    match read_backup(ctx) {
        Ok(None) => println!("override     inactive"),
        Ok(Some(backup)) if backup.profile == ctx.profile => println!(
            "override     active (profile `{}`, backup {})",
            backup.profile,
            path.display()
        ),
        Ok(Some(backup)) => println!(
            "override     active for profile `{}` (not this profile), backup {}",
            backup.profile,
            path.display()
        ),
        Err(error) => println!(
            "override     backup unreadable ({}): {error:#}",
            path.display()
        ),
    }
    let discarded = ctx.discarded_path();
    if discarded.exists() {
        println!(
            "discarded    {} (merge into config.toml, then delete)",
            discarded.display()
        );
    }
}

// ---------------------------------------------------------------------------
// override
// ---------------------------------------------------------------------------

pub fn apply(ctx: &Ctx) -> Result<()> {
    let backup_path = backup_path(ctx);
    if backup_path.exists() {
        bail!(
            "an override backup already exists at {}; run unoverride to restore config.toml, or \
             delete that file to abandon the override",
            backup_path.display()
        );
    }
    // Edits an earlier unoverride set aside would be merged over unseen.
    let discarded = ctx.discarded_path();
    if discarded.exists() {
        bail!(
            "{} exists from an earlier unoverride; merge it into config.toml by hand, then delete \
             it",
            discarded.display()
        );
    }

    let overlay = installed_overlay(ctx)?;

    let target = ctx.config_path();
    let original = read_optional(&target)?;
    let mut config: DocumentMut = original
        .as_deref()
        .unwrap_or("")
        .parse()
        .with_context(|| format!("{} is not valid TOML", target.display()))?;
    let mut merge = Merge::new(&config);
    merge.table(config.as_table_mut(), overlay.as_table());
    let applied = config.to_string();
    let parsed: toml::Value = toml::from_str(&applied).with_context(|| {
        format!(
            "merging the profile into {} would produce invalid TOML",
            target.display()
        )
    })?;
    // The document tree can hold an item that serialization then drops, so the
    // merge is judged by re-reading the text it actually produced.
    let profile: toml::Value = toml::from_str(&overlay.to_string())
        .with_context(|| format!("{} is not valid TOML", ctx.overlay_path().display()))?;
    let missing = unapplied(&parsed, &profile);
    if !missing.is_empty() {
        bail!(
            "merging the profile into {} would drop {}; nothing was written. Please report this, \
             and set those keys by hand.",
            target.display(),
            missing.join(", ")
        );
    }
    ensure_policy_can_merge(&config, &overlay, &target, &ctx.overlay_path())?;

    for line in &merge.lines {
        println!("{line}");
    }
    if ctx.dry_run {
        println!(
            "Dry run: would write {} and {}",
            target.display(),
            backup_path.display()
        );
        return Ok(());
    }

    // The backup goes first: an orphan backup is still restorable, a lost
    // original is not.
    let backup = Backup {
        version: 1,
        profile: ctx.profile.clone(),
        original,
        applied: applied.clone(),
    };
    atomic_create_new(
        &backup_path,
        &(serde_json::to_string_pretty(&backup)? + "\n"),
    )?;
    atomic_write(&target, &applied)
        .context("config.toml was not changed; run unoverride to clear the backup")?;
    println!(
        "Overridden {}. Run unoverride to restore it.",
        target.display()
    );
    Ok(())
}

/// The profile exactly as `install` left it. A half-installed profile is
/// refused here rather than merged into config.toml and met at the next start.
fn installed_overlay(ctx: &Ctx) -> Result<DocumentMut> {
    read_state(ctx).context("run install first")?;

    let overlay_path = ctx.overlay_path();
    let overlay_text = fs::read_to_string(&overlay_path).with_context(|| {
        format!(
            "could not read the installed profile {}; run install first",
            overlay_path.display()
        )
    })?;
    let overlay: DocumentMut = overlay_text
        .parse()
        .with_context(|| format!("{} is not valid TOML", overlay_path.display()))?;

    // A hand-edited profile may point somewhere else; validate what it names.
    let named = overlay
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .with_context(|| {
            format!(
                "{} does not set model_catalog_json; run install first",
                overlay_path.display()
            )
        })?;
    let path = ctx.codex_home.join(named);
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "could not read the model catalog {}; run install first",
            path.display()
        )
    })?;
    catalog::validate(&text).with_context(|| {
        format!(
            "{} is not a Codex models.json; run install first",
            path.display()
        )
    })?;
    Ok(overlay)
}

/// Codex rejects the whole config when `filters` sits next to `exclude` or
/// `include_only`; the two legacy keys together are fine. Neither spelling can
/// be translated into the other without deciding something for the user, so
/// this is judged on the merged document and refused with nothing written.
fn ensure_policy_can_merge(
    merged: &DocumentMut,
    overlay: &DocumentMut,
    target: &Path,
    overlay_path: &Path,
) -> Result<()> {
    const POLICY: &str = "shell_environment_policy";
    let Some(policy) = merged.get(POLICY).and_then(Item::as_table_like) else {
        return Ok(());
    };
    if !policy.contains_key("filters") {
        return Ok(());
    }
    let legacy: Vec<String> = ["exclude", "include_only"]
        .into_iter()
        .filter(|key| policy.contains_key(key))
        .map(|key| format!("{POLICY}.{key}"))
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }
    // Whichever file contributes `filters` holds the decision to be made.
    let owner = match overlay.get(POLICY).and_then(Item::as_table_like) {
        Some(policy) if policy.contains_key("filters") => overlay_path,
        _ => target,
    };
    bail!(
        "merging the profile into {target} would set {POLICY}.filters next to {legacy}; codex \
         refuses that combination (`cannot mix filters with legacy exclude or include_only`). \
         Reconcile {POLICY} in {owner} by hand, then run override again.",
        target = target.display(),
        owner = owner.display(),
        legacy = legacy.join(" and "),
    );
}

// ---------------------------------------------------------------------------
// unoverride
// ---------------------------------------------------------------------------

pub fn restore(ctx: &Ctx) -> Result<()> {
    let Some(backup) = read_backup(ctx)? else {
        println!("No override backup; nothing to restore.");
        return Ok(());
    };
    if backup.profile != ctx.profile {
        bail!(
            "override belongs to profile `{}`; run --profile {} unoverride",
            backup.profile,
            backup.profile
        );
    }

    let target = ctx.config_path();
    let current = read_optional(&target)?;
    // An unoverride that was interrupted after writing the original back left
    // the file already restored: finish the job instead of reading the
    // original as an edit and discarding it over the edits actually saved.
    let restored = current == backup.original;
    if !restored && current.as_deref() != Some(backup.applied.as_str()) {
        match &current {
            // Those edits are someone's work: set the file aside, do not judge
            // which half of it was the profile's.
            Some(text) => {
                let discarded = ctx.discarded_path();
                match read_optional(&discarded)? {
                    // Overwriting it would lose the earlier run's edits.
                    Some(saved) if saved != *text => bail!(
                        "{} already exists from an earlier unoverride; merge it into config.toml \
                         by hand and delete it, then run unoverride again",
                        discarded.display()
                    ),
                    Some(_) => {}
                    None if !ctx.dry_run => atomic_write(&discarded, text)?,
                    None => {}
                }
                println!(
                    "WARNING  config.toml changed since override; the edited file {} {}; merge \
                     it back by hand",
                    if ctx.dry_run {
                        "would be saved to"
                    } else {
                        "was saved to"
                    },
                    discarded.display()
                );
            }
            None => println!(
                "WARNING  config.toml is gone since the override; there is nothing to save, \
                 restoring the backup"
            ),
        }
    }
    if ctx.dry_run {
        println!(
            "Dry run: would restore {} and remove {}",
            target.display(),
            backup_path(ctx).display()
        );
        return Ok(());
    }

    match &backup.original {
        Some(text) => atomic_write(&target, text)?,
        None => match fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("could not remove {}", target.display()))
            }
        },
    }
    // Only a completed restore clears the marker.
    fs::remove_file(backup_path(ctx))
        .with_context(|| format!("could not remove {}", backup_path(ctx).display()))?;
    println!("Restored {}.", target.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

/// The running state of one merge: where in the profile we are, what changed,
/// and where the next new table goes.
struct Merge {
    path: Vec<String>,
    lines: Vec<String>,
    next: isize,
    /// True while `base` is an inline table, where a `Table` item has no
    /// representation and would vanish when the document is printed.
    inline: bool,
}

impl Merge {
    fn new(config: &DocumentMut) -> Self {
        Self {
            path: Vec::new(),
            lines: Vec::new(),
            next: max_position(config.as_table()) + 1,
            inline: false,
        }
    }

    /// Merges `overlay` into `base` top-down. Tables recurse, everything else
    /// replaces, with two exceptions that would otherwise cost the user
    /// something they cannot get back: the exclude list, and provider tables.
    fn table(&mut self, base: &mut dyn TableLike, overlay: &dyn TableLike) {
        for (key, incoming) in overlay.iter() {
            self.path.push(key.to_owned());
            let second = self.path.len() == 2;
            if second && self.path[0] == "model_providers" {
                self.provider(base, key, incoming);
            } else if second && self.path[0] == "shell_environment_policy" && key == "exclude" {
                self.union(base, key, incoming);
            } else if incoming.as_table_like().is_some()
                && base.get(key).and_then(Item::as_table_like).is_some()
            {
                let source = incoming.as_table_like().unwrap();
                // A table-like `Item::Value` can only be an inline table.
                let nested = self.inline || matches!(base.get(key), Some(Item::Value(_)));
                let into = base.get_mut(key).unwrap().as_table_like_mut().unwrap();
                let outer = std::mem::replace(&mut self.inline, nested);
                self.table(into, source);
                self.inline = outer;
            } else if !base
                .get(key)
                .is_some_and(|existing| same(existing, incoming))
            {
                self.note(base, key);
                self.set(base, key, plain(incoming));
            }
            self.path.pop();
        }
    }

    /// `ignore_default_excludes` defaults to true, so this list is the only
    /// filter Codex applies: replacing it wholesale would expose every
    /// `SECRET_*` pattern the user excluded to every subprocess. Union instead.
    fn union(&mut self, base: &mut dyn TableLike, key: &str, incoming: &Item) {
        let existing = base.get(key).and_then(Item::as_array).cloned();
        let (Some(mut merged), Some(extra)) = (existing, incoming.as_array()) else {
            self.note(base, key);
            self.set(base, key, plain(incoming));
            return;
        };
        let mut added = 0;
        for value in extra.iter() {
            if !merged.iter().any(|kept| same_value(kept, value)) {
                merged.push(plain_value(value));
                added += 1;
            }
        }
        if added > 0 {
            // `get_mut`, not `insert`: the user's own key decor stays put.
            *base.get_mut(key).unwrap() = Item::Value(Value::Array(merged));
            let at = self.at();
            self.lines.push(format!("extend {at} (+{added})"));
        }
    }

    /// The provider table is replaced whole, never deep-merged: a leftover
    /// `experimental_bearer_token` or `http_headers.authorization` of the
    /// user's must not survive next to the profile's `env_key`.
    fn provider(&mut self, base: &mut dyn TableLike, key: &str, incoming: &Item) {
        let dropped = base
            .get(key)
            .and_then(Item::as_table_like)
            .zip(incoming.as_table_like())
            .map(|(user, profile)| dropped_keys(user, profile, ""))
            .unwrap_or_default();
        self.note(base, key);
        self.set(base, key, plain(incoming));
        if !dropped.is_empty() {
            let at = self.at();
            self.lines.push(format!(
                "WARNING  {at} replaced whole; dropped your keys: {}",
                dropped.join(", ")
            ));
        }
    }

    /// One `add`/`replace` line, recorded before the key is written.
    fn note(&mut self, base: &dyn TableLike, key: &str) {
        let verb = if base.get(key).is_some() {
            "replace"
        } else {
            "add"
        };
        let at = self.at();
        self.lines.push(format!("{verb} {at}"));
    }

    fn set(&mut self, base: &mut dyn TableLike, key: &str, item: Item) {
        // An inline table holds values only; a `Table` stored in one is dropped
        // when the document is printed rather than rejected.
        let mut item = if self.inline { as_value(item) } else { item };
        // A table that replaces one already in the file stays where it was;
        // its own sub-tables have no position and so follow it.
        let previous = base
            .get(key)
            .and_then(Item::as_table)
            .and_then(Table::position);
        match (previous, &mut item) {
            (Some(position), Item::Table(table)) => table.set_position(Some(position)),
            _ => place(&mut item, &mut self.next),
        }
        match base.get_mut(key) {
            Some(slot) => *slot = item,
            None => {
                base.insert(key, item);
            }
        }
    }

    /// The key path being merged, quoted the way TOML needs it.
    fn at(&self) -> String {
        dotted(&self.path)
    }
}

/// A `Table` becomes an inline table, an array of tables an array of those.
fn as_value(item: Item) -> Item {
    match item.into_value() {
        Ok(value) => Item::Value(value),
        Err(unchanged) => unchanged,
    }
}

fn dotted(path: &[String]) -> String {
    path.iter()
        .map(|key| Key::new(key).to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// Leaf keys of `user` that `profile` does not define, dotted from `prefix`.
fn dropped_keys(user: &dyn TableLike, profile: &dyn TableLike, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (key, item) in user.iter() {
        let name = format!("{prefix}{key}");
        match (item.as_table_like(), profile.get(key)) {
            (Some(nested), Some(other)) => match other.as_table_like() {
                Some(other) => out.extend(dropped_keys(nested, other, &format!("{name}."))),
                None => out.push(name),
            },
            (_, None) => out.push(name),
            _ => {}
        }
    }
    out
}

/// Leaf keys of the profile that the merged *text* does not carry. The safety
/// net for every way an item can be written into the tree and lost again.
fn unapplied(merged: &toml::Value, profile: &toml::Value) -> Vec<String> {
    let mut missing = Vec::new();
    compare(Some(merged), profile, &mut Vec::new(), &mut missing);
    missing
}

fn compare(
    merged: Option<&toml::Value>,
    profile: &toml::Value,
    path: &mut Vec<String>,
    missing: &mut Vec<String>,
) {
    let Some(merged) = merged else {
        missing.push(dotted(path));
        return;
    };
    match profile {
        toml::Value::Table(table) => {
            let Some(into) = merged.as_table() else {
                missing.push(dotted(path));
                return;
            };
            for (key, child) in table {
                path.push(key.clone());
                compare(into.get(key), child, path, missing);
                path.pop();
            }
        }
        // The one key the merge unions instead of replacing.
        _ if is_exclude(path) => {
            let kept = profile
                .as_array()
                .zip(merged.as_array())
                .is_some_and(|(want, have)| want.iter().all(|entry| have.contains(entry)));
            if !kept {
                missing.push(dotted(path));
            }
        }
        _ => {
            if merged != profile {
                missing.push(dotted(path));
            }
        }
    }
}

fn is_exclude(path: &[String]) -> bool {
    path.len() == 2 && path[0] == "shell_environment_policy" && path[1] == "exclude"
}

/// A table cloned from the profile has no position, so it would sort to the
/// top of the document - between `[[array]]` elements, even. Append instead.
fn place(item: &mut Item, next: &mut isize) {
    if let Item::Table(table) = item {
        if !table.is_implicit() {
            table.set_position(Some(*next));
            *next += 1;
        }
        for (_, child) in table.iter_mut() {
            place(child, next);
        }
    }
}

fn max_position(table: &Table) -> isize {
    let mut max = table.position().unwrap_or(0);
    for (_, item) in table.iter() {
        match item {
            Item::Table(child) => max = max.max(max_position(child)),
            Item::ArrayOfTables(array) => {
                for child in array.iter() {
                    max = max.max(max_position(child));
                }
            }
            _ => {}
        }
    }
    max
}

/// The same item, rebuilt from plain values: without this the profile's own
/// comments (its five-line header, above all) travel into `config.toml`.
fn plain(item: &Item) -> Item {
    match item {
        Item::Value(value) => Item::Value(plain_value(value)),
        Item::Table(table) => {
            let mut out = Table::new();
            out.set_implicit(table.is_implicit());
            for (key, child) in table.iter() {
                out.insert(key, plain(child));
            }
            Item::Table(out)
        }
        // The profile never contains an array of tables.
        other => other.clone(),
    }
}

fn plain_value(value: &Value) -> Value {
    match value {
        Value::Array(array) => {
            let mut out = Array::new();
            for entry in array.iter() {
                out.push(plain_value(entry));
            }
            Value::Array(out)
        }
        Value::InlineTable(table) => {
            let mut out = InlineTable::new();
            for (key, entry) in table.iter() {
                out.insert(key, plain_value(entry));
            }
            Value::InlineTable(out)
        }
        scalar => {
            let mut out = scalar.clone();
            out.decor_mut().clear();
            out
        }
    }
}

fn same(left: &Item, right: &Item) -> bool {
    match (left.as_value(), right.as_value()) {
        (Some(left), Some(right)) => same_value(left, right),
        _ => false,
    }
}

/// Equality of what the values *mean*: `'AWS_*'` and `"AWS_*"` are one entry,
/// however they were spelled and whatever decor surrounds them.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(left), Value::String(right)) => left.value() == right.value(),
        (Value::Integer(left), Value::Integer(right)) => left.value() == right.value(),
        (Value::Float(left), Value::Float(right)) => left.value() == right.value(),
        (Value::Boolean(left), Value::Boolean(right)) => left.value() == right.value(),
        (Value::Datetime(left), Value::Datetime(right)) => left.value() == right.value(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| same_value(left, right))
        }
        (Value::InlineTable(left), Value::InlineTable(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right.get(key).is_some_and(|other| same_value(value, other))
                })
        }
        _ => false,
    }
}

/// NamedTempFile is created 0600 on Unix and keeps those bits after persist.
/// Intentional: this file may hold values copied out of the user's config.
fn atomic_write(path: &Path, text: &str) -> Result<()> {
    persist(path, text, false).with_context(|| format!("could not write {}", path.display()))
}

/// Refuses to overwrite: a second `override` racing the first must fail rather
/// than replace the backup that holds the real original.
fn atomic_create_new(path: &Path, text: &str) -> Result<()> {
    persist(path, text, true).with_context(|| {
        format!(
            "could not create {}; another override may have just written it",
            path.display()
        )
    })
}

fn persist(path: &Path, text: &str, new_only: bool) -> std::io::Result<()> {
    let mut temporary = NamedTempFile::new_in(
        path.parent()
            .unwrap_or_else(|| Path::new(std::path::Component::CurDir.as_os_str())),
    )?;
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    if new_only {
        temporary.persist_noclobber(path).map_err(|e| e.error)?;
    } else {
        temporary.persist(path).map_err(|e| e.error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &str, overlay: &str) -> (String, Vec<String>) {
        let mut config: DocumentMut = base.parse().unwrap();
        let source: DocumentMut = overlay.parse().unwrap();
        let mut merge = Merge::new(&config);
        merge.table(config.as_table_mut(), source.as_table());
        let text = config.to_string();
        let parsed: toml::Value = toml::from_str(&text).expect("the merge must stay valid TOML");
        let profile: toml::Value = toml::from_str(overlay).unwrap();
        assert_eq!(
            unapplied(&parsed, &profile),
            Vec::<String>::new(),
            "the merged text must carry every profile key"
        );
        (text, merge.lines)
    }

    fn value(text: &str, path: &[&str]) -> toml::Value {
        let mut item: toml::Value = toml::from_str(text).unwrap();
        for key in path {
            item = item
                .get(key)
                .unwrap_or_else(|| panic!("{key} missing"))
                .clone();
        }
        item
    }

    fn parse_value(text: &str) -> Value {
        let document: DocumentMut = format!("x = {text}").parse().unwrap();
        document["x"].as_value().unwrap().clone()
    }

    #[test]
    fn the_environment_exclude_list_is_unioned_not_replaced() {
        let (text, lines) = merged(
            "[shell_environment_policy]\nexclude = [\"SECRET_*\", \"AWS_*\"]\n",
            "[shell_environment_policy]\nexclude = [\"AWS_*\", \"COPILOT_GITHUB_TOKEN\"]\n",
        );
        assert_eq!(
            value(&text, &["shell_environment_policy", "exclude"]),
            toml::Value::try_from(["SECRET_*", "AWS_*", "COPILOT_GITHUB_TOKEN"]).unwrap()
        );
        assert_eq!(lines, ["extend shell_environment_policy.exclude (+1)"]);
    }

    #[test]
    fn a_missing_exclude_list_is_simply_added() {
        let (text, lines) = merged(
            "",
            "[shell_environment_policy]\nexclude = [\"COPILOT_GITHUB_TOKEN\"]\n",
        );
        assert_eq!(
            value(&text, &["shell_environment_policy", "exclude"]),
            toml::Value::try_from(["COPILOT_GITHUB_TOKEN"]).unwrap()
        );
        assert_eq!(lines, ["add shell_environment_policy"]);
    }

    #[test]
    fn a_provider_table_is_replaced_whole_and_names_the_dropped_keys() {
        let (text, lines) = merged(
            "[model_providers.copilot]\nname = \"old\"\nexperimental_bearer_token = \"ghu_x\"\n\
             [model_providers.copilot.http_headers]\nauthorization = \"Bearer x\"\n\
             [model_providers.other]\nname = \"mine\"\n",
            "[model_providers.copilot]\nname = \"OpenAI\"\nenv_key = \"COPILOT_GITHUB_TOKEN\"\n\
             [model_providers.copilot.http_headers]\n\"x-github-api-version\" = \"2026-08-01\"\n",
        );
        let provider = value(&text, &["model_providers", "copilot"]);
        assert_eq!(provider["name"].as_str(), Some("OpenAI"));
        assert!(provider.get("experimental_bearer_token").is_none());
        assert!(provider["http_headers"].get("authorization").is_none());
        // Providers the profile does not mention are left alone.
        assert_eq!(
            value(&text, &["model_providers", "other", "name"]).as_str(),
            Some("mine")
        );
        assert_eq!(
            lines,
            [
                "replace model_providers.copilot",
                "WARNING  model_providers.copilot replaced whole; dropped your keys: \
                 experimental_bearer_token, http_headers.authorization"
            ]
        );
    }

    #[test]
    fn other_tables_merge_key_by_key() {
        let (text, lines) = merged(
            "[features]\nmine = true\napps = true\n",
            "[features]\napps = false\nplugins = false\n",
        );
        assert_eq!(value(&text, &["features", "mine"]).as_bool(), Some(true));
        assert_eq!(value(&text, &["features", "apps"]).as_bool(), Some(false));
        assert_eq!(
            value(&text, &["features", "plugins"]).as_bool(),
            Some(false)
        );
        assert_eq!(lines, ["replace features.apps", "add features.plugins"]);
    }

    #[test]
    fn scalars_replace_and_identical_values_are_not_reported() {
        let (text, lines) = merged(
            "# keep me\nmodel = \"gpt-6-astra\" # and me\nmodel_reasoning_effort = \"high\"\n",
            "# profile header\nmodel = \"gpt-6-astra\"\nmodel_reasoning_effort = \"ultra\"\n\
             model_provider = \"copilot\"\n",
        );
        assert_eq!(
            value(&text, &["model_reasoning_effort"]).as_str(),
            Some("ultra")
        );
        assert_eq!(
            lines,
            ["replace model_reasoning_effort", "add model_provider"]
        );
        assert!(text.contains("# keep me"));
        assert!(text.contains("# and me"));
        assert!(!text.contains("# profile header"));
    }

    #[test]
    fn values_are_compared_by_meaning_not_by_spelling() {
        for (left, right) in [
            ("'AWS_*'", "\"AWS_*\""),
            ("\"\"\"AWS_*\"\"\"", "'AWS_*'"),
            ("[ 'a', 'b' ]", "[\"a\",\"b\"]"),
            ("{ a = 'x', b = 1 }", "{ b = 1, a = \"x\" }"),
            ("1", "1"),
            ("true", "true"),
        ] {
            assert!(
                same_value(&parse_value(left), &parse_value(right)),
                "{left} should equal {right}"
            );
        }
        for (left, right) in [
            ("'AWS_*'", "'AWS_'"),
            ("['a']", "['a', 'b']"),
            ("{ a = 1 }", "{ a = 2 }"),
            ("1", "'1'"),
            ("true", "false"),
        ] {
            assert!(
                !same_value(&parse_value(left), &parse_value(right)),
                "{left} should differ from {right}"
            );
        }
    }

    #[test]
    fn quoting_style_does_not_duplicate_an_exclude_entry() {
        let (text, lines) = merged(
            "[shell_environment_policy]\nexclude = ['AWS_*']\n",
            "[shell_environment_policy]\nexclude = [\"AWS_*\"]\n",
        );
        assert_eq!(
            value(&text, &["shell_environment_policy", "exclude"]),
            toml::Value::try_from(["AWS_*"]).unwrap()
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_table_merged_into_an_inline_table_becomes_an_inline_table() {
        // Left a `Table`, this is printed as dotted keys glued to the line it
        // replaced; an array of tables in the same slot vanishes entirely.
        let (text, lines) = merged(
            "model_providers = { copilot = { name = \"mine\" }, other = { name = \"mine\" } }\n",
            "[model_providers.copilot]\nname = \"OpenAI\"\n\
             [model_providers.copilot.http_headers]\n\"x-github-api-version\" = \"2026-08-01\"\n",
        );
        assert_eq!(
            value(&text, &["model_providers", "copilot", "name"]).as_str(),
            Some("OpenAI")
        );
        assert_eq!(
            value(
                &text,
                &[
                    "model_providers",
                    "copilot",
                    "http_headers",
                    "x-github-api-version"
                ]
            )
            .as_str(),
            Some("2026-08-01")
        );
        assert_eq!(
            value(&text, &["model_providers", "other", "name"]).as_str(),
            Some("mine")
        );
        assert!(!text.contains("[model_providers"), "{text}");
        assert!(text.contains("copilot = {"), "{text}");
        assert_eq!(lines, ["replace model_providers.copilot"]);
    }

    #[test]
    fn an_array_of_tables_merged_into_an_inline_table_is_not_dropped() {
        let (text, lines) = merged("a = { b = 0 }\n", "[[a.b]]\nk = 1\n[[a.b]]\nk = 2\n");
        assert_eq!(
            value(&text, &["a", "b"]),
            toml::from_str::<toml::Value>("b = [{ k = 1 }, { k = 2 }]").unwrap()["b"]
        );
        assert_eq!(lines, ["replace a.b"]);
    }

    #[test]
    fn a_new_table_lands_after_the_last_array_of_tables_element() {
        let (text, _) = merged(
            "[[mcp_servers]]\nname = \"one\"\n[[mcp_servers]]\nname = \"two\"\n",
            "[model_providers.copilot]\nenv_key = \"T\"\n",
        );
        assert!(
            text.ends_with("[model_providers.copilot]\nenv_key = \"T\"\n"),
            "{text}"
        );
    }

    #[test]
    fn a_user_subtree_replaced_by_a_scalar_is_named() {
        let (_, lines) = merged(
            "[model_providers.copilot.http_headers]\nauthorization = \"x\"\n",
            "[model_providers.copilot]\nhttp_headers = \"none\"\n",
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("dropped your keys: http_headers")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_inline_table_keeps_the_keys_the_profile_does_not_mention() {
        let (text, _) = merged(
            "analytics = { enabled = true, extra = \"keep\" }\n",
            "[analytics]\nenabled = false\n",
        );
        assert_eq!(
            value(&text, &["analytics", "enabled"]).as_bool(),
            Some(false)
        );
        assert_eq!(value(&text, &["analytics", "extra"]).as_str(), Some("keep"));
    }

    #[test]
    fn a_profile_key_missing_from_the_merged_text_is_detected() {
        let text: toml::Value = toml::from_str("[features]\napps = true\n").unwrap();
        let profile: toml::Value =
            toml::from_str("[features]\napps = false\nplugins = false\n").unwrap();
        assert_eq!(
            unapplied(&text, &profile),
            ["features.apps", "features.plugins"]
        );

        // The exclude list only has to contain the profile's own entries.
        let text: toml::Value =
            toml::from_str("[shell_environment_policy]\nexclude = ['A', 'B']\n").unwrap();
        let profile: toml::Value =
            toml::from_str("[shell_environment_policy]\nexclude = ['B']\n").unwrap();
        assert_eq!(unapplied(&text, &profile), Vec::<String>::new());
        let profile: toml::Value =
            toml::from_str("[shell_environment_policy]\nexclude = ['C']\n").unwrap();
        assert_eq!(
            unapplied(&text, &profile),
            ["shell_environment_policy.exclude"]
        );
    }
}
