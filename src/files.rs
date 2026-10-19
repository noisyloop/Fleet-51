//! Adding files to a bot. A file you add is copied into the bot's folder under a cleaned-up name;
//! your original is never touched. PDF and Word documents are read inside the sandbox (a
//! booby-trapped document can't reach the rest of the computer) and their text is saved next
//! to them for the bot to read. Images are passed to models that can see.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

pub const MAX_BYTES: usize = 50 * 1024 * 1024;
const MAX_TEXT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Text,
    Image,
    Pdf,
    Word,
    Other,
}

#[derive(Debug, Serialize)]
pub struct Added {
    /// The file's name in the bot's folder.
    pub name: String,
    pub kind: Kind,
    pub bytes: usize,
    /// For documents: the text file the bot reads instead.
    pub text_file: Option<String>,
    /// Why the text could not be read, if it couldn't.
    pub note: Option<String>,
}

/// A safe file name: no folders, no hidden files, no control characters, not too long.
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    s = s.trim().trim_start_matches('.').trim().to_string();
    if s.chars().count() > 100 {
        let (stem, ext) = split_ext(&s);
        let stem: String = stem.chars().take(90).collect();
        s = format!("{stem}{ext}");
    }
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 10 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// What a file is, judged by its first bytes (not just its name).
pub fn kind_of(name: &str, data: &[u8]) -> Kind {
    let ext = split_ext(name).1.to_ascii_lowercase();
    let is = |sig: &[u8]| data.starts_with(sig);
    if is(b"\x89PNG\r\n\x1a\n")
        || is(b"\xff\xd8\xff")
        || is(b"GIF87a")
        || is(b"GIF89a")
        || (is(b"RIFF") && data.get(8..12) == Some(b"WEBP"))
    {
        return Kind::Image;
    }
    if is(b"%PDF-") {
        return Kind::Pdf;
    }
    if is(b"PK\x03\x04") && ext == ".docx" {
        return Kind::Word;
    }
    if !data.contains(&0) && std::str::from_utf8(data).is_ok() {
        return Kind::Text;
    }
    Kind::Other
}

/// A name in `folder` that isn't taken yet: "notes.txt", "notes (2).txt", …
fn free_name(folder: &Path, name: &str) -> String {
    if !folder.join(name).exists() && !folder.join(name).is_symlink() {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|n| !folder.join(n).exists() && !folder.join(n).is_symlink())
        .expect("some name is free")
}

