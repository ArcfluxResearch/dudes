//! `dudes project add|remove`: edit `[floor.onboarding]` in the operator's own file, in place.
//!
//! The file is the operator's, hand-written, so an edit keeps every other byte and comment as it
//! was: it goes through `toml_edit`, into a temporary file beside the original, and is renamed
//! over it. Which change the daemon then adopts is `floor.enrollment.reload`'s business,
//! and `crate::run` makes that call.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use dudes_proto::record::Hex32;
use dudes_proto::rpc::floor::{CheckView, ChecksView, MAX_CHECKS_BYTES, WorkspaceChecks};
use dudes_proto::rpc::goal::{CompletionCheckDeclaration, ENROLLED_CHECK_ID};
use sha2::{Digest as _, Sha256};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

/// One workspace as `dudes project add` writes it. The label defaults to the id, and every
/// ceiling is optional; every other value is the operator's policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorkspace {
    /// The repository directory, exactly as typed.
    pub path: PathBuf,
    /// `workspace_id`.
    pub id: String,
    /// `label`; the id when not given.
    pub label: String,
    /// `completion_check_label`.
    pub completion_check_label: String,
    /// `department`.
    pub department: String,
    /// `agent_usd_per_day`, as the decimal text typed.
    pub agent_usd_per_day: Option<String>,
    /// `goal_usd`, as the decimal text typed.
    pub goal_usd: Option<String>,
    /// `hop_cap`.
    pub hop_cap: u32,
    /// `predicate_argv`.
    pub predicate_argv: Vec<String>,
    /// `predicate_timeout_s`.
    pub predicate_timeout_s: u64,
    /// `tokens_per_day`.
    pub tokens_per_day: Option<u64>,
    /// `goal_tokens`.
    pub goal_tokens: Option<u64>,
    /// `may_already_pass`: the check can pass before any work, as a project's own tests do.
    pub may_already_pass: bool,
}

impl NewWorkspace {
    /// A project with the settings `dudes project add` gives one unless told otherwise: the
    /// default department, turn cap and check timeout, and no ceilings. A check that only asks
    /// whether files changed may pass before any work, as it does on a clean tree.
    #[must_use]
    pub fn with_defaults(path: PathBuf, id: String, label: String, argv: Vec<String>) -> Self {
        Self {
            path,
            id,
            label,
            completion_check_label: crate::check::check_label(&argv),
            department: crate::check::DEFAULT_DEPARTMENT.to_owned(),
            agent_usd_per_day: None,
            goal_usd: None,
            hop_cap: crate::check::DEFAULT_HOP_CAP,
            may_already_pass: argv == crate::check::changed_check(),
            predicate_argv: argv,
            predicate_timeout_s: crate::check::DEFAULT_PREDICATE_TIMEOUT_S,
            tokens_per_day: None,
            goal_tokens: None,
        }
    }
}

/// One more completion check for an enrolled workspace, as `dudes project check add` writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewCheck {
    /// The workspace it is enrolled for.
    pub workspace: String,
    /// `id`, unique within the workspace.
    pub id: String,
    /// `label`.
    pub label: String,
    /// `argv`.
    pub argv: Vec<String>,
    /// `timeout_s`.
    pub timeout_s: u64,
    /// `may_already_pass`.
    pub may_already_pass: bool,
}

