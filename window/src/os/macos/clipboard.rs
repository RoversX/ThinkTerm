use crate::macos::{nsstring, nsstring_to_str};
use crate::{ClipboardContents, ClipboardImageFormat};
use cocoa::appkit::{NSFilenamesPboardType, NSPasteboard, NSStringPboardType};
use cocoa::base::*;
use cocoa::foundation::{NSArray, NSData};
use std::path::PathBuf;

pub struct Clipboard {
    pasteboard: id,
}

impl Clipboard {
    pub fn new() -> Self {
        let pasteboard = unsafe { NSPasteboard::generalPasteboard(nil) };
        if pasteboard.is_null() {
            panic!("NSPasteboard::generalPasteboard returned null");
        }
        Clipboard { pasteboard }
    }

    pub fn read(&self) -> anyhow::Result<String> {
        // Historical textual behavior: files flatten to a shell-quoted list.
        Ok(self.read_contents()?.to_text())
    }

    /// Files > text > image; see [`ClipboardContents`] for the rationale.
    pub fn read_contents(&self) -> anyhow::Result<ClipboardContents> {
        unsafe {
            let plist = self.pasteboard.propertyListForType(NSFilenamesPboardType);
            if !plist.is_null() {
                let mut paths = vec![];
                for i in 0..plist.count() {
                    paths.push(PathBuf::from(nsstring_to_str(plist.objectAtIndex(i))));
                }
                return Ok(ClipboardContents::FilePaths(paths));
            }
            let s = self.pasteboard.stringForType(NSStringPboardType);
            if !s.is_null() {
                return Ok(ClipboardContents::Text(nsstring_to_str(s).to_string()));
            }
            for (uti, format) in [
                ("public.png", ClipboardImageFormat::Png),
                ("public.tiff", ClipboardImageFormat::Tiff),
            ] {
                let data = self.pasteboard.dataForType(*nsstring(uti));
                if data.is_null() {
                    continue;
                }
                let len = data.length() as usize;
                if len == 0 {
                    continue;
                }
                let bytes = std::slice::from_raw_parts(data.bytes() as *const u8, len).to_vec();
                return Ok(ClipboardContents::Image { format, bytes });
            }
        }
        anyhow::bail!("pasteboard read returned empty");
    }

    pub fn write(&mut self, data: String) -> anyhow::Result<()> {
        unsafe {
            self.pasteboard.clearContents();
            let success: BOOL = self
                .pasteboard
                .writeObjects(NSArray::arrayWithObject(nil, *nsstring(&data)));
            anyhow::ensure!(success == YES, "pasteboard write returned false");
            Ok(())
        }
    }
}
