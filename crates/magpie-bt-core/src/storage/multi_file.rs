//! Multi-file `Storage` backend (ADR-0021).
//!
//! Maps a torrent's logical offset space onto a directory of files. Pairs
//! with the existing [`FileStorage`](super::FileStorage) (single-file) —
//! consumers pick the backend at construction time from the parsed
//! `FileListV1::{Single, Multi}`.
//!
//! Design mirrors `rakshasa/libtorrent`'s `FileList` + `FileManager` split:
//! the library owns all file-descriptor lifecycle, consumers supply only
//! paths and lengths. See [`MultiFileStorage::create`] for the public entry
//! point and the ADR for the full rationale.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::error::{StorageError, StorageErrorKind};
use super::traits::Storage;

/// Used as the `FdKey::storage_id` source; one per `MultiFileStorage`
/// instance process-wide.
static NEXT_STORAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Cap on the number of files in one layout.
///
/// Typical torrents have dozens; extreme cases (game installs, dataset
/// dumps) hit a few thousand. 100k is generously over that and protects
/// the library against a malformed or adversarial torrent declaring
/// millions of entries (`Vec<FileEntry>` OOM).
pub const MAX_ENTRIES: usize = 100_000;

/// Cap on a single path-component length in bytes.
///
/// Matches `NAME_MAX` on ext4/XFS/APFS/HFS+. Legitimate BitTorrent paths
/// fit well under this; the cap exists to reject malicious torrents
/// declaring gigabyte-long components that would OOM the process before
/// the filesystem ever sees them.
pub const MAX_PATH_COMPONENT_LEN: usize = 255;

// ---------------------------------------------------------------------------
// Public user-facing types
// ---------------------------------------------------------------------------

/// A single file as declared by the torrent's info dict.
///
/// Corresponds to one `info.files[]` entry (BEP 3). `path` is the ordered
/// list of path components from the bencoded dict — each component is one
/// directory or filename level. The library validates and joins them
/// against the download root; the caller does not pre-join.
#[derive(Debug, Clone)]
pub struct FileSpec {
    /// Ordered path components. Empty is rejected, `..` is rejected,
    /// absolute components are rejected.
    pub path: Vec<String>,
    /// Size of this file in bytes. Zero-length files are permitted (they
    /// occupy a slot in the torrent offset space with length 0).
    pub length: u64,
}

// ---------------------------------------------------------------------------
// Internal validated types
// ---------------------------------------------------------------------------

/// A validated file entry: the joined relative path + its span in the
/// torrent's logical offset space.
#[derive(Debug, Clone)]
struct FileEntry {
    /// Relative path from the download root. Each component has been
    /// validated as `Component::Normal`.
    path: PathBuf,
    /// First byte of this file in the torrent's logical offset space.
    torrent_offset: u64,
    /// Length of this file in bytes.
    length: u64,
}

/// Validate and resolve `specs` into a sorted list of [`FileEntry`], and
/// return the total capacity (sum of lengths).
fn validate_specs(specs: &[FileSpec]) -> Result<(Vec<FileEntry>, u64), StorageError> {
    if specs.len() > MAX_ENTRIES {
        return Err(StorageError::new(StorageErrorKind::Path(format!(
            "too many entries: {} (max {MAX_ENTRIES})",
            specs.len()
        ))));
    }
    let mut entries = Vec::with_capacity(specs.len());
    let mut seen_paths: HashMap<PathBuf, usize> = HashMap::with_capacity(specs.len());
    let mut running: u64 = 0;

    for (idx, spec) in specs.iter().enumerate() {
        let path = validate_path_components(&spec.path).map_err(|msg| {
            StorageError::new(StorageErrorKind::Path(format!("entry {idx}: {msg}")))
        })?;

        if let Some(&prev_idx) = seen_paths.get(&path) {
            return Err(StorageError::new(StorageErrorKind::Path(format!(
                "entry {idx} duplicates path already declared at entry {prev_idx}: {}",
                path.display()
            ))));
        }
        seen_paths.insert(path.clone(), idx);

        let torrent_offset = running;
        running = running.checked_add(spec.length).ok_or_else(|| {
            StorageError::new(StorageErrorKind::Path(format!(
                "total length overflow at entry {idx} (prev={running}, +{})",
                spec.length
            )))
        })?;

        entries.push(FileEntry {
            path,
            torrent_offset,
            length: spec.length,
        });
    }

    Ok((entries, running))
}

