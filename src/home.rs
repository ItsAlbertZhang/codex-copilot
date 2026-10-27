//! The dedicated Codex home, its state file, and the `override` swap.
//!
//! The dedicated home is a second CODEX_HOME next to `~/.codex`. It carries
//! `codex-copilot.json`, and that file moves with the directory when
//! `override` swaps the two directory names, so "which directory holds the
//! state file" is the only record of whether an override is active. There is
//! deliberately no marker or transaction file that could disagree with it.
//!
//! The one thing the swap does not move with a home is `packages/`, Codex's
//! own installation, which absolute links elsewhere point into: it stays at
//! its path (see [`packages_steps`]).

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tempfile::NamedTempFile;

use crate::STATE_FILE;

/// Name a directory is parked under for the middle step of a three-way swap.
/// A leftover one means an earlier swap died half-way, and only a human can
/// tell which directory is which, so swaps refuse to run while it exists.
pub const SWAP_TEMP_NAME: &str = ".codex.codex-copilot-swap";

/// Where a Codex home keeps Codex's own installation (see [`packages_steps`]).
pub const PACKAGES_DIR: &str = "packages";

/// Name a `packages/` directory is parked under while a swap puts the two
/// installations back in place. Refused when left over, like
/// [`SWAP_TEMP_NAME`].
pub const PACKAGES_SWAP_TEMP_NAME: &str = ".codex-packages.codex-copilot-swap";

/// The two sibling directories under the user's home directory.
#[derive(Debug, Clone)]
pub struct Homes {
    /// Parent directory (normally the user's home; tests pass a temp dir).
    pub root: PathBuf,
}

/// Where the dedicated home currently lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// No directory carries the state file.
    None,
    /// `~/.codex-copilot` carries it (normal).
    Normal,
    /// `~/.codex` carries it (override active).
    Overridden,
}

/// Contents of `codex-copilot.json` in the dedicated home.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct State {
    pub version: String,
    pub installed_at: String,
    pub listen: String,
    pub upstream: String,
    pub review_model: String,
    pub yolo: bool,
    pub codex_version: Option<String>,
}

impl Homes {
    /// `root` as given (made absolute), or the user's home directory.
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(root) => std::path::absolute(&root)
                .with_context(|| format!("could not resolve {}", root.display()))?,
            None => dirs::home_dir().context("could not determine the user's home directory")?,
        };
        Ok(Self { root })
    }
    pub fn codex(&self) -> PathBuf {
        self.root.join(crate::CODEX_HOME_NAME)
    }
    pub fn copilot(&self) -> PathBuf {
        self.root.join(crate::COPILOT_HOME_NAME)
    }
    /// Where a swap parks a directory between two renames.
    pub fn swap_temp(&self) -> PathBuf {
        self.root.join(SWAP_TEMP_NAME)
    }
    /// Where a swap parks a `packages/` directory between two renames.
    pub fn packages_swap_temp(&self) -> PathBuf {
        self.root.join(PACKAGES_SWAP_TEMP_NAME)
    }
    /// Which directory carries the state file.
    pub fn locate(&self) -> Result<Located> {
        let (copilot, codex) = (self.copilot(), self.codex());
        // Checked before the state file, which a link shows in both places
        // or in neither. With neither, a first install would write the
        // managed config (yolo on) straight into the regular Codex home;
        // with both, deleting "one" state file would delete the only one.
        if exists(&copilot)? && exists(&codex)? && same_dir(&copilot, &codex)? {
            bail!(
                "{} and {} are the same directory: one is a link to the other; refusing. \
                 codex-copilot tells its home from the regular Codex home by which of the two \
                 holds {STATE_FILE}, and `override` swaps their names, so they must be two \
                 separate directories. Replace the link with a real directory",
                copilot.display(),
                codex.display()
            );
        }
        let normal = carries_state(&copilot)?;
        let overridden = carries_state(&codex)?;
        match (normal, overridden) {
            (true, true) => bail!(
                "both {} and {} contain {STATE_FILE}, so codex-copilot cannot tell which one is \
                 its home; delete the state file from the one that is your regular Codex home",
                self.copilot().display(),
                self.codex().display()
            ),
            (true, false) => Ok(Located::Normal),
            (false, true) => Ok(Located::Overridden),
            (false, false) => Ok(Located::None),
        }
    }
    /// Path of the directory that is (or will be) the dedicated home.
    pub fn copilot_home(&self, located: Located) -> PathBuf {
        match located {
            Located::Overridden => self.codex(),
            Located::None | Located::Normal => self.copilot(),
        }
    }
}

fn carries_state(dir: &Path) -> Result<bool> {
    exists(&dir.join(STATE_FILE))
}

/// Whether `path` exists, links followed (a dangling one does not).
fn exists(path: &Path) -> Result<bool> {
    path.try_exists()
        .with_context(|| format!("could not check for {}", path.display()))
}

/// Whether two existing directories are one, reached through a symlink or a
/// junction.
fn same_dir(left: &Path, right: &Path) -> Result<bool> {
    let resolve = |path: &Path| {
        fs::canonicalize(path).with_context(|| format!("could not resolve {}", path.display()))
    };
    Ok(resolve(left)? == resolve(right)?)
}

