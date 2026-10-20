//! Take a few files out of a large zip on a web server without downloading all of it: the zip
//! is read through HTTP range requests, so only its table of contents and the files asked for
//! are fetched. Each file is checked against a pinned SHA-256 before it counts.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// How much to fetch past what was asked for, so reading a file takes a few requests, not
/// thousands.
const CHUNK: u64 = 4 << 20;
/// The end of the zip, fetched first: it holds the table of contents.
const TAIL: u64 = 256 << 10;

/// A remote file as `Read + Seek`, fetched in ranges and counting the bytes it downloads.
struct HttpRange<'a> {
    agent: ureq::Agent,
    url: String,
    /// Where `url` redirects to (GitHub sends downloads to a signed URL that expires).
    target: String,
    len: u64,
    pos: u64,
    tail: (u64, Vec<u8>),
    window: (u64, Vec<u8>),
    fetched: &'a AtomicU64,
    /// Read-ahead stops here: the end of the file being read, so files that weren't asked for
    /// aren't downloaded along the way.
    stop: &'a AtomicU64,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

impl<'a> HttpRange<'a> {
    fn open(url: &str, fetched: &'a AtomicU64, stop: &'a AtomicU64) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into();
        let mut r = HttpRange {
            agent,
            url: url.to_string(),
            target: String::new(),
            len: 0,
            pos: 0,
            tail: (0, Vec::new()),
            window: (0, Vec::new()),
            fetched,
            stop,
        };
        r.resolve()?;
        let start = r.len.saturating_sub(TAIL);
        r.tail = (start, r.fetch(start, r.len)?);
        Ok(r)
    }

    /// Follow redirects to the file itself and learn its length.
    fn resolve(&mut self) -> Result<()> {
        let mut url = self.url.clone();
        for _ in 0..5 {
            let resp = self
                .agent
                .get(&url)
                .header("Range", "bytes=0-0")
                .call()
                .with_context(|| format!("connecting to {}", self.url))?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                url = header(&resp, "location")
                    .context("a redirect without a location")?
                    .to_string();
                continue;
            }
            if status != 206 {
                bail!("{} answered {status} to a range request", self.url);
            }
            let total = header(&resp, "content-range")
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.parse().ok())
                .context("the server did not say how large the file is")?;
            self.target = url;
            self.len = total;
            return Ok(());
        }
        bail!("too many redirects from {}", self.url)
    }

    /// Bytes `start..end`, asking for a fresh redirect once if the signed URL has expired.
    fn fetch(&mut self, start: u64, end: u64) -> Result<Vec<u8>> {
        if start >= end {
            return Ok(Vec::new());
        }
        for attempt in 0..2 {
            let resp = self
                .agent
                .get(&self.target)
                .header("Range", &format!("bytes={start}-{}", end - 1))
                .call()?;
            let range_ok = header(&resp, "content-range")
                .is_some_and(|v| v.starts_with(&format!("bytes {start}-")));
            if resp.status().as_u16() == 206 && range_ok {
                let body = resp
                    .into_body()
                    .into_with_config()
                    // One more than asked for, so a server sending too much is noticed below.
                    .limit(end - start + 1)
                    .read_to_vec()?;
                if body.len() as u64 != end - start {
                    bail!("the download stopped part way");
                }
                self.fetched.fetch_add(body.len() as u64, Ordering::Relaxed);
                return Ok(body);
            }
            if attempt == 0 {
                self.resolve()?;
            }
        }
        bail!("{} would not send part of the file", self.url)
    }
}

fn header<'r>(resp: &'r ureq::http::Response<ureq::Body>, name: &str) -> Option<&'r str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

impl Read for HttpRange<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let inside = |(start, data): &(u64, Vec<u8>), pos: u64| {
            pos >= *start && pos < *start + data.len() as u64
        };
        if !inside(&self.tail, self.pos) && !inside(&self.window, self.pos) {
            let ahead = (self.pos + CHUNK).min(self.stop.load(Ordering::Relaxed));
            let end = self.len.min(ahead.max(self.pos + buf.len() as u64));
            self.window = (self.pos, self.fetch(self.pos, end).map_err(io_err)?);
        }
        let (start, data) = if inside(&self.tail, self.pos) {
            &self.tail
        } else {
            &self.window
        };
        let from = (self.pos - start) as usize;
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRange<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io_err("seek before the start of the file"))?;
        Ok(self.pos)
    }
}

