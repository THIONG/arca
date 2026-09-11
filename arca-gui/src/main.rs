#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod archive_ops;
mod clipboard;
mod controller;
mod gpui_shell;
mod gpui_theme;
mod i18n;
mod model;
mod settings;
mod tree;

// Keep the GPUI view focused on presentation while exposing the application
// contract at the crate boundary. These are crate-private, not public API.
pub(crate) use archive_ops::*;
pub(crate) use controller::*;
pub(crate) use model::*;
pub(crate) use settings::*;

use arca_core::{Codec, Level};
use i18n::{strings, Lang, Strings};
use tree::{parent_of, Row};
// How wide a column starts out and the least it can be pulled down to. The
// name gets the room because it is the thing being read; the rest hold a
// number or a word and are sized for it.
const NAME_WIDE: f32 = 320.0;
const NAME_LEAST: f32 = 140.0;
const CELL_WIDE: f32 = 95.0;
const CELL_LEAST: f32 = 60.0;

fn main() {
    gpui_shell::run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_attribute_letters_hold_their_places() {
        assert_eq!(attribute_letters(0), "----");
        assert_eq!(attribute_letters(0x01), "R---");
        assert_eq!(attribute_letters(0x20), "---A");
        assert_eq!(attribute_letters(0x01 | 0x02 | 0x04 | 0x20), "RHSA");
        // The directory bit is the list's job, not this column's.
        assert_eq!(attribute_letters(0x10), "----");
    }

    #[test]
    fn text_is_told_from_the_rest_by_what_it_does_not_have() {
        assert!(looks_like_text(b""), "an empty file opens as an empty page");
        assert!(looks_like_text(b"hola\r\nque tal\ttabulado\n"));
        assert!(
            looks_like_text("acentos y enes: aeiou \u{f1}\u{e1}".as_bytes()),
            "high bytes are a name with an accent, not a program"
        );
        assert!(
            !looks_like_text(b"MZ\x90\x00\x03\x00\x00\x00"),
            "a zero settles it"
        );
        // No zeros, but nothing readable either.
        let noise: Vec<u8> = (1..=200u8).map(|b| b % 0x1F + 1).collect();
        assert!(!looks_like_text(&noise));
    }

    #[test]
    fn a_later_version_is_the_one_with_the_larger_numbers() {
        assert!(newer("0.5.1", "0.6.0"));
        assert!(newer("0.9.0", "0.10.0"), "ten comes after nine");
        assert!(newer("0.5.1", "v0.5.2"), "a leading v is forgiven");
        assert!(!newer("0.6.0", "0.5.9"), "older is not newer");
        assert!(!newer("0.6.0", "0.6.0"), "the same is not newer");
        assert!(!newer("0.10.0", "0.9.0"), "and the other way round too");
        // Missing parts are zero, so these are the same version.
        assert!(!newer("0.6", "0.6.0"));
        assert!(!newer("0.6.0", "0.6"));
        // A release candidate is earlier than the release it is a candidate for.
        assert!(newer("0.6.0-rc1", "0.6.0"));
        assert!(!newer("0.6.0", "0.6.0-rc1"));
    }

    // Anything unreadable has to answer no. A window that cannot tell what the
    // announcement said should say nothing, not guess.
    #[test]
    fn nonsense_never_announces_an_update() {
        assert!(!newer("0.5.1", ""));
        assert!(!newer("0.5.1", "manana"));
        assert!(!newer("0.5.1", "0.5.uno"));
        assert!(!newer("", "9.9.9"));
        assert!(
            !newer("0.5.1", "99999999999999999999"),
            "past what a number holds"
        );
    }

    #[test]
    fn the_release_name_comes_out_of_the_reply() {
        let reply = r#"{"url":"https://x/1","tag_name":"v0.6.0","name":"Arca 0.6.0"}"#;
        assert_eq!(tag_of(reply).as_deref(), Some("v0.6.0"));
        // Spacing is the writer's business, not ours.
        assert_eq!(
            tag_of(r#"{ "tag_name" : "0.7.0" }"#).as_deref(),
            Some("0.7.0")
        );
        // And everything that is not an answer is not an answer.
        assert_eq!(tag_of("{}"), None);
        assert_eq!(tag_of(""), None);
        assert_eq!(tag_of(r#"{"tag_name":""}"#), None, "a name of nothing");
        assert_eq!(tag_of(r#"{"tag_name":"x"}"#).as_deref(), Some("x"));
        let long = format!(r#"{{"tag_name":"{}"}}"#, "v".repeat(64));
        assert_eq!(tag_of(&long), None, "somebody being funny");
    }

    #[test]
    fn la_respuesta_de_la_release_da_version_instalador_y_sumas() {
        let reply = r#"{"url":"https://api.github.com/x","tag_name":"v0.6.2","assets":[
            {"name":"SHA256SUMS.txt","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/SHA256SUMS.txt"},
            {"name":"arca-setup-0.6.2-x86_64.exe","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/arca-setup-0.6.2-x86_64.exe"},
            {"name":"arca-v0.6.2-linux-x86_64.tar.gz","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/arca-v0.6.2-linux-x86_64.tar.gz"}]}"#;
        let r = release_of(reply).expect("una release");
        assert_eq!(r.tag, "v0.6.2");
        assert_eq!(
            r.installer.as_deref(),
            Some("https://github.com/THIONG/arca/releases/download/v0.6.2/arca-setup-0.6.2-x86_64.exe")
        );
        assert!(r.sums.is_some());
    }

    #[test]
    fn una_direccion_que_no_sea_la_nuestra_no_se_acepta() {
        let reply = r#"{"tag_name":"v9.9.9","assets":[
            {"browser_download_url":"https://evil.example/arca-setup-9.9.9-x86_64.exe"},
            {"browser_download_url":"http://github.com/THIONG/arca/releases/download/v9/arca-setup-9-x86_64.exe"},
            {"browser_download_url":"https://github.com/otro/arca/releases/download/v9/arca-setup-9-x86_64.exe"},
            {"browser_download_url":"https://github.com/THIONG/arca/releases/download/v9/SHA256SUMS.txt"}]}"#;
        let r = release_of(reply).expect("una release");
        assert_eq!(r.tag, "v9.9.9");
        assert!(
            r.installer.is_none(),
            "ni otro dominio, ni sin cifrar, ni otro repositorio"
        );
        assert!(r.sums.is_some(), "la nuestra si");
    }

    // `sha256sum` escribe dos espacios para lo que leyo como texto y espacio y
    // asterisco para lo que leyo como binario. Las mitades de Windows de
    // nuestras propias releases salen con el asterisco.
    #[test]
    fn la_suma_se_encuentra_con_las_dos_escrituras() {
        let listing = "\
8c0a3844b53278b6fb1557c2cdc28f5c6b0a2eaf29e6214a734798d81473b5f3  arca-v0.6.1-linux-x86_64.tar.gz
c76ecf12e8e05b8f4730fb9933450ea121fe1ce3699ab9b7d20b058f872815f4 *arca-setup-0.6.1-x86_64.exe
";
        let binario = sum_for(listing, "arca-setup-0.6.1-x86_64.exe").expect("la del exe");
        assert_eq!(binario[0], 0xc7);
        assert_eq!(binario[31], 0xf4);
        let texto = sum_for(listing, "arca-v0.6.1-linux-x86_64.tar.gz").expect("la del tar");
        assert_eq!(texto[0], 0x8c);
        assert!(sum_for(listing, "arca-setup-0.6.2-x86_64.exe").is_none());
    }

    // Five settings were read at startup and never written, because the line
    // that built the file had quietly stopped mentioning them. A round trip
    // that never touches a disk is the only way that stays fixed.
    #[test]
    fn every_setting_survives_being_written_and_read_again() {
        let mut before = Settings {
            lang: Some(Lang::Es),
            theme: ThemePreference::Light,
            flat: true,
            tree: true,
            updates: false,
            page: arca_zip::pages::Page::Cp1252,
            ..Default::default()
        };
        before.columns.set(SortColumn::Crc, true);
        before.columns.set(SortColumn::Size, false);
        before.widths[0] = 271.0;
        before.widths[3] = 88.0;
        before.window = Some([12.0, 34.0, 1000.0, 700.0]);
        before.recent = vec!["C:\\uno.zip".into(), "D:\\dos, con coma.zip".into()];

        let after = Settings::parse(&before.text());

        assert_eq!(after.lang, before.lang);
        assert_eq!(after.theme, before.theme);
        assert_eq!(after.flat, before.flat);
        assert_eq!(after.tree, before.tree);
        assert_eq!(after.updates, before.updates);
        assert_eq!(after.page, before.page);
        assert_eq!(after.widths, before.widths);
        assert_eq!(after.window, before.window);
        assert_eq!(
            after.recent, before.recent,
            "y una coma en un nombre no parte nada"
        );
        for (which, name) in Columns::ALL {
            assert_eq!(
                after.columns.on(which),
                before.columns.on(which),
                "la columna {name}"
            );
        }
    }

    #[test]
    fn moving_reads_an_archive_written_with_backslashes() {
        // Windows's own Compress-Archive writes these, and the window shows and
        // compares forward slashes. Before this they matched nothing and a
        // move inside a folder did nothing without saying so.
        let moves = vec![("carpeta/f1.txt".to_string(), "f1.txt".to_string())];
        assert_eq!(moved_name(r"carpeta\f1.txt", &moves), "f1.txt");
        assert_eq!(moved_name(r"carpeta\f2.txt", &moves), "carpeta/f2.txt");
    }

    #[test]
    fn moving_carries_a_whole_branch_and_leaves_everything_else_alone() {
        let moves = vec![
            ("docs/notas".to_string(), "notas".to_string()),
            ("leeme.txt".to_string(), "docs/leeme.txt".to_string()),
        ];
        let of = |n: &str| moved_name(n, &moves);

        // The folder, both ways it can be written, and what is under it.
        assert_eq!(of("docs/notas"), "notas");
        assert_eq!(of("docs/notas/"), "notas/");
        assert_eq!(of("docs/notas/uno.md"), "notas/uno.md");
        assert_eq!(of("docs/notas/dos/tres.md"), "notas/dos/tres.md");
        // A file on its own.
        assert_eq!(of("leeme.txt"), "docs/leeme.txt");
        // Everything else, including names that begin the same way and are not
        // the same folder at all.
        assert_eq!(of("docs/notas2/otro.md"), "docs/notas2/otro.md");
        assert_eq!(of("docs/uno.txt"), "docs/uno.txt");
        assert_eq!(of("leeme.txt.bak"), "leeme.txt.bak");
        assert_eq!(of("otra/cosa.bin"), "otra/cosa.bin");
    }

    #[test]
    fn a_mask_picks_the_names_it_describes() {
        assert!(matches_mask("*.txt", "notes.txt"));
        assert!(
            matches_mask("*.TXT", "notes.txt"),
            "case is not the question"
        );
        assert!(!matches_mask("*.txt", "notes.txt.bak"));
        assert!(matches_mask("nota_?.md", "nota_3.md"));
        assert!(!matches_mask("nota_?.md", "nota_33.md"));
        assert!(matches_mask("*", "anything at all"));
        assert!(matches_mask("a*b*c", "axxbyyc"));
        assert!(!matches_mask("a*b*c", "axxbyy"));
        // A mask with nothing special in it is just a name.
        assert!(matches_mask("leeme.txt", "leeme.txt"));
        assert!(!matches_mask("leeme.txt", "leeme.txt.old"));
    }

    // A row of stars against a long name is the case that turns a naive
    // recursive matcher into a hang. It has to come back in no time at all.
    #[test]
    fn a_mask_of_nothing_but_stars_does_not_take_all_afternoon() {
        let name = "a".repeat(64);
        let mask = format!("{}b", "*a".repeat(20));
        let began = std::time::Instant::now();
        assert!(!matches_mask(&mask, &name));
        assert!(began.elapsed().as_millis() < 50, "backtracking ran away");
    }

    // The wheel is a button as well as a wheel, and pressing one moves the
    // hand: a dead zone is the difference between a list that waits and a list
    // that creeps for as long as the anchor is down.
    #[test]
    fn a_hand_resting_on_the_wheel_leaves_the_list_where_it_is() {
        assert_eq!(wheel_speed(0.0), 0.0);
        assert_eq!(wheel_speed(-8.0), 0.0);
        assert_eq!(wheel_speed(12.0), 0.0);
    }

    #[test]
    fn the_list_runs_the_way_the_pointer_went_and_harder_the_further_it_is() {
        assert!(wheel_speed(40.0) > 0.0);
        assert!(wheel_speed(-40.0) < 0.0);
        assert!(wheel_speed(90.0) > wheel_speed(40.0));
        // Up and down are the same gesture mirrored, and a list that ran
        // faster one way than the other would be maddening rather than wrong.
        assert_eq!(wheel_speed(40.0), -wheel_speed(-40.0));
        // Far enough out and it stops getting faster: everything past here is
        // a blur either way.
        assert_eq!(wheel_speed(900.0), wheel_speed(2000.0));
    }

    // The only arithmetic in this file that can be wrong without anyone
    // noticing: a date is either right or plausible, and plausible is worse.
    #[test]
    fn timestamps_become_the_dates_they_are() {
        for (secs, text) in [
            (0_i64, ""), // no date recorded
            (-1, ""),    // before the epoch: tar can hold these
            (1, "1970-01-01 00:00"),
            (951_827_696, "2000-02-29 12:34"), // leap day of a leap century
            (1_078_012_800, "2004-02-29 00:00"), // ordinary leap year
            (1_709_164_800, "2024-02-29 00:00"),
            (1_709_251_199, "2024-02-29 23:59"), // last minute of that day
            (1_735_689_600, "2025-01-01 00:00"), // year boundary
            (1_767_225_599, "2025-12-31 23:59"),
            (2_208_988_800, "2040-01-01 00:00"), // past a 32-bit second count
        ] {
            assert_eq!(when(Some(secs)), text, "{secs}");
        }
        assert_eq!(when(None), "");
    }

    // A folder keeps its whole name and a file loses its extension, and the
    // shell extension has a copy of this that has to agree.
    // Which way the sort mark points is a sign, and a sign is the one thing you
    // cannot check by looking at a screenshot of a list with one row in it.
    #[test]
    fn the_clock_reads_as_a_clock() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(7.4), "0:07");
        assert_eq!(clock(98.0), "1:38");
        assert_eq!(clock(3600.0), "1:00:00");
        assert_eq!(clock(7511.0), "2:05:11");
        // A guess made from almost nothing, and one made from nonsense.
        assert_eq!(clock(-5.0), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
        assert_eq!(clock(f64::INFINITY), "0:00");
    }

    // The path row is the one place the window can run out of width, and it did:
    // a deep folder pushed the trail out over the count at the other end.
    #[test]
    fn archive_stem_strips_what_it_should() {
        for (name, stem) in [
            ("game.zip", "game"),
            ("backup.tar.gz", "backup"),
            ("backup.tgz", "backup"),
            ("plain.tar", "plain"),
            ("UPPER.ZIP", "UPPER"),
            ("dots.in.name.zip", "dots.in.name"),
            ("no-extension", "no-extension"),
        ] {
            assert_eq!(archive_stem(Path::new(name)), stem, "{name}");
        }
    }

    #[test]
    fn turning_columns_on_and_off_survives_a_round_trip() {
        let mut c = Columns::default();
        c.set(SortColumn::Crc, true);
        c.set(SortColumn::Packed, false);
        assert!(c.on(SortColumn::Crc));
        assert!(!c.on(SortColumn::Packed));
        // Name is not a column anyone may turn off.
        c.set(SortColumn::Name, false);
        assert!(c.on(SortColumn::Name));
    }
}