/// The state file of `home`, or `None` when there is none.
pub fn read_state(home: &Path) -> Result<Option<State>> {
    let path = home.join(STATE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
    };
    let state = serde_json::from_str(&text)
        .with_context(|| format!("{} is corrupt; reinstall to rewrite it", path.display()))?;
    Ok(Some(state))
}

/// Writes the state file atomically, creating `home` if needed.
pub fn write_state(home: &Path, state: &State) -> Result<()> {
    fs::create_dir_all(home).with_context(|| format!("could not create {}", home.display()))?;
    let mut text = serde_json::to_string_pretty(state).context("could not serialize the state")?;
    text.push('\n');
    atomic_write(&home.join(STATE_FILE), &text)
}

/// Replaces `path` with `text` through a temp file in the same directory, so
/// a crash leaves either the old file or the new one, never half of one.
///
/// A symlinked `path` (a config.toml kept in a dotfiles repository) is
/// followed: the file it points at is replaced and the link stays. Renaming
/// onto the link itself would replace the link with a plain file.
///
/// NamedTempFile is created 0600 on Unix and keeps those bits after persist.
/// That is intentional: config.toml and the state file are the user's alone.
pub(crate) fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let target =
        follow_links(path).with_context(|| format!("could not resolve {}", path.display()))?;
    let dir = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let write = || -> std::io::Result<()> {
        let mut temporary = NamedTempFile::new_in(dir)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(&target).map_err(|err| err.error)?;
        Ok(())
    };
    write().with_context(|| {
        if target == path {
            format!("could not write {}", path.display())
        } else {
            format!(
                "could not write {} (linked from {})",
                target.display(),
                path.display()
            )
        }
    })
}

/// The file `path` names once every symlink on the way is followed. The end
/// of the chain need not exist yet (a dangling link is written through, like
/// `fs::write` would), which is why this is not `fs::canonicalize`.
fn follow_links(path: &Path) -> std::io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    // The limit Linux puts on a chain (ELOOP); Windows allows fewer.
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // A relative target is relative to the link's directory.
                let target = fs::read_link(&path)?;
                path = match path.parent() {
                    Some(dir) => dir.join(target),
                    None => target,
                };
            }
            Err(err) if err.kind() != ErrorKind::NotFound => return Err(err),
            _ => return Ok(path),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// RFC 3339 UTC timestamp for `installed_at`.
