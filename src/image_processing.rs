use crate::Error;

use image::io::Reader;
use img_hash::{HashAlg, HasherConfig};
use std::io::Cursor;

type HashStorage = [u8; 64];
pub type ImageHash = img_hash::ImageHash<HashStorage>;

const DIFFERENCE_THRESHOLD: u32 = 8;

pub fn process_image(image: Vec<u8>) -> Result<ImageHash, Error> {
    let hasher = HasherConfig::with_bytes_type::<HashStorage>()
        .hash_alg(HashAlg::Blockhash)
        .to_hasher();

    let start = std::time::Instant::now();
    let image = Reader::new(Cursor::new(image))
        .with_guessed_format()
        .expect("Cursor seeking can't fail")
        .decode()
        .map_err(Error::UnsupportedImageFormat)?;

    tracing::trace!(
        "It took {}ms to decode the image",
        start.elapsed().as_millis()
    );
    let start = std::time::Instant::now();
    let hash = hasher.hash_image(&image);
    tracing::trace!(
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
    tracing::trace!(
        "It took {}ms to compare a hash distance",
        start.elapsed().as_millis()
    );

    tracing::debug!("Distance was {}", dist);

    dist <= DIFFERENCE_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_logger() {
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::FmtSubscriber::builder()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .finish(),
        );
    }

    // TODO: collect appropriate licensed images to use for false positive
    // checking then un-`#[ignore]` this test
    #[test]
    #[ignore]
    fn false_positives() -> Result<(), Box<dyn std::error::Error>> {
        set_logger();

        for directory in std::fs::read_dir("false_positives")? {
            let directory = directory?;

            let (h1, h2) = {
                let entries = std::fs::read_dir(directory.path())?.try_fold(
                    Vec::new(),
                    |mut v, f| -> std::io::Result<_> {
                        v.push(std::fs::read(f?.path())?);
                        Ok(v)
                    },
                )?;

                (
                    process_image(entries[0].clone()).unwrap(),
                    process_image(entries[1].clone()).unwrap(),
                )
            };

            assert!(
                !similar_enough(&h1, h2.as_bytes()),
                "false positive found in directory {}",
                directory.path().display()
            );
        }

        Ok(())
    }

    // TODO: collect appropriate licensed images to use for true positive
    // checking then un-`#[ignore]` this test
    #[test]
    #[ignore]
    fn true_positives() -> Result<(), Box<dyn std::error::Error>> {
        set_logger();

        for directory in std::fs::read_dir("true_positives")? {
            let directory = directory?;

            let (h1, h2) = {
                let entries = std::fs::read_dir(directory.path())?.try_fold(
                    Vec::new(),
                    |mut v, f| -> std::io::Result<_> {
                        v.push(std::fs::read(f?.path())?);
                        Ok(v)
                    },
                )?;

                (
                    process_image(entries[0].clone()).unwrap(),
                    process_image(entries[1].clone()).unwrap(),
                )
            };

            assert!(
                similar_enough(&h1, h2.as_bytes()),
                "did not detect a duplicate in directory {}",
                directory.path().display()
            );
        }

        Ok(())
    }
}