fn validate_path_components(components: &[String]) -> Result<PathBuf, String> {
    if components.is_empty() {
        return Err("path has no components".into());
    }
    let mut out = PathBuf::new();
    for (i, comp) in components.iter().enumerate() {
        if comp.is_empty() {
            return Err(format!("component {i} is empty"));
        }
        if comp.len() > MAX_PATH_COMPONENT_LEN {
            return Err(format!(
                "component {i} is {} bytes (max {MAX_PATH_COMPONENT_LEN})",
                comp.len()
            ));
        }
        if comp == "." || comp == ".." {
            return Err(format!("component {i} is {comp:?} (reserved)"));
        }
        if comp.contains('/') || comp.contains('\\') {
            return Err(format!("component {i} {comp:?} contains a path separator"));
        }
        if comp.contains('\0') {
            return Err(format!("component {i} contains an interior NUL byte"));
        }
        let probe = Path::new(comp);
        let mut comps = probe.components();
        match comps.next() {
            Some(Component::Normal(_)) if comps.next().is_none() => {}
            Some(other) => {
                return Err(format!(
                    "component {i} {comp:?} classified as non-normal ({other:?})"
                ));
            }
            None => {
                return Err(format!("component {i} {comp:?} parsed to nothing"));
            }
        }
        out.push(comp);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// FdPool — bounded LRU file-descriptor cache
// ---------------------------------------------------------------------------

/// Bounded LRU file-descriptor cache, engine-global.
///
/// Mirrors `rakshasa/libtorrent`'s `FileManager` (`_tmp/rakshasa-libtorrent/
/// src/data/file_manager.{h,cc}`): proactive soft-cap enforcement (on open,
/// if at cap, evict the least-recently-touched entry), timestamp-based LRU
/// (no linked list), no pinning. One `FdPool` is shared across all
/// [`MultiFileStorage`] instances in an `Engine`; each storage takes an
/// `Arc<FdPool>` at construction.
///
/// # Default cap
///
/// [`FdPool::default_cap`] returns **128**, matching rakshasa's tier for
/// the default Linux `RLIMIT_NOFILE` soft limit (1024). This works within
/// 8-torrent budgets under default `ulimit -n` on every mainstream OS.
/// Consumers on hosts with raised fd limits can pass a larger value
/// explicitly via [`FdPool::with_cap`] — the cap is clamped to `[4, 65536]`.
/// (We do not probe `RLIMIT_NOFILE` at runtime because that would require
/// `unsafe` under the crate's `#![deny(unsafe_code)]` discipline, for
/// marginal benefit; a fixed sensible default + explicit tuning covers
/// the important cases.)
#[derive(Debug)]
pub struct FdPool {
    cap: usize,
    inner: Mutex<FdPoolInner>,
    opens_total: AtomicU64,
}

#[derive(Debug)]
struct FdPoolInner {
    entries: HashMap<FdKey, FdSlot>,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
struct FdKey {
    storage_id: u64,
    entry_idx: u32,
}

#[derive(Debug)]
struct FdSlot {
    file: Arc<File>,
    last_touched: Instant,
}

impl FdPool {
    /// Create a pool with an explicit cap. `cap` is clamped to `[4, 65536]`
    /// to mirror rakshasa's validation (`file_manager.cc:23`).
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        let cap = cap.clamp(4, 65_536);
        Self {
            cap,
            inner: Mutex::new(FdPoolInner {
                entries: HashMap::with_capacity(cap),
            }),
            opens_total: AtomicU64::new(0),
        }
    }

    /// Create a pool with the fixed sensible default cap (128).
    #[must_use]
    pub fn with_default_cap() -> Self {
        Self::with_cap(Self::default_cap())
    }

    /// Return the cap (final, clamped value).
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Cumulative `open()` call count. Used by tests (gate 12) to prove
    /// that LRU eviction was actually exercised (counter > cap).
    #[must_use]
    pub fn opens_total(&self) -> u64 {
        self.opens_total.load(Ordering::Relaxed)
    }

    /// Fixed sensible default cap (**128**). See the type-level docs for
    /// why we don't probe `RLIMIT_NOFILE` here.
    #[must_use]
    pub const fn default_cap() -> usize {
        128
    }

    /// Return an `Arc<File>` for the file identified by `key`, opening it
    /// from `path` if not already cached. On cache miss, evicts the
    /// least-recently-used entry if the pool is at capacity.
    ///
    /// The returned `Arc<File>` outlives any eviction: callers can hold
    /// it and issue I/O after releasing the pool mutex. Eviction only
    /// drops the pool's refcount; the fd stays open until every `Arc` is
    /// gone.
    ///
    /// We deliberately hold the mutex across the `OpenOptions::open` call
    /// so two concurrent cache-misses on the same key can't both open the
    /// file. The alternative (release-open-reacquire) would require a
    /// double-insertion check and is only a win if opens are slow enough
    /// to matter — cache-miss opens are amortised by the LRU hit path.
    #[allow(clippy::significant_drop_tightening)]
    fn get_or_open(&self, key: FdKey, path: &Path) -> io::Result<Arc<File>> {
        let mut inner = self.inner.lock().expect("FdPool mutex poisoned");
        let now = Instant::now();
        if let Some(slot) = inner.entries.get_mut(&key) {
            slot.last_touched = now;
            return Ok(Arc::clone(&slot.file));
        }
        if inner.entries.len() >= self.cap
            && let Some(victim) = lru_victim(&inner.entries)
        {
            inner.entries.remove(&victim);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        self.opens_total.fetch_add(1, Ordering::Relaxed);
        let file = Arc::new(file);
        inner.entries.insert(
            key,
            FdSlot {
                file: Arc::clone(&file),
                last_touched: now,
            },
        );
        Ok(file)
    }
}

fn lru_victim(entries: &HashMap<FdKey, FdSlot>) -> Option<FdKey> {
    entries
        .iter()
        .min_by_key(|(_, slot)| slot.last_touched)
        .map(|(k, _)| *k)
}

// ---------------------------------------------------------------------------
// MultiFileStorage
// ---------------------------------------------------------------------------

/// Directory-backed `Storage` for a multi-file BitTorrent layout.
///
/// See the module docs and ADR-0021 for the design rationale.
///
/// # Construction
///
/// Use [`Self::create`] for a fresh download (files are created and
/// `set_len`-pre-allocated) or [`Self::open`] to resume against an existing
/// directory (files must already exist with the declared lengths).
///
/// # Concurrency
///
/// All trait methods take `&self` (ADR-0004 invariant). Interior mutability
/// lives in the `FdPool`'s `Mutex`.
#[derive(Debug)]
pub struct MultiFileStorage {
    storage_id: u64,
    entries: Vec<FileEntry>,
    root: PathBuf,
    capacity: u64,
    fd_pool: Arc<FdPool>,
}

impl MultiFileStorage {
    /// Create a new multi-file layout under `root`. Each file is created,
    /// truncated, and `set_len`-ed to its declared length (sparse on
    /// filesystems that support it). `root` must already exist.
    pub fn create(
        root: impl AsRef<Path>,
        files: &[FileSpec],
        fd_pool: Arc<FdPool>,
    ) -> Result<Self, StorageError> {
        Self::construct(root, files, fd_pool, true)
    }

    /// Open an existing multi-file layout under `root`. Every declared
    /// file must exist with matching length. Use this for resume.
    pub fn open(
        root: impl AsRef<Path>,
        files: &[FileSpec],
        fd_pool: Arc<FdPool>,
    ) -> Result<Self, StorageError> {
        Self::construct(root, files, fd_pool, false)
    }

    fn construct(
        root: impl AsRef<Path>,
        files: &[FileSpec],
        fd_pool: Arc<FdPool>,
        create: bool,
    ) -> Result<Self, StorageError> {
        let root_in = root.as_ref();
        let root = root_in.canonicalize().map_err(|e| {
            StorageError::new(StorageErrorKind::Path(format!(
                "cannot canonicalise root {}: {e}",
                root_in.display()
            )))
        })?;
        if !root.is_dir() {
            return Err(StorageError::new(StorageErrorKind::Path(format!(
                "root {} is not a directory",
                root.display()
            ))));
        }

        let (entries, capacity) = validate_specs(files)?;

        for (idx, entry) in entries.iter().enumerate() {
            let full = root.join(&entry.path);
            check_path_stays_under_root(&root, &full).map_err(|msg| {
                StorageError::new(StorageErrorKind::Path(format!("entry {idx}: {msg}")))
            })?;
        }

        if create {
            for (idx, entry) in entries.iter().enumerate() {
                let full = root.join(&entry.path);
                if let Some(parent) = full.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        StorageError::new(StorageErrorKind::Path(format!(
                            "entry {idx}: cannot create parent dirs for {}: {e}",
                            full.display()
                        )))
                    })?;
                }
                // Second line of defense: re-check for a symlink at the
                // leaf immediately before open, to narrow the TOCTOU
                // window with `check_path_stays_under_root`. A racing
                // attacker can still swap in a symlink between this and
                // `open`, but for that they already need local write
                // access to the download root — a much more invasive
                // threat than the one we're guarding against.
                if let Ok(md) = std::fs::symlink_metadata(&full)
                    && md.file_type().is_symlink()
                {
                    return Err(StorageError::new(StorageErrorKind::Path(format!(
                        "entry {idx}: {} exists as a symlink; refusing to create through it",
                        full.display()
                    ))));
                }
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&full)
                    .map_err(|e| {
                        StorageError::new(StorageErrorKind::Path(format!(
                            "entry {idx}: cannot create {}: {e}",
                            full.display()
                        )))
                    })?;
                file.set_len(entry.length).map_err(|e| {
                    StorageError::new(StorageErrorKind::Path(format!(
                        "entry {idx}: set_len({}) failed on {}: {e}",
                        entry.length,
                        full.display()
                    )))
                })?;
            }
        } else {
            for (idx, entry) in entries.iter().enumerate() {
                let full = root.join(&entry.path);
                let md = std::fs::metadata(&full).map_err(|e| {
                    StorageError::new(StorageErrorKind::Path(format!(
                        "entry {idx}: {} missing: {e}",
                        full.display()
                    )))
                })?;
                if md.len() != entry.length {
                    return Err(StorageError::new(StorageErrorKind::Path(format!(
                        "entry {idx}: {} has len {} (expected {})",
                        full.display(),
                        md.len(),
                        entry.length
                    ))));
                }
            }
        }

        Ok(Self {
            storage_id: NEXT_STORAGE_ID.fetch_add(1, Ordering::Relaxed),
            entries,
            root,
            capacity,
            fd_pool,
        })
    }

    /// Create a multi-file storage from a parsed [`Info`](magpie_bt_metainfo::Info)
    /// dictionary. Routes to the v1 file list (present on both v1 and
    /// hybrid torrents). Rejects v2-only torrents and single-file torrents
    /// with a structured error.
    ///
    /// Fresh-download variant: files are created and pre-allocated. Use
    /// [`Self::open_from_info`] for resume.
    pub fn create_from_info(
        root: impl AsRef<Path>,
        info: &magpie_bt_metainfo::Info<'_>,
        fd_pool: Arc<FdPool>,
    ) -> Result<Self, StorageError> {
        let specs = specs_from_info(info)?;
        Self::create(root, &specs, fd_pool)
    }

    /// Resume variant of [`Self::create_from_info`]: all files must exist
    /// on disk with the declared lengths.
    pub fn open_from_info(
        root: impl AsRef<Path>,
        info: &magpie_bt_metainfo::Info<'_>,
        fd_pool: Arc<FdPool>,
    ) -> Result<Self, StorageError> {
        let specs = specs_from_info(info)?;
        Self::open(root, &specs, fd_pool)
    }

    /// Download root (canonical path).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Find the first entry whose range covers `offset`.
    ///
    /// `partition_point` returns the first entry whose `end > offset`,
    /// i.e. the entry that contains `offset`. Zero-length entries are
    /// transparent: they have `end == torrent_offset`, so they sort
    /// naturally with `partition_point` and the walk loop skips them.
    fn starting_entry(&self, offset: u64) -> usize {
        self.entries
            .partition_point(|e| e.torrent_offset + e.length <= offset)
    }

    fn bounds_check(&self, offset: u64, len: u64) -> Result<(), StorageError> {
        let end = offset.checked_add(len).ok_or_else(|| {
            StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len,
                capacity: self.capacity,
            })
        })?;
        if end > self.capacity {
            return Err(StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len,
                capacity: self.capacity,
            }));
        }
        Ok(())
    }

    fn get_fd(&self, entry_idx: usize) -> io::Result<Arc<File>> {
        let entry = &self.entries[entry_idx];
        let key = FdKey {
            storage_id: self.storage_id,
            entry_idx: u32::try_from(entry_idx).expect("entry_idx fits u32"),
        };
        let full = self.root.join(&entry.path);
        self.fd_pool.get_or_open(key, &full)
    }
}