pub fn now_rfc3339() -> String {
    iso8601(now_secs())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `2026-09-10T12:34:56Z` from a Unix timestamp, without pulling in a date
/// crate. Civil-from-days after Howard Hinnant.
fn iso8601(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// What a swap did, for messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapReport {
    pub steps: Vec<String>,
    /// How the Codex installs (`packages/`) were handled, when there were any.
    pub notes: Vec<String>,
}

/// Make `~/.codex` the dedicated home (and park the original at
/// `~/.codex-copilot`).
///
/// Does not look for running Codex processes; the command layer does that
/// before calling this.
pub fn override_homes(homes: &Homes) -> Result<SwapReport> {
    match homes.locate()? {
        Located::Normal => {}
        Located::None => bail!("codex-copilot is not installed; run `codex-copilot install`"),
        Located::Overridden => bail!(
            "already overridden: {} is the codex-copilot home",
            homes.codex().display()
        ),
    }
    swap(homes)
}

/// Undo [`override_homes`].
pub fn unoverride_homes(homes: &Homes) -> Result<SwapReport> {
    match homes.locate()? {
        Located::Overridden => {}
        Located::None => bail!("codex-copilot is not installed; nothing to unoverride"),
        Located::Normal => bail!(
            "not overridden: {} is already the codex-copilot home",
            homes.copilot().display()
        ),
    }
    swap(homes)
}

/// Exchanges the names `~/.codex` and `~/.codex-copilot`. When only one of
/// them exists (no regular Codex home to park), it is a single rename. Then
/// the Codex installs in `packages/` are put back where their links lead
/// (see [`packages_steps`]).
fn swap(homes: &Homes) -> Result<SwapReport> {
    let (codex, copilot, temp) = (homes.codex(), homes.copilot(), homes.swap_temp());
    for temp in [&temp, &homes.packages_swap_temp()] {
        if present(temp)? {
            bail!(
                "{} exists, probably left by an interrupted override or unoverride; check which \
                 directory is which, rename it by hand, then retry",
                temp.display()
            );
        }
    }
    let (codex_there, copilot_there) = (present(&codex)?, present(&copilot)?);
    let mut steps = match (codex_there, copilot_there) {
        (true, true) => vec![
            Step::Rename(codex.clone(), temp.clone()),
            Step::Rename(copilot.clone(), codex),
            Step::Rename(temp, copilot),
        ],
        (true, false) => vec![Step::Rename(codex, copilot)],
        (false, true) => vec![Step::Rename(copilot, codex)],
        // `locate` just found the state file in one of them.
        (false, false) => bail!(
            "neither {} nor {} exists",
            codex.display(),
            copilot.display()
        ),
    };
    let (packages, notes) = packages_steps(homes, codex_there, copilot_there)?;
    steps.extend(packages);
    let steps = run_steps(&steps)?;
    Ok(SwapReport { steps, notes })
}

/// The steps that follow the directory renames of a swap, plus notes saying
/// what they do.
///
/// `packages/` is Codex's own installation, not part of either identity: the
/// standalone installers (`install.sh`, `install.ps1`) put the releases in
/// `$CODEX_HOME/packages/standalone`, and the app server daemon's in
/// `$CODEX_HOME/packages/app-server-daemon`, then point absolute symlinks
/// (junctions on Windows) into it: `packages/standalone/current`, and the
/// `codex` command itself (`~/.local/bin/codex`,
/// `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`). Moved with its home, such a
/// tree leaves every one of those links dangling, `codex` included.
///
/// So each `packages/` stays at the path it was installed under: after the
/// homes swap names, the two `packages/` directories swap back (or the one
/// there is moves back, into an empty directory created for it when its home
/// went away entirely). The exception is a tree whose `current` links all
/// lead into the *other* home, as left by codex-copilot 2.0.0 and 2.0.1,
/// which moved `packages/` with its home: when every tree there is such a
/// leftover, the swap carries them to the path their links name instead.
///
/// `codex_there` / `copilot_there`: whether `~/.codex` / `~/.codex-copilot`
/// exist before the swap.
fn packages_steps(
    homes: &Homes,
    codex_there: bool,
    copilot_there: bool,
) -> Result<(Vec<Step>, Vec<String>)> {
    let (codex, copilot) = (homes.codex(), homes.copilot());
    let (codex_pkg, copilot_pkg) = (codex.join(PACKAGES_DIR), copilot.join(PACKAGES_DIR));
    let at_codex = codex_there && present(&codex_pkg)?;
    let at_copilot = copilot_there && present(&copilot_pkg)?;
    if !at_codex && !at_copilot {
        return Ok((Vec::new(), Vec::new()));
    }
    let leftover_at_codex = at_codex && installed_for(&codex_pkg, &copilot);
    let leftover_at_copilot = at_copilot && installed_for(&copilot_pkg, &codex);
    if (!at_codex || leftover_at_codex) && (!at_copilot || leftover_at_copilot) {
        // Moving with their homes takes them to where their links lead.
        let went = |from: &Path, to: &Path| {
            format!(
                "{} went with its directory to {}, where its links already led (an earlier \
                 codex-copilot had moved it away)",
                from.display(),
                to.display()
            )
        };
        let mut notes = Vec::new();
        if at_codex {
            notes.push(went(&codex_pkg, &copilot_pkg));
        }
        if at_copilot {
            notes.push(went(&copilot_pkg, &codex_pkg));
        }
        return Ok((Vec::new(), notes));
    }

    // After the directory renames, the tree from `~/.codex` sits under
    // `~/.codex-copilot` and the other way round.
    let mut steps = Vec::new();
    match (at_codex, at_copilot) {
        (true, true) => {
            let temp = homes.packages_swap_temp();
            steps.push(Step::Rename(codex_pkg.clone(), temp.clone()));
            steps.push(Step::Rename(copilot_pkg.clone(), codex_pkg.clone()));
            steps.push(Step::Rename(temp, copilot_pkg.clone()));
        }
        (true, false) => {
            // A lone ~/.codex was renamed away: recreate it to hold the tree.
            if !copilot_there {
                steps.push(Step::CreateDir(codex.clone()));
            }
            steps.push(Step::Rename(copilot_pkg.clone(), codex_pkg.clone()));
        }
        (false, true) => {
            if !codex_there {
                steps.push(Step::CreateDir(copilot.clone()));
            }
            steps.push(Step::Rename(codex_pkg.clone(), copilot_pkg.clone()));
        }
        (false, false) => unreachable!("returned above"),
    }
    let kept: Vec<String> = [(at_codex, &codex_pkg), (at_copilot, &copilot_pkg)]
        .into_iter()
        .filter(|(there, _)| *there)
        .map(|(_, path)| path.display().to_string())
        .collect();
    let notes = vec![format!(
        "{} stayed where {} (the Codex installation the `codex` command links into, not part \
         of either home)",
        kept.join(" and "),
        if kept.len() == 1 {
            "it was"
        } else {
            "they were"
        }
    )];
    Ok((steps, notes))
}

/// Whether the Codex installation in `packages` was made for `home`: it has
/// at least one `<package>/current` link, and every one of them leads into
/// `home`'s `packages/`. Unreadable means no.
fn installed_for(packages: &Path, home: &Path) -> bool {
    let Ok(entries) = fs::read_dir(packages) else {
        return false;
    };
    let target_dir = home.join(PACKAGES_DIR);
    let mut found = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let current = entry.path().join("current");
        match fs::read_link(&current) {
            Ok(target) if leads_into(&target, &target_dir) => found = true,
            Ok(_) => return false,
            // Not a link, or no `current` at all: nothing to go by.
            Err(_) => {}
        }
    }
    found
}

/// Whether the absolute link target `target` names something inside `dir`,
/// compared as text: the target may well not exist.
fn leads_into(target: &Path, dir: &Path) -> bool {
    #[cfg(windows)]
    {
        // A junction reads back as `\\?\C:\...`; compare case-insensitively
        // without that prefix, the way Windows resolves the names.
        let plain = |path: &Path| {
            let text = path.to_string_lossy().replace('/', "\\").to_lowercase();
            let text = match text.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("unc\\") => rest.to_string(),
                _ => text,
            };
            text.trim_end_matches('\\').to_string()
        };
        let (target, dir) = (plain(target), plain(dir));
        target == dir || target.starts_with(&format!("{dir}\\"))
    }
    #[cfg(not(windows))]
    {
        target.starts_with(dir)
    }
}

