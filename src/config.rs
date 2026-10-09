//! The managed `config.toml` inside the dedicated home.
//!
//! A fresh install gets [`render`]'s text verbatim. Afterwards the file is
//! shared: Codex writes project trust, hook state and `/model` choices back
//! into it, and the user may edit it. So a reinstall merges ([`apply`]) with
//! `toml_edit`, touching only the managed keys and keeping every comment and
//! unknown key. The rendered template is the single source of the managed
//! values: `apply` parses it and copies keys (with their comments) out of it.

use anyhow::{bail, Context, Result};
use toml_edit::{Decor, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::{PROVIDER_ID, TOKEN_ENV};

/// File name of the config inside the home.
pub const CONFIG_FILE: &str = "config.toml";

/// Root keys written only when absent, unless the command line asked for a
/// value ([`requested`]): Codex persists `/model` choices here and the user
/// may have tuned them.
const USER_OWNED_ROOT: &[&str] = &["model", "model_reasoning_effort", "model_context_window"];
/// Root keys overwritten on every install.
const MANAGED_ROOT: &[&str] = &["model_provider", "check_for_update_on_startup"];
/// The yolo / auto-review keys. Whichever the template carries is set; the
/// others are removed while they still hold what the other mode writes. So
/// switching modes never leaves a contradiction, and never takes a value of
/// the user's own with it (a `default_permissions` naming their
/// `[permissions.*]` profile). The one exception: a yolo `default_permissions`
/// that Codex needs to pick one of those profiles becomes [`WORKSPACE`].
const APPROVAL_KEYS: &[&str] = &[
    "approval_policy",
    "default_permissions",
    "approvals_reviewer",
];
/// What `--no-yolo` leaves in `default_permissions` instead of removing the
/// yolo value when Codex needs one ([`needs_default_permissions`]).
const WORKSPACE: &str = ":workspace";
/// The comment above a [`WORKSPACE`] that `--no-yolo` left. Yolo drops it.
const WORKSPACE_NOTE: &str =
    "# Codex refuses to load [permissions] profiles unless one is selected here.\n";
/// 1.x pointed this at a catalog file 2.0 no longer writes; Codex would fail
/// to load it.
const LEGACY_ROOT: &[&str] = &["model_catalog_json"];
/// The `[shell_environment_policy] filters` action that drops a variable.
const EXCLUDE: &str = "exclude";

/// Everything that varies in the managed config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOptions {
    /// `model`. `None` keeps a value already in the file (a `/model` choice)
    /// and writes [`crate::DEFAULT_MODEL`] only where there is none. `Some`
    /// is the user asking for this model, so it is written either way.
    pub model: Option<String>,
    /// `model_reasoning_effort`, kept or written like `model` (default
    /// [`crate::DEFAULT_REASONING_EFFORT`]).
    pub reasoning_effort: Option<String>,
    pub context_window: i64,
    /// `host:port` the relay listens on; becomes `base_url = "http://<listen>"`.
    pub listen: String,
    pub yolo: bool,
}

impl Default for ConfigOptions {
    /// The crate defaults, yolo on, nothing asked for explicitly.
    fn default() -> Self {
        Self {
            model: None,
            reasoning_effort: None,
            context_window: crate::DEFAULT_CONTEXT_WINDOW,
            listen: crate::DEFAULT_LISTEN.into(),
            yolo: true,
        }
    }
}

impl ConfigOptions {
    /// The same options in the other approval mode, whose template says what
    /// this tool writes for the approval keys `self`'s template lacks.
    fn other_mode(&self) -> Self {
        Self {
            yolo: !self.yolo,
            ..self.clone()
        }
    }
}

/// A complete config.toml for a fresh install.
pub fn render(opts: &ConfigOptions) -> String {
    let approvals = if opts.yolo {
        "# Equivalent to codex --yolo. `install --no-yolo` replaces these two keys with\n\
         # approvals_reviewer = \"auto_review\" (the relay maps the reviewer model).\n\
         approval_policy = \"never\"\n\
         default_permissions = \":danger-full-access\"\n"
    } else {
        "# Codex's auto-reviewer answers approval prompts (the relay maps its model).\n\
         approvals_reviewer = \"auto_review\"\n"
    };
    let model = opts.model.as_deref().unwrap_or(crate::DEFAULT_MODEL);
    let effort = opts
        .reasoning_effort
        .as_deref()
        .unwrap_or(crate::DEFAULT_REASONING_EFFORT);
    format!(
        r#"# codex-copilot {version} - managed keys for running Codex against GitHub
# Copilot CAPI through the local relay. `codex-copilot install` rewrites the
# managed keys and leaves everything else in this file alone (Codex writes
# project trust, hook state and model choices back into it).

model = {model}
model_reasoning_effort = {effort}
# Codex clamps this to each model's max_context_window: "use the maximum".
model_context_window = {window}
model_provider = {provider}
check_for_update_on_startup = false
{approvals}
[windows]
sandbox = "unelevated"

[model_providers.{PROVIDER_ID}]
# "OpenAI" is the name Codex keys remote compaction and web search on. The
# relay, not Codex, is what talks to Copilot.
name = "OpenAI"
base_url = {base_url}
wire_api = "responses"
env_key = {token}
supports_websockets = true

[analytics]
enabled = false

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[feedback]
enabled = false

[shell_environment_policy]
exclude = [{token}]
"#,
        version = env!("CARGO_PKG_VERSION"),
        model = quote(model),
        effort = quote(effort),
        window = opts.context_window,
        provider = quote(PROVIDER_ID),
        base_url = quote(&format!("http://{}", opts.listen)),
        token = quote(TOKEN_ENV),
    )
}

/// A TOML string literal, escaped however the value needs.
fn quote(s: &str) -> String {
    Value::from(s).to_string()
}

