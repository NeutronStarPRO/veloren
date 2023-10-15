use std::{path::PathBuf, sync::RwLock};

use crate::Concatenate;

use super::{fs::FileSystem, tar_source::Tar, ASSETS_PATH};
use assets_manager::{
    hot_reloading::{DynUpdateSender, EventSender},
    source::{FileContent, Source},
    AnyCache, AssetCache, BoxedError,
};

struct PluginEntry {
    path: PathBuf,
    cache: AssetCache<Tar>,
}

/// The source combining filesystem and plugins (typically used via
/// CombinedCache)
pub struct CombinedSource {
    fs: AssetCache<FileSystem>,
    plugin_list: RwLock<Vec<PluginEntry>>,
}

impl CombinedSource {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            fs: AssetCache::with_source(FileSystem::new()?),
            plugin_list: RwLock::new(Vec::new()),
        })
    }
}

impl CombinedSource {
    fn read_multiple(&self, id: &str, ext: &str) -> Vec<(Option<usize>, FileContent<'_>)> {
        let mut result = Vec::new();
        if let Ok(file_entry) = self.fs.raw_source().read(id, ext) {
            result.push((None, file_entry));
        }
        for (n, p) in self.plugin_list.read().unwrap().iter().enumerate() {
            if let Ok(entry) = p.cache.raw_source().read(id, ext) {
                // the data is behind an RwLockReadGuard, so own it for returning
                result.push((Some(n), match entry {
                    FileContent::Slice(s) => FileContent::Buffer(Vec::from(s)),
                    FileContent::Buffer(b) => FileContent::Buffer(b),
                    FileContent::Owned(s) => FileContent::Buffer(Vec::from(s.as_ref().as_ref())),
                }));
            }
        }
        result
    }

    // We don't want to keep the lock, so we clone
    fn plugin_path(&self, index: Option<usize>) -> Option<PathBuf> {
        if let Some(index) = index {
            self.plugin_list
                .read()
                .unwrap()
                .get(index)
                .map(|plugin| plugin.path.clone())
        } else {
            None
        }
    }
}

impl Source for CombinedSource {
    fn read(&self, id: &str, ext: &str) -> std::io::Result<FileContent<'_>> {
        // We could shortcut on fs if we dont check for conflicts
        let mut entries = self.read_multiple(id, ext);
        if entries.is_empty() {
            Err(std::io::ErrorKind::NotFound.into())
        } else {
            if entries.len() > 1 {
                let plugina = self.plugin_path(entries[0].0);
                let pluginb = self.plugin_path(entries[1].0);
                let patha = plugina.as_ref().unwrap_or(&ASSETS_PATH);
                let pathb = pluginb.as_ref().unwrap_or(&ASSETS_PATH);
                tracing::error!("Duplicate asset {id} in {patha:?} and {pathb:?}");
            }
            Ok(entries.swap_remove(0).1)
        }
    }

    fn read_dir(
        &self,
        id: &str,
        f: &mut dyn FnMut(assets_manager::source::DirEntry),
    ) -> std::io::Result<()> {
        // TODO: We should combine the sources, but this isn't used in veloren
        self.fs.raw_source().read_dir(id, f)
    }

    fn exists(&self, entry: assets_manager::source::DirEntry) -> bool {
        self.fs.raw_source().exists(entry)
            || self
                .plugin_list
                .read()
                .unwrap()
                .iter()
                .any(|plugin| plugin.cache.raw_source().exists(entry))
    }

    // TODO: Enable hot reloading for plugins
    fn make_source(&self) -> Option<Box<dyn Source + Send>> { self.fs.raw_source().make_source() }

    fn configure_hot_reloading(&self, events: EventSender) -> Result<DynUpdateSender, BoxedError> {
        self.fs.raw_source().configure_hot_reloading(events)
    }
}

/// A cache combining filesystem and plugin assets
pub struct CombinedCache(AssetCache<CombinedSource>);

impl CombinedCache {
    pub fn new() -> std::io::Result<Self> {
        CombinedSource::new().map(|combined_source| Self(AssetCache::with_source(combined_source)))
    }

    /// Combine objects from filesystem and plugins
    pub fn combine<T: Concatenate>(
        &self,
        load_from: impl Fn(AnyCache) -> Result<T, BoxedError>,
    ) -> Result<T, BoxedError> {
        let mut result = load_from(self.0.raw_source().fs.as_any_cache());
        // Report a severe error from the filesystem asset even if later overwritten by
        // an Ok value from a plugin
        if let Err(ref fs_error) = result {
            match fs_error
                .source()
                .and_then(|error_source| error_source.downcast_ref::<std::io::Error>())
                .map(|io_error| io_error.kind())
            {
                Some(std::io::ErrorKind::NotFound) => (),
                _ => tracing::error!("Filesystem asset load {fs_error:?}"),
            }
        }
        for plugin in self.0.raw_source().plugin_list.read().unwrap().iter() {
            match load_from(plugin.cache.as_any_cache()) {
                Ok(b) => {
                    result = if let Ok(a) = result {
                        Ok(a.concatenate(b))
                    } else {
                        Ok(b)
                    };
                },
                // Report any error other than NotFound
                Err(plugin_error) => {
                    match plugin_error
                        .source()
                        .and_then(|error_source| error_source.downcast_ref::<std::io::Error>())
                        .map(|io_error| io_error.kind())
                    {
                        Some(std::io::ErrorKind::NotFound) => (),
                        _ => tracing::error!(
                            "Loading from {:?} failed {plugin_error:?}",
                            plugin.path
                        ),
                    }
                },
            }
        }
        result
    }

    pub fn register_tar(&self, path: PathBuf) -> std::io::Result<()> {
        let tar_source = Tar::from_path(&path)?;
        let cache = AssetCache::with_source(tar_source);
        self.0
            .raw_source()
            .plugin_list
            .write()
            .unwrap()
            .push(PluginEntry { path, cache });
        Ok(())
    }
}

impl std::ops::Deref for CombinedCache {
    type Target = AssetCache<CombinedSource>;

    fn deref(&self) -> &Self::Target { &self.0 }
}
