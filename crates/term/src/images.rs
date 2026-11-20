//! Images sent with the Kitty graphics protocol, decoded to RGBA and kept per session.
//!
//! Only virtual placements (`U=1`) are drawn: the program prints U+10EEEE placeholder cells
//! whose foreground colour names the image, so pictures scroll and clip with the text. The UI
//! looks images up by id while it paints those cells.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;

use crate::kitty::KittyCommand;

/// Decoded bytes kept for all images of one session; the oldest go first past this.
const QUOTA: usize = 320 << 20;
/// Largest side in pixels, as kitty allows.
const MAX_SIDE: u32 = 10_000;
/// Largest file read for `t=f` / `t=t`.
const MAX_FILE: u64 = 64 << 20;

/// A decoded image.
#[derive(Debug, Clone)]
pub struct KittyImage {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major RGBA.
    pub rgba: Arc<Vec<u8>>,
    /// Changes whenever this id gets new pixels, so the UI knows to re-upload.
    pub generation: u64,
    /// The virtual placement's size in cells, 0 when none was given.
    pub columns: u32,
    pub rows: u32,
    /// The file it was read from (`t=f`), for "Open file"; None for bytes and temp files.
    pub path: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct ImageStore {
    images: HashMap<u32, KittyImage>,
    bytes: usize,
    generation: u64,
    /// `t=f` / `t=t` read files on this machine; false for SSH, whose paths are remote.
    local_files: bool,
    /// Replies reach the program. False behind ConPTY: its input parser drops the APC but types
    /// the final `\` into the program (lesson L21).
    replies: bool,
}

/// Why a command failed, as the reply's error code and message.
struct Failure(&'static str, String);

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self(code, message.into())
    }
}

impl ImageStore {
    pub(crate) fn new(local_files: bool, replies: bool) -> Self {
        Self {
            images: HashMap::new(),
            bytes: 0,
            generation: 0,
            local_files,
            replies,
        }
    }

    pub(crate) fn get(&self, id: u32) -> Option<KittyImage> {
        self.images.get(&id).cloned()
    }

    /// Bumped on every change, so a painter can tell its cached images may be stale.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Applies `command`; returns the reply to write back to the program, if any.
    pub(crate) fn apply(&mut self, command: &KittyCommand) -> Option<Vec<u8>> {
        let result = match command.action {
            b't' | b'T' => self.transmit(command),
            b'q' => decode(command, self.local_files).map(|_| ()),
            b'p' => self.place(command),
            b'd' => {
                self.delete(command);
                Ok(())
            }
            // Animation frames and the like: not supported, said quietly.
            _ => Err(Failure::new("EINVAL", "unsupported action")),
        };
        // Deletes never answer; queries always do unless quiet.
        if command.action == b'd' || !self.replies {
            return None;
        }
        reply(command, result.err())
    }

    fn transmit(&mut self, command: &KittyCommand) -> Result<(), Failure> {
        if command.id == 0 {
            return Err(Failure::new("EINVAL", "an image id is needed"));
        }
        let (width, height, rgba) = decode(command, self.local_files)?;
        self.remove(command.id);
        self.generation += 1;
        self.bytes += rgba.len();
        self.images.insert(
            command.id,
            KittyImage {
                id: command.id,
                width,
                height,
                rgba: Arc::new(rgba),
                generation: self.generation,
                columns: command.columns,
                rows: command.rows,
                path: (command.medium == b'f' && self.local_files)
                    .then(|| file_path(command))
                    .flatten(),
            },
        );
        self.evict(command.id);
        Ok(())
    }

    /// `a=p,U=1`: a virtual placement for an image sent earlier.
    fn place(&mut self, command: &KittyCommand) -> Result<(), Failure> {
        let image = self
            .images
            .get_mut(&command.id)
            .ok_or_else(|| Failure::new("ENOENT", "no image with this id"))?;
        image.columns = command.columns;
        image.rows = command.rows;
        self.generation += 1;
        Ok(())
    }

    fn delete(&mut self, command: &KittyCommand) {
        match command.delete.to_ascii_lowercase() {
            b'a' => {
                self.images.clear();
                self.bytes = 0;
            }
            b'i' => self.remove(command.id),
            // Deletes by position or z-index apply to classic placements, which are not drawn.
            _ => return,
        }
        self.generation += 1;
    }

    fn remove(&mut self, id: u32) {
        if let Some(old) = self.images.remove(&id) {
            self.bytes -= old.rgba.len();
        }
    }

    /// Frees the oldest images until the store fits [`QUOTA`], keeping `keep`.
    fn evict(&mut self, keep: u32) {
        while self.bytes > QUOTA {
            let Some(oldest) = self
                .images
                .values()
                .filter(|image| image.id != keep)
                .min_by_key(|image| image.generation)
                .map(|image| image.id)
            else {
                return;
            };
            self.remove(oldest);
        }
    }
}

