//! The converted MMS_FA weights: `weights.bin` mmapped once, `weights.json`
//! naming each tensor's f32 offset and shape (`tools/export_weights.py`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum WeightsError {
    #[error("opening {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("reading the weight index {path}: {source}")]
    Index {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} is {len} bytes, not a whole number of f32s")]
    Ragged { path: PathBuf, len: usize },
    #[error("weight {name} is not in the index")]
    Missing { name: String },
    #[error("weight {name} has shape {got:?}, the model needs {want:?}")]
    Shape {
        name: String,
        want: Vec<usize>,
        got: Vec<usize>,
    },
    #[error("weight {name} runs to f32 {end} but the file holds {len}")]
    Short {
        name: String,
        end: usize,
        len: usize,
    },
}

#[derive(Debug, Deserialize)]
struct Entry {
    off: usize,
    shape: Vec<usize>,
}

/// One read-only copy of every weight, shared by the backends.
pub struct Weights {
    map: memmap2::Mmap,
    index: HashMap<String, Entry>,
}

impl Weights {
    /// Maps `dir/weights.bin` and reads `dir/weights.json`.
    pub fn open(dir: &Path) -> Result<Self, WeightsError> {
        Self::open_files(&dir.join("weights.bin"), &dir.join("weights.json"))
    }

    pub fn open_files(bin: &Path, json: &Path) -> Result<Self, WeightsError> {
        let open_err = |path: &Path| {
            let path = path.to_owned();
            move |source| WeightsError::Open { path, source }
        };
        let raw = std::fs::read(json).map_err(open_err(json))?;
        let index = serde_json::from_slice(&raw).map_err(|source| WeightsError::Index {
            path: json.to_owned(),
            source,
        })?;
        let file = std::fs::File::open(bin).map_err(open_err(bin))?;
        // SAFETY: the file is a build output in the read-only store (or a test's
        // own temp file); nothing truncates or writes it while it is mapped.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(open_err(bin))?;
        if map.len() % 4 != 0 {
            return Err(WeightsError::Ragged {
                path: bin.to_owned(),
                len: map.len(),
            });
        }
        Ok(Self { map, index })
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn floats(&self) -> &[f32] {
        // SAFETY: an mmap starts page-aligned, so f32-aligned; the length was
        // checked to be a multiple of 4 at open, and every bit pattern is an f32.
        unsafe { std::slice::from_raw_parts(self.map.as_ptr().cast::<f32>(), self.map.len() / 4) }
    }

    /// The tensor `name`, which must have exactly `shape`.
    pub fn get(&self, name: &str, shape: &[usize]) -> Result<&[f32], WeightsError> {
        let entry = self
            .index
            .get(name)
            .ok_or_else(|| WeightsError::Missing { name: name.into() })?;
        if entry.shape != shape {
            return Err(WeightsError::Shape {
                name: name.into(),
                want: shape.to_vec(),
                got: entry.shape.clone(),
            });
        }
        let all = self.floats();
        let end = entry.off.saturating_add(shape.iter().product());
        all.get(entry.off..end).ok_or(WeightsError::Short {
            name: name.into(),
            end,
            len: all.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn write(name: &str, floats: &[f32], index: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pharos-align-weights-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bytes: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
        std::fs::write(dir.join("weights.bin"), bytes).unwrap();
        std::fs::write(dir.join("weights.json"), index).unwrap();
        dir
    }

    #[test]
    fn tensors_come_back_by_name_and_shape() {
        let dir = write(
            "ok",
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            r#"{"a": {"off": 0, "shape": [2]}, "b": {"off": 2, "shape": [2, 2]}}"#,
        );
        let w = Weights::open(&dir).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w.get("a", &[2]).unwrap(), &[1.0, 2.0]);
        assert_eq!(w.get("b", &[2, 2]).unwrap(), &[3.0, 4.0, 5.0, 6.0]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_misshapen_or_short_tensor_names_itself() {
        let dir = write(
            "bad",
            &[1.0, 2.0, 3.0],
            r#"{"a": {"off": 0, "shape": [2]}, "long": {"off": 2, "shape": [4]}}"#,
        );
        let w = Weights::open(&dir).unwrap();
        let e = w.get("nope", &[1]).unwrap_err();
        assert_eq!(e.to_string(), "weight nope is not in the index");
        let e = w.get("a", &[3]).unwrap_err();
        assert_eq!(e.to_string(), "weight a has shape [2], the model needs [3]");
        let e = w.get("long", &[4]).unwrap_err();
        assert_eq!(
            e.to_string(),
            "weight long runs to f32 6 but the file holds 3"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_ragged_file_or_bad_index_is_refused_at_open() {
        let dir = write("ragged", &[1.0], r#"{}"#);
        std::fs::write(dir.join("weights.bin"), [0u8; 5]).unwrap();
        assert!(matches!(
            Weights::open(&dir),
            Err(WeightsError::Ragged { len: 5, .. })
        ));
        std::fs::write(dir.join("weights.json"), "{").unwrap();
        assert!(matches!(
            Weights::open(&dir),
            Err(WeightsError::Index { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(
            Weights::open(&dir),
            Err(WeightsError::Open { .. })
        ));
    }
}
