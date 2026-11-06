//! Immutable directory recipes. This module does not traverse the filesystem.
use crate::{
    Error, Result, hashing,
    storage::{ChunkStore, publish, publish_if_missing, read_regular_file},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const MAX_NODE_BYTES: usize = 16 * 1024 * 1024;

/// A typed reference: file hashes identify Phase 1 manifest bytes, not raw chunks.
/// Length is logical file size; it must be checked against the manifest on restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryTarget {
    File { manifest: String, length: u64 },
    Directory { tree: String },
}

/// One validated immediate child, with no absolute or relative path embedded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectoryEntry {
    name: String,
    target: EntryTarget,
}

impl DirectoryEntry {
    /// Validate and construct a child reference. Does not resolve its target.
    pub fn new(name: impl Into<String>, target: EntryTarget) -> Result<Self> {
        let entry = Self {
            name: name.into(),
            target,
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Exact UTF-8 child name, without normalization.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Typed immutable child reference.
    pub fn target(&self) -> &EntryTarget {
        &self.target
    }

    fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        match &self.target {
            EntryTarget::File { manifest, .. } => hashing::validate_hash(manifest),
            EntryTarget::Directory { tree } => hashing::validate_hash(tree),
        }
    }
}

/// Version 1 directory node. Private fields keep construction canonical.
/// Deserialization is intentionally exposed only through validated `from_bytes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DirectoryNode {
    version: u32,
    object: String,
    entries: Vec<DirectoryEntry>,
}

impl DirectoryNode {
    /// Sort entries by UTF-8 bytes and reject duplicate names or invalid targets.
    pub fn new(mut entries: Vec<DirectoryEntry>) -> Result<Self> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let node = Self {
            version: 1,
            object: "rewind-directory".into(),
            entries,
        };
        node.validate()?;
        Ok(node)
    }

    /// Sorted immediate children. The returned slice cannot mutate the node.
    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 || self.object != "rewind-directory" {
            return Err(Error::Invalid(
                "unsupported directory node version or object type".into(),
            ));
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(Error::Invalid(
                "directory entries must be strictly sorted with unique names".into(),
            ));
        }
        Ok(())
    }

    /// Compact JSON in schema field order, without whitespace or a final newline.
    /// This exact encoding is the version 1 canonical format, not arbitrary JSON.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_NODE_BYTES {
            return Err(Error::Invalid("directory node exceeds 16 MiB".into()));
        }
        Ok(bytes)
    }

    /// Parse only canonical, supported nodes; reject alternate encodings.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_NODE_BYTES {
            return Err(Error::Invalid("directory node exceeds 16 MiB".into()));
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireNode {
            version: u32,
            object: String,
            entries: Vec<WireEntry>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEntry {
            name: String,
            target: EntryTarget,
        }
        let raw: WireNode = serde_json::from_slice(bytes)?;
        let node = Self {
            version: raw.version,
            object: raw.object,
            entries: raw
                .entries
                .into_iter()
                .map(|entry| DirectoryEntry::new(entry.name, entry.target))
                .collect::<Result<Vec<_>>>()?,
        };
        if node.canonical_bytes()? != bytes {
            return Err(Error::Invalid(
                "noncanonical directory node encoding".into(),
            ));
        }
        Ok(node)
    }

    /// BLAKE3 identity of the canonical node (including its schema discriminator).
    pub fn hash(&self) -> Result<String> {
        Ok(hashing::hash(&self.canonical_bytes()?))
    }

    /// Publish a node without replacing an existing object. References are not
    /// traversed here; callers must persist children before publishing a snapshot.
    pub fn save(&self, store: &ChunkStore) -> Result<String> {
        let bytes = self.canonical_bytes()?;
        let hash = hashing::hash(&bytes);
        let path = node_path(store, &hash)?;
        publish_if_missing(
            || Self::load(store, &hash).map(|_| ()),
            || publish(&path, &bytes),
        )?;
        Ok(hash)
    }

    /// Read a bounded object, verify its identity, then validate the node schema.
    /// Does not verify the existence or integrity of referenced descendants.
    pub fn load(store: &ChunkStore, hash: &str) -> Result<Self> {
        let bytes = read_regular_file(&node_path(store, hash)?, MAX_NODE_BYTES as u64)?;
        if bytes.len() > MAX_NODE_BYTES || hashing::hash(&bytes) != hash {
            return Err(Error::Corrupt(format!("directory node {hash}")));
        }
        Self::from_bytes(&bytes)
    }
}

fn node_path(store: &ChunkStore, hash: &str) -> Result<PathBuf> {
    hashing::validate_hash(hash)?;
    Ok(store.root().join("trees").join(&hash[..2]).join(&hash[2..]))
}

/// Conservative portable component policy: UTF-8, at most 255 bytes, no path
/// separators, Windows drive/stream syntax, control chars, or device names.
/// No normalization/case folding: names retain their exact UTF-8 bytes.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || device
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return Err(Error::Invalid(format!(
            "unsafe or unsupported directory entry name: {name:?}"
        )));
    }
    Ok(())
}