/// Whether `dir` holds nothing, or nothing but `packages/` (what a swap
/// leaves where there was no regular Codex home, only a Codex install).
pub fn holds_only_packages(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .into_iter()
            .all(|entry| entry.is_ok_and(|entry| entry.file_name() == PACKAGES_DIR))
    })
}

/// `symlink_metadata`, not `exists`: a dangling symlink still occupies the
/// name, and a rename onto it would fail (or, on Unix, replace it).
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("could not check {}", path.display())),
    }
}

/// One filesystem change of a swap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Rename(PathBuf, PathBuf),
    /// Creates an empty directory.
    CreateDir(PathBuf),
    /// Removes an empty directory: only ever the undo of `CreateDir`.
    RemoveDir(PathBuf),
}

impl Step {
    fn apply(&self) -> std::io::Result<()> {
        match self {
            Step::Rename(from, to) => fs::rename(from, to),
            Step::CreateDir(path) => fs::create_dir(path),
            Step::RemoveDir(path) => fs::remove_dir(path),
        }
    }

    fn undo(&self) -> Step {
        match self {
            Step::Rename(from, to) => Step::Rename(to.clone(), from.clone()),
            Step::CreateDir(path) => Step::RemoveDir(path.clone()),
            Step::RemoveDir(path) => Step::CreateDir(path.clone()),
        }
    }

    /// `rename a -> b`, for "could not ...".
    fn attempt(&self) -> String {
        match self {
            Step::Rename(from, to) => format!("rename {} -> {}", from.display(), to.display()),
            Step::CreateDir(path) => format!("create {}", path.display()),
            Step::RemoveDir(path) => format!("remove {}", path.display()),
        }
    }

    /// `renamed a -> b`, once done.
    fn done(&self) -> String {
        match self {
            Step::Rename(from, to) => renamed(from, to),
            Step::CreateDir(path) => format!("created {}", path.display()),
            Step::RemoveDir(path) => format!("removed {}", path.display()),
        }
    }
}

/// Performs the steps in order. When one fails, the ones already done are
/// undone in reverse, so a failed swap (typically Windows refusing to rename
/// a directory with an open file) leaves both directories where they were.
fn run_steps(steps: &[Step]) -> Result<Vec<String>> {
    run_steps_with(steps, Step::apply)
}

/// [`run_steps`] over any step runner, so tests can fail chosen calls.
fn run_steps_with(
    steps: &[Step],
    mut apply: impl FnMut(&Step) -> std::io::Result<()>,
) -> Result<Vec<String>> {
    let mut report = Vec::new();
    for (done, step) in steps.iter().enumerate() {
        if let Err(err) = apply(step) {
            let failed = format!("could not {}: {err}", step.attempt());
            return Err(roll_back(&steps[..done], &failed, &mut apply));
        }
        report.push(step.done());
    }
    Ok(report)
}

/// Undoes `done`, newest first, and stops at the first undo that fails: an
/// earlier step's name is only free again once the later steps are undone,
/// so carrying on could move a directory onto the wrong name. The error says
/// exactly which steps are still in effect.
fn roll_back(
    done: &[Step],
    failed: &str,
    apply: &mut impl FnMut(&Step) -> std::io::Result<()>,
) -> anyhow::Error {
    let mut undone = Vec::new();
    for (index, step) in done.iter().enumerate().rev() {
        if let Err(err) = apply(&step.undo()) {
            let in_effect: Vec<String> = done[..=index].iter().map(Step::done).collect();
            return anyhow!(
                "{failed}; undoing it failed as well (could not {}: {err}), so the rollback \
                 stopped there. Fix the directories by hand. Still in effect: {}. Undone: {}.",
                step.undo().attempt(),
                in_effect.join(", "),
                if undone.is_empty() {
                    "none".to_owned()
                } else {
                    undone.join(", ")
                }
            );
        }
        undone.push(step.done());
    }
    anyhow!("{failed}; nothing was changed (is a program using one of the directories?)")
}

