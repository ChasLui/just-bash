use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse(String),
    FileSystem(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) | Self::FileSystem(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

/// Default permission bits for a newly created regular file (`0644`).
pub const DEFAULT_FILE_MODE: u32 = 0o644;
/// Default permission bits for a newly created directory (`0755`).
pub const DEFAULT_DIR_MODE: u32 = 0o755;

/// Logical start of the in-memory clock.
///
/// There is no wall clock in this runtime: time is a monotonic counter owned by
/// the filesystem so that `ls -t`, `find -newer` and `test -nt` orderings are
/// reproducible across runs and hosts.
pub const EPOCH_SECONDS: i64 = 1_700_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum FsEntry {
    File {
        contents: String,
        mode: u32,
        mtime: i64,
    },
    Directory {
        mode: u32,
        mtime: i64,
    },
}

/// What kind of entry a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// Metadata returned by [`InMemoryFs::metadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub kind: EntryKind,
    /// Byte length for a file; `0` for a directory.
    pub size: u64,
    /// Permission bits only (no file-type bits).
    pub mode: u32,
    /// Logical modification time in seconds since [`EPOCH_SECONDS`].
    pub mtime: i64,
}

impl Metadata {
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, EntryKind::File)
    }

    /// The `rwxr-xr-x`-style permission string, without a leading type character.
    pub fn mode_string(&self) -> String {
        let mut output = String::with_capacity(9);
        for (bit, ch) in [
            (0o400, 'r'),
            (0o200, 'w'),
            (0o100, 'x'),
            (0o040, 'r'),
            (0o020, 'w'),
            (0o010, 'x'),
            (0o004, 'r'),
            (0o002, 'w'),
            (0o001, 'x'),
        ] {
            output.push(if self.mode & bit != 0 { ch } else { '-' });
        }
        output
    }

    /// The leading `ls -l` type character: `d` for directories, `-` for files.
    pub fn type_char(&self) -> char {
        if self.is_dir() { 'd' } else { '-' }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryFs {
    entries: BTreeMap<String, FsEntry>,
    /// Monotonic logical clock; every mutation advances it by one second.
    clock: i64,
}

impl Default for InMemoryFs {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        for directory in ["/", "/tmp", "/home", "/home/user"] {
            entries.insert(
                directory.to_string(),
                FsEntry::Directory {
                    mode: DEFAULT_DIR_MODE,
                    mtime: EPOCH_SECONDS,
                },
            );
        }
        Self {
            entries,
            clock: EPOCH_SECONDS,
        }
    }
}

