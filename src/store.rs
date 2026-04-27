use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub fn write_restricted(path: impl AsRef<Path>, content: &[u8]) -> Result<(), std::io::Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)?;
    Ok(())
}

pub struct Store {
    root: PathBuf,
}

fn sanitize_name(name: &str) -> Option<&str> {
    if name
        .chars()
        .any(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '-' | '_' | '0'..='9'))
    {
        return None;
    }

    Some(name)
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn kubeconfig(&self, name: &str) -> Option<PathBuf> {
        sanitize_name(name).map(|name| self.root.join(format!("{name}.yml")))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.kubeconfig(name)
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    pub fn store(&self, name: &str, content: impl AsRef<[u8]>) -> Result<(), std::io::Error> {
        let name = sanitize_name(name).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid context name")
        })?;

        fs::create_dir_all(&self.root)?;

        let path = self.root.join(format!("{name}.yml"));
        write_restricted(path, content.as_ref())
    }
}
