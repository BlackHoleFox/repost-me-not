use core::convert::TryInto;
use core::pin::Pin;

use crate::errors::{DatabaseError, Error};

#[cfg(test)]
use bytecheck::CheckBytes;
#[cfg(test)]
use rkyv::validation::validators::DefaultValidator;

use rkyv::{
    de::deserializers::SharedDeserializeMap,
    ser::{serializers::WriteSerializer, Serializer},
    Archive, Deserialize, Serialize,
};

const CURRENT_VERSION: u8 = 1;

const PTR_SIZE: usize = core::mem::size_of::<usize>();

mod migrations {
    use super::{Data, DatabaseError};

    fn inital_version(data: &Data) -> Result<(), DatabaseError> {
        data.db
            .open_tree(Data::STORAGE_TREE)
            .map_err(DatabaseError::Initalizing)?;
        data.db
            .open_tree(Data::SEEN_COUNT_TREE)
            .map_err(DatabaseError::Initalizing)?;
        data.db
            .open_tree(Data::HASH_TREE)
            .map_err(DatabaseError::Initalizing)?;

        Ok(())
    }

    type Migration = fn(&Data) -> Result<(), DatabaseError>;
    pub(super) const MIGRATORS: &[Migration] = &[inital_version];
}
use migrations::MIGRATORS;
use sled::IVec;

use crate::image_processing::{self, ImageHash};

#[derive(Clone)]
pub struct Data {
    db: sled::Db,
    stored_images: sled::Tree,
    seen_counts: sled::Tree,
    seen_hashes: sled::Tree,
}

impl Data {
    // --------- Keys -------------------
    const VERSION_KEY: &'static [u8] = b"version";
    const PTR_SIZE_KEY: &'static [u8] = b"usize";

    // --------- Trees ------------------

    /// Mapping of database ID --> image metadata
    const STORAGE_TREE: &'static [u8] = b"image_properties";
    /// Mapping of database ID --> times seen
    const SEEN_COUNT_TREE: &'static [u8] = b"seen_count";
    /// Mapping of image hash --> database ID
    const HASH_TREE: &'static [u8] = b"hash_tree";

    pub fn init(db_path: &str) -> Result<Self, DatabaseError> {
        #[cfg(not(test))]
        let db = sled::Config::new()
            .path(db_path)
            .open()
            .map_err(DatabaseError::Initalizing)?;

        #[cfg(test)]
        let db = {
            let mut config = sled::Config::new().temporary(true);

            if !db_path.is_empty() {
                config = config.path(db_path)
            }

            config.open().unwrap()
        };

        // Check that there aren't any 32/64 bit mixups
        match db
            .get(Self::PTR_SIZE_KEY)
            .map_err(DatabaseError::Initalizing)?
        {
            Some(ptr_size) => {
                if ptr_size.len() != PTR_SIZE {
                    panic!("attempted to open a database from an arch with a different usize")
                }
            }
            None => {
                db.insert(Self::PTR_SIZE_KEY, &PTR_SIZE.to_ne_bytes())
                    .map_err(DatabaseError::Initalizing)?;
            }
        };

        let version = match db
            .get(Self::VERSION_KEY)
            .map_err(DatabaseError::Initalizing)?
        {
            Some(v) => v.as_ref()[0],
            None => {
                db.insert(Self::VERSION_KEY, &[CURRENT_VERSION])
                    .map_err(DatabaseError::Initalizing)?;
                CURRENT_VERSION
            }
        };

        if version < CURRENT_VERSION {
            panic!("uhhh, time travel?")
        }

        let data = Self {
            stored_images: db
                .open_tree(Self::STORAGE_TREE)
                .map_err(DatabaseError::Initalizing)?,
            seen_counts: db
                .open_tree(Self::SEEN_COUNT_TREE)
                .map_err(DatabaseError::Initalizing)?,
            seen_hashes: db
                .open_tree(Self::HASH_TREE)
                .map_err(DatabaseError::Initalizing)?,
            db,
        };

        // V1 --> `inital_version()` --> Skips nothing.
        // V2 --> `migration_v2()`   --> Skips `inital_version()`
        // V3 --> `migration_v3()`   --> Skips `inital_version()` and `migration_v2()`.
        for migration in MIGRATORS.iter().skip(usize::from(version - 1)) {
            migration(&data)?;
        }

        Ok(data)
    }

    fn read_int(bytes: &[u8]) -> u64 {
        u64::from_ne_bytes(bytes.try_into().expect("bug: wrong number of bytes"))
    }

