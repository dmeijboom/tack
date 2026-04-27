use std::path::PathBuf;

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

    pub fn store(&self, name: &str, content: impl AsRef<[u8]>) -> Result<bool, std::io::Error> {
        if let Some(name) = sanitize_name(name) {
            std::fs::write(self.root.join(format!("{name}.yml")), content)?;
            return Ok(true);
        }

        Ok(false)
    }
}
