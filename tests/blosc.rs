//! Check the pure Rust blosc decoder against buffers encoded by numcodecs
//! (c-blosc) with every compressor, with/without shuffle and several block sizes.

use std::path::Path;

use copernicusmarine::codecs::blosc_decompress;

#[test]
fn decodes_numcodecs_blosc_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("blosc") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        let dtype = name.split('_').next().unwrap();
        let expected = std::fs::read(dir.join(format!("{dtype}.raw"))).unwrap();
        let encoded = std::fs::read(&path).unwrap();
        let decoded = blosc_decompress(&encoded).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(decoded == expected, "{name}: decoded data differs");
        checked += 1;
    }
    assert_eq!(checked, 81);
}