/// Reject if `full` resolves outside `root`. We tolerate missing leaves
/// (the file doesn't exist yet) by walking upward *only on* `NotFound`,
/// checking containment against the nearest real ancestor.
///
/// Explicitly rejects symlink leaves (including dangling symlinks): if
/// `symlink_metadata(full)` reports a symlink, we refuse even if
/// `canonicalize` would walk through it. Without this, a pre-existing
/// dangling symlink `dir/file -> /etc/target` would let
/// `OpenOptions::create(true).truncate(true)` create `/etc/target`.
fn check_path_stays_under_root(root: &Path, full: &Path) -> Result<(), String> {
    // Leaf symlink check: refuse regardless of where it points, because
    // `create(true)` will follow it.
    if let Ok(md) = std::fs::symlink_metadata(full)
        && md.file_type().is_symlink()
    {
        return Err(format!(
            "{} exists as a symlink; refusing to open through it",
            full.display()
        ));
    }
    let mut anchor: PathBuf = full.to_path_buf();
    loop {
        match anchor.canonicalize() {
            Ok(canonical) => {
                if canonical == *root || canonical.starts_with(root) {
                    return Ok(());
                }
                return Err(format!(
                    "{} resolves to {}, which is outside {}",
                    full.display(),
                    canonical.display(),
                    root.display()
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Missing intermediate dir or leaf — walk up.
                if !anchor.pop() {
                    return Err(format!("cannot resolve any ancestor of {}", full.display()));
                }
            }
            Err(e) => {
                // Permission-denied, interrupted, or any other error:
                // refuse to guess — fail closed.
                return Err(format!("cannot canonicalise {}: {e}", anchor.display()));
            }
        }
    }
}

impl Storage for MultiFileStorage {
    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn write_block(&self, offset: u64, buf: &[u8]) -> Result<(), StorageError> {
        let len_u64 = u64::try_from(buf.len()).map_err(|_| {
            StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len: u64::MAX,
                capacity: self.capacity,
            })
        })?;
        self.bounds_check(offset, len_u64)?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut cursor = offset;
        let mut remaining = buf;
        let mut idx = self.starting_entry(offset);
        while !remaining.is_empty() {
            debug_assert!(idx < self.entries.len(), "walk past last entry");
            let entry = &self.entries[idx];
            if entry.length == 0 {
                idx += 1;
                continue;
            }
            let file_offset = cursor - entry.torrent_offset;
            let available = entry.length - file_offset;
            let take = usize::try_from(u64::min(
                u64::try_from(remaining.len()).unwrap_or(u64::MAX),
                available,
            ))
            .expect("take fits usize (it is at most remaining.len())");
            let fd = self.get_fd(idx)?;
            fd.write_all_at(&remaining[..take], file_offset)?;
            remaining = &remaining[take..];
            cursor += take as u64;
            idx += 1;
        }
        Ok(())
    }

    fn read_block(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let len_u64 = u64::try_from(buf.len()).map_err(|_| {
            StorageError::new(StorageErrorKind::OutOfBounds {
                offset,
                len: u64::MAX,
                capacity: self.capacity,
            })
        })?;
        self.bounds_check(offset, len_u64)?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut cursor = offset;
        let mut remaining = buf;
        let mut idx = self.starting_entry(offset);
        while !remaining.is_empty() {
            debug_assert!(idx < self.entries.len(), "walk past last entry");
            let entry = &self.entries[idx];
            if entry.length == 0 {
                idx += 1;
                continue;
            }
            let file_offset = cursor - entry.torrent_offset;
            let available = entry.length - file_offset;
            let take = usize::try_from(u64::min(
                u64::try_from(remaining.len()).unwrap_or(u64::MAX),
                available,
            ))
            .expect("take fits usize (it is at most remaining.len())");
            let fd = self.get_fd(idx)?;
            let (target, rest) = remaining.split_at_mut(take);
            fd.read_exact_at(target, file_offset)?;
            remaining = rest;
            cursor += take as u64;
            idx += 1;
        }
        Ok(())
    }

    fn delete(&self) -> Result<(), StorageError> {
        let mut touched_dirs: Vec<PathBuf> = Vec::new();
        for entry in &self.entries {
            let full = self.root.join(&entry.path);
            match std::fs::remove_file(&full) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(StorageError::from(e)),
            }
            if let Some(parent) = full.parent()
                && parent != self.root
            {
                touched_dirs.push(parent.to_path_buf());
            }
        }
        touched_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        touched_dirs.dedup();
        for dir in touched_dirs {
            prune_empty_dirs(&dir, &self.root);
        }
        Ok(())
    }
}