impl InMemoryFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_files(files: BTreeMap<String, String>) -> Result<Self, Error> {
        let mut fs = Self::new();
        for (path, contents) in files {
            fs.create_dir_all(&parent_dir(&path))?;
            fs.write_file(&path, &contents)?;
        }
        Ok(fs)
    }

    /// Current logical time (the modification time any next write would receive).
    pub fn now(&self) -> i64 {
        self.clock
    }

    /// Advance the logical clock and return the new stamp.
    ///
    /// Time never moves backwards, and each mutation gets a distinct stamp, so
    /// two writes in one script are strictly ordered.
    pub fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    pub fn read_file(&self, path: &str) -> Result<&str, Error> {
        match self.entries.get(path) {
            Some(FsEntry::File { contents, .. }) => Ok(contents),
            Some(FsEntry::Directory { .. }) => {
                Err(Error::FileSystem(format!("{path}: Is a directory")))
            }
            None => Err(Error::FileSystem(format!(
                "{path}: No such file or directory"
            ))),
        }
    }

    pub fn write_file(&mut self, path: &str, contents: &str) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        self.ensure_parent_dir(&normalized)?;
        let stamp = self.tick();
        // An overwrite keeps the existing permission bits (bash `>` does not
        // reset the mode); a new file gets the default mode.
        let mode = match self.entries.get(&normalized) {
            Some(FsEntry::Directory { .. }) => {
                return Err(Error::FileSystem(format!("{normalized}: Is a directory")));
            }
            Some(FsEntry::File { mode, .. }) => *mode,
            None => DEFAULT_FILE_MODE,
        };
        self.entries.insert(
            normalized,
            FsEntry::File {
                contents: contents.to_string(),
                mode,
                mtime: stamp,
            },
        );
        Ok(())
    }

    pub fn append_file(&mut self, path: &str, contents: &str) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        let mut next = match self.entries.get(&normalized) {
            Some(FsEntry::File { contents, .. }) => contents.clone(),
            Some(FsEntry::Directory { .. }) => {
                return Err(Error::FileSystem(format!("{normalized}: Is a directory")));
            }
            None => String::new(),
        };
        next.push_str(contents);
        self.write_file(&normalized, &next)
    }

    /// `touch`: create an empty file if missing, otherwise bump its mtime.
    pub fn touch(&mut self, path: &str) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        if self.is_dir(&normalized) {
            let stamp = self.tick();
            if let Some(FsEntry::Directory { mtime, .. }) = self.entries.get_mut(&normalized) {
                *mtime = stamp;
            }
            return Ok(());
        }
        if self.is_file(&normalized) {
            let stamp = self.tick();
            if let Some(FsEntry::File { mtime, .. }) = self.entries.get_mut(&normalized) {
                *mtime = stamp;
            }
            return Ok(());
        }
        self.write_file(&normalized, "")
    }

    /// Set an explicit modification time (`touch -t`), without advancing the clock.
    pub fn set_mtime(&mut self, path: &str, mtime: i64) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        match self.entries.get_mut(&normalized) {
            Some(FsEntry::File { mtime: slot, .. })
            | Some(FsEntry::Directory { mtime: slot, .. }) => {
                *slot = mtime;
                Ok(())
            }
            None => Err(Error::FileSystem(format!(
                "{normalized}: No such file or directory"
            ))),
        }
    }

    pub fn create_dir(&mut self, path: &str) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        if self.entries.contains_key(&normalized) {
            return Err(Error::FileSystem(format!("{normalized}: File exists")));
        }
        self.ensure_parent_dir(&normalized)?;
        let stamp = self.tick();
        self.entries.insert(
            normalized,
            FsEntry::Directory {
                mode: DEFAULT_DIR_MODE,
                mtime: stamp,
            },
        );
        Ok(())
    }

    pub fn create_dir_all(&mut self, path: &str) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        let mut current = String::from("/");
        for part in normalized.split('/').filter(|part| !part.is_empty()) {
            if current != "/" {
                current.push('/');
            }
            current.push_str(part);
            match self.entries.get(&current) {
                Some(FsEntry::File { .. }) => {
                    return Err(Error::FileSystem(format!("{current}: Not a directory")));
                }
                // Existing directories keep their mode and mtime.
                Some(FsEntry::Directory { .. }) => continue,
                None => {}
            }
            let stamp = self.tick();
            self.entries.insert(
                current.clone(),
                FsEntry::Directory {
                    mode: DEFAULT_DIR_MODE,
                    mtime: stamp,
                },
            );
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &str, recursive: bool) -> Result<(), Error> {
        let normalized = normalize_absolute(path);
        if normalized == "/" {
            return Err(Error::FileSystem("cannot remove '/'".to_string()));
        }
        match self.entries.get(&normalized) {
            None => {
                return Err(Error::FileSystem(format!(
                    "{normalized}: No such file or directory"
                )));
            }
            Some(FsEntry::Directory { .. }) if !recursive && self.has_children(&normalized) => {
                return Err(Error::FileSystem(format!(
                    "{normalized}: Directory not empty"
                )));
            }
            _ => {}
        }
        let prefix = format!("{normalized}/");
        let paths: Vec<String> = self
            .entries
            .keys()
            .filter(|candidate| *candidate == &normalized || candidate.starts_with(&prefix))
            .cloned()
            .collect();
        for candidate in paths {
            self.entries.remove(&candidate);
        }
        Ok(())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    pub fn is_dir(&self, path: &str) -> bool {
        matches!(self.entries.get(path), Some(FsEntry::Directory { .. }))
    }

    pub fn is_file(&self, path: &str) -> bool {
        matches!(self.entries.get(path), Some(FsEntry::File { .. }))
    }

    /// Metadata for a path, or a filesystem error when it does not exist.
    pub fn metadata(&self, path: &str) -> Result<Metadata, Error> {
        let normalized = normalize_absolute(path);
        match self.entries.get(&normalized) {
            Some(FsEntry::File {
                contents,
                mode,
                mtime,
            }) => Ok(Metadata {
                kind: EntryKind::File,
                size: contents.len() as u64,
                mode: *mode,
                mtime: *mtime,
            }),
            Some(FsEntry::Directory { mode, mtime }) => Ok(Metadata {
                kind: EntryKind::Directory,
                size: 0,
                mode: *mode,
                mtime: *mtime,
            }),
            None => Err(Error::FileSystem(format!(
                "{normalized}: No such file or directory"
            ))),
        }
    }

    /// `chmod`: replace the permission bits (values above `0o7777` are rejected).
    pub fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Error> {
        if mode > 0o7777 {
            return Err(Error::FileSystem(format!(
                "{path}: invalid mode '{mode:o}'"
            )));
        }
        let normalized = normalize_absolute(path);
        match self.entries.get_mut(&normalized) {
            Some(FsEntry::File { mode: slot, .. })
            | Some(FsEntry::Directory { mode: slot, .. }) => {
                *slot = mode;
                Ok(())
            }
            None => Err(Error::FileSystem(format!(
                "{normalized}: No such file or directory"
            ))),
        }
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, Error> {
        if !self.is_dir(path) {
            return Err(Error::FileSystem(format!("{path}: Not a directory")));
        }
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut names = BTreeSet::new();
        for candidate in self.entries.keys() {
            if candidate == path || !candidate.starts_with(&prefix) {
                continue;
            }
            let rest = &candidate[prefix.len()..];
            if let Some(name) = rest.split('/').next()
                && !name.is_empty()
            {
                names.insert(name.to_string());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Every entry in the filesystem as `(absolute_path, is_dir)`, sorted.
    ///
    /// Pathname expansion walks this list; there are no symlinks, so a path has
    /// exactly one kind.
    pub fn all_entries(&self) -> Vec<(String, bool)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.clone(), matches!(entry, FsEntry::Directory { .. })))
            .collect()
    }

    /// Returns all entries under `path` (inclusive of the dir itself) as
    /// `(absolute_path, Option<contents>)` where `None` means a directory.
    pub fn entries_under(&self, path: &str) -> Result<Vec<(String, Option<String>)>, Error> {
        let normalized = normalize_absolute(path);
        if !self.is_dir(&normalized) {
            return Err(Error::FileSystem(format!("{normalized}: Not a directory")));
        }
        let prefix = format!("{normalized}/");
        let mut result = vec![(normalized.clone(), None)];
        for (entry_path, entry) in &self.entries {
            if entry_path.starts_with(&prefix) {
                let contents = match entry {
                    FsEntry::File { contents, .. } => Some(contents.clone()),
                    FsEntry::Directory { .. } => None,
                };
                result.push((entry_path.clone(), contents));
            }
        }
        Ok(result)
    }

    /// Metadata for every entry under `path`, directories first, then names
    /// sorted, using the same ordering rules as [`list_dir`].
    pub fn metadata_of_children(&self, path: &str) -> Result<Vec<(String, Metadata)>, Error> {
        let mut result = Vec::new();
        for name in self.list_dir(path)? {
            let child = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            result.push((name, self.metadata(&child)?));
        }
        Ok(result)
    }

    fn ensure_parent_dir(&self, path: &str) -> Result<(), Error> {
        let parent = parent_dir(path);
        match self.entries.get(&parent) {
            Some(FsEntry::Directory { .. }) => Ok(()),
            Some(FsEntry::File { .. }) => {
                Err(Error::FileSystem(format!("{parent}: Not a directory")))
            }
            None => Err(Error::FileSystem(format!(
                "{parent}: No such file or directory"
            ))),
        }
    }

    fn has_children(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.entries
            .keys()
            .any(|candidate| candidate.starts_with(&prefix))
    }
}

pub fn normalize_absolute(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

pub fn parent_dir(path: &str) -> String {
    let normalized = normalize_absolute(path);
    if normalized == "/" {
        return "/".to_string();
    }
    match normalized.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => "/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_with(entries: &[(&str, &str)]) -> InMemoryFs {
        let mut fs = InMemoryFs::new();
        for (path, contents) in entries {
            fs.create_dir_all(&parent_dir(path)).unwrap();
            fs.write_file(path, contents).unwrap();
        }
        fs
    }

    #[test]
    fn files_start_with_default_mode_and_a_logical_mtime() {
        let fs = fs_with(&[("/a.txt", "hello")]);
        let meta = fs.metadata("/a.txt").unwrap();
        assert_eq!(meta.kind, EntryKind::File);
        assert_eq!(meta.size, 5);
        assert_eq!(meta.mode, DEFAULT_FILE_MODE);
        assert!(meta.mtime > EPOCH_SECONDS, "writes advance the clock");

        let dir = fs.metadata("/").unwrap();
        assert!(dir.is_dir());
        assert_eq!(dir.mode, DEFAULT_DIR_MODE);
    }

    #[test]
    fn writes_are_strictly_ordered_by_mtime() {
        let mut fs = InMemoryFs::new();
        fs.write_file("/first", "1").unwrap();
        let first = fs.metadata("/first").unwrap().mtime;
        fs.write_file("/second", "2").unwrap();
        let second = fs.metadata("/second").unwrap().mtime;
        assert!(second > first, "{second} should be after {first}");
    }

    #[test]
    fn overwrite_preserves_mode_but_updates_size_and_mtime() {
        let mut fs = fs_with(&[("/a", "hello")]);
        fs.set_mode("/a", 0o600).unwrap();
        let before = fs.metadata("/a").unwrap();
        fs.write_file("/a", "hi").unwrap();
        let after = fs.metadata("/a").unwrap();
        assert_eq!(after.mode, 0o600);
        assert_eq!(after.size, 2);
        assert!(after.mtime > before.mtime);
    }

    #[test]
    fn append_updates_mtime_and_size() {
        let mut fs = fs_with(&[("/a", "ab")]);
        let before = fs.metadata("/a").unwrap();
        fs.append_file("/a", "cd").unwrap();
        let after = fs.metadata("/a").unwrap();
        assert_eq!(after.size, 4);
        assert!(after.mtime > before.mtime);
    }

    #[test]
    fn touch_creates_then_stamps() {
        let mut fs = InMemoryFs::new();
        fs.touch("/tmp/new").unwrap();
        let created = fs.metadata("/tmp/new").unwrap();
        assert_eq!(created.size, 0);
        assert!(fs.is_file("/tmp/new"));
        fs.touch("/tmp/new").unwrap();
        assert!(fs.metadata("/tmp/new").unwrap().mtime > created.mtime);
    }

    #[test]
    fn set_mtime_is_exact_and_does_not_move_the_clock() {
        let mut fs = fs_with(&[("/a", "x")]);
        let before = fs.now();
        fs.set_mtime("/a", 42).unwrap();
        assert_eq!(fs.metadata("/a").unwrap().mtime, 42);
        // Setting an explicit time must not move the logical clock.
        assert_eq!(fs.now(), before);
        assert!(fs.set_mtime("/missing", 1).is_err());
    }

    #[test]
    fn chmod_replaces_bits_and_rejects_oversized_modes() {
        let mut fs = fs_with(&[("/a", "x")]);
        fs.set_mode("/a", 0o755).unwrap();
        let meta = fs.metadata("/a").unwrap();
        assert_eq!(meta.mode, 0o755);
        assert_eq!(meta.mode_string(), "rwxr-xr-x");
        assert_eq!(meta.type_char(), '-');
        assert!(fs.set_mode("/a", 0o10000).is_err());
        assert!(fs.set_mode("/missing", 0o644).is_err());
    }

    #[test]
    fn metadata_reports_directories_and_missing_paths() {
        let fs = InMemoryFs::new();
        let meta = fs.metadata("/tmp").unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.type_char(), 'd');
        assert!(fs.metadata("/nope").is_err());
    }

    #[test]
    fn all_entries_lists_everything_with_kinds() {
        let fs = fs_with(&[("/home/user/a", "1"), ("/tmp/b", "2")]);
        let entries = fs.all_entries();
        assert!(entries.contains(&("/home/user/a".to_string(), false)));
        assert!(entries.contains(&("/tmp/b".to_string(), false)));
        assert!(entries.contains(&("/tmp".to_string(), true)));
        // Sorted by path.
        let mut sorted = entries.clone();
        sorted.sort();
        assert_eq!(entries, sorted);
    }

    #[test]
    fn children_metadata_is_name_sorted() {
        let fs = fs_with(&[("/tmp/b", "2"), ("/tmp/a", "1")]);
        let children = fs.metadata_of_children("/tmp").unwrap();
        let names: Vec<&str> = children.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn create_dir_all_keeps_existing_directory_stamps() {
        let mut fs = InMemoryFs::new();
        let before = fs.metadata("/tmp").unwrap();
        fs.create_dir_all("/tmp/one/two").unwrap();
        assert_eq!(fs.metadata("/tmp").unwrap().mtime, before.mtime);
        assert!(fs.is_dir("/tmp/one/two"));
    }

    #[test]
    fn glob_walk_sees_nested_entries() {
        let fs = fs_with(&[("/home/user/src/main.rs", "fn main(){}")]);
        let entries = fs.all_entries();
        assert!(
            entries
                .iter()
                .any(|(path, is_dir)| path == "/home/user/src/main.rs" && !is_dir)
        );
        assert!(
            entries
                .iter()
                .any(|(path, is_dir)| path == "/home/user/src" && *is_dir)
        );
    }
}
