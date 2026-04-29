use reqwest::blocking::get;
use std::{env, fs, path::PathBuf};

const DDI_DOWNLOADS: [(&str, &str); 3] = [
    (
        "BuildManifest.plist",
        "https://github.com/doronz88/DeveloperDiskImage/raw/refs/heads/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/BuildManifest.plist",
    ),
    (
        "Image.dmg",
        "https://github.com/doronz88/DeveloperDiskImage/raw/refs/heads/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/Image.dmg",
    ),
    (
        "Image.dmg.trustcache",
        "https://github.com/doronz88/DeveloperDiskImage/raw/refs/heads/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/Image.dmg.trustcache",
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Windows.ico");

    embed_ddi_bundle();

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("Windows.ico");
        res.compile().expect("Failed to compile Windows resources");
    }
}

fn embed_ddi_bundle() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is not set"));
    let ddi_dir = out_dir.join("DDI");
    fs::create_dir_all(&ddi_dir).expect("Failed to create build DDI directory");

    for (name, url) in DDI_DOWNLOADS {
        let path = ddi_dir.join(name);
        if path.exists() {
            continue;
        }

        let response = get(url)
            .and_then(|response| response.error_for_status())
            .expect("Failed to download DDI file");
        let bytes = response.bytes().expect("Failed to read downloaded DDI file");
        fs::write(&path, &bytes).expect("Failed to write DDI file into build directory");
    }

    let generated = format!(
        concat!(
            "pub const BUILD_MANIFEST: &[u8] = include_bytes!(r#\"{}\"#);\n",
            "pub const IMAGE_DMG: &[u8] = include_bytes!(r#\"{}\"#);\n",
            "pub const IMAGE_TRUSTCACHE: &[u8] = include_bytes!(r#\"{}\"#);\n"
        ),
        ddi_dir.join("BuildManifest.plist").display(),
        ddi_dir.join("Image.dmg").display(),
        ddi_dir.join("Image.dmg.trustcache").display()
    );

    fs::write(out_dir.join("ddi_bundle.rs"), generated)
        .expect("Failed to generate embedded DDI source");
}
