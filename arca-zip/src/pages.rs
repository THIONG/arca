//! The single byte code pages a zip's names might be written in.
//!
//! A zip either flags its names as UTF-8 or leaves them in whatever the machine
//! that wrote them was using, and never says which that was. CP437 is what the
//! format nominally means and what everything assumes, but an archive made on a
//! Spanish Windows is CP1252 and one made on a Russian DOS is CP866, and read
//! as CP437 those names come out as rubbish. Nothing in the file says which, so
//! this is a question only the person looking at it can answer.
//!
//! The tables are the system's own, read out of Windows rather than typed from
//! memory: a wrong entry here is a name that is quietly wrong, which is worse
//! than one that is obviously broken.
//!
//! Only single byte pages. Shift-JIS and GBK are multi byte and would need a
//! different kind of decoder; when somebody turns up with one of those it can
//! have its own answer rather than a table that half works.

const CP850_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00F8}', '\u{00A3}', '\u{00D8}', '\u{00D7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{00AE}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{00C1}', '\u{00C2}', '\u{00C0}',
    '\u{00A9}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{00A2}', '\u{00A5}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{00E3}', '\u{00C3}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{00A4}',
    '\u{00F0}', '\u{00D0}', '\u{00CA}', '\u{00CB}', '\u{00C8}', '\u{0131}', '\u{00CD}', '\u{00CE}',
    '\u{00CF}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{00A6}', '\u{00CC}', '\u{2580}',
    '\u{00D3}', '\u{00DF}', '\u{00D4}', '\u{00D2}', '\u{00F5}', '\u{00D5}', '\u{00B5}', '\u{00FE}',
    '\u{00DE}', '\u{00DA}', '\u{00DB}', '\u{00D9}', '\u{00FD}', '\u{00DD}', '\u{00AF}', '\u{00B4}',
    '\u{00AD}', '\u{00B1}', '\u{2017}', '\u{00BE}', '\u{00B6}', '\u{00A7}', '\u{00F7}', '\u{00B8}',
    '\u{00B0}', '\u{00A8}', '\u{00B7}', '\u{00B9}', '\u{00B3}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

const CP866_HIGH: [char; 128] = [
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
    '\u{0401}', '\u{0451}', '\u{0404}', '\u{0454}', '\u{0407}', '\u{0457}', '\u{040E}', '\u{045E}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{2116}', '\u{00A4}', '\u{25A0}', '\u{00A0}',
];

const CP1251_HIGH: [char; 128] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{0098}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
];

const CP1252_HIGH: [char; 128] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    '\u{00A0}', '\u{00A1}', '\u{00A2}', '\u{00A3}', '\u{00A4}', '\u{00A5}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{00AA}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{00AF}',
    '\u{00B0}', '\u{00B1}', '\u{00B2}', '\u{00B3}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{00B9}', '\u{00BA}', '\u{00BB}', '\u{00BC}', '\u{00BD}', '\u{00BE}', '\u{00BF}',
    '\u{00C0}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{00C7}',
    '\u{00C8}', '\u{00C9}', '\u{00CA}', '\u{00CB}', '\u{00CC}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    '\u{00D0}', '\u{00D1}', '\u{00D2}', '\u{00D3}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{00D7}',
    '\u{00D8}', '\u{00D9}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{00DD}', '\u{00DE}', '\u{00DF}',
    '\u{00E0}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{00E7}',
    '\u{00E8}', '\u{00E9}', '\u{00EA}', '\u{00EB}', '\u{00EC}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    '\u{00F0}', '\u{00F1}', '\u{00F2}', '\u{00F3}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{00F7}',
    '\u{00F8}', '\u{00F9}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{00FD}', '\u{00FE}', '\u{00FF}',
];

/// Which of them to read a name with.
///
/// `Cp437` is what an unflagged zip nominally means and stays the default;
/// everything else is somebody saying "no, it was made over there".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Page {
    #[default]
    Cp437,
    Cp850,
    Cp1252,
    Cp866,
    Cp1251,
    Latin1,
}

impl Page {
    /// All of them, with the short name each is written as in the settings
    /// file and the name a person reads.
    pub const ALL: [(Page, &'static str, &'static str); 6] = [
        (Page::Cp437, "cp437", "CP437 · DOS"),
        (Page::Cp850, "cp850", "CP850 · DOS Latin"),
        (Page::Cp1252, "cp1252", "CP1252 · Windows Latin"),
        (Page::Cp866, "cp866", "CP866 · DOS Cyrillic"),
        (Page::Cp1251, "cp1251", "CP1251 · Windows Cyrillic"),
        (Page::Latin1, "latin1", "ISO 8859-1"),
    ];

    pub fn from_code(code: &str) -> Option<Page> {
        Page::ALL
            .iter()
            .find(|(_, name, _)| *name == code)
            .map(|(p, _, _)| *p)
    }

    pub fn code(self) -> &'static str {
        Page::ALL
            .iter()
            .find(|(p, _, _)| *p == self)
            .map(|(_, name, _)| *name)
            .unwrap_or("cp437")
    }

    fn high(self) -> Option<&'static [char; 128]> {
        match self {
            Page::Cp437 => Some(&super::CP437_HIGH),
            Page::Cp850 => Some(&CP850_HIGH),
            Page::Cp1252 => Some(&CP1252_HIGH),
            Page::Cp866 => Some(&CP866_HIGH),
            Page::Cp1251 => Some(&CP1251_HIGH),
            // Latin-1 is the one page that needs no table: the byte is the
            // character.
            Page::Latin1 => None,
        }
    }
}

/// Reads `bytes` as a name in `page`.
///
/// The low half is ASCII in every one of these, which is why a zip full of
/// plain names looks the same whichever is chosen and why the question only
/// comes up for the archives where it matters.
pub fn decode(bytes: &[u8], page: Page) -> String {
    let high = page.high();
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii() {
                b as char
            } else {
                match high {
                    Some(table) => table[usize::from(b) - 0x80],
                    None => b as char,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The same bytes read four ways. This is the whole feature: one archive,
    // several answers, and only the person looking at it knows which is right.
    #[test]
    fn the_same_bytes_say_different_things_in_different_pages() {
        // 0x80 and 0xA0, which mean something different in every one of them.
        let bytes = [b'a', 0x80, 0xA0, b'z'];
        let read = |p| decode(&bytes, p);
        assert_eq!(
            read(Page::Cp437),
            "a\u{00C7}\u{00E1}z",
            "C cedilla, a acute"
        );
        assert_eq!(
            read(Page::Cp850),
            "a\u{00C7}\u{00E1}z",
            "the DOS pages agree here"
        );
        assert_eq!(read(Page::Cp1252), "a\u{20AC}\u{00A0}z", "euro, hard space");
        assert_eq!(
            read(Page::Cp866),
            "a\u{0410}\u{0430}z",
            "A and a, in Cyrillic"
        );
        assert_eq!(
            read(Page::Latin1),
            "a\u{0080}\u{00A0}z",
            "the byte is the character"
        );
    }

    #[test]
    fn plain_names_read_the_same_whichever_page_is_chosen() {
        let plain = b"documento_1.txt";
        for (page, _, _) in Page::ALL {
            assert_eq!(decode(plain, page), "documento_1.txt");
        }
    }

    #[test]
    fn every_page_answers_to_the_name_it_is_written_under() {
        for (page, code, _) in Page::ALL {
            assert_eq!(Page::from_code(code), Some(page));
            assert_eq!(page.code(), code);
        }
        assert_eq!(Page::from_code("klingon"), None);
    }
}