/// The reply for `command` (`ESC _G i=<id> ; OK|<error> ESC \`), honouring `q`.
fn reply(command: &KittyCommand, failure: Option<Failure>) -> Option<Vec<u8>> {
    // Without an id or number the program cannot match a reply, so kitty sends none.
    if command.id == 0 && command.number == 0 {
        return None;
    }
    match (&failure, command.quiet) {
        (_, 2) | (None, 1) => return None,
        _ => {}
    }
    let mut keys = Vec::new();
    if command.id != 0 {
        keys.push(format!("i={}", command.id));
    }
    if command.number != 0 {
        keys.push(format!("I={}", command.number));
    }
    if command.placement != 0 {
        keys.push(format!("p={}", command.placement));
    }
    let message = match failure {
        None => "OK".to_owned(),
        Some(Failure(code, message)) => format!("{code}:{message}"),
    };
    Some(format!("\x1b_G{};{message}\x1b\\", keys.join(",")).into_bytes())
}

/// The path a `t=f` / `t=t` command names.
fn file_path(command: &KittyCommand) -> Option<PathBuf> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(strip_whitespace(&command.payload))
        .ok()?;
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Reads and decodes the command's pixels to RGBA.
fn decode(command: &KittyCommand, local_files: bool) -> Result<(u32, u32, Vec<u8>), Failure> {
    if command.truncated {
        return Err(Failure::new("EFBIG", "image too large"));
    }
    let payload = base64::engine::general_purpose::STANDARD
        .decode(strip_whitespace(&command.payload))
        .map_err(|_| Failure::new("EINVAL", "bad base64"))?;
    let data = match command.medium {
        b'd' => payload,
        b'f' | b't' => {
            if !local_files {
                return Err(Failure::new("EBADF", "files are not read over SSH"));
            }
            let path = PathBuf::from(
                String::from_utf8(payload).map_err(|_| Failure::new("EINVAL", "bad path"))?,
            );
            let data = read_file(&path, command.offset, command.size)?;
            if command.medium == b't' && is_temp_image(&path) {
                let _ = std::fs::remove_file(&path);
            }
            data
        }
        _ => return Err(Failure::new("EINVAL", "unsupported transmission medium")),
    };
    let data = match command.compression {
        None => data,
        Some(b'z') => inflate(&data)?,
        Some(_) => return Err(Failure::new("EINVAL", "unsupported compression")),
    };
    match command.format {
        100 => {
            let image = image::load_from_memory_with_format(&data, image::ImageFormat::Png)
                .map_err(|err| Failure::new("EBADPNG", err.to_string()))?
                .into_rgba8();
            let (width, height) = image.dimensions();
            check_size(width, height)?;
            Ok((width, height, image.into_raw()))
        }
        32 | 24 => {
            let (width, height) = (command.width, command.height);
            check_size(width, height)?;
            let channels = if command.format == 32 { 4 } else { 3 };
            let wanted = width as usize * height as usize * channels;
            if data.len() < wanted {
                return Err(Failure::new("ENODATA", "fewer pixels than s*v"));
            }
            let data = &data[..wanted];
            let rgba = if channels == 4 {
                data.to_vec()
            } else {
                data.as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|&[r, g, b]| [r, g, b, 0xff])
                    .collect()
            };
            Ok((width, height, rgba))
        }
        _ => Err(Failure::new("EINVAL", "unsupported format")),
    }
}

fn check_size(width: u32, height: u32) -> Result<(), Failure> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(Failure::new("EINVAL", "bad image size"));
    }
    Ok(())
}

fn strip_whitespace(payload: &[u8]) -> Vec<u8> {
    payload
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect()
}

fn read_file(path: &Path, offset: u32, size: u32) -> Result<Vec<u8>, Failure> {
    let file = std::fs::File::open(path).map_err(|err| Failure::new("EBADF", err.to_string()))?;
    let meta = file
        .metadata()
        .map_err(|err| Failure::new("EBADF", err.to_string()))?;
    // Regular files only: never a device, pipe or directory.
    if !meta.is_file() {
        return Err(Failure::new("EBADF", "not a regular file"));
    }
    let mut file = std::io::BufReader::new(file);
    std::io::copy(
        &mut (&mut file).take(u64::from(offset)),
        &mut std::io::sink(),
    )
    .map_err(|err| Failure::new("EBADF", err.to_string()))?;
    let limit = if size == 0 {
        MAX_FILE
    } else {
        u64::from(size).min(MAX_FILE)
    };
    let mut data = Vec::new();
    file.take(limit)
        .read_to_end(&mut data)
        .map_err(|err| Failure::new("EBADF", err.to_string()))?;
    Ok(data)
}