/// Write a new file, never following or replacing anything already there.
fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("saving {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

/// Add a file to a bot's folder. `exe` is this program, used to read documents in the sandbox.
pub fn add(folder: &Path, exe: &Path, name: &str, data: &[u8]) -> Result<Added> {
    if data.len() > MAX_BYTES {
        bail!("that file is too big (the limit is 50 MB)");
    }
    let folder = folder.canonicalize()?;
    let name = free_name(&folder, &clean_name(name));
    let kind = kind_of(&name, data);
    write_new(&folder.join(&name), data)?;
    let mut added = Added {
        name: name.clone(),
        kind,
        bytes: data.len(),
        text_file: None,
        note: None,
    };
    if matches!(kind, Kind::Pdf | Kind::Word) {
        let text_name = free_name(&folder, &format!("{name}.txt"));
        let sandbox = crate::sandbox::available();
        let run = crate::sandbox::run_program(
            sandbox,
            &folder,
            exe,
            &["extract", &name, &text_name],
            Some(exe),
        );
        match run {
            Ok((Some(0), _)) if folder.join(&text_name).is_file() => {
                added.text_file = Some(text_name)
            }
            Ok((_, out)) => {
                let _ = std::fs::remove_file(folder.join(&text_name));
                let why = out.lines().last().unwrap_or("").trim().to_string();
                added.note = Some(if why.is_empty() {
                    "couldn't read the text in this document".into()
                } else {
                    format!("couldn't read the text in this document: {why}")
                });
            }
            Err(e) => added.note = Some(format!("couldn't read the text in this document: {e}")),
        }
    }
    Ok(added)
}

/// `fleet51 extract <input> <output>`: save the text of a PDF or Word document. Runs inside
/// the sandbox, in the bot's folder; both paths are names in that folder.
pub fn extract_command(input: &Path, output: &Path) -> Result<()> {
    let data = std::fs::read(input)?;
    let text = match kind_of(&input.to_string_lossy(), &data) {
        Kind::Pdf => pdf_extract::extract_text_from_mem(&data)
            .map_err(|e| anyhow::anyhow!("not a readable PDF ({e})"))?,
        Kind::Word => docx_text(&data)?,
        _ => bail!("not a PDF or Word document"),
    };
    let text = text.trim();
    if text.is_empty() {
        bail!("no text found (it may be a scanned document)");
    }
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    write_new(output, &text.as_bytes()[..end])
}

/// The text of a .docx: the paragraphs of word/document.xml, without the markup.
fn docx_text(data: &[u8]) -> Result<String> {
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(data)).context("not a Word document")?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .context("not a Word document")?
        .take(20 * 1024 * 1024)
        .read_to_string(&mut xml)?;
    let xml = xml
        .replace("</w:p>", "\n")
        .replace("<w:tab/>", "\t")
        .replace("<w:br/>", "\n");
    let tags = regex::Regex::new(r"<[^>]*>").expect("valid regex");
    let text = tags.replace_all(&xml, "");
    Ok(text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&"))
}

/// The image files among `names` in `folder`, as data: URLs a vision model can take.
pub fn image_urls(folder: &Path, names: &[String]) -> Vec<String> {
    use base64_lite::encode;
    names
        .iter()
        .filter_map(|n| {
            let p: PathBuf = folder.join(clean_name(n));
            let data = std::fs::read(&p)
                .ok()
                .filter(|d| d.len() <= 20 * 1024 * 1024)?;
            if kind_of(n, &data) != Kind::Image {
                return None;
            }
            let mime = if data.starts_with(b"\x89PNG") {
                "image/png"
            } else if data.starts_with(b"\xff\xd8") {
                "image/jpeg"
            } else if data.starts_with(b"GIF") {
                "image/gif"
            } else {
                "image/webp"
            };
            Some(format!("data:{mime};base64,{}", encode(&data)))
        })
        .collect()
}

/// A small standard base64 encoder (no extra dependency).
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if c.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_never_reach_outside() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\x\\report.pdf"), "report.pdf");
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("..."), "file");
        assert_eq!(clean_name("a\u{0}b\nc.txt"), "abc.txt");
    }

    #[test]
    fn kinds_come_from_content() {
        assert_eq!(kind_of("x.png", b"\x89PNG\r\n\x1a\nrest"), Kind::Image);
        assert_eq!(kind_of("x.png", b"just text"), Kind::Text);
        assert_eq!(kind_of("x.txt", b"%PDF-1.4"), Kind::Pdf);
        assert_eq!(kind_of("x.bin", b"\x00\x01"), Kind::Other);
    }

    #[test]
    fn adding_never_overwrites_and_base64_is_standard() {
        let base = std::env::temp_dir().join(format!("fleet51-files-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = std::env::current_exe().unwrap();
        let a = add(&base, &exe, "notes.txt", b"one").unwrap();
        let b = add(&base, &exe, "../notes.txt", b"two").unwrap();
        assert_eq!(a.name, "notes.txt");
        assert_eq!(b.name, "notes (2).txt");
        assert_eq!(
            std::fs::read_to_string(base.join("notes.txt")).unwrap(),
            "one"
        );
        assert_eq!(base64_lite::encode(b"Man"), "TWFu");
        assert_eq!(base64_lite::encode(b"Ma"), "TWE=");
        let _ = std::fs::remove_dir_all(&base);
    }
}
