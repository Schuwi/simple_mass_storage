#[path = "build/fat_image.rs"]
mod fat_image;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Total size of the (decompressed) embedded image. Must match `IMAGE_SIZE` in
/// `src/main.rs` (`IMAGE_BLOCKS_NUM * IMAGE_BLOCK_SIZE`); the generated module
/// re-exports it as `IMAGE_LEN` and `main.rs` asserts the two agree.
const IMAGE_SIZE: usize = 4 * 1024 * 1024;

/// Decompressed size of one ZX0 chunk. Each chunk is compressed independently so
/// the firmware can decode one at a time into a single reusable buffer.
///
/// Capped at 32 KiB on purpose: ZX0's maximum back-reference distance is 32640,
/// so a 32768-byte buffer can serve as *both* the decode window and the resident
/// chunk cache (see `zx0_decompress::decompress_into`). Going larger would force a
/// second output buffer. Must stay `>= zx0_decompress::MIN_WINDOW_LEN` (32641).
const CHUNK_SIZE: usize = 32 * 1024;

fn main() {
    download_svd();
    build_image();
}

/// Build the FAT image from `assets/`, split it into [`CHUNK_SIZE`] chunks,
/// ZX0-compress each independently, and emit:
///   * `$OUT_DIR/image.zx0`  -- the concatenated compressed chunks, and
///   * `$OUT_DIR/image.rs`   -- constants + a `(offset, len)` table into the blob,
/// which `main.rs` pulls in with `include!`.
fn build_image() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let assets = manifest_dir.join("assets");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let image = fat_image::build(&assets, IMAGE_SIZE, &manifest_dir);

    // Deduplicate identical chunks before compressing. Runs of identical sectors
    // (most commonly the all-zero free space of a sparse volume) collapse to a
    // single unique chunk, so each distinct chunk is compressed -- and stored --
    // exactly once. Unique chunks keep first-occurrence order to keep the blob
    // layout deterministic.
    let mut unique: Vec<&[u8]> = Vec::new();
    let mut chunk_ids: Vec<usize> = Vec::new();
    let mut seen: HashMap<&[u8], usize> = HashMap::new();
    for chunk in image.chunks(CHUNK_SIZE) {
        let id = *seen.entry(chunk).or_insert_with(|| {
            unique.push(chunk);
            unique.len() - 1
        });
        chunk_ids.push(id);
    }

    // ZX0's optimal parser is very slow on highly repetitive data (a 32 KiB
    // all-zero chunk takes ~20 s), which dominates dev rebuilds. Use the faster
    // "quick" parser for everything but `release`, where the best ratio is worth
    // the wait. Both emit standard ZX0 streams (verified below).
    let quick = env::var("PROFILE").map_or(true, |p| p != "release");

    // Compress the unique chunks in parallel across the available CPUs.
    let started = Instant::now();
    let compressed = parallel_compress(&unique, quick);
    let elapsed = started.elapsed();

    // Lay the unique compressed chunks out into the blob, then point every chunk
    // (including the deduplicated ones) at its unique chunk's `(offset, len)`.
    let mut blob = Vec::new();
    let unique_entries: Vec<(u32, u32)> = compressed
        .iter()
        .map(|c| {
            let entry = (blob.len() as u32, c.len() as u32);
            blob.extend_from_slice(c);
            entry
        })
        .collect();
    let table: Vec<(u32, u32)> = chunk_ids.iter().map(|&id| unique_entries[id]).collect();

    verify_roundtrip(&image, &blob, &table);

    fs::write(out_dir.join("image.zx0"), &blob).expect("Failed to write image.zx0");
    fs::write(out_dir.join("image.rs"), generate_module(&table))
        .expect("Failed to write image.rs");

    println!(
        "cargo:warning=Embedded image: {} chunks ({} unique), {} -> {} bytes ZX0 {} ({:.1}%) in {} ms",
        table.len(),
        unique.len(),
        image.len(),
        blob.len(),
        if quick { "quick" } else { "optimal" },
        100.0 * blob.len() as f64 / image.len() as f64,
        elapsed.as_millis(),
    );

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

/// ZX0-compress each unique chunk, spreading the work across the available CPUs.
///
/// `quick` selects ZX0's faster (smaller-dictionary) parser over the optimal one.
/// Output is indexed to match `unique`, so the caller's blob layout stays
/// deterministic regardless of how the work is scheduled. Uses scoped threads
/// (no extra dependency); a single CPU degrades to a plain sequential pass.
fn parallel_compress(unique: &[&[u8]], quick: bool) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![Vec::new(); unique.len()];
    if unique.is_empty() {
        return out;
    }

    let threads = std::thread::available_parallelism().map_or(1, |p| p.get());
    let per_thread = unique.len().div_ceil(threads);

    std::thread::scope(|scope| {
        for (out_part, in_part) in out.chunks_mut(per_thread).zip(unique.chunks(per_thread)) {
            scope.spawn(move || {
                for (slot, chunk) in out_part.iter_mut().zip(in_part) {
                    *slot = zx0::Compressor::new().quick_mode(quick).compress(chunk).output;
                }
            });
        }
    });

    out
}

/// Decode the compressed blob with the firmware's own decompressor and assert it
/// reproduces `image` exactly, using the same single-buffer scheme the device
/// does. Catches any ZX0 compressor/decompressor mismatch before it is flashed.
fn verify_roundtrip(image: &[u8], blob: &[u8], table: &[(u32, u32)]) {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut reconstructed = Vec::with_capacity(image.len());
    for &(offset, len) in table {
        let compressed = &blob[offset as usize..(offset + len) as usize];
        let decoded = zx0_decompress::decompress_into(compressed, &mut buf)
            .expect("embedded chunk must decode back");
        reconstructed.extend_from_slice(decoded);
    }
    assert!(
        reconstructed == image,
        "ZX0 round-trip mismatch: decompressed blob does not match the source image"
    );
}

/// Render the generated `image.rs` source pulled in by `main.rs`.
fn generate_module(table: &[(u32, u32)]) -> String {
    let mut entries = String::new();
    for (offset, len) in table {
        entries.push_str(&format!("    ({offset}, {len}),\n"));
    }
    format!(
        "// @generated by build.rs -- do not edit.\n\
         /// Decompressed length of the whole image.\n\
         pub const IMAGE_LEN: usize = {image_len};\n\
         /// Decompressed length of one chunk (also the decode buffer size).\n\
         pub const CHUNK_SIZE: usize = {chunk_size};\n\
         /// Number of ZX0 chunks the image is split into.\n\
         pub const CHUNK_COUNT: usize = {count};\n\
         /// `(offset, compressed_len)` of each chunk within [`COMPRESSED`].\n\
         pub static CHUNK_TABLE: [(u32, u32); CHUNK_COUNT] = [\n{entries}];\n\
         /// The concatenated, independently ZX0-compressed chunks.\n\
         pub static COMPRESSED: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/image.zx0\"));\n",
        image_len = IMAGE_SIZE,
        chunk_size = CHUNK_SIZE,
        count = table.len(),
    )
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
