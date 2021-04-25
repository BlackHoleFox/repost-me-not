use crate::Error;

use image::io::Reader;
use img_hash::{HashAlg, HasherConfig};
use std::io::Cursor;

type HashStorage = [u8; 8];
pub type ImageHash = img_hash::ImageHash<HashStorage>;

const DIFFERENCE_THRESHOLD: u32 = 7;

pub fn process_image(image: Vec<u8>) -> Result<ImageHash, Error> {
    let hasher = HasherConfig::with_bytes_type::<HashStorage>()
        .hash_alg(HashAlg::DoubleGradient)
        .preproc_dct()
        .to_hasher();

    let start = std::time::Instant::now();
    let image = Reader::new(Cursor::new(image))
        .with_guessed_format()
        .expect("Cursor seeking can't fail")
        .decode()
        .map_err(Error::UnsupportedImageFormat)?;

    println!(
        "It took {}ms to decode the image",
        start.elapsed().as_millis()
    );
    let start = std::time::Instant::now();
    let hash = hasher.hash_image(&image);
    println!(
        "It took {}ms to hash the image",
        start.elapsed().as_millis()
    );

    Ok(hash)
}

pub fn similar_enough(new: &ImageHash, seen: &[u8]) -> bool {
    let seen = match ImageHash::from_bytes(seen) {
        Ok(h) => h,
        _ => unreachable!("bug: sled returned the wrong key size"),
    };

    let start = std::time::Instant::now();
    let dist = new.dist(&seen);
    println!(
        "It took {}ms to compare a hash distance",
        start.elapsed().as_millis()
    );

    println!("Distance was {}", dist);

    dist <= DIFFERENCE_THRESHOLD
}