fn renamed(from: &Path, to: &Path) -> String {
    format!("renamed {} -> {}", from.display(), to.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CODEX_HOME_NAME, COPILOT_HOME_NAME};

    fn homes(dir: &Path) -> Homes {
        Homes::new(Some(dir.to_path_buf())).unwrap()
    }

    /// A directory holding a marker file that names it, optionally with the
    /// state file.
    fn make_dir(path: &Path, marker: &str, state: bool) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("marker"), marker).unwrap();
        if state {
            fs::write(path.join(STATE_FILE), "{}").unwrap();
        }
    }

    fn marker(path: &Path) -> String {
        fs::read_to_string(path.join("marker")).unwrap()
    }

    fn state() -> State {
        State {
            version: "2.0.0".into(),
            installed_at: "2026-10-07T00:00:00Z".into(),
            listen: crate::DEFAULT_LISTEN.into(),
            upstream: crate::DEFAULT_UPSTREAM.into(),
            review_model: crate::DEFAULT_REVIEW_MODEL.into(),
            yolo: true,
            codex_version: Some("0.160.0".into()),
        }
    }

    #[test]
    fn new_makes_the_root_absolute() {
        let homes = Homes::new(Some(PathBuf::from("relative"))).unwrap();
        assert!(homes.root.is_absolute());
        assert!(homes.root.ends_with("relative"));
        assert!(homes.codex().ends_with(CODEX_HOME_NAME));
        assert!(homes.copilot().ends_with(COPILOT_HOME_NAME));
    }

    #[test]
    fn locate_follows_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        assert_eq!(h.locate().unwrap(), Located::None);

        // A plain ~/.codex and an empty ~/.codex-copilot carry nothing.
        make_dir(&h.codex(), "codex", false);
        fs::create_dir_all(h.copilot()).unwrap();
        assert_eq!(h.locate().unwrap(), Located::None);

        fs::write(h.copilot().join(STATE_FILE), "{}").unwrap();
        assert_eq!(h.locate().unwrap(), Located::Normal);
        assert_eq!(h.copilot_home(Located::Normal), h.copilot());

        fs::remove_file(h.copilot().join(STATE_FILE)).unwrap();
        fs::write(h.codex().join(STATE_FILE), "{}").unwrap();
        assert_eq!(h.locate().unwrap(), Located::Overridden);
        assert_eq!(h.copilot_home(Located::Overridden), h.codex());
    }

    #[test]
    fn locate_refuses_two_state_files() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "codex", true);
        make_dir(&h.copilot(), "copilot", true);
        let err = h.locate().unwrap_err().to_string();
        assert!(err.contains(&h.codex().display().to_string()), "{err}");
        assert!(err.contains(&h.copilot().display().to_string()), "{err}");
        assert!(err.contains("delete the state file"), "{err}");
    }

    /// Makes `link` a directory link to `target`, or returns false.
    #[cfg(unix)]
    fn link_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    /// A directory symlink, or else a junction, which needs no privilege.
    #[cfg(windows)]
    fn link_dir(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
            || std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|out| out.status.success())
    }

    /// Makes `link` a file symlink to `target`, or returns false.
    #[cfg(unix)]
    fn link_file(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    /// Fails without Developer Mode or the symlink privilege.
    #[cfg(windows)]
    fn link_file(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    #[test]
    fn locate_names_a_link_between_the_two_homes() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.copilot(), "dedicated", true);
        if !link_dir(&h.copilot(), &h.codex()) {
            eprintln!("skipped: cannot link directories here");
            return;
        }
        let err = h.locate().unwrap_err().to_string();
        assert!(err.contains("same directory"), "{err}");
        assert!(err.contains("one is a link to the other"), "{err}");
        assert!(!err.contains("delete the state file"), "{err}");
        assert!(h.copilot().join(STATE_FILE).exists());
    }

    #[test]
    fn locate_refuses_a_link_before_anything_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        // Two separate directories without the state file: nothing installed.
        fs::create_dir_all(h.copilot()).unwrap();
        assert_eq!(h.locate().unwrap(), Located::None);

        // The same, but ~/.codex-copilot leads to ~/.codex: an install would
        // write its config into the regular Codex home.
        fs::remove_dir(h.copilot()).unwrap();
        if !link_dir(&h.codex(), &h.copilot()) {
            eprintln!("skipped: cannot link directories here");
            return;
        }
        let err = h.locate().unwrap_err().to_string();
        assert!(err.contains("one is a link to the other"), "{err}");
        assert!(!h.codex().join(STATE_FILE).exists());
    }

    #[test]
    fn atomic_write_replaces_what_a_symlink_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let home = dir.path().join("home");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&home).unwrap();
        let real = dotfiles.join("config.toml");
        fs::write(&real, "old").unwrap();
        // Relative, the way dotfile managers make them.
        let link = home.join("config.toml");
        let relative = Path::new("..").join("dotfiles").join("config.toml");
        if !link_file(&relative, &link) {
            eprintln!("skipped: cannot create symlinks here");
            return;
        }

        atomic_write(&link, "new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        // No temp file is left on either side.
        assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&dotfiles).unwrap().count(), 1);

        // A chain of links to a file that does not exist yet.
        let end = dotfiles.join(STATE_FILE);
        let middle = dir.path().join("middle.json");
        let first = home.join(STATE_FILE);
        assert!(link_file(&end, &middle) && link_file(&middle, &first));
        write_state(&home, &state()).unwrap();
        assert_eq!(read_state(&dotfiles).unwrap(), Some(state()));
        assert!(fs::symlink_metadata(&first).unwrap().is_symlink());
        assert!(fs::symlink_metadata(&middle).unwrap().is_symlink());
    }

    #[test]
    fn follow_links_leaves_plain_and_missing_paths_alone() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert_eq!(follow_links(&plain).unwrap(), plain);
        let missing = dir.path().join("missing").join("file");
        assert_eq!(follow_links(&missing).unwrap(), missing);
    }

    #[test]
    fn state_round_trips_and_may_be_absent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(COPILOT_HOME_NAME);
        assert_eq!(read_state(&home).unwrap(), None);

        write_state(&home, &state()).unwrap();
        assert_eq!(read_state(&home).unwrap(), Some(state()));
        let text = fs::read_to_string(home.join(STATE_FILE)).unwrap();
        assert!(text.contains("\n  \"version\": \"2.0.0\""), "{text}");

        // Overwriting goes through the same atomic path.
        let next = State {
            yolo: false,
            codex_version: None,
            ..state()
        };
        write_state(&home, &next).unwrap();
        assert_eq!(read_state(&home).unwrap(), Some(next));
    }

    #[test]
    fn a_corrupt_state_file_names_its_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(STATE_FILE), "{ not json").unwrap();
        let err = format!("{:#}", read_state(dir.path()).unwrap_err());
        assert!(err.contains(STATE_FILE), "{err}");
    }

    #[test]
    fn override_swaps_with_a_parked_codex_home_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);

        // Both directions park ~/.codex first.
        let rename =
            |from: PathBuf, to: PathBuf| format!("renamed {} -> {}", from.display(), to.display());
        let steps = vec![
            rename(h.codex(), h.swap_temp()),
            rename(h.copilot(), h.codex()),
            rename(h.swap_temp(), h.copilot()),
        ];

        let report = override_homes(&h).unwrap();
        assert_eq!(report.steps, steps);
        assert_eq!(h.locate().unwrap(), Located::Overridden);
        assert_eq!(marker(&h.codex()), "dedicated");
        assert_eq!(marker(&h.copilot()), "original");
        assert!(!h.swap_temp().exists());

        let report = unoverride_homes(&h).unwrap();
        assert_eq!(report.steps, steps);
        assert_eq!(h.locate().unwrap(), Located::Normal);
        assert_eq!(marker(&h.codex()), "original");
        assert_eq!(marker(&h.copilot()), "dedicated");
        assert!(!h.swap_temp().exists());
    }

    #[test]
    fn override_without_a_codex_home_is_one_rename() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.copilot(), "dedicated", true);

        let report = override_homes(&h).unwrap();
        assert_eq!(
            report.steps,
            vec![format!(
                "renamed {} -> {}",
                h.copilot().display(),
                h.codex().display()
            )]
        );
        assert_eq!(h.locate().unwrap(), Located::Overridden);
        assert_eq!(marker(&h.codex()), "dedicated");
        assert!(!h.copilot().exists());

        let report = unoverride_homes(&h).unwrap();
        assert_eq!(report.steps.len(), 1);
        assert_eq!(h.locate().unwrap(), Located::Normal);
        assert_eq!(marker(&h.copilot()), "dedicated");
        assert!(!h.codex().exists());
    }

    #[test]
    fn swaps_check_the_current_state() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        let err = override_homes(&h).unwrap_err().to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(err.contains("codex-copilot install"), "{err}");
        assert!(unoverride_homes(&h).is_err());

        make_dir(&h.copilot(), "dedicated", true);
        let err = unoverride_homes(&h).unwrap_err().to_string();
        assert!(err.contains("not overridden"), "{err}");

        override_homes(&h).unwrap();
        let err = override_homes(&h).unwrap_err().to_string();
        assert!(err.contains("already overridden"), "{err}");
    }

    #[test]
    fn a_leftover_swap_directory_blocks_both_swaps() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);
        make_dir(&h.swap_temp(), "leftover", false);

        let err = override_homes(&h).unwrap_err().to_string();
        assert!(err.contains(SWAP_TEMP_NAME), "{err}");
        // Nothing moved.
        assert_eq!(marker(&h.codex()), "original");
        assert_eq!(marker(&h.copilot()), "dedicated");
        assert_eq!(marker(&h.swap_temp()), "leftover");

        // Same refusal in the other direction.
        fs::remove_file(h.copilot().join(STATE_FILE)).unwrap();
        fs::write(h.codex().join(STATE_FILE), "{}").unwrap();
        assert_eq!(h.locate().unwrap(), Located::Overridden);
        let err = unoverride_homes(&h).unwrap_err().to_string();
        assert!(err.contains(SWAP_TEMP_NAME), "{err}");
        assert_eq!(marker(&h.swap_temp()), "leftover");
    }

    #[test]
    fn a_failed_rename_rolls_back_the_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        make_dir(&a, "a", false);
        let missing = dir.path().join("missing");
        let err = run_steps(&[
            Step::Rename(a.clone(), b.clone()),
            Step::Rename(missing, a.clone()),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(marker(&a), "a");
        assert!(!b.exists());
    }

    /// Runs a real swap's three renames with the chosen calls failing
    /// (numbered from 0, undos included). Returns the error and the calls.
    fn swap_failing(h: &Homes, fail: &[usize]) -> (String, Vec<Step>) {
        let steps = vec![
            Step::Rename(h.codex(), h.swap_temp()),
            Step::Rename(h.copilot(), h.codex()),
            Step::Rename(h.swap_temp(), h.copilot()),
        ];
        let mut calls = Vec::new();
        let err = run_steps_with(&steps, |step| {
            calls.push(step.clone());
            if fail.contains(&(calls.len() - 1)) {
                return Err(std::io::Error::other("in use"));
            }
            step.apply()
        })
        .unwrap_err()
        .to_string();
        (err, calls)
    }

    #[test]
    fn a_failed_undo_stops_the_rollback_and_says_what_is_in_effect() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);

        // The third rename fails, then undoing the second: the first must
        // not be undone, since ~/.codex is taken.
        let (err, calls) = swap_failing(&h, &[2, 3]);
        assert_eq!(calls.len(), 4, "{calls:?}");
        let in_effect = format!(
            "Still in effect: {}, {}. Undone: none.",
            renamed(&h.codex(), &h.swap_temp()),
            renamed(&h.copilot(), &h.codex())
        );
        assert!(err.contains(&in_effect), "{err}");
        assert!(err.contains("Fix the directories by hand"), "{err}");
        // The directories are where the message says.
        assert_eq!(marker(&h.swap_temp()), "original");
        assert_eq!(marker(&h.codex()), "dedicated");
        assert!(!h.copilot().exists());
    }

    #[test]
    fn a_rollback_reports_the_undos_that_worked() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);

        // Undoing the second rename works, undoing the first does not.
        let (err, calls) = swap_failing(&h, &[2, 4]);
        assert_eq!(calls.len(), 5, "{calls:?}");
        let in_effect = format!(
            "Still in effect: {}. Undone: {}.",
            renamed(&h.codex(), &h.swap_temp()),
            renamed(&h.copilot(), &h.codex())
        );
        assert!(err.contains(&in_effect), "{err}");
        assert_eq!(marker(&h.swap_temp()), "original");
        assert_eq!(marker(&h.copilot()), "dedicated");
        assert!(!h.codex().exists());
    }

    /// A standalone Codex install under `home`, laid out like `install.sh`
    /// leaves it: `packages/standalone/current` is an absolute link into
    /// `installed_for`'s `packages/` (normally `home` itself). Also links
    /// `bin/codex` at it, as `~/.local/bin/codex` is. Returns that link, or
    /// `None` where links cannot be made.
    fn standalone(root: &Path, home: &Path, installed_for: &Path, tag: &str) -> Option<PathBuf> {
        let release = home
            .join(PACKAGES_DIR)
            .join("standalone")
            .join("releases")
            .join("0.164.0");
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::write(release.join("bin").join("codex"), tag).unwrap();
        let target = installed_for
            .join(PACKAGES_DIR)
            .join("standalone")
            .join("releases")
            .join("0.164.0");
        let current = home.join(PACKAGES_DIR).join("standalone").join("current");
        if !link_dir(&target, &current) {
            return None;
        }
        let bin = root.join("bin").join(tag);
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let visible = installed_for
            .join(PACKAGES_DIR)
            .join("standalone")
            .join("current")
            .join("bin");
        link_dir(&visible, &bin).then_some(bin)
    }

    /// What the `codex` the link `bin` leads to says, if it resolves.
    fn runs(bin: &Path) -> Option<String> {
        fs::read_to_string(bin.join("codex")).ok()
    }

    #[test]
    fn the_codex_install_stays_in_place_through_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);
        let Some(bin) = standalone(dir.path(), &h.codex(), &h.codex(), "regular") else {
            eprintln!("skipped: cannot link directories here");
            return;
        };
        // An app-server daemon package as well, also under ~/.codex.
        let daemon = h.codex().join(PACKAGES_DIR).join("app-server-daemon");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(daemon.join("marker"), "daemon").unwrap();

        let report = override_homes(&h).unwrap();
        assert_eq!(marker(&h.codex()), "dedicated");
        assert_eq!(marker(&h.copilot()), "original");
        // The install did not move, so `codex` still runs.
        assert_eq!(runs(&bin).as_deref(), Some("regular"));
        assert_eq!(marker(&daemon), "daemon");
        assert!(!h.copilot().join(PACKAGES_DIR).exists());
        assert_eq!(
            report.steps[3..],
            [renamed(
                &h.copilot().join(PACKAGES_DIR),
                &h.codex().join(PACKAGES_DIR)
            ),]
        );
        assert!(
            report.notes[0].contains("stayed where it was"),
            "{report:?}"
        );

        unoverride_homes(&h).unwrap();
        assert_eq!(marker(&h.codex()), "original");
        assert_eq!(marker(&h.copilot()), "dedicated");
        assert_eq!(runs(&bin).as_deref(), Some("regular"));
        assert!(!h.copilot().join(PACKAGES_DIR).exists());
        assert!(!h.packages_swap_temp().exists());
    }

    #[test]
    fn two_codex_installs_each_stay_at_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "original", false);
        make_dir(&h.copilot(), "dedicated", true);
        // A codex that updated itself under CODEX_HOME=~/.codex-copilot
        // installs there and relinks the command into it.
        let (Some(regular), Some(dedicated)) = (
            standalone(dir.path(), &h.codex(), &h.codex(), "regular"),
            standalone(dir.path(), &h.copilot(), &h.copilot(), "dedicated"),
        ) else {
            eprintln!("skipped: cannot link directories here");
            return;
        };

        let report = override_homes(&h).unwrap();
        assert_eq!(report.steps.len(), 6, "{report:?}");
        assert!(
            report.notes[0].contains("stayed where they were"),
            "{report:?}"
        );
        assert_eq!(runs(&regular).as_deref(), Some("regular"));
        assert_eq!(runs(&dedicated).as_deref(), Some("dedicated"));
        assert_eq!(marker(&h.codex()), "dedicated");
        assert!(!h.packages_swap_temp().exists());

        unoverride_homes(&h).unwrap();
        assert_eq!(runs(&regular).as_deref(), Some("regular"));
        assert_eq!(runs(&dedicated).as_deref(), Some("dedicated"));
        assert_eq!(marker(&h.codex()), "original");
    }

    #[test]
    fn a_lone_home_leaves_its_codex_install_in_a_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.copilot(), "dedicated", true);
        override_homes(&h).unwrap();
        // Codex installed after the override, into the only home there is.
        let Some(bin) = standalone(dir.path(), &h.codex(), &h.codex(), "installed") else {
            eprintln!("skipped: cannot link directories here");
            return;
        };

        let report = unoverride_homes(&h).unwrap();
        assert_eq!(
            report.steps,
            [
                renamed(&h.codex(), &h.copilot()),
                format!("created {}", h.codex().display()),
                renamed(
                    &h.copilot().join(PACKAGES_DIR),
                    &h.codex().join(PACKAGES_DIR)
                ),
            ]
        );
        assert_eq!(h.locate().unwrap(), Located::Normal);
        assert_eq!(runs(&bin).as_deref(), Some("installed"));
        assert!(holds_only_packages(&h.codex()));
        assert!(!holds_only_packages(&h.copilot()));

        // And back: now a three-way swap, the install still in place.
        override_homes(&h).unwrap();
        assert_eq!(runs(&bin).as_deref(), Some("installed"));
        assert_eq!(marker(&h.codex()), "dedicated");
        assert!(holds_only_packages(&h.copilot()));
        assert_eq!(fs::read_dir(h.copilot()).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_packages_step_undoes_the_whole_swap() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "dedicated", true);
        fs::create_dir_all(h.codex().join(PACKAGES_DIR)).unwrap();
        let steps = packages_steps(&h, true, false).unwrap().0;
        let mut all = vec![Step::Rename(h.codex(), h.copilot())];
        all.extend(steps);
        // Created ~/.codex, then failed to move the install back into it.
        let err = run_steps_with(&all, |step| match step {
            Step::Rename(from, _) if from.ends_with(PACKAGES_DIR) => {
                Err(std::io::Error::other("in use"))
            }
            step => step.apply(),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(marker(&h.codex()), "dedicated");
        assert!(h.codex().join(PACKAGES_DIR).is_dir());
        assert!(!h.copilot().exists());
    }

    /// codex-copilot 2.0.1 moved `packages/` with its home. Its leftovers go
    /// back to where their links lead.
    #[test]
    fn an_install_an_older_swap_moved_goes_back_where_it_links() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        // Overridden by 2.0.1: the regular home, install included, is at
        // ~/.codex-copilot, while its links still name ~/.codex.
        make_dir(&h.codex(), "dedicated", true);
        make_dir(&h.copilot(), "original", false);
        let Some(bin) = standalone(dir.path(), &h.copilot(), &h.codex(), "regular") else {
            eprintln!("skipped: cannot link directories here");
            return;
        };
        assert_eq!(runs(&bin), None);

        let report = unoverride_homes(&h).unwrap();
        assert_eq!(report.steps.len(), 3, "{report:?}");
        assert!(
            report.notes[0].contains("went with its directory"),
            "{report:?}"
        );
        assert_eq!(runs(&bin).as_deref(), Some("regular"));
        assert_eq!(marker(&h.codex()), "original");

        // From here on it stays put.
        override_homes(&h).unwrap();
        assert_eq!(runs(&bin).as_deref(), Some("regular"));
    }

    /// Reinstalled by hand after a 2.0.1 override: the fresh install under
    /// ~/.codex works and stays; the stale copy is left where it is.
    #[test]
    fn a_working_install_wins_over_a_stale_copy() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.codex(), "dedicated", true);
        make_dir(&h.copilot(), "original", false);
        let (Some(fresh), Some(_)) = (
            standalone(dir.path(), &h.codex(), &h.codex(), "fresh"),
            standalone(dir.path(), &h.copilot(), &h.codex(), "stale"),
        ) else {
            eprintln!("skipped: cannot link directories here");
            return;
        };
        assert_eq!(runs(&fresh).as_deref(), Some("fresh"));

        unoverride_homes(&h).unwrap();
        assert_eq!(runs(&fresh).as_deref(), Some("fresh"));
        assert_eq!(marker(&h.codex()), "original");
    }

    #[test]
    fn a_leftover_packages_swap_directory_blocks_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let h = homes(dir.path());
        make_dir(&h.copilot(), "dedicated", true);
        make_dir(&h.packages_swap_temp(), "leftover", false);
        let err = override_homes(&h).unwrap_err().to_string();
        assert!(err.contains(PACKAGES_SWAP_TEMP_NAME), "{err}");
        assert_eq!(marker(&h.copilot()), "dedicated");
    }

    #[test]
    fn link_targets_are_compared_by_component() {
        let home = Path::new("/home/u/.codex/packages");
        assert!(leads_into(
            Path::new("/home/u/.codex/packages/standalone/releases/x"),
            home
        ));
        assert!(!leads_into(
            Path::new("/home/u/.codex-copilot/packages/standalone"),
            home
        ));
        assert!(!leads_into(Path::new("/home/u/.codex/packagesx"), home));
    }

    #[cfg(windows)]
    #[test]
    fn junction_targets_match_without_their_prefix_or_case() {
        let home = Path::new(r"C:\Users\u\.codex\packages");
        assert!(leads_into(
            Path::new(r"\\?\C:\Users\U\.Codex\packages\standalone\releases\x"),
            home
        ));
        assert!(!leads_into(
            Path::new(r"\\?\C:\Users\u\.codex-copilot\packages\standalone"),
            home
        ));
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_788_998_400), "2026-09-10T00:00:00Z");
        // A leap day, which the civil-from-days arithmetic has to get right.
        assert_eq!(iso8601(1_709_208_000), "2024-02-29T12:00:00Z");
        let now = now_rfc3339();
        assert_eq!(now.len(), "2026-10-07T00:00:00Z".len());
        assert!(now.ends_with('Z'));
    }
}
