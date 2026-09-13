use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

const CUSTOM: [&str; 4] = [
    "icons/file-output.svg",
    "icons/lock.svg",
    "icons/scissors.svg",
    "icons/trash-2.svg",
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let data: &'static [u8] = match path {
            "icons/file-output.svg" => include_bytes!("../assets/icons/file-output.svg"),
            "icons/lock.svg" => include_bytes!("../assets/icons/lock.svg"),
            "icons/scissors.svg" => include_bytes!("../assets/icons/scissors.svg"),
            "icons/trash-2.svg" => include_bytes!("../assets/icons/trash-2.svg"),
            _ => return gpui::AssetSource::load(&gpui_kit_assets::Assets, path),
        };
        Ok(Some(Cow::Borrowed(data)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = gpui::AssetSource::list(&gpui_kit_assets::Assets, path)?;
        assets.extend(
            CUSTOM
                .into_iter()
                .filter(|asset| asset.starts_with(path))
                .map(Into::into),
        );
        Ok(assets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_icons_load_alongside_the_kit() {
        let assets = Assets;
        for path in CUSTOM {
            let data = assets.load(path).unwrap().unwrap();
            assert!(data.starts_with(b"<svg"), "invalid SVG asset: {path}");
        }
        assert!(assets.load("icons/copy.svg").unwrap().is_some());
    }
}