/// Download `files` (SHA-256, path in the zip) from the zip at `url` into `dir`, reporting
/// (bytes downloaded, bytes expected). Fails if any file is missing or its hash differs.
pub fn fetch_files(
    url: &str,
    dir: &Path,
    files: &[(&str, &str)],
    progress: &dyn Fn(u64, u64),
) -> Result<()> {
    let fetched = AtomicU64::new(0);
    let stop = AtomicU64::new(u64::MAX);
    // A known offset: detecting one would read the whole zip from the start.
    let config = zip::read::Config {
        archive_offset: zip::read::ArchiveOffset::Known(0),
    };
    let mut zip = zip::ZipArchive::with_config(config, HttpRange::open(url, &fetched, &stop)?)
        .context("reading the zip's table of contents")?;
    let mut wanted = Vec::new();
    let mut total = 0;
    for &(sha256, name) in files {
        let index = zip
            .index_for_name(name)
            .with_context(|| format!("{name} is missing from the download"))?;
        let entry = zip.by_index_data(index)?;
        total += entry.compressed_size();
        // The local header (30 bytes) repeats the name and may carry extra fields.
        let end = entry.header_start() + 30 + name.len() as u64 + 1024 + entry.compressed_size();
        wanted.push((entry.header_start(), index, sha256, name, end));
    }
    // In the order they sit in the zip, so reading only moves forward.
    wanted.sort();
    for (_, index, sha256, name, end) in wanted {
        stop.store(end, Ordering::Relaxed);
        let mut entry = zip.by_index(index)?;
        let Some(rel) = entry.enclosed_name() else {
            bail!("unexpected path in the download: {name}");
        };
        let to = dir.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = Hashing {
            file: std::fs::File::create(&to)?,
            hash: Sha256::new(),
            report: &|| progress(fetched.load(Ordering::Relaxed).min(total), total),
        };
        io::copy(&mut entry, &mut out).with_context(|| format!("downloading {name}"))?;
        out.file.flush()?;
        let got: String = out
            .hash
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got != sha256 {
            bail!("{name} failed its checksum (got {got})");
        }
    }
    progress(total, total);
    Ok(())
}

/// Writes a file while hashing it, reporting progress as it goes.
struct Hashing<'a> {
    file: std::fs::File,
    hash: Sha256,
    report: &'a dyn Fn(),
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        (self.report)();
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Arc;

    fn sha(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A zip with a small program, a big file nobody wants, and a library, served with range
    /// support (and a redirect at /moved, like GitHub's). Returns (base url, requests served).
    fn serve(files: &[(&str, Vec<u8>)]) -> (String, Arc<AtomicU64>) {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, data) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(data).unwrap();
        }
        let body = Arc::new(zip.finish().unwrap().into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sent = Arc::new(AtomicU64::new(0));
        let counter = sent.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut range = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                    }
                }
                if first.contains("/moved") {
                    let to = format!("{}/zip", base_of(&stream));
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let (a, b) = range.unwrap();
                let b = b.min(body.len() - 1);
                counter.fetch_add((b + 1 - a) as u64, Ordering::Relaxed);
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len(), b + 1 - a).unwrap();
                stream.write_all(&body[a..=b]).unwrap();
            }
        });
        (base, sent)
    }

    fn base_of(stream: &std::net::TcpStream) -> String {
        format!("http://{}", stream.local_addr().unwrap())
    }

    /// Bytes that don't compress, so sizes in the test mean what they say.
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u32 = 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn only_the_files_asked_for_are_downloaded_and_checked() {
        let exe = b"the program".to_vec();
        let lib = noise(300_000);
        let (base, sent) = serve(&[
            ("ollama.exe", exe.clone()),
            ("lib/ollama/cuda_v13/huge.dll", noise(6_000_000)),
            ("lib/ollama/ggml-base.dll", lib.clone()),
        ]);
        let dir = std::env::temp_dir().join(format!("fleet51-zip-range-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (exe_sha, lib_sha) = (sha(&exe), sha(&lib));
        let files = [
            (exe_sha.as_str(), "ollama.exe"),
            (lib_sha.as_str(), "lib/ollama/ggml-base.dll"),
        ];
        let last = std::sync::Mutex::new((0, 0));
        fetch_files(&format!("{base}/moved"), &dir, &files, &|d, t| {
            *last.lock().unwrap() = (d, t)
        })
        .unwrap();
        assert_eq!(std::fs::read(dir.join("ollama.exe")).unwrap(), exe);
        assert_eq!(
            std::fs::read(dir.join("lib/ollama/ggml-base.dll")).unwrap(),
            lib
        );
        assert!(!dir.join("lib/ollama/cuda_v13").exists());
        // The 6 MB file in between was skipped.
        let sent = sent.load(Ordering::Relaxed);
        assert!(sent < 1_000_000, "downloaded {sent} bytes");
        let (done, total) = *last.lock().unwrap();
        assert_eq!(done, total);

        // A wrong hash or a missing file fails.
        let bad = [("0".repeat(64).leak() as &str, "ollama.exe")];
        let err = fetch_files(&format!("{base}/zip"), &dir, &bad, &|_, _| {}).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        let missing = [(exe_sha.as_str(), "nope.exe")];
        let err = fetch_files(&format!("{base}/zip"), &dir, &missing, &|_, _| {}).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