/// Why an edit was refused. The file is unchanged in every case.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// No current directory was available for the default path.
    #[error(
        "The current directory cannot be read. Pass the project folder to `dudes project add`."
    )]
    NoCurrentDirectory,
    /// `add` was given a relative path.
    #[error("{0} is not an absolute path")]
    RelativePath(PathBuf),
    /// `add` was given a path that does not exist.
    #[error("{0} does not exist")]
    Missing(PathBuf),
    /// `add` was given a path that is not a directory.
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    /// `add` was given a path that goes through a symlink.
    #[error("{typed} goes through a symlink to {canonical}; add the canonical path")]
    NotCanonical {
        /// What the operator typed.
        typed: PathBuf,
        /// Where it leads.
        canonical: PathBuf,
    },
    /// `add` was given a directory that is not a repository.
    #[error("{0} is not a git repository. Run `dudes project add` in a folder that holds .git.")]
    NotARepository(PathBuf),
    /// A check whose program is a file inside the repository the agent works in.
    #[error(
        "The check program `{program}` is inside the project's repository. An agent working \
         there could change it, so it cannot be the judge of the agent's work. Use a program \
         installed outside the repository, such as the project's usual test command. To use a \
         script from the repository, run it through its interpreter, for example \
         `sh {example}` for a shell script. The agent can still edit that script, so review its \
         changes before you accept the work.",
        example = quoted_word(.program)
    )]
    CheckInsideRepository {
        /// The program as typed.
        program: String,
    },
    /// A path TOML cannot carry.
    #[error("{0} is not valid UTF-8")]
    NotUtf8(PathBuf),
    /// An id outside 1–128 bytes of `[A-Za-z0-9_-]`.
    #[error("`{0}` is not a project id: use 1 to 128 of A-Z, a-z, 0-9, _ and -")]
    InvalidId(String),
    /// `add` was given an id the file already holds.
    #[error("a project named `{0}` is already added")]
    DuplicateId(String),
    /// `remove` was given an id the file does not hold.
    #[error("no project is named `{0}`")]
    UnknownId(String),
    /// A check id outside 1 to 64 bytes of `[a-z][a-z0-9_-]*`.
    #[error("`{0}` is not a check id: use 1 to 64 of a-z, 0-9, _ and -, starting with a letter")]
    InvalidCheckId(String),
    /// A check label that names nothing or carries a control character.
    #[error("a check label is 1 to 128 bytes with no control characters")]
    InvalidCheckLabel,
    /// `check add` was given an id its workspace already uses.
    #[error("project `{workspace}` already has a check `{id}`")]
    DuplicateCheck {
        /// The workspace.
        workspace: String,
        /// The check id.
        id: String,
    },
    /// `check remove` was given an id the workspace's `checks` table does not hold.
    #[error("project `{workspace}` has no check `{id}` in its checks table")]
    UnknownCheck {
        /// The workspace.
        workspace: String,
        /// The check id.
        id: String,
    },
    /// The Floor's list of every enrolled check would pass its cap.
    #[error(
        "the Floor's list of enrolled checks would exceed its {cap} byte cap; shorten labels or \
         remove a check first"
    )]
    ChecksTooLarge {
        /// The cap.
        cap: usize,
    },
    /// The file is not TOML, or its workspaces are not a list of `[[floor.onboarding.workspaces]]`.
    #[error("{0} does not hold a readable [floor.onboarding] list")]
    Unreadable(PathBuf),
    /// Reading or writing the file failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
}

/// One edit on disk: the bytes before it, absent for a file that did not exist, and after it.
#[derive(Debug)]
pub struct Edit {
    /// What to restore when the daemon declines.
    pub previous: Option<Vec<u8>>,
    /// Exactly what was written.
    pub written: Vec<u8>,
}

/// `word` quoted when it holds whitespace or a quote, so the example in a refusal can be pasted.
fn quoted_word(word: &str) -> String {
    if word.contains(|c: char| c.is_whitespace() || c == '\'') {
        format!("'{}'", word.replace('\'', r"'\''"))
    } else {
        word.to_owned()
    }
}

impl Edit {
    /// The SHA-256 of the written bytes, as the daemon reports it.
    #[must_use]
    pub fn sha256(&self) -> String {
        sha256_hex(&self.written)
    }
}

/// A SHA-256 as 64 lowercase hex digits.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Check `new`, then append it to the file as one `[[floor.onboarding.workspaces]]` entry.
///
/// # Errors
///
/// The first refusal, in a fixed order, before anything is written.
pub fn add(config: &Path, new: &NewWorkspace) -> Result<Edit, WorkspaceError> {
    let previous = read(config)?;
    let written = appended(config, previous.as_deref(), new)?;
    write(config, previous, written)
}

/// [`add`]'s checks and edit applied to the file's bytes `previous`, which the caller read; the
/// bytes to write, with nothing written here.
///
/// # Errors
///
/// The first refusal, in a fixed order.
pub fn appended(
    config: &Path,
    previous: Option<&[u8]>,
    new: &NewWorkspace,
) -> Result<Vec<u8>, WorkspaceError> {
    let cwd = checked(new)?;
    let mut document = parse(config, previous)?;
    push(config, &mut document, new, &cwd)?;
    Ok(document.to_string().into_bytes())
}

