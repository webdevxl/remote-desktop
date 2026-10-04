//! The Mac's clipboard (`NSPasteboard`), for sharing it with another Mac.
//!
//! Reading `changeCount` costs next to nothing and macOS's pasteboard privacy (an alert when an
//! app reads the clipboard without the user pasting, announced for a future macOS) leaves it
//! alone, so callers poll that and read the contents only when they need them.

use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardContentsOptions};
use objc2_foundation::{NSData, NSDictionary, NSString};

/// A pasteboard: the one everybody copies to and pastes from, or one of LanKVM's own by name.
pub struct Pasteboard {
    board: Retained<NSPasteboard>,
    /// Made by name: released (forgotten by the system) when dropped.
    named: bool,
}

// SAFETY: NSPasteboard isn't tied to a thread (it talks to the pasteboard server). A `Pasteboard`
// isn't `Sync`, so it is used from one thread at a time.
unsafe impl Send for Pasteboard {}

impl Pasteboard {
    /// The clipboard of this Mac's user.
    pub fn general() -> Self {
        Self { board: NSPasteboard::generalPasteboard(), named: false }
    }

    /// A pasteboard of its own, which other processes reach by the same name (tests, or a second
    /// copy of LanKVM on the same Mac that mustn't share the real clipboard with the first).
    pub fn named(name: &str) -> Self {
        Self { board: NSPasteboard::pasteboardWithName(&NSString::from_str(name)), named: true }
    }

    /// Changes whenever anyone replaces the contents.
    pub fn change_count(&self) -> i64 {
        autoreleasepool(|_| self.board.changeCount() as i64)
    }

    /// The data of each of `kinds` (UTIs) the clipboard has, in the clipboard's order (its owner's
    /// preference, richest first). Only the first item is read, as for a plain paste.
    pub fn read(&self, kinds: &[&str]) -> Vec<(String, Vec<u8>)> {
        autoreleasepool(|_| {
            let Some(types) = self.board.types() else { return Vec::new() };
            let mut out: Vec<(String, Vec<u8>)> = Vec::new();
            for kind in types.iter() {
                let name = kind.to_string();
                if !kinds.contains(&name.as_str()) || out.iter().any(|(k, _)| *k == name) {
                    continue;
                }
                if let Some(data) = self.board.dataForType(&kind) {
                    out.push((name, data.to_vec()));
                }
            }
            out
        })
    }

    /// Replaces the contents with `items` (type, data), as one item. Returns the change count
    /// that makes (writing the data doesn't change it again). Nothing: just empties it.
    ///
    /// The contents stay on this Mac: Universal Clipboard doesn't hand them to the user's other
    /// devices, which may be the very Mac they came from.
    pub fn write(&self, items: &[(String, Vec<u8>)]) -> i64 {
        autoreleasepool(|_| {
            let count = self.board.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly) as i64;
            for (kind, data) in items {
                if !self.board.setData_forType(Some(&NSData::with_bytes(data)), &NSString::from_str(kind)) {
                    tracing::warn!(kind, "couldn't write to the clipboard");
                }
            }
            count
        })
    }
}

impl Drop for Pasteboard {
    fn drop(&mut self) {
        if self.named {
            // SAFETY: `releaseGlobally` takes no arguments and returns nothing.
            autoreleasepool(|_| unsafe { msg_send![&*self.board, releaseGlobally] })
        }
    }
}

/// A TIFF image as PNG, a fraction of the size (TIFF on the clipboard is usually uncompressed).
pub fn png_from_tiff(tiff: &[u8]) -> Option<Vec<u8>> {
    autoreleasepool(|_| {
        let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(tiff))?;
        // SAFETY: an empty properties dictionary is valid for every file type.
        let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        Some(png.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(name: &str) -> Pasteboard {
        Pasteboard::named(&format!("lankvm-test-{name}-{}", std::process::id()))
    }

    #[test]
    fn writes_and_reads_its_own_pasteboard() {
        let board = unique("rw");
        let before = board.change_count();
        let items = vec![
            ("public.utf8-plain-text".to_string(), "héllo".as_bytes().to_vec()),
            ("public.html".to_string(), b"<b>h\xc3\xa9llo</b>".to_vec()),
            ("org.nspasteboard.ConcealedType".to_string(), Vec::new()),
        ];
        let count = board.write(&items);
        assert_ne!(count, before);
        assert_eq!(board.change_count(), count, "writing the data doesn't change the count again");
        let mut read = board.read(&["public.html", "public.utf8-plain-text", "org.nspasteboard.ConcealedType", "public.png"]);
        read.sort();
        let mut want = items.clone();
        want.sort();
        assert_eq!(read, want);
        assert_eq!(board.read(&["public.png"]), Vec::new(), "only the kinds asked for");
        // Emptied: nothing to read, and a new count.
        let emptied = board.write(&[]);
        assert_ne!(emptied, count);
        assert!(board.read(&["public.utf8-plain-text"]).is_empty());
    }

    #[test]
    fn tiff_becomes_png() {
        let png = png_from_tiff(&tiny_tiff()).expect("converts");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png_from_tiff(b"not an image").is_none());
    }

    /// A 1×1 black RGB TIFF, uncompressed.
    fn tiny_tiff() -> Vec<u8> {
        let mut t = b"II*\0\x08\0\0\0".to_vec();
        let entries: [(u16, u16, u32, u32); 9] = [
            (256, 3, 1, 1),   // width
            (257, 3, 1, 1),   // height
            (258, 3, 1, 8),   // bits per sample (one value: fine for readers)
            (259, 3, 1, 1),   // no compression
            (262, 3, 1, 2),   // RGB
            (273, 4, 1, 122), // strip offset
            (277, 3, 1, 3),   // samples per pixel
            (278, 3, 1, 1),   // rows per strip
            (279, 4, 1, 3),   // strip byte count
        ];
        t.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, kind, count, value) in entries {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&kind.to_le_bytes());
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&value.to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(t.len(), 122);
        t.extend_from_slice(&[0, 0, 0]);
        t
    }
}
