// The icon the desktop shows for a kind of file, so the list looks like the
// file manager next to it rather than like a program with its own opinions.
//
// The name is all that goes in. Entries inside an archive do not exist on
// disk, so nothing here may touch the filesystem: the lookup is by extension
// alone, and on Windows that is exactly what SHGFI_USEFILEATTRIBUTES means.
#![cfg_attr(not(windows), forbid(unsafe_code))]

/// An icon as straight RGBA, ready to hand to a texture.
pub struct Icon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The key an icon is worth caching under. Two files with the same extension
/// get the same icon, so the extension is the whole question; folders are
/// their own case and share one.
pub fn cache_key(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "\u{0}dir".into();
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 16 => ext.to_ascii_lowercase(),
        _ => "\u{0}none".into(),
    }
}

#[cfg(not(windows))]
pub fn lookup(_name: &str, _is_dir: bool) -> Option<Icon> {
    // No single call answers this off Windows, and the two platforms left cost
    // very different amounts.
    //
    // macOS is the cheaper one: NSWorkspace has iconForContentType, so it is
    // roughly this file again through objc2, plus turning an NSImage into
    // pixels.
    //
    // Linux has no such call at all. It takes four steps, each its own
    // dependency: extension to MIME type, MIME type to an icon name by the
    // freedesktop rule, icon name to a file by walking the current theme and
    // whatever it inherits, and finally rasterising the SVG that usually comes
    // out. That last one alone pulls in a renderer. It also depends on a theme
    // being installed and varies by desktop, so it can quietly return nothing
    // on a machine that is working perfectly well.
    //
    // Until one of those earns its keep, the window draws its own.
    None
}

// Every question goes to one thread that the crate owns, and it is the only
// thread that ever talks to the shell. That is not tidiness: the icon API
// behaves as though it is thread affine, and asked from whichever thread
// happens to call it returns nothing for names it answers happily elsewhere.
// The tests caught it, where .toml worked and .txt did not depending on which
// test ran alongside. One owner, one answer, and the round trip does not
// matter because callers cache by extension.
#[cfg(windows)]
pub fn lookup(name: &str, is_dir: bool) -> Option<Icon> {
    match ask(name, is_dir, false)? {
        Answer::Picture(icon) => icon,
        Answer::Words(_) => None,
    }
}

/// What the desktop calls this kind of file: "Text Document", "SQL Source
/// File". The same question as [`lookup`] with a different flag, so it goes
/// down the same thread and caches under the same key.
#[cfg(not(windows))]
pub fn type_name(_name: &str, _is_dir: bool) -> Option<String> {
    None
}

#[cfg(windows)]
pub fn type_name(name: &str, is_dir: bool) -> Option<String> {
    match ask(name, is_dir, true)? {
        Answer::Words(text) => text,
        Answer::Picture(_) => None,
    }
}

#[cfg(windows)]
enum Answer {
    Picture(Option<Icon>),
    Words(Option<String>),
}

#[cfg(windows)]
fn ask(name: &str, is_dir: bool, words: bool) -> Option<Answer> {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::OnceLock;

    type Question = (String, bool, bool, Sender<Answer>);
    static ASK: OnceLock<Sender<Question>> = OnceLock::new();

    let ask = ASK.get_or_init(|| {
        let (tx, rx) = channel::<Question>();
        std::thread::spawn(move || {
            windows_impl::start_com();
            for (name, is_dir, words, reply) in rx {
                let answer = if words {
                    Answer::Words(windows_impl::type_name(&name, is_dir))
                } else {
                    Answer::Picture(windows_impl::lookup(&name, is_dir))
                };
                let _ = reply.send(answer);
            }
        });
        tx
    });

    let (tx, rx) = channel();
    ask.send((name.to_string(), is_dir, words, tx)).ok()?;
    rx.recv().ok()
}