/// [`add`]'s checks and entry, into a document the caller writes.
pub fn append(
    config: &Path,
    document: &mut DocumentMut,
    new: &NewWorkspace,
) -> Result<(), WorkspaceError> {
    let cwd = checked(new)?;
    push(config, document, new, &cwd)
}

/// The id of the entry whose `cwd` is `cwd`, if the document holds one.
pub fn id_at(document: &DocumentMut, cwd: &Path) -> Option<String> {
    let cwd = cwd.to_str()?;
    document
        .get("floor")?
        .get("onboarding")?
        .get("workspaces")?
        .as_array_of_tables()?
        .iter()
        .find(|entry| entry.get("cwd").and_then(Item::as_str) == Some(cwd))?
        .get("workspace_id")?
        .as_str()
        .map(str::to_owned)
}

/// The canonical repository `new` names, once its id is valid.
fn checked(new: &NewWorkspace) -> Result<PathBuf, WorkspaceError> {
    let cwd = canonical_repository(&new.path)?;
    if !valid_id(&new.id) {
        return Err(WorkspaceError::InvalidId(new.id.clone()));
    }
    outside_repository(&cwd, &new.predicate_argv)?;
    Ok(cwd)
}

fn push(
    config: &Path,
    document: &mut DocumentMut,
    new: &NewWorkspace,
    cwd: &Path,
) -> Result<(), WorkspaceError> {
    let list = workspaces(config, document)?;
    if position(list, &new.id).is_some() {
        return Err(WorkspaceError::DuplicateId(new.id.clone()));
    }
    let cwd = cwd
        .to_str()
        .ok_or_else(|| WorkspaceError::NotUtf8(cwd.to_path_buf()))?;
    let mut entry = Table::new();
    entry["workspace_id"] = value(&new.id);
    entry["label"] = value(&new.label);
    entry["completion_check_label"] = value(&new.completion_check_label);
    entry["cwd"] = value(cwd);
    entry["department"] = value(&new.department);
    entry["isolation"] = value("default");
    if let Some(usd) = &new.agent_usd_per_day {
        entry["agent_usd_per_day"] = decimal(usd);
    }
    if let Some(usd) = &new.goal_usd {
        entry["goal_usd"] = decimal(usd);
    }
    entry["hop_cap"] = value(i64::from(new.hop_cap));
    entry["predicate_argv"] = value(new.predicate_argv.iter().collect::<Array>());
    entry["predicate_timeout_s"] = integer(new.predicate_timeout_s);
    if let Some(tokens) = new.tokens_per_day {
        entry["tokens_per_day"] = integer(tokens);
    }
    if let Some(tokens) = new.goal_tokens {
        entry["goal_tokens"] = integer(tokens);
    }
    if new.may_already_pass {
        entry["may_already_pass"] = value(true);
    }
    list.push(entry);
    Ok(())
}

/// Refuse a check whose program has a path separator and resolves inside `repo`.
///
/// This mirrors the daemon's `resolve` and `spec_for` in `bins/duded/src/predicate.rs`: a relative
/// program with a separator resolves against the working folder, an absolute one as written, both
/// canonicalized, and one inside the canonical tree is refused. A bare name is left to the daemon,
/// which alone owns the `PATH` lookup, and so is a program that does not resolve to a file.
///
/// # Errors
///
/// [`WorkspaceError::CheckInsideRepository`] when the program resolves inside `repo`.
pub fn outside_repository(repo: &Path, argv: &[String]) -> Result<(), WorkspaceError> {
    let Some(program) = argv.first() else {
        return Ok(());
    };
    let candidate = Path::new(program);
    let found = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if candidate.components().count() > 1 {
        repo.join(candidate)
    } else {
        return Ok(());
    };
    let (Ok(program_path), Ok(tree)) = (found.canonicalize(), repo.canonicalize()) else {
        return Ok(());
    };
    if program_path.starts_with(tree) {
        return Err(WorkspaceError::CheckInsideRepository {
            program: program.clone(),
        });
    }
    Ok(())
}

