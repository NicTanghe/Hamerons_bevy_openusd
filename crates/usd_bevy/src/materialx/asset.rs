//! Host-provided access to MaterialX documents and texture assets.

use std::path::Path;
use std::sync::Arc;

use bevy::prelude::Resource;

type AssetReadFn = dyn Fn(&Path) -> Result<Vec<u8>, String> + Send + Sync + 'static;

/// Reads MaterialX assets from a host-owned source such as a browser file picker.
///
/// When this resource is absent, `usd_bevy` keeps using the native filesystem.
#[derive(Resource, Clone)]
pub struct MaterialXAssetReader {
    read: Arc<AssetReadFn>,
}

impl MaterialXAssetReader {
    pub fn new(read: impl Fn(&Path) -> Result<Vec<u8>, String> + Send + Sync + 'static) -> Self {
        Self {
            read: Arc::new(read),
        }
    }

    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        (self.read)(path)
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.read(path).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegates_reads_to_the_host() {
        let reader = MaterialXAssetReader::new(|path| {
            (path == Path::new("material.mtlx"))
                .then(|| b"<materialx/>".to_vec())
                .ok_or_else(|| "missing".to_owned())
        });

        assert!(reader.contains(Path::new("material.mtlx")));
        assert_eq!(
            reader.read(Path::new("material.mtlx")).unwrap(),
            b"<materialx/>"
        );
        assert!(!reader.contains(Path::new("missing.mtlx")));
    }
}