/// Convert a parsed info dict into the `FileSpec` list `MultiFileStorage`
/// expects. Uses v1's file list (present on hybrid and v1-only torrents).
/// Rejects v2-only and single-file with `StorageErrorKind::Path`.
fn specs_from_info(info: &magpie_bt_metainfo::Info<'_>) -> Result<Vec<FileSpec>, StorageError> {
    let Some(v1) = info.v1.as_ref() else {
        return Err(StorageError::new(StorageErrorKind::Path(
            "multi-file storage requires a v1 info dict; this torrent is v2-only (BEP 52 \
             v2-tree storage is a future milestone)"
                .into(),
        )));
    };
    let files = match &v1.files {
        magpie_bt_metainfo::FileListV1::Multi { files } => files,
        magpie_bt_metainfo::FileListV1::Single { .. } => {
            return Err(StorageError::new(StorageErrorKind::Path(
                "this is a single-file torrent; use FileStorage instead of MultiFileStorage".into(),
            )));
        }
    };
    let mut specs = Vec::with_capacity(files.len());
    for (idx, f) in files.iter().enumerate() {
        let mut path_components = Vec::with_capacity(f.path.len());
        for (c_idx, comp) in f.path.iter().enumerate() {
            let s = std::str::from_utf8(comp).map_err(|_| {
                StorageError::new(StorageErrorKind::Path(format!(
                    "entry {idx} component {c_idx} is not valid UTF-8"
                )))
            })?;
            path_components.push(s.to_owned());
        }
        specs.push(FileSpec {
            path: path_components,
            length: f.length,
        });
    }
    Ok(specs)
}