/// Append one row to the workspace's `[[floor.onboarding.workspaces.checks]]` table.
///
/// # Errors
///
/// An unknown workspace, an invalid or duplicate id, an invalid label, a list of checks the Floor
/// could not carry, or a file that cannot be read or written. Nothing is written then.
pub fn add_check(config: &Path, new: &NewCheck) -> Result<Edit, WorkspaceError> {
    if !CompletionCheckDeclaration::valid_id(&new.id) {
        return Err(WorkspaceError::InvalidCheckId(new.id.clone()));
    }
    if new.label.len() > 128
        || new.label.chars().any(char::is_control)
        || CompletionCheckDeclaration::new(&new.id, &new.label).is_err()
    {
        return Err(WorkspaceError::InvalidCheckLabel);
    }
    let previous = read(config)?;
    let mut document = parse(config, previous.as_deref())?;
    let list = workspaces(config, &mut document)?;
    let index = position(list, &new.workspace)
        .ok_or_else(|| WorkspaceError::UnknownId(new.workspace.clone()))?;
    let entry = list.get_mut(index).expect("a position in the list");
    if let Some(repo) = entry.get("cwd").and_then(Item::as_str) {
        outside_repository(Path::new(repo), &new.argv)?;
    }
    if check_ids(entry).any(|id| id == new.id) {
        return Err(WorkspaceError::DuplicateCheck {
            workspace: new.workspace.clone(),
            id: new.id.clone(),
        });
    }
    let mut row = Table::new();
    row["id"] = value(&new.id);
    row["label"] = value(&new.label);
    row["argv"] = value(new.argv.iter().collect::<Array>());
    row["timeout_s"] = integer(new.timeout_s);
    if new.may_already_pass {
        row["may_already_pass"] = value(true);
    }
    entry
        .entry("checks")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| WorkspaceError::Unreadable(config.to_path_buf()))?
        .push(row);
    if !checks_view(workspaces(config, &mut document)?).fits() {
        return Err(WorkspaceError::ChecksTooLarge {
            cap: MAX_CHECKS_BYTES,
        });
    }
    write(config, previous, document.to_string().into_bytes())
}

/// Delete one row of the workspace's `checks` table. The workspace's own check, in its entry's
/// keys, changes only through `dudes project add`.
///
/// # Errors
///
/// An unknown workspace or check, or a file that cannot be read or written.
pub fn remove_check(config: &Path, workspace: &str, id: &str) -> Result<Edit, WorkspaceError> {
    let previous = read(config)?;
    let mut document = parse(config, previous.as_deref())?;
    let list = workspaces(config, &mut document)?;
    let index =
        position(list, workspace).ok_or_else(|| WorkspaceError::UnknownId(workspace.to_owned()))?;
    let unknown = || WorkspaceError::UnknownCheck {
        workspace: workspace.to_owned(),
        id: id.to_owned(),
    };
    let rows = list
        .get_mut(index)
        .expect("a position in the list")
        .get_mut("checks")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(unknown)?;
    let row = rows
        .iter()
        .position(|row| row.get("id").and_then(Item::as_str) == Some(id))
        .ok_or_else(unknown)?;
    rows.remove(row);
    write(config, previous, document.to_string().into_bytes())
}

/// Every check id a workspace entry uses: its own check's, then its table's.
fn check_ids(entry: &Table) -> impl Iterator<Item = &str> {
    let own = entry
        .get("completion_check_id")
        .and_then(Item::as_str)
        .unwrap_or(ENROLLED_CHECK_ID);
    std::iter::once(own).chain(
        entry
            .get("checks")
            .and_then(Item::as_array_of_tables)
            .into_iter()
            .flat_map(ArrayOfTables::iter)
            .filter_map(|row| row.get("id").and_then(Item::as_str)),
    )
}