/// Whether the command line named `key`'s value, which makes a user-owned
/// key managed for this install: `install --model x` beats a kept `/model`
/// choice.
fn requested(opts: &ConfigOptions, key: &str) -> bool {
    match key {
        "model" => opts.model.is_some(),
        "model_reasoning_effort" => opts.reasoning_effort.is_some(),
        _ => false,
    }
}

/// Merge the managed keys into an existing config.toml, preserving everything
/// else (comments, keys Codex wrote back, user additions).
pub fn apply(existing: &str, opts: &ConfigOptions) -> Result<String> {
    if existing.trim().is_empty() {
        return Ok(render(opts));
    }
    let mut doc: DocumentMut = existing.parse().context("not valid TOML")?;
    let managed = template(opts)?;
    let other_mode = template(&opts.other_mode())?;
    let template = managed.as_table();
    let written = other_mode.as_table();
    // New tables go after everything already in the file.
    let mut next = max_position(doc.as_table()) + 1;
    let root = doc.as_table_mut();

    for key in USER_OWNED_ROOT {
        if requested(opts, key) {
            set(root, template, key);
        } else {
            keep(root, template, key);
        }
    }
    for key in MANAGED_ROOT.iter().chain(APPROVAL_KEYS) {
        set(root, template, key);
    }
    // Yolo sets `default_permissions` for its own reasons; the note
    // `--no-yolo` left above it (`select_workspace`) would be stale.
    if template.contains_key("default_permissions") {
        edit_prefix(root, "default_permissions", |above| {
            above
                .strip_suffix(WORKSPACE_NOTE)
                .unwrap_or(above)
                .to_owned()
        });
    }
    section(root, template, "windows", &mut next, |table, tmpl| {
        keep(table, tmpl, "sandbox");
        Ok(())
    })?;
    for name in ["analytics", "otel", "feedback"] {
        section(root, template, name, &mut next, |table, tmpl| {
            for (key, _) in tmpl.iter() {
                set(table, tmpl, key);
            }
            Ok(())
        })?;
    }
    section(
        root,
        template,
        "shell_environment_policy",
        &mut next,
        exclude_token,
    )?;
    provider(root, template, &mut next)?;
    // A removed key takes the comment lines above it along.
    for key in APPROVAL_KEYS {
        if !template.contains_key(key) && holds(root, written, key) {
            if *key == "default_permissions" && needs_default_permissions(root) {
                select_workspace(root, written);
            } else {
                root.remove(key);
            }
        }
    }
    for key in LEGACY_ROOT {
        root.remove(key);
    }

    let text = doc.to_string();
    verify(&text, opts)?;
    Ok(text)
}

fn template(opts: &ConfigOptions) -> Result<DocumentMut> {
    render(opts)
        .parse()
        .context("the managed config template is not valid TOML")
}

/// Inserts `key` from the template when `table` lacks it.
fn keep(table: &mut dyn TableLike, template: &Table, key: &str) {
    if !table.contains_key(key) {
        insert_from(table, template, key);
    }
}

/// Sets `key` to the template's value. An existing key keeps its comments
/// (the lines above it and any trailing one), and an equal value is left as
/// spelled, so a second install changes nothing. A key the template lacks is
/// left alone.
fn set(table: &mut dyn TableLike, template: &Table, key: &str) {
    let Some(new) = template.get(key).and_then(Item::as_value) else {
        return;
    };
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            if !same(old, new) {
                let decor = old.decor().clone();
                *old = new.clone();
                *old.decor_mut() = decor;
            }
        }
        // A table where a value belongs, e.g. an `[otel.exporter.otlp-http]`
        // exporter: replaced by the value.
        Some(slot) => *slot = Item::Value(new.clone()),
        None => insert_from(table, template, key),
    }
}

/// Copies `key` and its own comment ([`own_comment`]) out of the template.
fn insert_from(table: &mut dyn TableLike, template: &Table, key: &str) {
    let Some((name, item)) = template.get_key_value(key) else {
        return;
    };
    let mut name = name.clone();
    name.leaf_decor_mut()
        .set_prefix(own_comment(template, key).to_owned());
    table.entry_format(&name).or_insert(item.clone());
}

/// The comment lines the template puts directly above `key`. A comment
/// separated from the key by a blank line (the file header above `model`) is
/// about the file, not the key, and is not part of it.
fn own_comment<'t>(template: &'t Table, key: &str) -> &'t str {
    let above = template
        .get_key_value(key)
        .and_then(|(name, _)| prefix(name.leaf_decor()))
        .unwrap_or("");
    above.rsplit_once("\n\n").map_or(above, |(_, after)| after)
}

fn prefix(decor: &Decor) -> Option<&str> {
    decor.prefix().and_then(|raw| raw.as_str())
}

/// Whether root `key` still holds the value the `written` template sets.
fn holds(root: &Table, written: &Table, key: &str) -> bool {
    match (root.get(key), written.get(key)) {
        (Some(Item::Value(have)), Some(Item::Value(wrote))) => same(have, wrote),
        _ => false,
    }
}

/// Whether Codex would refuse to load the file without `default_permissions`:
/// it does when `[permissions]` defines a profile and no `sandbox_mode`
/// selects the legacy settings instead (`core/src/config/mod.rs`).
fn needs_default_permissions(root: &Table) -> bool {
    let profiles = root
        .get("permissions")
        .and_then(Item::as_table_like)
        .is_some_and(|profiles| !profiles.is_empty());
    profiles && !root.contains_key("sandbox_mode")
}

