use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const PREVIEW_CHARS: usize = 2000;
pub const READ_LINES: usize = 2000;

/// Files hold originals. The process keeps only configuration and transient file metadata.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    ttl: Duration,
    max_bytes: u64,
    max_entries: usize,
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Lives outside /tmp: boot-time /tmp cleanup removes it, and Docker Desktop then recreates a
/// missing bind source as root, which this store rejects.
pub fn default_root() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/state/codex-turn-proxy/tool-results")
}

impl Store {
    pub fn new(
        root: PathBuf,
        ttl: Duration,
        max_bytes: u64,
        max_entries: usize,
    ) -> io::Result<Self> {
        if !root.is_absolute() || ttl.is_zero() || max_bytes == 0 || max_entries == 0 {
            return Err(io::Error::other("invalid output store configuration"));
        }
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
        {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&root)?;
        // SAFETY: geteuid has no preconditions.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "output directory must belong to this user with mode 0700",
            ));
        }
        Ok(Self {
            root,
            ttl,
            max_bytes,
            max_entries,
        })
    }

    pub fn path(&self, id: &str) -> io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid output ID",
            ));
        }
        Ok(self.root.join(format!("{id}.txt")))
    }

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(".lock"))?;
        // SAFETY: the file descriptor remains open for the lifetime of this lock guard.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }

    fn open(&self, id: &str) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(id)?)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_OUTPUT_BYTES as u64 {
            return Err(io::Error::other("invalid stored output file"));
        }
        Ok(file)
    }

    /// Serializes capture, reads, and cleanup across host processes and the container sweeper.
    pub fn purge_expired(&self, now: SystemTime) -> io::Result<usize> {
        let _lock = self.lock()?;
        self.prune(now, 0, 0)
    }

    fn prune(
        &self,
        now: SystemTime,
        reserve_bytes: u64,
        reserve_entries: usize,
    ) -> io::Result<usize> {
        let mut live = Vec::new();
        let mut total = 0u64;
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            // No live writer can hold this lock concurrently. Remove interrupted writes.
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(".pending-"))
                && entry.file_type()?.is_file()
            {
                fs::remove_file(entry.path())?;
                removed += 1;
                continue;
            }
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".txt")) else {
                continue;
            };
            if !valid_id(id) || !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            let modified = metadata.modified()?;
            if now.duration_since(modified).unwrap_or_default() >= self.ttl {
                fs::remove_file(entry.path())?;
                removed += 1;
            } else {
                total += metadata.len();
                live.push((modified, metadata.len(), entry.path()));
            }
        }
        if total + reserve_bytes <= self.max_bytes
            && live.len() + reserve_entries <= self.max_entries
        {
            return Ok(removed);
        }
        live.sort_unstable_by_key(|entry| entry.0);
        let mut count = live.len();
        for (_, bytes, path) in live {
            if total + reserve_bytes <= self.max_bytes
                && count + reserve_entries <= self.max_entries
            {
                break;
            }
            fs::remove_file(path)?;
            total -= bytes;
            count -= 1;
            removed += 1;
        }
        Ok(removed)
    }

    pub fn persist(
        &self,
        session: &str,
        tool_id: &str,
        code: i32,
        output: &[u8],
        binary: &Path,
    ) -> io::Result<String> {
        if [session, tool_id].iter().any(|value| {
            value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) || output.len() > MAX_OUTPUT_BYTES
            || output.len() as u64 > self.max_bytes
        {
            return Err(io::Error::other(
                "output exceeds store limits or lacks tool identity",
            ));
        }
        let mut hash = blake3::Hasher::new();
        hash.update(session.as_bytes());
        hash.update(&[0]);
        hash.update(tool_id.as_bytes());
        let id = hash.finalize().to_hex().to_string();
        let path = self.path(&id)?;
        let _lock = self.lock()?;
        match self.open(&id) {
            Ok(mut file) => {
                let mut existing = Vec::new();
                io::Read::read_to_end(&mut file, &mut existing)?;
                if existing != output {
                    return Err(io::Error::other(
                        "tool ID already contains different output",
                    ));
                }
                file.set_modified(SystemTime::now())?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.prune(SystemTime::now(), output.len() as u64, 1)?;
                let mut staging = tempfile::Builder::new()
                    .prefix(".pending-")
                    .tempfile_in(&self.root)?;
                staging.write_all(output)?;
                staging
                    .persist_noclobber(&path)
                    .map_err(|error| error.error)?;
            }
            Err(error) => return Err(error),
        }
        let preview: String = String::from_utf8_lossy(output)
            .chars()
            .take(PREVIEW_CHARS)
            .collect();
        Ok(format!(
            "<persisted-output>\nID: {id}\nBash exit: {code} | {} bytes\nFull output saved to: {}\nRead when needed: {} read {id} 1 {READ_LINES}\nPreview (first {PREVIEW_CHARS} characters at most):\n{preview}\n</persisted-output>",
            output.len(),
            path.display(),
            crate::cli::quote(&binary.to_string_lossy())
        ))
    }

    pub fn read(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
        out: &mut impl Write,
    ) -> io::Result<()> {
        if offset == 0 || limit == 0 || limit > READ_LINES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset must be positive; limit must be 1..=2000",
            ));
        }
        let _lock = self.lock()?;
        let file = self.open(id)?;
        let now = SystemTime::now();
        if now
            .duration_since(file.metadata()?.modified()?)
            .unwrap_or_default()
            >= self.ttl
        {
            fs::remove_file(self.path(id)?)?;
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "original expired; rerun the original tool",
            ));
        }
        file.set_modified(now)?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut number = 0usize;
        while reader.read_until(b'\n', &mut line)? != 0 {
            number += 1;
            if number >= offset {
                if number - offset >= limit {
                    writeln!(
                        out,
                        "\n[Output continues; read ID {id} with offset {number}]"
                    )?;
                    break;
                }
                out.write_all(&line)?;
            }
            line.clear();
        }
        Ok(())
    }
}
