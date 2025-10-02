use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use filetime::{set_file_mtime, FileTime};
use flate2::read::{GzDecoder, GzEncoder};
use flate2::Compression;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct DiskCacheConfig {
    pub root: PathBuf,
    pub cap_bytes: u64,
    pub sweep_interval: Duration,
}

pub struct DiskCache {
    cfg: DiskCacheConfig,
    size_bytes: Arc<Mutex<u64>>, // cached approximate current size
}

impl DiskCache {
    pub async fn new(cfg: DiskCacheConfig) -> io::Result<Arc<Self>> {
        tokio::fs::create_dir_all(&cfg.root).await?;
        let size = initial_size(&cfg.root).await.unwrap_or(0);
        let me = Arc::new(Self {
            cfg,
            size_bytes: Arc::new(Mutex::new(size)),
        });
        if me.cfg.cap_bytes > 0 {
            Self::spawn_sweeper(me.clone());
        }
        Ok(me)
    }

    fn spawn_sweeper(me: Arc<Self>) {
        let interval = me.cfg.sweep_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = me.enforce_cap().await {
                    tracing::error!(error = %e, "disk cache sweep error");
                }
            }
        });
    }

    pub async fn get(&self, key: &str) -> io::Result<Option<String>> {
        let path = self.path_for(key);
        let Some(p) = path else { return Ok(None) };
        if !p.exists() {
            return Ok(None);
        }
        // read and decompress
        let gz = match tokio::fs::read(&p).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // Update mtime to act as access hint
        let _ = set_file_mtime(&p, FileTime::from_system_time(SystemTime::now()));
        let mut dec = GzDecoder::new(&gz[..]);
        let mut s = String::new();
        use std::io::Read;
        dec.read_to_string(&mut s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(s))
    }

    pub async fn put(&self, key: &str, value: &str) -> io::Result<()> {
        let Some(path) = self.path_for(key) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "create_dir_all {} for key {} failed: {}",
                        parent.display(), key, e
                    ),
                )
            })?;
        }
        // compress
        let mut enc = GzEncoder::new(value.as_bytes(), Compression::default());
        let mut buf = Vec::new();
        use std::io::Read;
        enc.read_to_end(&mut buf)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        // write atomically
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &buf).await.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("write temp {} for key {} failed: {}", tmp.display(), key, e),
            )
        })?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("rename {} -> {} for key {} failed: {}", tmp.display(), path.display(), key, e),
            )
        })?;
        // update size counter
        let mut size = self.size_bytes.lock().await;
        *size = size.saturating_add(buf.len() as u64);
        drop(size);
        // sweeper thread enforces cap periodically
        Ok(())
    }

    async fn enforce_cap(&self) -> io::Result<()> {
        if self.cfg.cap_bytes == 0 {
            return Ok(());
        }
        let mut size = *self.size_bytes.lock().await;
        if size <= self.cfg.cap_bytes {
            return Ok(());
        }
        // collect files with mtime
        let mut entries = Vec::new();
        collect_files(&self.cfg.root, &mut entries).await?;
        // min-heap by mtime (oldest first)
        let mut heap: BinaryHeap<(Reverse<SystemTime>, u64, PathBuf)> = BinaryHeap::new();
        for (p, meta) in entries {
            if let Ok(mtime) = meta.modified() {
                let len = meta.len();
                heap.push((Reverse(mtime), len, p));
            }
        }
        while size > self.cfg.cap_bytes {
            if let Some((_mt, len, p)) = heap.pop() {
                let _ = tokio::fs::remove_file(&p).await;
                size = size.saturating_sub(len);
            } else {
                break;
            }
        }
        *self.size_bytes.lock().await = size;
        Ok(())
    }

    fn path_for(&self, key: &str) -> Option<PathBuf> {
        // shard by simple FNV-1a 64-bit hash of key
        let h = fnv1a64(key.as_bytes());
        let a = ((h >> 56) & 0xff) as u8;
        let b = ((h >> 48) & 0xff) as u8;
        let file = sanitize_filename(key);
        let trimmed = file.trim_start_matches(|c| c == '/' || c == '\\');
        let safe = if trimmed.is_empty() { "_" } else { trimmed };
        let path = self
            .cfg
            .root
            .join(format!("{:02x}", a))
            .join(format!("{:02x}", b))
            .join(format!("{}.md.gz", safe));
        Some(path)
    }
}