/// Replaces the yolo `default_permissions` with [`WORKSPACE`] in place,
/// trading the `written` template's own comment for [`WORKSPACE_NOTE`].
fn select_workspace(root: &mut Table, written: &Table) {
    let key = "default_permissions";
    if let Some(Item::Value(old)) = root.get_mut(key) {
        let decor = old.decor().clone();
        *old = Value::from(WORKSPACE);
        *old.decor_mut() = decor;
    }
    let own = own_comment(written, key);
    edit_prefix(root, key, |above| {
        let kept = above.strip_suffix(own).unwrap_or(above);
        if kept.ends_with(WORKSPACE_NOTE) {
            kept.to_owned()
        } else {
            format!("{kept}{WORKSPACE_NOTE}")
        }
    });
}

/// Rewrites the lines above root `key`, if it is there and they change.
fn edit_prefix(root: &mut Table, key: &str, edit: impl FnOnce(&str) -> String) {
    let Some(mut name) = root.key_mut(key) else {
        return;
    };
    let above = prefix(name.leaf_decor()).unwrap_or("");
    let edited = edit(above);
    if edited != above {
        name.leaf_decor_mut().set_prefix(edited);
    }
}

/// Equality of what two scalars mean, however they are spelled.
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(l), Value::String(r)) => l.value() == r.value(),
        (Value::Boolean(l), Value::Boolean(r)) => l.value() == r.value(),
        (Value::Integer(l), Value::Integer(r)) => l.value() == r.value(),
        _ => false,
    }
}

/// Runs `fill` on table `name`, or copies the template's table whole when the
/// file has none.
fn section(
    root: &mut Table,
    template: &Table,
    name: &str,
    next: &mut isize,
    fill: impl FnOnce(&mut dyn TableLike, &Table) -> Result<()>,
) -> Result<()> {
    let Some(tmpl) = template.get(name).and_then(Item::as_table) else {
        bail!("the managed config template has no [{name}]");
    };
    let Some(item) = root.get_mut(name) else {
        let mut item = Item::Table(tmpl.clone());
        place(&mut item, next);
        root.insert(name, item);
        return Ok(());
    };
    let table = item
        .as_table_like_mut()
        .with_context(|| format!("`{name}` is not a table"))?;
    fill(table, tmpl)?;
    if let Item::Table(table) = item {
        // `[otel.exporter.otlp-http]` alone makes `otel` implicit, and an
        // implicit table's own keys would not get a header. One that gained
        // none (only `[shell_environment_policy.filters]` changed) stays
        // implicit rather than printing an empty header.
        if !table.is_dotted() && !table.get_values().is_empty() {
            table.set_implicit(false);
        }
    }
    Ok(())
}

/// Keeps the token out of every command Codex runs. Codex takes the patterns
/// in one of two forms and refuses a table that mixes them
/// (`config/src/shell_environment_policy.rs`), so the token joins whichever
/// form the file already uses:
///
/// * `filters`, keyed by pattern (`filters = { "AWS_*" = "exclude" }` or a
///   `[shell_environment_policy.filters]` table): the token's key is set to
///   `"exclude"`. Codex compares keys case-insensitively and rejects two that
///   differ only in case, so a key in any case is reused, not duplicated.
/// * the legacy `exclude` list: the token is added. It is the only filter
///   Codex applies by default (`ignore_default_excludes` defaults to true), so
///   the user's own entries must survive: union, never replace.
fn exclude_token(table: &mut dyn TableLike, template: &Table) -> Result<()> {
    if table.contains_key("filters")
        && (table.contains_key("exclude") || table.contains_key("include_only"))
    {
        bail!(
            "[shell_environment_policy] sets both `filters` and `exclude` / `include_only`, \
             which Codex refuses to load; keep one form"
        );
    }
    match table.get_mut("filters") {
        None => {}
        Some(Item::Value(Value::InlineTable(filters))) => {
            if !exclude_existing(filters) {
                push_inline(filters, TOKEN_ENV, EXCLUDE);
            }
            return Ok(());
        }
        Some(Item::Table(filters)) => {
            if !exclude_existing(filters) {
                filters.insert(TOKEN_ENV, toml_edit::value(EXCLUDE));
            }
            return Ok(());
        }
        Some(_) => bail!("`shell_environment_policy.filters` is not a table"),
    }
    let Some(slot) = table.get_mut("exclude") else {
        insert_from(table, template, "exclude");
        return Ok(());
    };
    let array = slot
        .as_array_mut()
        .context("`shell_environment_policy.exclude` is not an array")?;
    if array.iter().any(|entry| entry.as_str() == Some(TOKEN_ENV)) {
        return Ok(());
    }
    // In a one-entry-per-line array, the new entry gets a line of its own.
    let indent = array
        .iter()
        .last()
        .and_then(|entry| prefix(entry.decor()))
        .filter(|prefix| prefix.contains('\n'))
        .map(str::to_owned);
    array.push(TOKEN_ENV);
    if let (Some(indent), Some(entry)) = (indent, array.iter_mut().last()) {
        entry.decor_mut().set_prefix(indent);
    }
    Ok(())
}