/// The Floor's list of checks the file would enroll, as the daemon serializes it. Revisions are
/// fixed-width digests, so a placeholder sizes the list exactly.
fn checks_view(list: &ArrayOfTables) -> ChecksView {
    let check = |id: &str, label: Option<&str>, may_already_pass: Option<bool>| CheckView {
        id: id.to_owned(),
        label: label.unwrap_or_default().trim().to_owned(),
        revision: Hex32::new([0; 32]),
        may_already_pass: may_already_pass.unwrap_or(false),
    };
    let mut workspaces: Vec<WorkspaceChecks> = list
        .iter()
        .map(|entry| WorkspaceChecks {
            workspace_id: entry
                .get("workspace_id")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_owned(),
            checks: std::iter::once(check(
                entry
                    .get("completion_check_id")
                    .and_then(Item::as_str)
                    .unwrap_or(ENROLLED_CHECK_ID),
                entry.get("completion_check_label").and_then(Item::as_str),
                entry.get("may_already_pass").and_then(Item::as_bool),
            ))
            .chain(
                entry
                    .get("checks")
                    .and_then(Item::as_array_of_tables)
                    .into_iter()
                    .flat_map(ArrayOfTables::iter)
                    .map(|row| {
                        check(
                            row.get("id").and_then(Item::as_str).unwrap_or_default(),
                            row.get("label").and_then(Item::as_str),
                            row.get("may_already_pass").and_then(Item::as_bool),
                        )
                    }),
            )
            .collect(),
        })
        .collect();
    workspaces.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
    ChecksView { workspaces }
}

/// Delete the one entry `id` names. No folder is touched.
///
/// # Errors
///
/// An id the file does not hold, or a file that cannot be read or written.
pub fn remove(config: &Path, id: &str) -> Result<Edit, WorkspaceError> {
    let previous = read(config)?;
    let written = removed(config, previous.as_deref(), id)?;
    write(config, previous, written)
}

/// [`remove`]'s edit applied to the file's bytes `previous`, which the caller read; the bytes to
/// write, with nothing written here.
///
/// # Errors
///
/// An id the bytes do not hold, or bytes that cannot be read as a config.
pub fn removed(
    config: &Path,
    previous: Option<&[u8]>,
    id: &str,
) -> Result<Vec<u8>, WorkspaceError> {
    let mut document = parse(config, previous)?;
    let list = workspaces(config, &mut document)?;
    let index = position(list, id).ok_or_else(|| WorkspaceError::UnknownId(id.to_owned()))?;
    list.remove(index);
    Ok(document.to_string().into_bytes())
}

/// The ids the file enrolls, in file order.
///
/// # Errors
///
/// A file that cannot be read, or whose list cannot be read.
pub fn enrolled(config: &Path) -> Result<Vec<String>, WorkspaceError> {
    enrolled_in(config, read(config)?.as_deref())
}

/// [`enrolled`] for the file's bytes `bytes`, which the caller read.
///
/// # Errors
///
/// Bytes whose list cannot be read.
pub fn enrolled_in(config: &Path, bytes: Option<&[u8]>) -> Result<Vec<String>, WorkspaceError> {
    let mut document = parse(config, bytes)?;
    let list = workspaces(config, &mut document)?;
    Ok(list
        .iter()
        .filter_map(|entry| entry.get("workspace_id")?.as_str().map(str::to_owned))
        .collect())
}

/// The directory the enrolled project `id` points at.
///
/// # Errors
///
/// [`WorkspaceError`] when the file cannot be read or holds no such project.
pub fn directory(config: &Path, id: &str) -> Result<PathBuf, WorkspaceError> {
    let bytes = read(config)?;
    let mut document = parse(config, bytes.as_deref())?;
    let list = workspaces(config, &mut document)?;
    let index = position(list, id).ok_or_else(|| WorkspaceError::UnknownId(id.to_owned()))?;
    list.get(index)
        .and_then(|entry| entry.get("cwd"))
        .and_then(Item::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| WorkspaceError::Unreadable(config.to_path_buf()))
}

/// Put the file back as it was before an edit the daemon did not adopt.
///
/// # Errors
///
/// The restoring write or removal failed.
pub fn restore(config: &Path, previous: Option<&[u8]>) -> Result<(), WorkspaceError> {
    let io = |source| WorkspaceError::Io {
        path: config.to_path_buf(),
        source,
    };
    match previous {
        Some(bytes) => replace(config, bytes).map_err(io),
        None => fs::remove_file(config).map_err(io),
    }
}

