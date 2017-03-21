use super::{Repository, RepositoryError};
use super::metadata::{FileType, Inode};

use ::util::*;

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::fs::{self, File};
use std::path::{self, Path, PathBuf};
use std::collections::{HashMap, VecDeque};

use quick_error::ResultExt;
use chrono::prelude::*;


static HEADER_STRING: [u8; 7] = *b"zvault\x03";
static HEADER_VERSION: u8 = 1;


quick_error!{
    #[derive(Debug)]
    pub enum BackupError {
        Io(err: io::Error, path: PathBuf) {
            cause(err)
            context(path: &'a Path, err: io::Error) -> (err, path.to_path_buf())
            description("Failed to read/write backup")
            display("Failed to read/write backup {:?}: {}", path, err)
        }
        Decode(err: msgpack::DecodeError, path: PathBuf) {
            cause(err)
            context(path: &'a Path, err: msgpack::DecodeError) -> (err, path.to_path_buf())
            description("Failed to decode backup")
            display("Failed to decode backup of {:?}: {}", path, err)
        }
        Encode(err: msgpack::EncodeError, path: PathBuf) {
            cause(err)
            context(path: &'a Path, err: msgpack::EncodeError) -> (err, path.to_path_buf())
            description("Failed to encode backup")
            display("Failed to encode backup of {:?}: {}", path, err)
        }
        WrongHeader(path: PathBuf) {
            description("Wrong header")
            display("Wrong header on backup {:?}", path)
        }
        WrongVersion(path: PathBuf, version: u8) {
            description("Wrong version")
            display("Wrong version on backup {:?}: {}", path, version)
        }
        Decryption(err: EncryptionError, path: PathBuf) {
            cause(err)
            context(path: &'a Path, err: EncryptionError) -> (err, path.to_path_buf())
            description("Decryption failed")
            display("Decryption failed on backup {:?}: {}", path, err)
        }
        Encryption(err: EncryptionError) {
            from()
            cause(err)
            description("Encryption failed")
            display("Encryption failed: {}", err)
        }
    }
}


#[derive(Default, Debug, Clone)]
struct BackupHeader {
    pub encryption: Option<Encryption>
}
serde_impl!(BackupHeader(u8) {
    encryption: Option<Encryption> => 0
});


#[derive(Default, Debug, Clone)]
pub struct Backup {
    pub root: ChunkList,
    pub total_data_size: u64, // Sum of all raw sizes of all entities
    pub changed_data_size: u64, // Sum of all raw sizes of all entities actively stored
    pub deduplicated_data_size: u64, // Sum of all raw sizes of all new bundles
    pub encoded_data_size: u64, // Sum al all encoded sizes of all new bundles
    pub bundle_count: usize,
    pub chunk_count: usize,
    pub avg_chunk_size: f32,
    pub date: i64,
    pub duration: f32,
    pub file_count: usize,
    pub dir_count: usize,
    pub host: String,
    pub path: String
}
serde_impl!(Backup(u8) {
    root: Vec<Chunk> => 0,
    total_data_size: u64 => 1,
    changed_data_size: u64 => 2,
    deduplicated_data_size: u64 => 3,
    encoded_data_size: u64 => 4,
    bundle_count: usize => 5,
    chunk_count: usize => 6,
    avg_chunk_size: f32 => 7,
    date: i64 => 8,
    duration: f32 => 9,
    file_count: usize => 10,
    dir_count: usize => 11,
    host: String => 12,
    path: String => 13
});

impl Backup {
    pub fn read_from<P: AsRef<Path>>(crypto: &Crypto, path: P) -> Result<Self, BackupError> {
        let path = path.as_ref();
        let mut file = BufReader::new(try!(File::open(path).context(path)));
        let mut header = [0u8; 8];
        try!(file.read_exact(&mut header).context(&path as &Path));
        if header[..HEADER_STRING.len()] != HEADER_STRING {
            return Err(BackupError::WrongHeader(path.to_path_buf()))
        }
        let version = header[HEADER_STRING.len()];
        if version != HEADER_VERSION {
            return Err(BackupError::WrongVersion(path.to_path_buf(), version))
        }
        let header: BackupHeader = try!(msgpack::decode_from_stream(&mut file).context(path));
        let mut data = Vec::new();
        try!(file.read_to_end(&mut data).context(path));
        if let Some(ref encryption) = header.encryption {
            data = try!(crypto.decrypt(encryption, &data));
        }
        Ok(try!(msgpack::decode(&data).context(path)))
    }

    pub fn save_to<P: AsRef<Path>>(&self, crypto: &Crypto, encryption: Option<Encryption>, path: P) -> Result<(), BackupError> {
        let path = path.as_ref();
        let mut data = try!(msgpack::encode(self).context(path));
        if let Some(ref encryption) = encryption {
            data = try!(crypto.encrypt(encryption, &data));
        }
        let mut file = BufWriter::new(try!(File::create(path).context(path)));
        try!(file.write_all(&HEADER_STRING).context(path));
        try!(file.write_all(&[HEADER_VERSION]).context(path));
        let header = BackupHeader { encryption: encryption };
        try!(msgpack::encode_to_stream(&header, &mut file).context(path));
        try!(file.write_all(&data).context(path));
        Ok(())
    }
}



impl Repository {
    pub fn get_backups(&self) -> Result<(HashMap<String, Backup>, bool), RepositoryError> {
        let mut backups = HashMap::new();
        let mut paths = Vec::new();
        let base_path = self.path.join("backups");
        paths.push(base_path.clone());
        let mut some_failed = false;
        while let Some(path) = paths.pop() {
            for entry in try!(fs::read_dir(path)) {
                let entry = try!(entry);
                let path = entry.path();
                if path.is_dir() {
                    paths.push(path);
                } else {
                    let relpath = path.strip_prefix(&base_path).unwrap();
                    let name = relpath.to_string_lossy().to_string();
                    if let Ok(backup) = self.get_backup(&name) {
                        backups.insert(name, backup);
                    } else {
                        some_failed = true;
                    }
                }
            }
        }
        if some_failed {
            warn!("Some backups could not be read");
        }
        Ok((backups, some_failed))
    }

    pub fn get_backup(&self, name: &str) -> Result<Backup, RepositoryError> {
        Ok(try!(Backup::read_from(&self.crypto.lock().unwrap(), self.path.join("backups").join(name))))
    }

    pub fn save_backup(&mut self, backup: &Backup, name: &str) -> Result<(), RepositoryError> {
        let path = self.path.join("backups").join(name);
        try!(fs::create_dir_all(path.parent().unwrap()));
        Ok(try!(backup.save_to(&self.crypto.lock().unwrap(), self.config.encryption.clone(), path)))
    }

    pub fn delete_backup(&self, name: &str) -> Result<(), RepositoryError> {
        let mut path = self.path.join("backups").join(name);
        try!(fs::remove_file(&path));
        loop {
            path = path.parent().unwrap().to_owned();
            if fs::remove_dir(&path).is_err() {
                break
            }
        }
        Ok(())
    }


    pub fn prune_backups(&self, prefix: &str, daily: Option<usize>, weekly: Option<usize>, monthly: Option<usize>, yearly: Option<usize>, force: bool) -> Result<(), RepositoryError> {
        let mut backups = Vec::new();
        let (backup_map, some_failed) = try!(self.get_backups());
        if some_failed {
            info!("Ignoring backups that can not be read");
        }
        for (name, backup) in backup_map {
            if name.starts_with(prefix) {
                let date = Local.timestamp(backup.date, 0);
                backups.push((name, date, backup));
            }
        }
        backups.sort_by_key(|backup| backup.2.date);
        let mut keep = Bitmap::new(backups.len());

        fn mark_needed<K: Eq, F: Fn(&DateTime<Local>) -> K>(backups: &[(String, DateTime<Local>, Backup)], keep: &mut Bitmap, max: usize, keyfn: F) {
            let mut unique = VecDeque::with_capacity(max+1);
            let mut last = None;
            for (i, backup) in backups.iter().enumerate() {
                let val = keyfn(&backup.1);
                let cur = Some(val);
                if cur != last {
                    last = cur;
                    unique.push_back(i);
                    if unique.len() > max {
                        unique.pop_front();
                    }
                }
            }
            for i in unique {
                keep.set(i);
            }
        }
        if let Some(max) = yearly {
            mark_needed(&backups, &mut keep, max, |d| d.year());
        }
        if let Some(max) = monthly {
            mark_needed(&backups, &mut keep, max, |d| (d.year(), d.month()));
        }
        if let Some(max) = weekly {
            mark_needed(&backups, &mut keep, max, |d| (d.isoweekdate().0, d.isoweekdate().1));
        }
        if let Some(max) = daily {
            mark_needed(&backups, &mut keep, max, |d| (d.year(), d.month(), d.day()));
        }
        let mut remove = Vec::new();
        for (i, backup) in backups.into_iter().enumerate() {
            if !keep.get(i) {
                remove.push(backup.0);
            }
        }
        info!("Removing the following backups: {:?}", remove);
        if force {
            for name in remove {
                try!(self.delete_backup(&name));
            }
        }
        Ok(())
    }

    pub fn restore_inode_tree<P: AsRef<Path>>(&mut self, inode: Inode, path: P) -> Result<(), RepositoryError> {
        let mut queue = VecDeque::new();
        queue.push_back((path.as_ref().to_owned(), inode));
        while let Some((path, inode)) = queue.pop_front() {
            try!(self.save_inode_at(&inode, &path));
            if inode.file_type == FileType::Directory {
                let path = path.join(inode.name);
                for chunks in inode.children.unwrap().values() {
                    let inode = try!(self.get_inode(&chunks));
                    queue.push_back((path.clone(), inode));
                }
            }
        }
        Ok(())
    }

    #[inline]
    pub fn restore_backup<P: AsRef<Path>>(&mut self, backup: &Backup, path: P) -> Result<(), RepositoryError> {
        let inode = try!(self.get_inode(&backup.root));
        self.restore_inode_tree(inode, path)
    }

    #[allow(dead_code)]
    pub fn create_backup<P: AsRef<Path>>(&mut self, path: P, reference: Option<&Backup>) -> Result<Backup, RepositoryError> {
        let reference_inode = reference.and_then(|b| self.get_inode(&b.root).ok());
        let mut scan_stack = vec![(path.as_ref().to_owned(), reference_inode)];
        let mut save_stack = vec![];
        let mut directories = HashMap::new();
        let mut backup = Backup::default();
        backup.host = get_hostname().unwrap_or_else(|_| "".to_string());
        backup.path = path.as_ref().to_string_lossy().to_string();
        let info_before = self.info();
        let start = Local::now();
        while let Some((path, reference_inode)) = scan_stack.pop() {
            // Create an inode for this path containing all attributes and contents
            // (for files) but no children (for directories)
            let mut inode = try!(self.create_inode(&path, reference_inode.as_ref()));
            backup.total_data_size += inode.size;
            if let Some(ref ref_inode) = reference_inode {
                if !ref_inode.is_unchanged(&inode) {
                    backup.changed_data_size += inode.size;
                }
            } else {
                backup.changed_data_size += inode.size;
            }
            if inode.file_type == FileType::Directory {
                backup.dir_count +=1;
                // For directories we need to put all children on the stack too, so there will be inodes created for them
                // Also we put directories on the save stack to save them in order
                save_stack.push(path.clone());
                inode.children = Some(HashMap::new());
                directories.insert(path.clone(), inode);
                for ch in try!(fs::read_dir(&path)) {
                    let child = try!(ch);
                    let name = child.file_name().to_string_lossy().to_string();
                    let ref_child = reference_inode.as_ref()
                        .and_then(|inode| inode.children.as_ref())
                        .and_then(|map| map.get(&name))
                        .and_then(|chunks| self.get_inode(chunks).ok());
                    scan_stack.push((child.path(), ref_child));
                }
            } else {
                backup.file_count +=1;
                // Non-directories are stored directly and the chunks are put into the children map of their parents
                if let Some(parent) = path.parent() {
                    let parent = parent.to_owned();
                    if !directories.contains_key(&parent) {
                        // This is a backup of one one file, put it in the directories map so it will be saved later
                        assert!(scan_stack.is_empty() && save_stack.is_empty() && directories.is_empty());
                        save_stack.push(path.clone());
                        directories.insert(path.clone(), inode);
                    } else {
                        let mut parent = directories.get_mut(&parent).unwrap();
                        let chunks = try!(self.put_inode(&inode));
                        let children = parent.children.as_mut().unwrap();
                        children.insert(inode.name.clone(), chunks);
                    }
                }
            }
        }
        loop {
            let path = save_stack.pop().unwrap();
            // Now that all children have been saved the directories can be saved in order, adding their chunks to their parents as well
            let inode = directories.remove(&path).unwrap();
            let chunks = try!(self.put_inode(&inode));
            if let Some(parent) = path.parent() {
                let parent = parent.to_owned();
                if let Some(ref mut parent) = directories.get_mut(&parent) {
                    let children = parent.children.as_mut().unwrap();
                    children.insert(inode.name.clone(), chunks);
                } else if save_stack.is_empty() {
                    backup.root = chunks;
                    break
                }
            } else if save_stack.is_empty() {
                backup.root = chunks;
                break
            }
        }
        try!(self.flush());
        let elapsed = Local::now().signed_duration_since(start);
        backup.date = start.timestamp();
        backup.duration = elapsed.num_milliseconds() as f32 / 1_000.0;
        let info_after = self.info();
        backup.deduplicated_data_size = info_after.raw_data_size - info_before.raw_data_size;
        backup.encoded_data_size = info_after.encoded_data_size - info_before.encoded_data_size;
        backup.bundle_count = info_after.bundle_count - info_before.bundle_count;
        backup.chunk_count = info_after.chunk_count - info_before.chunk_count;
        backup.avg_chunk_size = backup.deduplicated_data_size as f32 / backup.chunk_count as f32;
        Ok(backup)
    }

    pub fn get_backup_inode<P: AsRef<Path>>(&mut self, backup: &Backup, path: P) -> Result<Inode, RepositoryError> {
        let mut inode = try!(self.get_inode(&backup.root));
        for c in path.as_ref().components() {
            if let path::Component::Normal(name) = c {
                let name = name.to_string_lossy();
                if let Some(chunks) = inode.children.as_mut().and_then(|c| c.remove(&name as &str)) {
                    inode = try!(self.get_inode(&chunks));
                } else {
                    return Err(RepositoryError::NoSuchFileInBackup(backup.clone(), path.as_ref().to_owned()));
                }
            }
        }
        Ok(inode)
    }
}