fn prune_empty_dirs(start: &Path, stop_at: &Path) {
    let mut cur: PathBuf = start.to_path_buf();
    while cur != *stop_at && cur.starts_with(stop_at) {
        match std::fs::read_dir(&cur) {
            Ok(mut it) => {
                if it.next().is_some() {
                    return;
                }
            }
            Err(_) => return,
        }
        if std::fs::remove_dir(&cur).is_err() {
            return;
        }
        if !cur.pop() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
// Most tests in this module create real files via FileStorage and FdPool.
// The remaining tests cover path-spec validation (`validate_specs`) and
// FdPool capacity clamps — pure safe Rust with no unsafe code, so miri
// adds little signal there. Excluding the whole module from miri keeps
// new FS tests auto-covered. See docs/DISCIPLINES.md.
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn spec(path: &[&str], length: u64) -> FileSpec {
        FileSpec {
            path: path.iter().map(|s| (*s).to_owned()).collect(),
            length,
        }
    }

    fn path_err(err: StorageError) -> String {
        match err.kind {
            StorageErrorKind::Path(msg) => msg,
            other => panic!("expected Path error, got {other:?}"),
        }
    }

    // ----- path validation -----

    #[test]
    fn empty_components_rejected() {
        let msg = path_err(validate_specs(&[spec(&[], 1)]).unwrap_err());
        assert!(msg.contains("no components"), "msg={msg}");
    }

    #[test]
    fn empty_string_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["", "file.bin"], 1)]).unwrap_err());
        assert!(msg.contains("empty"), "msg={msg}");
    }

    #[test]
    fn parent_dir_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["..", "evil.bin"], 1)]).unwrap_err());
        assert!(msg.contains("reserved"), "msg={msg}");
    }

    #[test]
    fn current_dir_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&[".", "file.bin"], 1)]).unwrap_err());
        assert!(msg.contains("reserved"), "msg={msg}");
    }

    #[test]
    fn path_separator_in_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["a/b"], 1)]).unwrap_err());
        assert!(msg.contains("separator"), "msg={msg}");
    }

    #[test]
    fn backslash_in_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["a\\b"], 1)]).unwrap_err());
        assert!(msg.contains("separator"), "msg={msg}");
    }

    #[test]
    fn nul_byte_in_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["a\0b"], 1)]).unwrap_err());
        assert!(msg.contains("NUL"), "msg={msg}");
    }

    #[test]
    fn absolute_component_rejected() {
        let msg = path_err(validate_specs(&[spec(&["/"], 1)]).unwrap_err());
        assert!(
            msg.contains("separator") || msg.contains("non-normal"),
            "msg={msg}"
        );
    }

    #[test]
    fn duplicate_path_rejected() {
        let err = validate_specs(&[spec(&["a", "file.bin"], 1), spec(&["a", "file.bin"], 1)])
            .unwrap_err();
        assert!(path_err(err).contains("duplicates"));
    }

    #[test]
    fn overflow_total_length_rejected() {
        let err = validate_specs(&[spec(&["a"], u64::MAX), spec(&["b"], 1)]).unwrap_err();
        assert!(path_err(err).contains("overflow"));
    }

    #[test]
    fn happy_path_two_files() {
        let (entries, cap) = validate_specs(&[
            spec(&["dir", "first.bin"], 100),
            spec(&["dir", "second.bin"], 50),
        ])
        .unwrap();
        assert_eq!(cap, 150);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].torrent_offset, 0);
        assert_eq!(entries[0].length, 100);
        assert_eq!(entries[0].path, PathBuf::from("dir").join("first.bin"));
        assert_eq!(entries[1].torrent_offset, 100);
        assert_eq!(entries[1].length, 50);
    }

    #[test]
    fn happy_path_with_zero_length() {
        let (entries, cap) =
            validate_specs(&[spec(&["a"], 10), spec(&["b"], 0), spec(&["c"], 20)]).unwrap();
        assert_eq!(cap, 30);
        assert_eq!(entries[0].torrent_offset, 0);
        assert_eq!(entries[1].torrent_offset, 10);
        assert_eq!(entries[1].length, 0);
        assert_eq!(entries[2].torrent_offset, 10);
        assert_eq!(entries[2].length, 20);
    }

    // ----- FdPool -----

    #[test]
    fn fd_pool_cap_clamped() {
        assert_eq!(FdPool::with_cap(0).cap(), 4);
        assert_eq!(FdPool::with_cap(1_000_000).cap(), 65_536);
        assert_eq!(FdPool::with_cap(128).cap(), 128);
    }

    #[test]
    fn default_cap_is_128() {
        assert_eq!(FdPool::default_cap(), 128);
        assert_eq!(FdPool::with_default_cap().cap(), 128);
    }

    #[test]
    fn fd_pool_cache_hit_reuses_fd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        let pool = FdPool::with_cap(4);
        let key = FdKey {
            storage_id: 1,
            entry_idx: 0,
        };
        let a = pool.get_or_open(key, &path).unwrap();
        let b = pool.get_or_open(key, &path).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(pool.opens_total(), 1);
    }

    #[test]
    fn fd_pool_evicts_lru_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5u32 {
            std::fs::write(dir.path().join(format!("f{i}")), b"").unwrap();
        }
        let pool = FdPool::with_cap(4);
        for i in 0..4u32 {
            let key = FdKey {
                storage_id: 1,
                entry_idx: i,
            };
            pool.get_or_open(key, &dir.path().join(format!("f{i}")))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(pool.opens_total(), 4);
        // Touch f0 so f1 becomes LRU.
        let key0 = FdKey {
            storage_id: 1,
            entry_idx: 0,
        };
        pool.get_or_open(key0, &dir.path().join("f0")).unwrap();
        // Open f4 — forces eviction of f1.
        let key4 = FdKey {
            storage_id: 1,
            entry_idx: 4,
        };
        pool.get_or_open(key4, &dir.path().join("f4")).unwrap();
        assert_eq!(pool.opens_total(), 5);
        // f1 was evicted: re-open increments counter.
        let key1 = FdKey {
            storage_id: 1,
            entry_idx: 1,
        };
        pool.get_or_open(key1, &dir.path().join("f1")).unwrap();
        assert_eq!(pool.opens_total(), 6);
        // f0 still cached: hit does not increment.
        pool.get_or_open(key0, &dir.path().join("f0")).unwrap();
        assert_eq!(pool.opens_total(), 6);
    }

    #[test]
    fn fd_pool_different_storage_ids_dont_collide() {
        let dir = tempfile::tempdir().unwrap();
        let p0 = dir.path().join("a");
        let p1 = dir.path().join("b");
        std::fs::write(&p0, b"x").unwrap();
        std::fs::write(&p1, b"y").unwrap();
        let pool = FdPool::with_cap(4);
        pool.get_or_open(
            FdKey {
                storage_id: 1,
                entry_idx: 0,
            },
            &p0,
        )
        .unwrap();
        pool.get_or_open(
            FdKey {
                storage_id: 2,
                entry_idx: 0,
            },
            &p1,
        )
        .unwrap();
        assert_eq!(pool.opens_total(), 2);
    }

    #[test]
    fn fd_pool_eviction_does_not_close_held_arc() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6u32 {
            std::fs::write(dir.path().join(format!("f{i}")), b"data").unwrap();
        }
        let pool = FdPool::with_cap(4);
        let held = pool
            .get_or_open(
                FdKey {
                    storage_id: 1,
                    entry_idx: 0,
                },
                &dir.path().join("f0"),
            )
            .unwrap();
        for i in 1..5u32 {
            pool.get_or_open(
                FdKey {
                    storage_id: 1,
                    entry_idx: i,
                },
                &dir.path().join(format!("f{i}")),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut buf = [0u8; 4];
        held.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"data");
    }

    // ----- MultiFileStorage -----

    fn build_storage(root: &Path, specs: &[FileSpec]) -> MultiFileStorage {
        MultiFileStorage::create(root, specs, Arc::new(FdPool::with_cap(8))).unwrap()
    }

    #[test]
    fn create_makes_files_with_declared_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let specs = vec![spec(&["a"], 100), spec(&["sub", "b"], 50), spec(&["c"], 0)];
        let s = build_storage(dir.path(), &specs);
        assert_eq!(s.capacity(), 150);
        assert_eq!(std::fs::metadata(dir.path().join("a")).unwrap().len(), 100);
        assert_eq!(
            std::fs::metadata(dir.path().join("sub").join("b"))
                .unwrap()
                .len(),
            50
        );
        assert_eq!(std::fs::metadata(dir.path().join("c")).unwrap().len(), 0);
    }

    #[test]
    fn write_read_within_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(dir.path(), &[spec(&["a"], 100), spec(&["b"], 100)]);
        s.write_block(10, &[1, 2, 3, 4, 5]).unwrap();
        let mut out = [0u8; 5];
        s.read_block(10, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn write_read_spans_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(dir.path(), &[spec(&["a"], 10), spec(&["b"], 10)]);
        // Write 4 bytes starting at offset 8 → 2 in file a, 2 in file b.
        s.write_block(8, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u8; 4];
        s.read_block(8, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
        // Verify the split on disk.
        let a_bytes = std::fs::read(dir.path().join("a")).unwrap();
        let b_bytes = std::fs::read(dir.path().join("b")).unwrap();
        assert_eq!(&a_bytes[8..10], &[1, 2]);
        assert_eq!(&b_bytes[0..2], &[3, 4]);
    }

    #[test]
    fn write_read_spans_three_files_with_zero_in_middle() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(
            dir.path(),
            &[spec(&["a"], 5), spec(&["empty"], 0), spec(&["b"], 5)],
        );
        s.write_block(3, &[1, 2, 3, 4]).unwrap();
        let mut out = [0u8; 4];
        s.read_block(3, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4]);
        let a = std::fs::read(dir.path().join("a")).unwrap();
        let b = std::fs::read(dir.path().join("b")).unwrap();
        assert_eq!(&a[3..5], &[1, 2]);
        assert_eq!(&b[0..2], &[3, 4]);
    }

    #[test]
    fn write_at_offset_zero_and_exact_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(dir.path(), &[spec(&["a"], 4), spec(&["b"], 4)]);
        // Write straddles the exact boundary offset 4.
        s.write_block(0, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut out = [0u8; 8];
        s.read_block(0, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn read_past_capacity_is_out_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(dir.path(), &[spec(&["a"], 10)]);
        let mut buf = [0u8; 10];
        let err = s.read_block(5, &mut buf).unwrap_err();
        assert!(matches!(err.kind, StorageErrorKind::OutOfBounds { .. }));
    }

    #[test]
    fn delete_removes_files_and_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let s = build_storage(
            root,
            &[
                spec(&["d1", "a"], 10),
                spec(&["d1", "b"], 10),
                spec(&["d2", "c"], 10),
            ],
        );
        assert!(root.join("d1").is_dir());
        assert!(root.join("d2").is_dir());
        s.delete().unwrap();
        assert!(!root.join("d1").exists());
        assert!(!root.join("d2").exists());
        // Root itself is preserved.
        assert!(root.is_dir());
    }

    #[test]
    fn open_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::open(dir.path(), &[spec(&["absent"], 1)], pool).unwrap_err();
        assert!(matches!(err.kind, StorageErrorKind::Path(_)));
    }

    #[test]
    fn open_rejects_wrong_length() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"short").unwrap();
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::open(dir.path(), &[spec(&["f"], 100)], pool).unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("has len"), "msg={msg}");
    }

    #[test]
    fn too_many_entries_rejected() {
        let specs: Vec<FileSpec> = (0..=MAX_ENTRIES)
            .map(|i| spec(&[&format!("f{i}")], 1))
            .collect();
        let err = validate_specs(&specs).unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("too many entries"), "msg={msg}");
    }

    #[test]
    fn component_too_long_rejected() {
        let long = "a".repeat(MAX_PATH_COMPONENT_LEN + 1);
        let err = validate_specs(&[FileSpec {
            path: vec![long],
            length: 1,
        }])
        .unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("bytes"), "msg={msg}");
    }

    #[test]
    fn component_at_limit_accepted() {
        // Exactly MAX_PATH_COMPONENT_LEN must be accepted (boundary).
        let at_limit = "a".repeat(MAX_PATH_COMPONENT_LEN);
        let (entries, _) = validate_specs(&[FileSpec {
            path: vec![at_limit],
            length: 1,
        }])
        .expect("exactly MAX_PATH_COMPONENT_LEN must be accepted");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn dangling_symlink_leaf_rejected() {
        // A pre-existing dangling symlink at the leaf would let
        // OpenOptions::create(true).truncate(true) create the target.
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("evil");
        std::os::unix::fs::symlink("/nonexistent/target", &leaf).unwrap();
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::create(dir.path(), &[spec(&["evil"], 10)], pool).unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("symlink"), "msg={msg}");
    }

    #[test]
    fn symlink_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Create dir/escape -> outside/, then try to place a file under it.
        let link_path = dir.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link_path).unwrap();
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::create(dir.path(), &[spec(&["escape", "hosed"], 1)], pool)
            .unwrap_err();
        let msg = path_err(err);
        assert!(
            msg.contains("outside") || msg.contains("resolves"),
            "msg={msg}"
        );
    }

    #[test]
    fn capacity_matches_sum_of_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_storage(
            dir.path(),
            &[spec(&["a"], 3), spec(&["b"], 5), spec(&["c"], 7)],
        );
        assert_eq!(s.capacity(), 15);
    }

    // ----- from_info bridge -----

    /// Build a minimal `Info` with a multi-file v1 list, for testing.
    fn info_v1_multi<'a>(
        files: &'a [magpie_bt_metainfo::FileV1<'a>],
    ) -> magpie_bt_metainfo::Info<'a> {
        magpie_bt_metainfo::Info {
            name: b"root",
            piece_length: 16_384,
            private: false,
            v1: Some(magpie_bt_metainfo::InfoV1 {
                pieces: &[],
                files: magpie_bt_metainfo::FileListV1::Multi {
                    files: files.to_vec(),
                },
            }),
            v2: None,
        }
    }

    #[test]
    fn create_from_info_v1_multi_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            magpie_bt_metainfo::FileV1 {
                length: 10,
                path: vec![b"a"],
            },
            magpie_bt_metainfo::FileV1 {
                length: 10,
                path: vec![b"sub", b"b"],
            },
        ];
        let info = info_v1_multi(&files);
        let pool = Arc::new(FdPool::with_cap(4));
        let s = MultiFileStorage::create_from_info(dir.path(), &info, pool).unwrap();
        assert_eq!(s.capacity(), 20);
        s.write_block(5, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut out = [0u8; 8];
        s.read_block(5, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn from_info_rejects_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let info = magpie_bt_metainfo::Info {
            name: b"root",
            piece_length: 16_384,
            private: false,
            v1: Some(magpie_bt_metainfo::InfoV1 {
                pieces: &[],
                files: magpie_bt_metainfo::FileListV1::Single { length: 100 },
            }),
            v2: None,
        };
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::create_from_info(dir.path(), &info, pool).unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("single-file"), "msg={msg}");
    }

    #[test]
    fn from_info_rejects_v2_only() {
        let dir = tempfile::tempdir().unwrap();
        let info = magpie_bt_metainfo::Info {
            name: b"root",
            piece_length: 16_384,
            private: false,
            v1: None,
            v2: Some(magpie_bt_metainfo::InfoV2 {
                meta_version: 2,
                file_tree: magpie_bt_metainfo::FileTreeNode::File {
                    length: 100,
                    pieces_root: None,
                },
            }),
        };
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::create_from_info(dir.path(), &info, pool).unwrap_err();
        let msg = path_err(err);
        assert!(
            msg.contains("v2-only") || msg.contains("v1 info dict"),
            "msg={msg}"
        );
    }

    #[test]
    fn from_info_accepts_hybrid() {
        // Hybrid torrents have both v1 and v2 populated. We route to v1.
        let dir = tempfile::tempdir().unwrap();
        let v1_files = [magpie_bt_metainfo::FileV1 {
            length: 10,
            path: vec![b"h"],
        }];
        let info = magpie_bt_metainfo::Info {
            name: b"root",
            piece_length: 16_384,
            private: false,
            v1: Some(magpie_bt_metainfo::InfoV1 {
                pieces: &[],
                files: magpie_bt_metainfo::FileListV1::Multi {
                    files: v1_files.to_vec(),
                },
            }),
            v2: Some(magpie_bt_metainfo::InfoV2 {
                meta_version: 2,
                file_tree: magpie_bt_metainfo::FileTreeNode::File {
                    length: 10,
                    pieces_root: None,
                },
            }),
        };
        let pool = Arc::new(FdPool::with_cap(4));
        let s = MultiFileStorage::create_from_info(dir.path(), &info, pool).unwrap();
        assert_eq!(s.capacity(), 10);
    }

    #[test]
    fn from_info_rejects_non_utf8_path() {
        let dir = tempfile::tempdir().unwrap();
        let bad = [0xff, 0xfe, 0xfd];
        let files = [magpie_bt_metainfo::FileV1 {
            length: 10,
            path: vec![&bad],
        }];
        let info = info_v1_multi(&files);
        let pool = Arc::new(FdPool::with_cap(4));
        let err = MultiFileStorage::create_from_info(dir.path(), &info, pool).unwrap_err();
        let msg = path_err(err);
        assert!(msg.contains("UTF-8"), "msg={msg}");
    }

    #[test]
    fn open_from_info_resumes_existing() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            magpie_bt_metainfo::FileV1 {
                length: 10,
                path: vec![b"a"],
            },
            magpie_bt_metainfo::FileV1 {
                length: 10,
                path: vec![b"b"],
            },
        ];
        let info = info_v1_multi(&files);
        let pool = Arc::new(FdPool::with_cap(4));
        // Create first to lay the files down.
        let s1 = MultiFileStorage::create_from_info(dir.path(), &info, Arc::clone(&pool)).unwrap();
        s1.write_block(0, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
            .unwrap();
        drop(s1);
        // Re-open and verify content survives.
        let s2 = MultiFileStorage::open_from_info(dir.path(), &info, pool).unwrap();
        let mut out = [0u8; 12];
        s2.read_block(0, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }
}
