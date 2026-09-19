#![no_main]
use libfuzzer_sys::fuzz_target;
use zeppelin_embed::property_graph::catalog::CatalogImage;

fn exercise(data: &[u8]) {
    if let Ok(image) = CatalogImage::decode(data, 1024 * 1024, &mut || Ok(())) {
        let mut bytes = vec![0; image.encoded_len(&mut || Ok(())).expect("checked length")];
        image
            .encode_into(&mut bytes, &mut || Ok(()))
            .expect("checked encoding");
        let restored =
            CatalogImage::decode(&bytes, 1024 * 1024, &mut || Ok(())).expect("encoded catalog");
        assert_eq!(restored.declaration, image.declaration);
        assert_eq!(restored.symbols.high_waters(), image.symbols.high_waters());
        assert_eq!(restored.symbols.entries(), image.symbols.entries());
    }
}
fuzz_target!(|data: &[u8]| {
    exercise(data);
    // Retain the raw corruption leg; repair only the outer length/checksum in
    // this second leg so mutations reach row, interpretation and UTF-8 parsing.
    if data.len() >= 128 {
        let mut repaired = data.to_vec();
        repaired[8..16].copy_from_slice(&(data.len() as u64).to_le_bytes());
        let end = repaired.len() - 8;
        let checksum = xxhash_rust::xxh3::xxh3_64(&repaired[..end]);
        repaired[end..].copy_from_slice(&checksum.to_le_bytes());
        exercise(&repaired);
    }
});
