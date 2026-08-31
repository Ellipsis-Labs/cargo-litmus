use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{LitmusError, Result};
use crate::model::IndexMetadata;

pub fn save_index(path: &Path, metadata: &IndexMetadata) -> Result<u64> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LitmusError::io(parent, source))?;
    }

    let bytes =
        rkyv::to_bytes::<rkyv::rancor::BoxedError>(metadata).map_err(LitmusError::Serialize)?;
    let byte_len = bytes.len() as u64;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = path.with_extension(format!("rkyv.{}.{}.tmp", std::process::id(), nonce));
    fs::write(&tmp_path, &bytes).map_err(|source| LitmusError::io(&tmp_path, source))?;
    fs::rename(&tmp_path, path).map_err(|source| LitmusError::io(path, source))?;
    Ok(byte_len)
}

pub fn load_index(path: &Path) -> Result<IndexMetadata> {
    let bytes = fs::read(path).map_err(|source| LitmusError::io(path, source))?;
    rkyv::from_bytes::<IndexMetadata, rkyv::rancor::BoxedError>(&bytes)
        .map_err(LitmusError::Deserialize)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::model::{INDEX_VERSION, IndexMetadata};

    #[test]
    fn rkyv_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("target/cargo-litmus.rkyv");
        let metadata = IndexMetadata {
            version: INDEX_VERSION,
            tool_version: "test".to_string(),
            repo_root_hint: "/repo".to_string(),
            git_head: Some("head".to_string()),
            rust_file_list_hash: "rust".to_string(),
            tracked_file_list_hash: "tracked".to_string(),
            cargo_metadata_hash: "cargo".to_string(),
            generated_at_unix_ms: 1,
            files: Vec::new(),
            items: Vec::new(),
            packages: Vec::new(),
            targets: Vec::new(),
            package_dependencies: Vec::new(),
            module_declarations: Vec::new(),
            source_dependencies: Vec::new(),
            edges: Vec::new(),
        };

        save_index(&path, &metadata).unwrap();
        let loaded = load_index(&path).unwrap();

        assert_eq!(loaded, metadata);
    }
}
