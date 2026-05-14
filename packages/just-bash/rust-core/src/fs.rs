use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashError {
    Parse(String),
    FileSystem(String),
}

impl fmt::Display for BashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) | Self::FileSystem(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for BashError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FsEntry {
    File(String),
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryFs {
    entries: BTreeMap<String, FsEntry>,
}

impl Default for InMemoryFs {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert("/".to_string(), FsEntry::Directory);
        entries.insert("/tmp".to_string(), FsEntry::Directory);
        entries.insert("/home".to_string(), FsEntry::Directory);
        entries.insert("/home/user".to_string(), FsEntry::Directory);
        Self { entries }
    }
}

impl InMemoryFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_files(files: BTreeMap<String, String>) -> Result<Self, BashError> {
        let mut fs = Self::new();
        for (path, contents) in files {
            fs.write_file(&path, &contents)?;
        }
        Ok(fs)
    }

    pub fn read_file(&self, path: &str) -> Result<&str, BashError> {
        match self.entries.get(path) {
            Some(FsEntry::File(contents)) => Ok(contents),
            Some(FsEntry::Directory) => {
                Err(BashError::FileSystem(format!("{path}: Is a directory")))
            }
            None => Err(BashError::FileSystem(format!(
                "{path}: No such file or directory"
            ))),
        }
    }

    pub fn write_file(&mut self, path: &str, contents: &str) -> Result<(), BashError> {
        let normalized = normalize_absolute(path);
        self.ensure_parent_dir(&normalized)?;
        self.entries
            .insert(normalized, FsEntry::File(contents.to_string()));
        Ok(())
    }

    pub fn append_file(&mut self, path: &str, contents: &str) -> Result<(), BashError> {
        let normalized = normalize_absolute(path);
        let mut next = match self.entries.get(&normalized) {
            Some(FsEntry::File(existing)) => existing.clone(),
            Some(FsEntry::Directory) => {
                return Err(BashError::FileSystem(format!(
                    "{normalized}: Is a directory"
                )));
            }
            None => String::new(),
        };
        next.push_str(contents);
        self.write_file(&normalized, &next)
    }

    pub fn create_dir_all(&mut self, path: &str) -> Result<(), BashError> {
        let normalized = normalize_absolute(path);
        let mut current = String::from("/");
        for part in normalized.split('/').filter(|part| !part.is_empty()) {
            if current != "/" {
                current.push('/');
            }
            current.push_str(part);
            if matches!(self.entries.get(&current), Some(FsEntry::File(_))) {
                return Err(BashError::FileSystem(format!("{current}: Not a directory")));
            }
            self.entries.insert(current.clone(), FsEntry::Directory);
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &str, recursive: bool) -> Result<(), BashError> {
        let normalized = normalize_absolute(path);
        if normalized == "/" {
            return Err(BashError::FileSystem("cannot remove '/'".to_string()));
        }
        match self.entries.get(&normalized) {
            None => {
                return Err(BashError::FileSystem(format!(
                    "{normalized}: No such file or directory"
                )));
            }
            Some(FsEntry::Directory) if !recursive && self.has_children(&normalized) => {
                return Err(BashError::FileSystem(format!(
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
        matches!(self.entries.get(path), Some(FsEntry::Directory))
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, BashError> {
        if !self.is_dir(path) {
            return Err(BashError::FileSystem(format!("{path}: Not a directory")));
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
            if let Some(name) = rest.split('/').next() {
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    fn ensure_parent_dir(&mut self, path: &str) -> Result<(), BashError> {
        let parent = parent_dir(path);
        if !self.entries.contains_key(&parent) {
            self.create_dir_all(&parent)?;
        }
        if !self.is_dir(&parent) {
            return Err(BashError::FileSystem(format!("{parent}: Not a directory")));
        }
        Ok(())
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