    // Ensure that the buffers used are correct
    #[cfg(test)]
    fn read_archived<'a, T: Archive>(buf: &'a [u8]) -> &'a T::Archived
    where
        T::Archived: CheckBytes<DefaultValidator<'a>>,
    {
        rkyv::validation::validators::check_archived_root::<T>(buf)
            .expect("bad bug: image record had invalid buffer")
    }

    #[cfg(not(test))]
    fn read_archived<T: Archive>(buf: &[u8]) -> &T::Archived {
        // SAFETY: The data was stored in a buffer on its own and was on the same endian.
        unsafe { rkyv::archived_root::<T>(buf) }
    }

    pub fn record_image(
        &self,
        image_hash: &ImageHash,
        properties: SeenImage,
    ) -> Result<PreviouslySeen, Error> {
        // See if we know about this exact image already.
        if let Some(id_of_existing) = self
            .seen_hashes
            .get(image_hash.as_bytes())
            .map_err(DatabaseError::Recording)?
        {
            // If we do, increment and return the times its been seen
            let times_seen =
                self.seen_counts
                    .update_and_fetch(&id_of_existing, |old| {
                        let mut new = Self::read_int(old.expect(
                            "bug: database ID pointed at a hash but no seen_count was found",
                        ));
                        new += 1;

                        Some(IVec::from(&new.to_ne_bytes()))
                    })
                    .map_err(DatabaseError::Recording)?
                    .expect("bug: record_image update_and_fetch returned None");

            // Then return it to the caller.
            let times_seen = Self::read_int(&times_seen);
            let old = self
                .stored_images
                .get(&id_of_existing)
                .map_err(DatabaseError::Recording)?
                .expect("bug: database ID pointed at dead image");

            let start = std::time::Instant::now();
            let mut deserializer = SharedDeserializeMap::new();
            let image = Self::read_archived::<SeenImage>(&old);
            let image = image
                .deserialize(&mut deserializer)
                .expect("deserialization can never fail");
            tracing::trace!("It took {}ms to deserialize", start.elapsed().as_millis());

            return Ok(PreviouslySeen::Yes { image, times_seen });
        }

        // Otherwise, its new-ish. Lets see if its similar to anything else we have!
        for entry in self.seen_hashes.iter() {
            let (hash, id) = entry.map_err(DatabaseError::Recording)?;

            // Skip what was just inserted above.
            if hash == image_hash.as_bytes() {
                continue;
            }

            // If it was similar, record it as a duplicate and tell the caller.
            if image_processing::similar_enough(image_hash, &hash) {
                // Update the count...
                let times_seen = self
                    .seen_counts
                    .update_and_fetch(&id, |old_count| {
                        let mut new =
                            Self::read_int(old_count.expect("bug: image existed but hash didn't"));
                        new += 1;

                        Some(IVec::from(&new.to_ne_bytes()))
                    })
                    .map_err(DatabaseError::Recording)?
                    .expect("bug: record_image update_and_fetch 2 returned None");

                let times_seen = Self::read_int(&times_seen);

                let old = self
                    .stored_images
                    .get(&id)
                    .map_err(DatabaseError::Recording)?
                    .expect("bug: database ID pointed at dead image");

                // Now mark this hash as the same image.
                self.seen_hashes
                    .insert(image_hash.as_bytes(), id)
                    .map_err(DatabaseError::Recording)?;

                let start = std::time::Instant::now();
                let mut deserializer = SharedDeserializeMap::new();
                let image = Self::read_archived::<SeenImage>(&old);
                let image = image
                    .deserialize(&mut deserializer)
                    .expect("deserialization can never fail"); // reuturns rkyv::Unreachable
                tracing::trace!("It took {}ms to deserialize", start.elapsed().as_millis());

                return Ok(PreviouslySeen::Yes { image, times_seen });
            }
        }

        let mut serializer = WriteSerializer::new(Vec::new());
        serializer
            .serialize_value(&properties)
            .expect("bug: serialization failed");

        let value = serializer.into_inner();

        // Finally it must be something brand new
        let id = self
            .db
            .generate_id()
            .map_err(DatabaseError::Recording)?
            .to_ne_bytes();

        const NEW_IMAGE_COUNT: &[u8] = &1u64.to_ne_bytes();

        self.seen_counts
            .insert(id, NEW_IMAGE_COUNT)
            .map_err(DatabaseError::Recording)?;
        self.stored_images
            .insert(id, value)
            .map_err(DatabaseError::Recording)?;
        self.seen_hashes
            .insert(image_hash.as_bytes(), &id)
            .map_err(DatabaseError::Recording)?;

        Ok(PreviouslySeen::No)
    }

    pub fn access_image<F: Fn(Pin<&mut ArchivedSeenImage>) -> bool>(
        &self,
        image_hash: &[u8],
        f: F,
    ) -> Result<(), DatabaseError> {
        if let Some(id) = self
            .seen_hashes
            .get(image_hash)
            .map_err(DatabaseError::Accessing)?
        {
            let mut buf = self
                .stored_images
                .get(&id)
                .map_err(DatabaseError::Accessing)?
                .expect("bug: image_access knew about a hash but nothing was stored");

            let needs_modified = {
                let buffer = Pin::new(buf.as_mut());

                // SAFETY: We know we're pulling out of the images table, which are the right type, and this is tested.
                let archived = unsafe { rkyv::archived_root_mut::<SeenImage>(buffer) };

                f(archived)
            };

            if needs_modified {
                self.stored_images
                    .insert(id, buf)
                    .map_err(DatabaseError::Accessing)?;
            }
        }

        Ok(())
    }

    pub fn total_seen(&self) -> usize {
        self.stored_images.len()
    }
}

