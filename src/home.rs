//! The dedicated Codex home, its state file, and the `override` swap.
//!
//! The dedicated home is a second CODEX_HOME next to `~/.codex`. It carries
//! `codex-copilot.json`, and that file moves with the directory when
//! `override` swaps the two directory names, so "which directory holds the
//! state file" is the only record of whether an override is active. There is
//! deliberately no marker or transaction file that could disagree with it.

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
/// them exists (no regular Codex home to park), it is a single rename.
fn swap(homes: &Homes) -> Result<SwapReport> {
    let (codex, copilot, temp) = (homes.codex(), homes.copilot(), homes.swap_temp());
    if present(&temp)? {
        bail!(
            "{} exists, probably left by an interrupted override or unoverride; check which \
             directory is which, rename it by hand, then retry",
            temp.display()
        );
    }
    let renames = match (present(&codex)?, present(&copilot)?) {
        (true, true) => vec![
            (codex.clone(), temp.clone()),
            (copilot.clone(), codex),
            (temp, copilot),
        ],
        (true, false) => vec![(codex, copilot)],
        (false, true) => vec![(copilot, codex)],
        // `locate` just found the state file in one of them.
        (false, false) => bail!(
            "neither {} nor {} exists",
            codex.display(),
            copilot.display()
        ),
    };
    run_renames(&renames)
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

/// Performs the renames in order. When one fails, the ones already done are
/// undone in reverse, so a failed swap (typically Windows refusing to rename
/// a directory with an open file) leaves both directories where they were.
fn run_renames(renames: &[(PathBuf, PathBuf)]) -> Result<SwapReport> {
    run_renames_with(renames, |from, to| fs::rename(from, to))
}

/// [`run_renames`] over any rename function, so tests can fail chosen calls.
fn run_renames_with(
    renames: &[(PathBuf, PathBuf)],
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<SwapReport> {
    let mut steps = Vec::new();
    for (done, (from, to)) in renames.iter().enumerate() {
        if let Err(err) = rename(from, to) {
            let failed = format!(
                "could not rename {} -> {}: {err}",
                from.display(),
                to.display()
            );
            return Err(roll_back(&renames[..done], &failed, &mut rename));
        }
        steps.push(renamed(from, to));
    }
    Ok(SwapReport { steps })
}

/// Undoes `done`, newest first, and stops at the first undo that fails: an
/// earlier step's name is only free again once the later steps are undone,
/// so carrying on could move a directory onto the wrong name. The error says
/// exactly which renames are still in effect.
fn roll_back(
    done: &[(PathBuf, PathBuf)],
    failed: &str,
    rename: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> anyhow::Error {
    let mut undone = Vec::new();
    for (index, (from, to)) in done.iter().enumerate().rev() {
        if let Err(err) = rename(to, from) {
            let in_effect: Vec<String> = done[..=index]
                .iter()
                .map(|(from, to)| renamed(from, to))
                .collect();
            return anyhow!(
                "{failed}; undoing it failed as well (could not move {} back to {}: {err}), so \
                 the rollback stopped there. Fix the directories by hand. Still in effect: {}. \
                 Undone: {}.",
                to.display(),
                from.display(),
                in_effect.join(", "),
                if undone.is_empty() {
                    "none".to_owned()
                } else {
                    undone.join(", ")
                }
            );
        }
        undone.push(renamed(from, to));
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
        let err = run_renames(&[(a.clone(), b.clone()), (missing, a.clone())])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing was changed"), "{err}");
        assert_eq!(marker(&a), "a");
        assert!(!b.exists());
    }

    /// Runs a real swap's three renames with the chosen calls failing
    /// (numbered from 0, undos included). Returns the error and the calls.
    fn swap_failing(h: &Homes, fail: &[usize]) -> (String, Vec<(PathBuf, PathBuf)>) {
        let renames = vec![
            (h.codex(), h.swap_temp()),
            (h.copilot(), h.codex()),
            (h.swap_temp(), h.copilot()),
        ];
        let mut calls = Vec::new();
        let err = run_renames_with(&renames, |from, to| {
            calls.push((from.to_path_buf(), to.to_path_buf()));
            if fail.contains(&(calls.len() - 1)) {
                return Err(std::io::Error::other("in use"));
            }
            fs::rename(from, to)
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
