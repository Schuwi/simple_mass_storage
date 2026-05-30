#[path = "build/fat_image.rs"]
mod fat_image;

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Total size of the embedded image. Must match `IMAGE_SIZE` in `src/main.rs`
/// (`IMAGE_BLOCKS_NUM * IMAGE_BLOCK_SIZE`). `include_bytes!` in `main.rs` pins
/// the file to exactly this many bytes, so a mismatch is a compile error there.
const IMAGE_SIZE: usize = 512 * 512;

fn main() {
    download_svd();
    build_image();
}

/// Build the FAT image from `assets/` into `$OUT_DIR/image.img`, ready for
/// `include_bytes!`.
fn build_image() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let assets = manifest_dir.join("assets");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("image.img");

    let image = fat_image::build(&assets, IMAGE_SIZE, &manifest_dir);
    fs::write(&out, &image).expect("Failed to write image.img");

    // Rebuild the image whenever the asset tree or the builder itself changes.
    // (Cargo already re-runs this script when build.rs changes.)
    println!("cargo:rerun-if-changed=build/fat_image.rs");
    rerun_if_changed_recursive(&assets);
    // Entry timestamps come from git history, so a new commit can change the
    // image even when no asset file does; watch the reflog to catch commits.
    let git_log = manifest_dir.join(".git/logs/HEAD");
    if git_log.exists() {
        println!("cargo:rerun-if-changed={}", git_log.display());
    }
}

/// Emit `rerun-if-changed` for `path` and, if it is a directory, everything
/// underneath it, so adding/removing/editing any asset triggers a rebuild.
fn rerun_if_changed_recursive(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            rerun_if_changed_recursive(&entry.path());
        }
    }
}

/// Download the patched SVD for the target chip into the project root (once).
fn download_svd() {
    // Retrieve the target chip series from the environment variable
    let target = "stm32f401.svd";

    let file_name = format!("{target}.svd");
    let output_path = Path::new(&file_name);
    let url = format!("https://stm32-rs.github.io/stm32-rs/{target}.patched");

    // Check if the file already exists
    if output_path.exists() {
        println!(
            "SVD file already exists at {:?}, skipping download.",
            output_path
        );
    } else {
        // Ensure the output directory exists
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create directory for SVD file");
        }

        // Download the file
        println!("Downloading SVD file from {}...", url);
        let response = reqwest::blocking::get(&url).expect("Failed to fetch SVD file");
        let content = response.text().expect("Failed to read response body");

        // Write the downloaded content to the file
        let mut file = fs::File::create(output_path).expect("Failed to create SVD file");
        file.write_all(content.as_bytes())
            .expect("Failed to write SVD file");

        println!("SVD file saved to {:?}", output_path);
    }
}
