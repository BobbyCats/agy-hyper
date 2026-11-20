use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JailError {
    #[error("empty or invalid path")]
    InvalidPath,
    #[error("path contains NUL byte")]
    NulByte,
    #[error("workspace root does not exist or cannot be canonicalized: {0}")]
    InvalidRoot(String),
    #[error("path escapes workspace jail: {path} (workspace: {root})")]
    OutsideWorkspace { path: String, root: String },
    #[error("dangling or escaping symlink blocked: {0}")]
    SymlinkEscape(String),
    #[error("access to sensitive secret file blocked by policy: {0}")]
    SecretBlocked(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct WorkspaceJail {
    canonical_root: PathBuf,
    allow_outside_workspace: bool,
    block_secret_files: bool,
}

impl WorkspaceJail {
    pub fn new(
        root: impl AsRef<Path>,
        allow_outside_workspace: bool,
        block_secret_files: bool,
    ) -> Result<Self, JailError> {
        let raw = root.as_ref();
        let canonical_root = fs::canonicalize(raw)
            .map_err(|e| JailError::InvalidRoot(format!("{}: {}", raw.display(), e)))?;
        if !canonical_root.is_dir() {
            return Err(JailError::InvalidRoot(format!(
                "{} is not a directory",
                canonical_root.display()
            )));
        }
        Ok(Self {
            canonical_root,
            allow_outside_workspace,
            block_secret_files,
        })
    }

    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// Resolve a path that must already exist (for reading/searching).
    pub fn resolve_existing(&self, input: impl AsRef<Path>) -> Result<PathBuf, JailError> {
        let candidate = self.normalize_input(input.as_ref())?;
        let canonical = fs::canonicalize(&candidate)?;
        self.verify_containment(&canonical)?;
        self.verify_secret_policy(&canonical)?;
        Ok(canonical)
    }

    /// Resolve a path for writing/creating. Safely rejects:
    /// - `..` escapes
    /// - existing symlinks pointing outside the workspace
    /// - dangling symlinks pointing outside the workspace
    /// - sensitive secret files when `block_secret_files` is enabled
    pub fn resolve_write(&self, input: impl AsRef<Path>) -> Result<PathBuf, JailError> {
        let candidate = self.normalize_input(input.as_ref())?;

        // If the leaf itself exists (including as a dangling symlink), inspect symlink_metadata first.
        if let Ok(meta) = fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                let real = fs::canonicalize(&candidate)
                    .map_err(|_| JailError::SymlinkEscape(candidate.display().to_string()))?;
                self.verify_containment(&real)?;
                self.verify_secret_policy(&real)?;
                return Ok(real);
            }
            let canonical = fs::canonicalize(&candidate)?;
            self.verify_containment(&canonical)?;
            self.verify_secret_policy(&canonical)?;
            return Ok(canonical);
        }

        // Target does not exist yet: walk up to the deepest entry that exists on disk
        // (using symlink_metadata so dangling intermediate directory symlinks are caught, not skipped!).
        let mut existing = candidate.as_path();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while fs::symlink_metadata(existing).is_err() {
            let Some(name) = existing.file_name() else {
                return Err(JailError::InvalidPath);
            };
            tail.push(name.to_os_string());
            match existing.parent() {
                Some(p) if !p.as_os_str().is_empty() => existing = p,
                _ => return Err(JailError::InvalidPath),
            }
        }
        let canonical_ancestor = fs::canonicalize(existing)
            .map_err(|_| JailError::SymlinkEscape(existing.display().to_string()))?;
        self.verify_containment(&canonical_ancestor)?;

        let mut final_path = canonical_ancestor;
        for seg in tail.into_iter().rev() {
            if seg == ".." || seg == "." {
                return Err(JailError::OutsideWorkspace {
                    path: candidate.display().to_string(),
                    root: self.canonical_root.display().to_string(),
                });
            }
            final_path.push(seg);
        }
        self.verify_containment(&final_path)?;
        self.verify_secret_policy(&final_path)?;
        Ok(final_path)
    }

    pub fn relative_display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.canonical_root).unwrap_or(path);
        rel.to_string_lossy().replace('\\', "/")
    }

    fn normalize_input(&self, input: &Path) -> Result<PathBuf, JailError> {
        let s = input.to_string_lossy();
        if s.trim().is_empty() {
            return Err(JailError::InvalidPath);
        }
        if s.contains('\0') {
            return Err(JailError::NulByte);
        }
        let expanded = if s == "~" || s.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                if s == "~" {
                    home
                } else {
                    home.join(&s[2..])
                }
            } else {
                input.to_path_buf()
            }
        } else if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.canonical_root.join(input)
        };
        Ok(lexical_clean(&expanded))
    }

    fn verify_containment(&self, canonical: &Path) -> Result<(), JailError> {
        if self.allow_outside_workspace {
            return Ok(());
        }
        if canonical.starts_with(&self.canonical_root) {
            Ok(())
        } else {
            Err(JailError::OutsideWorkspace {
                path: canonical.display().to_string(),
                root: self.canonical_root.display().to_string(),
            })
        }
    }

    fn verify_secret_policy(&self, path: &Path) -> Result<(), JailError> {
        if !self.block_secret_files {
            return Ok(());
        }
        if is_secret_path(path) {
            return Err(JailError::SecretBlocked(self.relative_display(path)));
        }
        Ok(())
    }
}