/// Kitty deletes a `t=t` file only when it lives in a temp folder and its name says it was
/// made for this protocol, so a bad command cannot delete arbitrary files.
fn is_temp_image(path: &Path) -> bool {
    let in_temp = path.starts_with(std::env::temp_dir())
        || path.starts_with("/tmp")
        || path.starts_with("/dev/shm");
    let named = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("tty-graphics-protocol"));
    in_temp && named
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(u64::from(MAX_SIDE) * u64::from(MAX_SIDE) * 4)
        .read_to_end(&mut out)
        .map_err(|_| Failure::new("EINVAL", "bad zlib data"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(data: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(data)
            .into_bytes()
    }

    fn png_1x2() -> Vec<u8> {
        let image = image::RgbaImage::from_raw(1, 2, vec![255, 0, 0, 255, 0, 255, 0, 128]).unwrap();
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn transmit_png(id: u32, quiet: u8) -> KittyCommand {
        KittyCommand {
            action: b'T',
            format: 100,
            id,
            virtual_placement: true,
            columns: 4,
            rows: 2,
            quiet,
            payload: b64(&png_1x2()),
            ..KittyCommand::default()
        }
    }

    #[test]
    fn png_is_decoded_and_stored() {
        let mut store = ImageStore::new(true, true);
        assert_eq!(store.apply(&transmit_png(31, 2)), None);
        let image = store.get(31).unwrap();
        assert_eq!(
            (image.width, image.height, image.columns, image.rows),
            (1, 2, 4, 2)
        );
        assert_eq!(image.rgba.as_slice(), &[255, 0, 0, 255, 0, 255, 0, 128]);
    }

    #[test]
    fn replies_follow_quiet() {
        let mut store = ImageStore::new(true, true);
        assert_eq!(
            store.apply(&transmit_png(7, 0)),
            Some(b"\x1b_Gi=7;OK\x1b\\".to_vec())
        );
        assert_eq!(store.apply(&transmit_png(7, 1)), None);
        let mut bad = transmit_png(8, 1);
        bad.payload = b64(b"not a png");
        let reply = String::from_utf8(store.apply(&bad).unwrap()).unwrap();
        assert!(reply.starts_with("\x1b_Gi=8;EBADPNG:"), "{reply}");
    }

    #[test]
    fn claude_code_probe_is_answered_ok() {
        let mut store = ImageStore::new(true, true);
        let probe = KittyCommand {
            action: b'q',
            format: 24,
            width: 1,
            height: 1,
            id: 31,
            payload: b"AAAA".to_vec(),
            ..KittyCommand::default()
        };
        assert_eq!(store.apply(&probe), Some(b"\x1b_Gi=31;OK\x1b\\".to_vec()));
        assert!(store.get(31).is_none(), "a query stores nothing");
    }

    #[test]
    fn file_medium_reads_local_files_only() {
        let path = std::env::temp_dir().join("maomi-kitty-test.rgba");
        std::fs::write(&path, [1, 2, 3, 4]).unwrap();
        let command = KittyCommand {
            action: b'q',
            medium: b'f',
            width: 1,
            height: 1,
            id: 3,
            payload: b64(path.to_string_lossy().as_bytes()),
            ..KittyCommand::default()
        };
        assert_eq!(
            ImageStore::new(true, true).apply(&command),
            Some(b"\x1b_Gi=3;OK\x1b\\".to_vec())
        );
        let remote =
            String::from_utf8(ImageStore::new(false, true).apply(&command).unwrap()).unwrap();
        assert!(remote.contains("EBADF"), "{remote}");
        assert!(path.exists(), "t=f leaves the file in place");
        let mut store = ImageStore::new(true, true);
        store.apply(&KittyCommand {
            action: b't',
            ..command.clone()
        });
        assert_eq!(store.get(3).unwrap().path.as_deref(), Some(path.as_path()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rgb_and_zlib() {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &[10, 20, 30, 40, 50, 60]).unwrap();
        let command = KittyCommand {
            format: 24,
            width: 2,
            height: 1,
            id: 4,
            compression: Some(b'z'),
            payload: b64(&encoder.finish().unwrap()),
            ..KittyCommand::default()
        };
        let mut store = ImageStore::new(true, true);
        store.apply(&command);
        assert_eq!(
            store.get(4).unwrap().rgba.as_slice(),
            &[10, 20, 30, 255, 40, 50, 60, 255]
        );
    }

    #[test]
    fn delete_frees_and_bumps_generation() {
        let mut store = ImageStore::new(true, true);
        store.apply(&transmit_png(1, 2));
        store.apply(&transmit_png(2, 2));
        let before = store.generation();
        store.apply(&KittyCommand {
            action: b'd',
            delete: b'I',
            id: 1,
            ..KittyCommand::default()
        });
        assert!(store.get(1).is_none());
        assert!(store.get(2).is_some());
        assert!(store.generation() > before);
        store.apply(&KittyCommand {
            action: b'd',
            ..KittyCommand::default()
        });
        assert!(store.get(2).is_none());
        assert_eq!(store.bytes, 0);
    }

    #[test]
    fn truncated_payload_is_refused() {
        let mut command = transmit_png(5, 0);
        command.truncated = true;
        let reply =
            String::from_utf8(ImageStore::new(true, true).apply(&command).unwrap()).unwrap();
        assert!(reply.contains("EFBIG"), "{reply}");
    }
}