#[cfg(windows)]
mod windows_impl {
    use super::Icon;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    };
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_TYPENAME,
        SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    // Run once on the thread that owns the shell calls. A host that already
    // put this thread in an apartment says RPC_E_CHANGED_MODE, which is not our
    // problem: the result is discarded either way.
    pub fn start_com() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn lookup(name: &str, is_dir: bool) -> Option<Icon> {
        // A bare name, never a path: a caller must not be able to make this
        // reach into the filesystem by passing something absolute.
        let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let asked = if leaf.is_empty() { "file" } else { leaf };
        let text = wide(asked);

        let mut info = SHFILEINFOW::default();
        let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };

        unsafe {
            // USEFILEATTRIBUTES is the flag that makes this a question about a
            // name instead of about a file. Without it the shell would go
            // looking on disk for something that is not there.
            let ok = SHGetFileInfoW(
                PCWSTR(text.as_ptr()),
                attrs,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            );
            if ok == 0 || info.hIcon.is_invalid() {
                return None;
            }
            let icon = to_rgba(info.hIcon);
            let _ = DestroyIcon(info.hIcon);
            icon
        }
    }

    /// The shell's own words for this kind of file, which is what the Explorer
    /// puts in its Type column and WinRAR copies.
    pub fn type_name(name: &str, is_dir: bool) -> Option<String> {
        let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let asked = if leaf.is_empty() { "file" } else { leaf };
        let text = wide(asked);

        let mut info = SHFILEINFOW::default();
        let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };

        unsafe {
            let ok = SHGetFileInfoW(
                PCWSTR(text.as_ptr()),
                attrs,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
            );
            if ok == 0 {
                return None;
            }
            let end = info
                .szTypeName
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szTypeName.len());
            let out = String::from_utf16_lossy(&info.szTypeName[..end]);
            (!out.is_empty()).then_some(out)
        }
    }

    unsafe fn to_rgba(hicon: HICON) -> Option<Icon> {
        let mut ii = ICONINFO::default();
        GetIconInfo(hicon, &mut ii).ok()?;
        let colour = ii.hbmColor;
        let mask = ii.hbmMask;

        let mut bm = BITMAP::default();
        let got = GetObjectW(
            colour,
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(colour);
            let _ = DeleteObject(mask);
            return None;
        }
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);

        let dc = GetDC(HWND::default());
        let mut pixels = read_bits(dc, colour, w, h);

        // A 32-bit icon carries its own alpha. An older one leaves it at zero
        // and keeps the shape in a separate mask, where a set bit means
        // transparent. Without this second pass those icons come out invisible.
        if pixels.iter().skip(3).step_by(4).all(|&a| a == 0) {
            let m = read_bits(dc, mask, w, h);
            for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
                let transparent = m.get(i * 4).copied().unwrap_or(0) != 0;
                px[3] = if transparent { 0 } else { 255 };
            }
        }

        ReleaseDC(HWND::default(), dc);
        let _ = DeleteObject(colour);
        let _ = DeleteObject(mask);

        // GDI hands these over as BGRA; a texture wants RGBA.
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Some(Icon { width: w, height: h, rgba: pixels })
    }

    unsafe fn read_bits(dc: HDC, bitmap: HBITMAP, w: u32, h: u32) -> Vec<u8> {
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // Negative means top down, which spares flipping the rows back.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        GetDIBits(
            dc,
            bitmap,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut _),
            &mut header,
            DIB_RGB_COLORS,
        );
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_key_is_the_extension() {
        assert_eq!(cache_key("notes.TXT", false), "txt");
        assert_eq!(cache_key("a.b.c.zip", false), "zip");
        assert_eq!(cache_key("anything", true), cache_key("other", true));
        assert_ne!(cache_key("x.txt", false), cache_key("x.txt", true));
    }

    // A name with no extension, or a silly one, must not become a key that
    // collides with a real extension.
    #[test]
    fn odd_names_get_their_own_key() {
        assert_eq!(cache_key("README", false), "\u{0}none");
        assert_eq!(cache_key("", false), "\u{0}none");
        assert_eq!(cache_key("trailing.", false), "\u{0}none");
        // Long enough not to be an extension: some names just end in a dot and
        // a sentence.
        assert_eq!(cache_key("x.notreallyanextension", false), "\u{0}none");
    }

    #[test]
    fn garbage_names_do_not_panic() {
        for name in ["", ".", "..", "ñ.txt", "a\\b\\c.png", "a/b/c.png", "🙂.zip"] {
            let _ = cache_key(name, false);
            let _ = lookup(name, false);
            let _ = lookup(name, true);
        }
    }

    // One test, on purpose: these all go through the shell, and the shell wants
    // one thread. Split into three they run in parallel and fail for a reason
    // that has nothing to do with what they are checking.
    #[cfg(windows)]
    #[test]
    fn the_shell_hands_back_usable_icons() {
        let file = lookup("something.txt", false).expect("the shell should know .txt");
        assert!(file.width > 0 && file.height > 0);
        assert_eq!(file.rgba.len(), (file.width * file.height * 4) as usize);
        // Not a blank square: an icon nobody can see would pass every other
        // check here.
        assert!(file.rgba.chunks_exact(4).any(|px| px[3] > 0), "fully transparent");

        let dir = lookup("thing", true).expect("folder icon");
        assert_ne!(file.rgba, dir.rgba, "a folder and a file look the same");

        // Nothing here may go to disk: the entries live inside an archive, so a
        // name that exists and one that does not have to give one answer.
        let real = lookup("Cargo.toml", false).expect("icon");
        let invented = lookup("no-such-file-anywhere.toml", false).expect("icon");
        assert_eq!(real.rgba, invented.rgba);
    }
}
