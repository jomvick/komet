//! Local prompt snippets used by the composer's `/prompt` command.
//!
//! The library is deliberately device-local for now.  It follows the same
//! atomic/corrupt-tolerant persistence rules as [`super::composer`].

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "prompts.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSnippet {
    pub name: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptLibrary {
    pub prompts: Vec<PromptSnippet>,
}

impl PromptLibrary {
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "prompt library corrupt; using empty library");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    pub fn find(&self, name: &str) -> Option<&PromptSnippet> {
        self.prompts
            .iter()
            .find(|prompt| prompt.name.eq_ignore_ascii_case(name))
    }

    pub fn upsert(&mut self, prompt: PromptSnippet) {
        if let Some(existing) = self
            .prompts
            .iter_mut()
            .find(|existing| existing.name.eq_ignore_ascii_case(&prompt.name))
        {
            *existing = prompt;
        } else {
            self.prompts.push(prompt);
        }
    }

    pub fn delete(&mut self, name: &str) -> bool {
        let before = self.prompts.len();
        self.prompts
            .retain(|prompt| !prompt.name.eq_ignore_ascii_case(name));
        self.prompts.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let mut library = PromptLibrary::default();
        library.upsert(PromptSnippet {
            name: "review".into(),
            body: "Review this diff".into(),
            harness: Some("claude".into()),
        });
        library.upsert(PromptSnippet {
            name: "REVIEW".into(),
            body: "Review this patch".into(),
            harness: None,
        });
        library.save(dir.path()).unwrap();
        assert_eq!(PromptLibrary::load(dir.path()), library);
        assert_eq!(library.find("review").unwrap().body, "Review this patch");
    }

    #[test]
    fn corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(PromptLibrary::path(dir.path()), "not json").unwrap();
        assert_eq!(PromptLibrary::load(dir.path()), PromptLibrary::default());
    }
}