/// The canonical form of an existing repository directory the operator typed canonically.
pub fn canonical_repository(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if !path.is_absolute() {
        return Err(WorkspaceError::RelativePath(path.to_path_buf()));
    }
    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            WorkspaceError::Missing(path.to_path_buf())
        } else {
            WorkspaceError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !metadata.is_dir() {
        return Err(WorkspaceError::NotADirectory(path.to_path_buf()));
    }
    let canonical = path.canonicalize().map_err(|source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if canonical != path {
        return Err(WorkspaceError::NotCanonical {
            typed: path.to_path_buf(),
            canonical,
        });
    }
    if !canonical.join(".git").exists() {
        return Err(WorkspaceError::NotARepository(canonical));
    }
    Ok(canonical)
}

/// The id a project at `path` gets: its folder name as a slug, suffixed to be unique among `ids`.
#[must_use]
pub fn project_id(path: &Path, ids: &[String]) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(118)
        .collect();
    let base = slug.trim_matches('-');
    let base = if base.is_empty() { "project" } else { base };
    let mut id = base.to_owned();
    let mut suffix = 2_u32;
    while ids.contains(&id) {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

/// The workspace id: 1–128 bytes of `[A-Za-z0-9_-]`.
fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn read(config: &Path) -> Result<Option<Vec<u8>>, WorkspaceError> {
    read_bytes(config).map_err(|source| WorkspaceError::Io {
        path: config.to_path_buf(),
        source,
    })
}

/// The operator file's bytes; `None` when it does not exist.
pub fn read_bytes(config: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(config) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

fn parse(config: &Path, bytes: Option<&[u8]>) -> Result<DocumentMut, WorkspaceError> {
    let unreadable = || WorkspaceError::Unreadable(config.to_path_buf());
    let text = std::str::from_utf8(bytes.unwrap_or_default()).map_err(|_| unreadable())?;
    text.parse().map_err(|_| unreadable())
}

/// `floor.onboarding.workspaces`, created empty when absent.
fn workspaces<'d>(
    config: &Path,
    document: &'d mut DocumentMut,
) -> Result<&'d mut ArrayOfTables, WorkspaceError> {
    let unreadable = || WorkspaceError::Unreadable(config.to_path_buf());
    let mut table = document.as_table_mut();
    for key in ["floor", "onboarding"] {
        table = table
            .entry(key)
            .or_insert_with(|| {
                let mut implicit = Table::new();
                implicit.set_implicit(true);
                Item::Table(implicit)
            })
            .as_table_mut()
            .ok_or_else(unreadable)?;
    }
    table
        .entry("workspaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(unreadable)
}

fn position(list: &ArrayOfTables, id: &str) -> Option<usize> {
    list.iter()
        .position(|entry| entry.get("workspace_id").and_then(Item::as_str) == Some(id))
}

/// A dollar amount written exactly as typed, which the parser has checked is a decimal number.
fn decimal(text: &str) -> Item {
    text.parse::<toml_edit::Value>()
        .map_or_else(|_| value(text), Item::Value)
}

fn integer(number: u64) -> Item {
    i64::try_from(number).map_or_else(|_| value(number.to_string()), value)
}

fn write(
    config: &Path,
    previous: Option<Vec<u8>>,
    written: Vec<u8>,
) -> Result<Edit, WorkspaceError> {
    replace(config, &written).map_err(|source| WorkspaceError::Io {
        path: config.to_path_buf(),
        source,
    })?;
    Ok(Edit { previous, written })
}

/// The temporary file, created readable by its owner only on unix, so a new operator file's
/// content is never written to a wider file.
fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Write `bytes` to a temporary file beside the operator file, or beside its symlink's target, and
/// rename it over the original, keeping the original's permissions.
pub fn replace(config: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = match fs::symlink_metadata(config) {
        Ok(metadata) if metadata.file_type().is_symlink() => config.canonicalize()?,
        Ok(_) => config.to_path_buf(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => config.to_path_buf(),
        Err(error) => return Err(error),
    };
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = target
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy().into_owned());
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    // `create_new` refuses a name that already exists, a planted symlink included.
    let mut file = create_private(&temporary)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| match fs::metadata(&target) {
            Ok(metadata) => file.set_permissions(metadata.permissions()),
            Err(_) => Ok(()),
        })
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| fs::rename(&temporary, &target)) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}