fn sanitize_filename(id: &str) -> String {
    // arXiv ids are ASCII; replace any unexpected chars just in case
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | ':' | '/' => c,
            _ => '_',
        })
        .collect()
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

async fn initial_size(root: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let mut it = tokio::fs::read_dir(root).await?;
    while let Some(entry) = it.next_entry().await? {
        let path = entry.path();
        if entry.file_type().await?.is_dir() {
            total = total.saturating_add(dir_size(&path).await?);
        } else if entry.file_type().await?.is_file() {
            total = total.saturating_add(entry.metadata().await?.len());
        }
    }
    Ok(total)
}

async fn dir_size(dir: &Path) -> io::Result<u64> {
    let mut size = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut it = tokio::fs::read_dir(&d).await?;
        while let Some(entry) = it.next_entry().await? {
            let p = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                size = size.saturating_add(entry.metadata().await?.len());
            }
        }
    }
    Ok(size)
}

async fn collect_files(root: &Path, out: &mut Vec<(PathBuf, Metadata)>) -> io::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut it = tokio::fs::read_dir(&d).await?;
        while let Some(entry) = it.next_entry().await? {
            let p = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                out.push((p, entry.metadata().await?));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("mk-dc-{}", uuid()));
        let cfg = DiskCacheConfig {
            root: tmp.clone(),
            cap_bytes: 10_000_000,
            sweep_interval: Duration::from_secs(3600),
        };
        let dc = DiskCache::new(cfg).await.unwrap();
        dc.put("1234.5678", "hello world").await.unwrap();
        let got = dc.get("1234.5678").await.unwrap();
        assert_eq!(got.as_deref(), Some("hello world"));
        let _ = tokio::fs::remove_dir_all(tmp).await;
    }

    #[tokio::test]
    async fn put_handles_leading_slash_key() {
        let tmp = std::env::temp_dir().join(format!("mk-dc-{}", uuid()));
        let cfg = DiskCacheConfig {
            root: tmp.clone(),
            cap_bytes: 10_000_000,
            sweep_interval: Duration::from_secs(3600),
        };
        let dc = DiskCache::new(cfg).await.unwrap();
        dc.put("/abs/1234.5678", "hello world").await.unwrap();

        let mut stack = vec![tmp.clone()];
        let mut found = false;
        while let Some(dir) = stack.pop() {
            let mut it = tokio::fs::read_dir(&dir).await.unwrap();
            while let Some(entry) = it.next_entry().await.unwrap() {
                let path = entry.path();
                let ty = entry.file_type().await.unwrap();
                if ty.is_dir() {
                    stack.push(path);
                } else if ty.is_file() {
                    if path.extension().and_then(|s| s.to_str()) == Some("gz") {
                        found = true;
                    }
                }
            }
        }
        assert!(found, "expected cached file inside disk cache root");

        let _ = tokio::fs::remove_dir_all(tmp).await;
    }

    #[tokio::test]
    async fn enforce_cap_deletes_oldest() {
        let tmp = std::env::temp_dir().join(format!("mk-dc-{}", uuid()));
        let cfg = DiskCacheConfig {
            root: tmp.clone(),
            cap_bytes: 200,
            sweep_interval: Duration::from_secs(3600),
        };
        let dc = DiskCache::new(cfg).await.unwrap();
        for i in 0..20 {
            let _ = dc.put(&format!("id{}", i), &"x".repeat(50)).await;
        }
        // force enforcement now
        dc.enforce_cap().await.unwrap();
        // size under or equal cap
        assert!(*dc.size_bytes.lock().await <= 200);
        let _ = tokio::fs::remove_dir_all(tmp).await;
    }

    fn uuid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:x}", nanos)
    }
}