#[derive(Debug, Archive, Deserialize, Serialize)]
#[cfg_attr(
    test,
    derive(Clone, PartialEq),
    archive_attr(derive(CheckBytes, Debug))
)]
pub struct SeenImage {
    /// Is this image ignored from repost checking.
    pub ignored: bool,
    /// User who sent the message containing an image.
    pub author: String,
    /// Timestamp of message when the message was received - std::time::UNIX_EPOCH, in seconds.
    ///
    /// Good enough for constructing durations
    pub sent: u64,
    /// ID of the message that contained this message.
    ///
    /// This is the message that will be replied to when a repost occurs.
    pub original_message_id: u64,
    /// ID of the channel that an image was seen in.
    ///
    /// Helps determine if a reply can be used or if a jumplink is needed.
    pub channel_id: u64,
}

impl SeenImage {
    pub fn new(author: String, sent: u64, original_message_id: u64, channel_id: u64) -> Self {
        Self {
            ignored: false,
            author,
            sent,
            original_message_id,
            channel_id,
        }
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum PreviouslySeen {
    Yes { image: SeenImage, times_seen: u64 },
    No,
}

#[cfg(test)]
impl PartialEq<SeenImage> for ArchivedSeenImage {
    fn eq(&self, other: &SeenImage) -> bool {
        self.ignored == other.ignored
            && self.author == other.author
            && self.sent == other.sent
            && self.original_message_id == other.original_message_id
            && self.channel_id == other.channel_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sled::IVec;

    #[test]
    fn mismatched_usize_fails_to_init() {
        let test_path = "./target/usize_test";

        // Fake a DB made on a 32-bit system.
        let db = sled::Config::new().path(test_path).open().unwrap();
        let db = Data {
            stored_images: db.open_tree(Data::STORAGE_TREE).unwrap(),
            seen_counts: db.open_tree(Data::SEEN_COUNT_TREE).unwrap(),
            seen_hashes: db.open_tree(Data::HASH_TREE).unwrap(),
            db,
        };

        let smaller_usize = core::mem::size_of::<u32>() as u32;
        db.db
            .insert(Data::PTR_SIZE_KEY, &smaller_usize.to_ne_bytes())
            .unwrap();

        drop(db);

        let failed = std::thread::spawn(move || {
            let _db = Data::init(test_path);
        })
        .join();

        assert!(failed.is_err())
    }

    #[test]
    fn databse_version_moves() {
        let db = Data::init("").unwrap();

        assert_eq!(
            db.db.get(Data::VERSION_KEY).unwrap(),
            Some(IVec::from(&[1]))
        );
    }

    #[test]
    fn store_and_fetch() {
        let db = Data::init("").unwrap();

        let original = SeenImage::new("testing".to_string(), 773, 242343331, 238484343);

        let hash = ImageHash::from_bytes(&[1, 1, 1, 1, 1, 1, 1, 1]).unwrap();
        db.record_image(&hash, original.clone()).unwrap();

        db.access_image(&[1, 2, 3], |fetched| {
            assert_eq!(*fetched, original);
            false
        })
        .unwrap();
    }

    #[test]
    fn store_duplicates() {
        let db = Data::init("").unwrap();
        let id = ImageHash::from_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        let original = SeenImage::new(
            "testing_but_looooooooooooonnnng".to_string(),
            26543654564,
            59849292,
            3424324234,
        );

        let existing = db.record_image(&id, original.clone()).unwrap();
        assert_eq!(existing, PreviouslySeen::No);

        let (db_id, _) = db.stored_images.first().unwrap().unwrap();

        let seen_count = db.seen_counts.get(&db_id).unwrap().unwrap();
        assert_eq!(Data::read_int(&seen_count), 1);

        let newer = SeenImage::new("someone else".to_string(), 555555555, 4384834303, 434343423);

        let old = db.record_image(&id, newer).unwrap();

        let (old, times_seen) = match old {
            PreviouslySeen::Yes { image, times_seen } => (image, times_seen),
            _ => panic!("wrong seen variant"),
        };

        assert_eq!(old, original);
        assert_eq!(times_seen, 2);

        let seen_count = db.seen_counts.get(db_id).unwrap().unwrap();
        assert_eq!(Data::read_int(&seen_count), 2);
    }

    #[test]
    fn store_similar() {
        let db = Data::init("").unwrap();
        let id = ImageHash::from_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

        let original = SeenImage::new(
            "testing_but_looooooooooooonnnng".to_string(),
            26543654564,
            59849292,
            43434234342,
        );

        db.record_image(&id, original.clone()).unwrap();

        let newer = SeenImage::new("someone else".to_string(), 555555555, 4384834303, 323243434);
        let newer_id = ImageHash::from_bytes(&[1, 2, 3, 4, 5, 6, 7, 7]).unwrap();

        let old = db.record_image(&newer_id, newer).unwrap();

        let old = match old {
            PreviouslySeen::Yes { image, .. } => image,
            _ => panic!("wrong seen variant"),
        };

        assert_eq!(old, original)
    }
}