/// Whether two `filters` keys name one pattern, compared the way Codex does.
fn same_pattern(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

/// Sets every `filters` key naming the token, in any case, to `"exclude"`.
/// False when there is none.
fn exclude_existing(filters: &mut dyn TableLike) -> bool {
    let mut found = false;
    for (key, item) in filters.iter_mut() {
        if !same_pattern(key.get(), TOKEN_ENV) {
            continue;
        }
        found = true;
        match item {
            Item::Value(old) if old.as_str() == Some(EXCLUDE) => {}
            Item::Value(old) => {
                let decor = old.decor().clone();
                *old = Value::from(EXCLUDE);
                *old.decor_mut() = decor;
            }
            other => *other = toml_edit::value(EXCLUDE),
        }
    }
    found
}

/// Appends `key = "value"` to an inline table in the style of its last
/// entry: on a line of its own in a one-entry-per-line table, and with the
/// spacing before `}` (or before a trailing comma) moved over from the old
/// last value.
fn push_inline(table: &mut InlineTable, key: &str, value: &str) {
    let mut indent = None;
    let mut closing = None;
    if let Some((last_key, last)) = table.iter_mut().last() {
        indent = prefix(last_key.leaf_decor())
            .filter(|prefix| prefix.contains('\n'))
            .map(str::to_owned);
        closing = last
            .decor()
            .suffix()
            .and_then(|raw| raw.as_str())
            .map(str::to_owned);
        last.decor_mut().set_suffix("");
    }
    let mut new = Value::from(value);
    if let Some(closing) = closing {
        new.decor_mut().set_suffix(closing);
    }
    table.insert(key, new);
    if let (Some(indent), Some(mut name)) = (indent, table.key_mut(key)) {
        name.leaf_decor_mut().set_prefix(indent);
    }
}

/// Replaces `[model_providers.copilot]` whole: a stale key of an earlier
/// install (1.x `http_headers`, a bearer token) must not survive next to the
/// relay's `base_url`. Other providers are untouched.
fn provider(root: &mut Table, template: &Table, next: &mut isize) -> Result<()> {
    let Some(tmpl) = template.get("model_providers").and_then(Item::as_table) else {
        bail!("the managed config template has no [model_providers]");
    };
    let Some(mut managed) = tmpl.get(PROVIDER_ID).and_then(Item::as_table).cloned() else {
        bail!("the managed config template has no [model_providers.{PROVIDER_ID}]");
    };
    match root.get_mut("model_providers") {
        None => {
            let mut item = Item::Table(tmpl.clone());
            place(&mut item, next);
            root.insert("model_providers", item);
        }
        Some(Item::Table(providers)) => {
            // A table that replaces one already in the file stays where it was
            // and keeps the comments above its header.
            match providers.get(PROVIDER_ID) {
                Some(Item::Table(old)) if old.position().is_some() && !old.is_dotted() => {
                    managed.set_position(old.position());
                    *managed.decor_mut() = old.decor().clone();
                }
                _ => {
                    managed.set_position(Some(*next));
                    *next += 1;
                }
            }
            match providers.get_mut(PROVIDER_ID) {
                Some(slot) => *slot = Item::Table(managed),
                None => {
                    providers.insert(PROVIDER_ID, Item::Table(managed));
                }
            }
        }
        // `model_providers = { ... }`: an inline table holds values only, and
        // comments inside one are not valid TOML 1.0.
        Some(Item::Value(Value::InlineTable(providers))) => {
            let mut inline = InlineTable::new();
            for (key, item) in managed.iter() {
                if let Some(value) = item.as_value() {
                    inline.insert(key, value.clone());
                }
            }
            inline.fmt();
            let value = Value::InlineTable(inline);
            match providers.get_mut(PROVIDER_ID) {
                Some(slot) => *slot = value,
                None => {
                    providers.insert(PROVIDER_ID, value);
                }
            }
        }
        Some(_) => bail!("`model_providers` is not a table"),
    }
    Ok(())
}

/// A table cloned from the template carries the template's position, which
/// would sort it among the file's own tables. Append instead.
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

/// The safety net for every way an edit can be lost or mangled on the way
/// to text: the merged file must parse and carry every managed key.
fn verify(text: &str, opts: &ConfigOptions) -> Result<()> {
    let merged: toml::Value =
        toml::from_str(text).context("the merged config is not valid TOML")?;
    let parse = |opts: &ConfigOptions| -> Result<toml::Value> {
        toml::from_str(&render(opts)).context("the managed config template is not valid TOML")
    };
    let managed = parse(opts)?;
    let written = parse(&opts.other_mode())?;
    let mut wrong = Vec::new();
    compare(Some(&merged), &managed, opts, &mut Vec::new(), &mut wrong);
    for key in APPROVAL_KEYS {
        // What the other mode wrote must be gone; a value of the user's own
        // stays.
        let leftover = merged
            .get(key)
            .is_some_and(|have| written.get(key) == Some(have));
        if managed.get(key).is_none() && leftover {
            wrong.push((*key).to_owned());
        }
    }
    for key in LEGACY_ROOT {
        if merged.get(key).is_some() {
            wrong.push((*key).to_owned());
        }
    }
    if !wrong.is_empty() {
        bail!(
            "the merge lost or mangled managed keys ({}); nothing was written",
            wrong.join(", ")
        );
    }
    Ok(())
}

/// Walks the managed template and records each key the merged file does not
/// carry as `apply` promises: present for user-owned keys the command line
/// did not name, the template's patterns excluded for
/// `[shell_environment_policy]`, equal (as a whole, for the provider table)
/// for the rest.
fn compare(
    merged: Option<&toml::Value>,
    managed: &toml::Value,
    opts: &ConfigOptions,
    path: &mut Vec<String>,
    wrong: &mut Vec<String>,
) {
    let Some(merged) = merged else {
        wrong.push(path.join("."));
        return;
    };
    let at: Vec<&str> = path.iter().map(String::as_str).collect();
    match at.as_slice() {
        [key] if USER_OWNED_ROOT.contains(key) && !requested(opts, key) => {}
        ["windows", "sandbox"] => {}
        ["shell_environment_policy"] => {
            if let Some(problem) = policy_problem(merged, managed) {
                wrong.push(problem.to_owned());
            }
        }
        ["model_providers", PROVIDER_ID] => {
            if merged != managed {
                wrong.push(path.join("."));
            }
        }
        _ => match (managed.as_table(), merged.as_table()) {
            (Some(table), Some(into)) => {
                for (key, child) in table {
                    path.push(key.clone());
                    compare(into.get(key), child, opts, path, wrong);
                    path.pop();
                }
            }
            (Some(_), None) => wrong.push(path.join(".")),
            (None, _) => {
                if merged != managed {
                    wrong.push(path.join("."));
                }
            }
        },
    }
}

/// What keeps `[shell_environment_policy]` from excluding every pattern the
/// template excludes, in whichever form the file uses ([`exclude_token`]).
/// Mixing the forms counts too: Codex refuses to load such a table.
fn policy_problem(merged: &toml::Value, managed: &toml::Value) -> Option<&'static str> {
    let want = managed
        .get("exclude")
        .and_then(toml::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let Some(policy) = merged.as_table() else {
        return Some("shell_environment_policy");
    };
    let Some(filters) = policy.get("filters") else {
        let ok = policy
            .get("exclude")
            .and_then(toml::Value::as_array)
            .is_some_and(|have| want.iter().all(|entry| have.contains(entry)));
        return (!ok).then_some("shell_environment_policy.exclude");
    };
    if policy.contains_key("exclude") || policy.contains_key("include_only") {
        return Some("shell_environment_policy (filters mixed with exclude / include_only)");
    }
    let Some(filters) = filters.as_table() else {
        return Some("shell_environment_policy.filters");
    };
    // Every key naming the pattern, in any case, must say "exclude".
    let excluded = |pattern: &str| {
        let mut actions = filters
            .iter()
            .filter(|(key, _)| same_pattern(key, pattern))
            .map(|(_, action)| action.as_str())
            .peekable();
        actions.peek().is_some() && actions.all(|action| action == Some(EXCLUDE))
    };
    let ok = want
        .iter()
        .all(|entry| entry.as_str().is_some_and(excluded));
    (!ok).then_some("shell_environment_policy.filters")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented fresh-install file, `{VERSION}` aside.
    const TEMPLATE: &str = r#"# codex-copilot {VERSION} - managed keys for running Codex against GitHub
# Copilot CAPI through the local relay. `codex-copilot install` rewrites the
# managed keys and leaves everything else in this file alone (Codex writes
# project trust, hook state and model choices back into it).

model = "gpt-6-astra"
model_reasoning_effort = "ultra"
# Codex clamps this to each model's max_context_window: "use the maximum".
model_context_window = 1000000
model_provider = "copilot"
check_for_update_on_startup = false
# Equivalent to codex --yolo. `install --no-yolo` replaces these two keys with
# approvals_reviewer = "auto_review" (the relay maps the reviewer model).
approval_policy = "never"
default_permissions = ":danger-full-access"

[windows]
sandbox = "unelevated"

[model_providers.copilot]
# "OpenAI" is the name Codex keys remote compaction and web search on. The
# relay, not Codex, is what talks to Copilot.
name = "OpenAI"
base_url = "http://127.0.0.1:12899"
wire_api = "responses"
env_key = "COPILOT_GITHUB_TOKEN"
supports_websockets = true

[analytics]
enabled = false

[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"

[feedback]
enabled = false

[shell_environment_policy]
exclude = ["COPILOT_GITHUB_TOKEN"]
"#;

    /// A config Codex and the user have both written to, with leftovers of
    /// a 1.x install.
    const LIVED_IN: &str = r#"# My own notes. Keep me.
model = "gpt-5.5" # picked with /model
model_provider = "openai"
model_catalog_json = 'C:\Users\x\.codex\copilot_config_toml\models-catalog.json'
approvals_reviewer = "auto_review"

[projects.'d:\dev']
trust_level = "trusted"

[hooks.state]
# hook bookkeeping
last_seen = 3

[tui.model_availability_nux]
"gpt-6-astra" = 1

# The 1.x provider.
[model_providers.copilot]
name = "OpenAI"
base_url = "https://api.enterprise.githubcopilot.com"
wire_api = "responses"
env_key = "COPILOT_GITHUB_TOKEN"

[model_providers.copilot.http_headers]
"copilot-integration-id" = "copilot-developer-cli"

[model_providers.other]
name = "Other"
base_url = "https://example.com/v1"

[mcp_servers.x]
command = "npx"
args = ["-y", "x"]

[windows]
sandbox = "elevated"

[shell_environment_policy]
inherit = "core"
exclude = ["AWS_*", "SECRET"]
"#;

    fn opts(yolo: bool) -> ConfigOptions {
        ConfigOptions {
            yolo,
            ..ConfigOptions::default()
        }
    }

    fn parse(text: &str) -> toml::Value {
        toml::from_str(text).expect("valid TOML")
    }

    #[test]
    fn render_is_the_documented_template() {
        let expected = TEMPLATE.replace("{VERSION}", env!("CARGO_PKG_VERSION"));
        assert_eq!(render(&opts(true)), expected);
        parse(&expected);
    }

    #[test]
    fn render_without_yolo_uses_the_auto_reviewer() {
        let text = render(&opts(false));
        let doc = parse(&text);
        assert_eq!(doc["approvals_reviewer"].as_str(), Some("auto_review"));
        assert!(doc.get("approval_policy").is_none());
        assert!(doc.get("default_permissions").is_none());
        assert!(text.contains(
            "check_for_update_on_startup = false\n# Codex's auto-reviewer answers approval \
             prompts (the relay maps its model).\napprovals_reviewer = \"auto_review\"\n\n[windows]"
        ));
    }

    #[test]
    fn render_takes_the_options() {
        let text = render(&ConfigOptions {
            model: Some("gpt-5\"x".into()),
            reasoning_effort: Some("high".into()),
            context_window: 400_000,
            listen: "127.0.0.1:5000".into(),
            yolo: true,
        });
        let doc = parse(&text);
        assert_eq!(doc["model"].as_str(), Some("gpt-5\"x"));
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("high"));
        assert_eq!(doc["model_context_window"].as_integer(), Some(400_000));
        assert_eq!(
            doc["model_providers"]["copilot"]["base_url"].as_str(),
            Some("http://127.0.0.1:5000")
        );
    }

    #[test]
    fn apply_keeps_what_codex_and_the_user_wrote() {
        let merged = apply(LIVED_IN, &opts(true)).unwrap();
        for kept in [
            "# My own notes. Keep me.\n",
            "model = \"gpt-5.5\" # picked with /model\n",
            "[projects.'d:\\dev']\ntrust_level = \"trusted\"\n",
            "[hooks.state]\n# hook bookkeeping\nlast_seen = 3\n",
            "[tui.model_availability_nux]\n\"gpt-6-astra\" = 1\n",
            "# The 1.x provider.\n[model_providers.copilot]\n",
            "[mcp_servers.x]\ncommand = \"npx\"\n",
        ] {
            assert!(merged.contains(kept), "lost {kept:?} in:\n{merged}");
        }
        let doc = parse(&merged);
        // User-owned keys stay as the user left them...
        assert_eq!(doc["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(doc["windows"]["sandbox"].as_str(), Some("elevated"));
        // ...missing ones are filled in...
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("ultra"));
        assert_eq!(doc["model_context_window"].as_integer(), Some(1_000_000));
        // ...and managed ones are forced.
        assert_eq!(doc["model_provider"].as_str(), Some("copilot"));
        assert_eq!(doc["check_for_update_on_startup"].as_bool(), Some(false));
        assert_eq!(doc["analytics"]["enabled"].as_bool(), Some(false));
        assert_eq!(doc["otel"]["metrics_exporter"].as_str(), Some("none"));
        assert_eq!(doc["feedback"]["enabled"].as_bool(), Some(false));
        assert_eq!(
            doc["projects"]["d:\\dev"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(doc["mcp_servers"]["x"]["args"][1].as_str(), Some("x"));
        assert!(doc.get("model_catalog_json").is_none());
    }

    #[test]
    fn apply_replaces_the_copilot_provider_whole() {
        let merged = apply(LIVED_IN, &opts(true)).unwrap();
        let doc = parse(&merged);
        let managed = parse(&render(&opts(true)));
        assert_eq!(
            doc["model_providers"]["copilot"],
            managed["model_providers"]["copilot"]
        );
        assert!(!merged.contains("http_headers"), "{merged}");
        assert_eq!(
            doc["model_providers"]["other"]["base_url"].as_str(),
            Some("https://example.com/v1")
        );
        // The replacement stays where the old table was, before `other`.
        let copilot = merged.find("[model_providers.copilot]").unwrap();
        let other = merged.find("[model_providers.other]").unwrap();
        assert!(copilot < other, "{merged}");
        assert!(
            merged.contains("# \"OpenAI\" is the name Codex keys"),
            "{merged}"
        );
    }

    #[test]
    fn apply_unions_the_exclude_list() {
        let merged = apply(LIVED_IN, &opts(true)).unwrap();
        assert!(
            merged.contains("exclude = [\"AWS_*\", \"SECRET\", \"COPILOT_GITHUB_TOKEN\"]"),
            "{merged}"
        );
        assert_eq!(
            parse(&merged)["shell_environment_policy"]["inherit"].as_str(),
            Some("core")
        );

        let multiline = "[shell_environment_policy]\nexclude = [\n  \"AWS_*\",\n]\n";
        let merged = apply(multiline, &opts(true)).unwrap();
        assert!(
            merged.contains("exclude = [\n  \"AWS_*\",\n  \"COPILOT_GITHUB_TOKEN\",\n]"),
            "{merged}"
        );

        let present = "[shell_environment_policy]\nexclude = [\"COPILOT_GITHUB_TOKEN\", \"X\"]\n";
        let merged = apply(present, &opts(true)).unwrap();
        assert!(
            merged.contains("exclude = [\"COPILOT_GITHUB_TOKEN\", \"X\"]\n"),
            "{merged}"
        );

        let broken = "[shell_environment_policy]\nexclude = \"COPILOT_GITHUB_TOKEN\"\n";
        assert!(apply(broken, &opts(true)).is_err());
    }

    #[test]
    fn apply_switches_between_yolo_and_auto_review() {
        let reviewed = apply(&render(&opts(true)), &opts(false)).unwrap();
        let doc = parse(&reviewed);
        assert_eq!(doc["approvals_reviewer"].as_str(), Some("auto_review"));
        assert!(doc.get("approval_policy").is_none(), "{reviewed}");
        assert!(doc.get("default_permissions").is_none(), "{reviewed}");
        assert!(
            !reviewed.contains("Equivalent to codex --yolo"),
            "{reviewed}"
        );
        assert!(
            reviewed.contains("# Codex's auto-reviewer answers"),
            "{reviewed}"
        );

        let yolo = apply(&reviewed, &opts(true)).unwrap();
        let doc = parse(&yolo);
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(
            doc["default_permissions"].as_str(),
            Some(":danger-full-access")
        );
        assert!(doc.get("approvals_reviewer").is_none(), "{yolo}");
        assert!(yolo.contains("# Equivalent to codex --yolo"), "{yolo}");
        // A round trip lands on the fresh-install text again.
        assert_eq!(yolo, render(&opts(true)));

        // A user-set policy gives way to yolo too.
        let asked = "approval_policy = \"on-request\"\n";
        let doc = parse(&apply(asked, &opts(true)).unwrap());
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
    }

    #[test]
    fn an_explicit_model_replaces_a_kept_one() {
        let asked = ConfigOptions {
            model: Some("gpt-5.5".into()),
            reasoning_effort: Some("high".into()),
            ..ConfigOptions::default()
        };
        let merged = apply(&render(&ConfigOptions::default()), &asked).unwrap();
        let doc = parse(&merged);
        assert_eq!(doc["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("high"));
        // Only the values changed.
        assert_eq!(merged, render(&asked));

        // Without the flags, a `/model` choice survives...
        let chosen = merged.replace("\"gpt-5.5\"", "\"gpt-6-luna\" # picked with /model");
        assert_eq!(apply(&chosen, &ConfigOptions::default()).unwrap(), chosen);
        // ...and with them it is replaced, its comment kept.
        let again = apply(&chosen, &asked).unwrap();
        assert!(
            again.contains("\nmodel = \"gpt-5.5\" # picked with /model\n"),
            "{again}"
        );
        // The safety net holds an explicit value to equality.
        assert!(verify(&chosen, &ConfigOptions::default()).is_ok());
        let err = verify(&chosen, &asked).unwrap_err().to_string();
        assert!(err.contains("model"), "{err}");
    }

    #[test]
    fn a_mode_switch_keeps_approval_values_of_the_users_own() {
        let own = render(&opts(true)).replace(
            "default_permissions = \":danger-full-access\"",
            "default_permissions = \"dev\"",
        ) + "\n[permissions.dev]\nextends = \":workspace\"\n";
        let reviewed = apply(&own, &opts(false)).unwrap();
        let doc = parse(&reviewed);
        assert_eq!(doc["default_permissions"].as_str(), Some("dev"));
        assert_eq!(
            doc["permissions"]["dev"]["extends"].as_str(),
            Some(":workspace")
        );
        // What yolo wrote is gone, the reviewer is in.
        assert!(doc.get("approval_policy").is_none(), "{reviewed}");
        assert_eq!(doc["approvals_reviewer"].as_str(), Some("auto_review"));
        assert_eq!(apply(&reviewed, &opts(false)).unwrap(), reviewed);

        // A policy of the user's own stays with auto review, too.
        let asked = "approval_policy = \"on-request\"\n";
        let doc = parse(&apply(asked, &opts(false)).unwrap());
        assert_eq!(doc["approval_policy"].as_str(), Some("on-request"));

        // The safety net tells the two apart.
        assert!(verify(&reviewed, &opts(false)).is_ok());
        let leftover = format!("approval_policy = \"never\"\n{reviewed}");
        let err = verify(&leftover, &opts(false)).unwrap_err().to_string();
        assert!(err.contains("approval_policy"), "{err}");
    }

    #[test]
    fn no_yolo_keeps_a_permissions_profile_selected() {
        // Codex refuses to load `[permissions]` profiles without a
        // `default_permissions`, so the yolo value turns into `:workspace`.
        let yolo = render(&opts(true)) + "\n[permissions.dev]\nextends = \":workspace\"\n";
        let reviewed = apply(&yolo, &opts(false)).unwrap();
        let doc = parse(&reviewed);
        assert_eq!(doc["default_permissions"].as_str(), Some(":workspace"));
        assert!(doc.get("approval_policy").is_none(), "{reviewed}");
        assert_eq!(doc["approvals_reviewer"].as_str(), Some("auto_review"));
        assert_eq!(
            doc["permissions"]["dev"]["extends"].as_str(),
            Some(":workspace")
        );
        assert!(
            reviewed.contains(
                "check_for_update_on_startup = false\n# Codex refuses \
                 to load [permissions] profiles unless one is selected here.\n\
                 default_permissions = \":workspace\"\n# Codex's auto-reviewer answers"
            ),
            "{reviewed}"
        );
        assert!(verify(&reviewed, &opts(false)).is_ok());
        assert_eq!(apply(&reviewed, &opts(false)).unwrap(), reviewed);

        // Yolo sets its value again and drops the note; the modes keep
        // alternating between the same two files.
        let back = apply(&reviewed, &opts(true)).unwrap();
        let doc = parse(&back);
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(
            doc["default_permissions"].as_str(),
            Some(":danger-full-access")
        );
        assert!(doc.get("approvals_reviewer").is_none(), "{back}");
        assert!(!back.contains(WORKSPACE_NOTE), "{back}");
        assert_eq!(apply(&back, &opts(true)).unwrap(), back);
        assert_eq!(apply(&back, &opts(false)).unwrap(), reviewed);

        // Without a profile, or with `sandbox_mode` selecting the legacy
        // settings, Codex loads the file as it is: the key goes.
        for loads in [
            render(&opts(true)) + "\n[permissions]\n",
            format!(
                "sandbox_mode = \"workspace-write\"\n{}\n[permissions.dev]\nextends = \":workspace\"\n",
                render(&opts(true))
            ),
        ] {
            let reviewed = apply(&loads, &opts(false)).unwrap();
            assert!(
                parse(&reviewed).get("default_permissions").is_none(),
                "{reviewed}"
            );
            assert!(!reviewed.contains(WORKSPACE_NOTE), "{reviewed}");
        }
    }

    #[test]
    fn apply_adds_the_token_to_keyed_filters_instead_of_exclude() {
        let cases = [
            (
                "[shell_environment_policy]\nfilters = { \"AWS_*\" = \"exclude\" }\n",
                "filters = { \"AWS_*\" = \"exclude\", COPILOT_GITHUB_TOKEN = \"exclude\" }\n",
            ),
            (
                "[shell_environment_policy]\nfilters = {}\n",
                "filters = { COPILOT_GITHUB_TOKEN = \"exclude\" }\n",
            ),
            // TOML 1.1: one entry per line, trailing comma.
            (
                "[shell_environment_policy]\nfilters = {\n  \"AWS_*\" = \"exclude\",\n}\n",
                "filters = {\n  \"AWS_*\" = \"exclude\",\n  COPILOT_GITHUB_TOKEN = \"exclude\",\n}\n",
            ),
            (
                "[shell_environment_policy]\ninherit = \"core\"\n\n\
                 [shell_environment_policy.filters]\n\"AWS_*\" = \"exclude\"\nPATH = \"include\"\n",
                "[shell_environment_policy.filters]\n\"AWS_*\" = \"exclude\"\nPATH = \"include\"\n\
                 COPILOT_GITHUB_TOKEN = \"exclude\"\n",
            ),
            (
                "[shell_environment_policy]\nfilters.\"AWS_*\" = \"exclude\"\n",
                "filters.\"AWS_*\" = \"exclude\"\nfilters.COPILOT_GITHUB_TOKEN = \"exclude\"\n",
            ),
            // No `[shell_environment_policy]` header of its own: none is added.
            (
                "[shell_environment_policy.filters]\n\"AWS_*\" = \"exclude\"\n",
                "default_permissions = \":danger-full-access\"\n\
                 [shell_environment_policy.filters]\n\"AWS_*\" = \"exclude\"\n\
                 COPILOT_GITHUB_TOKEN = \"exclude\"\n",
            ),
            // Codex rejects keys that differ only in case: reuse, and turn
            // an include around.
            (
                "[shell_environment_policy]\nfilters = { copilot_github_token = \"include\" }\n",
                "filters = { copilot_github_token = \"exclude\" }\n",
            ),
        ];
        for (text, expected) in cases {
            let merged = apply(text, &opts(true)).unwrap();
            assert!(merged.contains(expected), "{text:?} became:\n{merged}");
            let policy = &parse(&merged)["shell_environment_policy"];
            assert!(policy.get("exclude").is_none(), "{merged}");
            assert_eq!(apply(&merged, &opts(true)).unwrap(), merged);
        }

        // Mixing the forms is what Codex refuses to load.
        for mixed in [
            "[shell_environment_policy]\nexclude = [\"X\"]\nfilters = { Y = \"exclude\" }\n",
            "[shell_environment_policy]\ninclude_only = [\"X\"]\nfilters = { Y = \"exclude\" }\n",
        ] {
            let err = format!("{:#}", apply(mixed, &opts(true)).unwrap_err());
            assert!(err.contains("filters"), "{err}");
        }
        assert!(apply("[shell_environment_policy]\nfilters = \"x\"\n", &opts(true)).is_err());
    }

    #[test]
    fn verify_accepts_either_policy_form_but_not_both() {
        let with = |policy: &str| {
            render(&opts(true)).replace(
                "[shell_environment_policy]\nexclude = [\"COPILOT_GITHUB_TOKEN\"]\n",
                &format!("[shell_environment_policy]\n{policy}"),
            )
        };
        for good in [
            "exclude = [\"A\", \"COPILOT_GITHUB_TOKEN\"]\n",
            "filters = { COPILOT_GITHUB_TOKEN = \"exclude\" }\n",
            "filters = { copilot_github_token = \"exclude\", A = \"include\" }\n",
        ] {
            verify(&with(good), &opts(true)).unwrap();
        }
        for bad in [
            "exclude = [\"A\"]\n",
            "filters = { A = \"exclude\" }\n",
            "filters = { COPILOT_GITHUB_TOKEN = \"include\" }\n",
            "filters = { COPILOT_GITHUB_TOKEN = \"exclude\" }\nexclude = [\"COPILOT_GITHUB_TOKEN\"]\n",
            "filters = { COPILOT_GITHUB_TOKEN = \"exclude\" }\ninclude_only = [\"PATH\"]\n",
        ] {
            let err = verify(&with(bad), &opts(true)).unwrap_err().to_string();
            assert!(err.contains("shell_environment_policy"), "{bad}: {err}");
        }
    }

    #[test]
    fn apply_is_idempotent() {
        for yolo in [true, false] {
            let once = apply(LIVED_IN, &opts(yolo)).unwrap();
            let twice = apply(&once, &opts(yolo)).unwrap();
            assert_eq!(once, twice);
            let fresh = render(&opts(yolo));
            assert_eq!(apply(&fresh, &opts(yolo)).unwrap(), fresh);
        }
    }

    #[test]
    fn apply_adds_missing_tables_after_the_existing_ones() {
        let merged = apply("[[skills.config]]\nname = \"a\"\n", &opts(true)).unwrap();
        let doc = parse(&merged);
        assert_eq!(doc["skills"]["config"][0]["name"].as_str(), Some("a"));
        assert!(merged.starts_with("model = \"gpt-6-astra\"\n"), "{merged}");
        assert!(
            merged.find("[[skills.config]]").unwrap() < merged.find("[windows]").unwrap(),
            "{merged}"
        );
        // The file header is not copied into a merged file.
        assert!(
            !merged.contains("managed keys for running Codex"),
            "{merged}"
        );
        assert!(merged.contains("# Codex clamps this"), "{merged}");
        assert_eq!(apply(&merged, &opts(true)).unwrap(), merged);
    }

    #[test]
    fn apply_overrides_a_table_valued_exporter_and_inline_providers() {
        let text = r#"model_providers = { copilot = { name = "x", http_headers = { a = "b" } }, other = { name = "o" } }

[otel.exporter.otlp-http]
endpoint = "https://x"
"#;
        let merged = apply(text, &opts(true)).unwrap();
        let doc = parse(&merged);
        assert_eq!(doc["otel"]["exporter"].as_str(), Some("none"));
        assert_eq!(doc["otel"]["trace_exporter"].as_str(), Some("none"));
        assert_eq!(
            doc["model_providers"]["copilot"]["base_url"].as_str(),
            Some("http://127.0.0.1:12899")
        );
        assert!(doc["model_providers"]["copilot"]
            .get("http_headers")
            .is_none());
        assert_eq!(doc["model_providers"]["other"]["name"].as_str(), Some("o"));
    }

    #[test]
    fn apply_of_an_empty_file_renders() {
        assert_eq!(apply("", &opts(true)).unwrap(), render(&opts(true)));
        assert_eq!(apply("\n  \n", &opts(false)).unwrap(), render(&opts(false)));
    }

    #[test]
    fn apply_rejects_what_it_cannot_merge() {
        assert!(apply("model = ", &opts(true)).is_err());
        assert!(apply("windows = 3\n", &opts(true)).is_err());
        assert!(apply("model_providers = \"x\"\n", &opts(true)).is_err());
    }
}
