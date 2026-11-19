use std::path::Path;

#[derive(Clone, Copy, Default, PartialEq)]
pub enum BundledScript {
    Geode,
    MacIos,
    #[default]
    Universal,
    Legacy,
}

impl BundledScript {
    pub const ALL: [Self; 4] = [Self::Geode, Self::Legacy, Self::MacIos, Self::Universal];

    pub fn name(self) -> &'static str {
        match self {
            Self::Geode => "Geode.js",
            Self::MacIos => "maciOS.js",
            Self::Universal => "universal.js",
            Self::Legacy => "legacy.js",
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::Geode => include_str!("../scripts/Geode.js"),
            Self::MacIos => include_str!("../scripts/maciOS.js"),
            Self::Universal => include_str!("../scripts/universal.js"),
            Self::Legacy => include_str!("../scripts/legacy.js"),
        }
    }

    pub fn recommended(target: &str) -> Option<Self> {
        match script_name_key(target).as_str() {
            "geode" => Some(Self::Geode),
            "macios" => Some(Self::MacIos),
            "amethyst" | "applesauce" | "dukex" | "henge" | "husk" | "manicemu" | "melocafe"
            | "melocafé" | "melonx" | "rpcs3" | "utsuro" | "xenios" | "aetherps4" => {
                Some(Self::Universal)
            }
            "utm" | "dolphinios" | "flycast" | "armsx2" | "armsx2ios" => Some(Self::Legacy),
            _ => None,
        }
    }
}

pub fn script_name_key(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    stem.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