pub fn is_secret_path(path: &Path) -> bool {
    let Some(name_os) = path.file_name() else {
        return false;
    };
    let name = name_os.to_string_lossy().to_ascii_lowercase();

    // Explicit safe templates/examples
    if name.ends_with(".example")
        || name.ends_with(".sample")
        || name.ends_with(".template")
        || name.ends_with(".dist")
    {
        return false;
    }

    if name == ".env" || name == ".envrc" || name.starts_with(".env.") {
        return true;
    }
    if name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".p8")
        || name.ends_with(".keystore")
        || name.ends_with(".jks")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == "id_ecdsa"
        || name == ".npmrc"
        || name == ".netrc"
        || name == ".pypirc"
        || name == ".htpasswd"
    {
        return true;
    }
    false
}

fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn blocks_parent_traversal_escape() {
        let dir = tempdir().unwrap();
        let jail = WorkspaceJail::new(dir.path(), false, true).unwrap();
        let err = jail.resolve_write("../../etc/passwd").unwrap_err();
        assert!(matches!(err, JailError::OutsideWorkspace { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape_and_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let ws = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret_target = outside.path().join("outside.txt");
        fs::write(&secret_target, "top-secret").unwrap();

        let link = ws.path().join("escape.txt");
        symlink(&secret_target, &link).unwrap();

        let dangling = ws.path().join("dangling.txt");
        symlink(outside.path().join("nonexistent.txt"), &dangling).unwrap();

        let dangling_dir = ws.path().join("dangling_dir");
        symlink(outside.path().join("nonexistent_dir"), &dangling_dir).unwrap();

        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();
        assert!(matches!(
            jail.resolve_existing("escape.txt"),
            Err(JailError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            jail.resolve_write("dangling.txt"),
            Err(JailError::SymlinkEscape(_))
        ));
        assert!(matches!(
            jail.resolve_write("dangling_dir/nested.txt"),
            Err(JailError::SymlinkEscape(_))
        ));
    }

    #[test]
    fn blocks_dotenv_but_allows_dotenv_example() {
        let ws = tempdir().unwrap();
        fs::write(ws.path().join(".env"), "SECRET=1").unwrap();
        fs::write(ws.path().join(".env.example"), "SECRET=").unwrap();
        let jail = WorkspaceJail::new(ws.path(), false, true).unwrap();
        assert!(matches!(
            jail.resolve_existing(".env"),
            Err(JailError::SecretBlocked(_))
        ));
        assert!(jail.resolve_existing(".env.example").is_ok());
    }
}
