use ::prelude::*;
use super::*;

use std::path::{Path, PathBuf};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};
use std::io;


quick_error!{
    #[derive(Debug)]
    pub enum BundleDbError {
        ListBundles(err: io::Error) {
            cause(err)
            description("Failed to list bundles")
            display("Bundle db error: failed to list bundles\n\tcaused by: {}", err)
        }
        Reader(err: BundleReaderError) {
            from()
            cause(err)
            description("Failed to read bundle")
            display("Bundle db error: failed to read bundle\n\tcaused by: {}", err)
        }
        Writer(err: BundleWriterError) {
            from()
            cause(err)
            description("Failed to write bundle")
            display("Bundle db error: failed to write bundle\n\tcaused by: {}", err)
        }
        Cache(err: BundleCacheError) {
            from()
            cause(err)
            description("Failed to read/write bundle cache")
            display("Bundle db error: failed to read/write bundle cache\n\tcaused by: {}", err)
        }
        Io(err: io::Error, path: PathBuf) {
            cause(err)
            context(path: &'a Path, err: io::Error) -> (err, path.to_path_buf())
            description("Io error")
            display("Bundle db error: io error on {:?}\n\tcaused by: {}", path, err)
        }
        NoSuchBundle(bundle: BundleId) {
            description("No such bundle")
            display("Bundle db error: no such bundle: {:?}", bundle)
        }
        Remove(err: io::Error, bundle: BundleId) {
            cause(err)
            description("Failed to remove bundle")
            display("Bundle db error: failed to remove bundle {}\n\tcaused by: {}", bundle, err)
        }
    }
}


pub fn load_bundles(path: &Path, base: &Path, bundles: &mut HashMap<BundleId, StoredBundle>, crypto: Arc<Mutex<Crypto>>) -> Result<(Vec<StoredBundle>, Vec<StoredBundle>), BundleDbError> {
    let mut paths = vec![path.to_path_buf()];
    let mut bundle_paths = HashSet::new();
    while let Some(path) = paths.pop() {
        for entry in try!(fs::read_dir(path).map_err(BundleDbError::ListBundles)) {
            let entry = try!(entry.map_err(BundleDbError::ListBundles));
            let path = entry.path();
            if path.is_dir() {
                paths.push(path);
            } else {
                bundle_paths.insert(path.strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    let mut gone = HashSet::new();
    for (id, bundle) in bundles.iter_mut() {
        if !bundle_paths.contains(&bundle.path) {
            gone.insert(id.clone());
        } else {
            bundle_paths.remove(&bundle.path);
        }
    }
    let mut new = vec![];
    for path in bundle_paths {
        let bundle = StoredBundle {
            info: try!(BundleReader::load_info(base.join(&path), crypto.clone())),
            path: path
        };
        let id = bundle.info.id.clone();
        if !bundles.contains_key(&id) {
            new.push(bundle.clone());
        } else {
            gone.remove(&id);
        }
        bundles.insert(id, bundle);
    }
    let gone = gone.iter().map(|id| bundles.remove(id).unwrap()).collect();
    Ok((new, gone))
}



pub struct BundleDb {
    pub layout: RepositoryLayout,
    crypto: Arc<Mutex<Crypto>>,
    local_bundles: HashMap<BundleId, StoredBundle>,
    remote_bundles: HashMap<BundleId, StoredBundle>,
    bundle_cache: LruCache<BundleId, (BundleReader, Vec<u8>)>
}


impl BundleDb {
    fn new(layout: RepositoryLayout, crypto: Arc<Mutex<Crypto>>) -> Self {
        BundleDb {
            layout: layout,
            crypto: crypto,
            local_bundles: HashMap::new(),
            remote_bundles: HashMap::new(),
            bundle_cache: LruCache::new(5, 10)
        }
    }

    fn load_bundle_list(&mut self) -> Result<(Vec<StoredBundle>, Vec<StoredBundle>), BundleDbError> {
        if let Ok(list) = StoredBundle::read_list_from(&self.layout.local_bundle_cache_path()) {
            for bundle in list {
                self.local_bundles.insert(bundle.id(), bundle);
            }
        }
        if let Ok(list) = StoredBundle::read_list_from(&self.layout.remote_bundle_cache_path()) {
            for bundle in list {
                self.remote_bundles.insert(bundle.id(), bundle);
            }
        }
        let base_path = self.layout.base_path();
        let (new, gone) = try!(load_bundles(&self.layout.local_bundles_path(), base_path, &mut self.local_bundles, self.crypto.clone()));
        if !new.is_empty() || !gone.is_empty() {
            let bundles: Vec<_> = self.local_bundles.values().cloned().collect();
            try!(StoredBundle::save_list_to(&bundles, &self.layout.local_bundle_cache_path()));
        }
        let (new, gone) = try!(load_bundles(&self.layout.remote_bundles_path(), base_path, &mut self.remote_bundles, self.crypto.clone()));
        if !new.is_empty() || !gone.is_empty() {
            let bundles: Vec<_> = self.remote_bundles.values().cloned().collect();
            try!(StoredBundle::save_list_to(&bundles, &self.layout.remote_bundle_cache_path()));
        }
        Ok((new, gone))
    }

    pub fn save_cache(&self) -> Result<(), BundleDbError> {
        let bundles: Vec<_> = self.local_bundles.values().cloned().collect();
        try!(StoredBundle::save_list_to(&bundles, &self.layout.local_bundle_cache_path()));
        let bundles: Vec<_> = self.remote_bundles.values().cloned().collect();
        Ok(try!(StoredBundle::save_list_to(&bundles, &self.layout.remote_bundle_cache_path())))
    }

    pub fn update_cache(&mut self, new: &[StoredBundle], gone: &[StoredBundle]) -> Result<(), BundleDbError> {
        for bundle in new {
            if bundle.info.mode == BundleMode::Meta {
                info!("Copying new meta bundle to local cache: {}", bundle.info.id);
                try!(self.copy_remote_bundle_to_cache(bundle));
            }
        }
        let base_path = self.layout.base_path();
        for bundle in gone {
            if let Some(bundle) = self.local_bundles.remove(&bundle.id()) {
                try!(fs::remove_file(base_path.join(&bundle.path)).map_err(|e| BundleDbError::Remove(e, bundle.id())))
            }
        }
        Ok(())
    }

    #[inline]
    pub fn open(layout: RepositoryLayout, crypto: Arc<Mutex<Crypto>>) -> Result<(Self, Vec<BundleInfo>, Vec<BundleInfo>), BundleDbError> {
        let mut self_ = Self::new(layout, crypto);
        let (new, gone) = try!(self_.load_bundle_list());
        try!(self_.update_cache(&new, &gone));
        let new = new.into_iter().map(|s| s.info).collect();
        let gone = gone.into_iter().map(|s| s.info).collect();
        Ok((self_, new, gone))
    }

    #[inline]
    pub fn create(layout: RepositoryLayout) -> Result<(), BundleDbError> {
        try!(fs::create_dir_all(layout.remote_bundles_path()).context(&layout.remote_bundles_path() as &Path));
        try!(fs::create_dir_all(layout.local_bundles_path()).context(&layout.local_bundles_path() as &Path));
        try!(fs::create_dir_all(layout.temp_bundles_path()).context(&layout.temp_bundles_path() as &Path));
        Ok(())
    }

    #[inline]
    pub fn create_bundle(&self, mode: BundleMode, hash_method: HashMethod, compression: Option<Compression>, encryption: Option<Encryption>) -> Result<BundleWriter, BundleDbError> {
        Ok(try!(BundleWriter::new(mode, hash_method, compression, encryption, self.crypto.clone())))
    }

    fn get_stored_bundle(&self, bundle_id: &BundleId) -> Result<&StoredBundle, BundleDbError> {
        if let Some(stored) = self.local_bundles.get(bundle_id).or_else(|| self.remote_bundles.get(bundle_id)) {
            Ok(stored)
        } else {
            Err(BundleDbError::NoSuchBundle(bundle_id.clone()))
        }
    }

    fn get_bundle(&self, stored: &StoredBundle) -> Result<BundleReader, BundleDbError> {
        let base_path = self.layout.base_path();
        Ok(try!(BundleReader::load(base_path.join(&stored.path), self.crypto.clone())))
    }

    pub fn get_chunk(&mut self, bundle_id: &BundleId, id: usize) -> Result<Vec<u8>, BundleDbError> {
        if let Some(&mut (ref mut bundle, ref data)) = self.bundle_cache.get_mut(bundle_id) {
            let (pos, len) = try!(bundle.get_chunk_position(id));
            let mut chunk = Vec::with_capacity(len);
            chunk.extend_from_slice(&data[pos..pos+len]);
            return Ok(chunk);
        }
        let mut bundle = try!(self.get_stored_bundle(bundle_id).and_then(|s| self.get_bundle(s)));
        let (pos, len) = try!(bundle.get_chunk_position(id));
        let mut chunk = Vec::with_capacity(len);
        let data = try!(bundle.load_contents());
        chunk.extend_from_slice(&data[pos..pos+len]);
        self.bundle_cache.put(bundle_id.clone(), (bundle, data));
        Ok(chunk)
    }

    fn copy_remote_bundle_to_cache(&mut self, bundle: &StoredBundle) -> Result<(), BundleDbError> {
        let id = bundle.id();
        let (folder, filename) = self.layout.local_bundle_path(&id, self.local_bundles.len());
        try!(fs::create_dir_all(&folder).context(&folder as &Path));
        let bundle = try!(bundle.copy_to(&self.layout.base_path(), folder.join(filename)));
        self.local_bundles.insert(id, bundle);
        Ok(())
    }

    #[inline]
    pub fn add_bundle(&mut self, bundle: BundleWriter) -> Result<BundleInfo, BundleDbError> {
        let bundle = try!(bundle.finish(&self));
        if bundle.info.mode == BundleMode::Meta {
            try!(self.copy_remote_bundle_to_cache(&bundle))
        }
        let (folder, filename) = self.layout.remote_bundle_path(self.remote_bundles.len());
        try!(fs::create_dir_all(&folder).context(&folder as &Path));
        let bundle = try!(bundle.move_to(&self.layout.base_path(), folder.join(filename)));
        self.remote_bundles.insert(bundle.id(), bundle.clone());
        Ok(bundle.info)
    }

    #[inline]
    pub fn get_chunk_list(&self, bundle: &BundleId) -> Result<ChunkList, BundleDbError> {
        let mut bundle = try!(self.get_stored_bundle(bundle).and_then(|stored| self.get_bundle(&stored)));
        Ok(try!(bundle.get_chunk_list()).clone())
    }

    #[inline]
    pub fn get_bundle_info(&self, bundle: &BundleId) -> Option<&StoredBundle> {
        self.get_stored_bundle(bundle).ok()
    }

    #[inline]
    pub fn list_bundles(&self) -> Vec<&BundleInfo> {
        self.remote_bundles.values().map(|b| &b.info).collect()
    }

    #[inline]
    pub fn delete_local_bundle(&mut self, bundle: &BundleId) -> Result<(), BundleDbError> {
        if let Some(bundle) = self.local_bundles.remove(bundle) {
            let path = self.layout.base_path().join(&bundle.path);
            try!(fs::remove_file(path).map_err(|e| BundleDbError::Remove(e, bundle.id())))
        }
        Ok(())
    }

    #[inline]
    pub fn delete_bundle(&mut self, bundle: &BundleId) -> Result<(), BundleDbError> {
        try!(self.delete_local_bundle(bundle));
        if let Some(bundle) = self.remote_bundles.remove(bundle) {
            let path = self.layout.base_path().join(&bundle.path);
            fs::remove_file(path).map_err(|e| BundleDbError::Remove(e, bundle.id()))
        } else {
            Err(BundleDbError::NoSuchBundle(bundle.clone()))
        }
    }

    #[inline]
    pub fn check(&mut self, full: bool) -> Result<(), BundleDbError> {
        for stored in self.remote_bundles.values() {
            let mut bundle = try!(self.get_bundle(stored));
            try!(bundle.check(full))
        }
        Ok(())
    }
}
