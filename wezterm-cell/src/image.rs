//! Images.
//! This module has some helpers for modeling terminal cells that are filled
//! with image data.
//! We're targeting the iTerm image protocol initially, with sixel as an obvious
//! follow up.
//! Kitty has an extensive and complex graphics protocol
//! whose docs are here:
//! <https://github.com/kovidgoyal/kitty/blob/master/docs/graphics-protocol.rst>
//! Both iTerm2 and Sixel appear to have semantics that allow replacing the
//! contents of a single chararcter cell with image data, whereas the kitty
//! protocol appears to track the images out of band as attachments with
//! z-order.

use ordered_float::NotNan;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
#[cfg(feature = "std")]
use wezterm_blob_leases::{BlobLease, BlobManager};

#[cfg(feature = "use_serde")]
fn deserialize_notnan<'de, D>(deserializer: D) -> Result<NotNan<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f32::deserialize(deserializer)?;
    NotNan::new(value).map_err(|e| serde::de::Error::custom(format!("{:?}", e)))
}

#[cfg(feature = "use_serde")]
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_notnan<S>(value: &NotNan<f32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    value.into_inner().serialize(serializer)
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureCoordinate {
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub x: NotNan<f32>,
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub y: NotNan<f32>,
}

impl TextureCoordinate {
    pub fn new(x: NotNan<f32>, y: NotNan<f32>) -> Self {
        Self { x, y }
    }

    pub fn new_f32(x: f32, y: f32) -> Self {
        let x = NotNan::new(x).unwrap();
        let y = NotNan::new(y).unwrap();
        Self::new(x, y)
    }
}

/// Tracks data for displaying an image in the place of the normal cell
/// character data.  Since an Image can span multiple cells, we need to logically
/// carve up the image and track each slice of it.  Each cell needs to know
/// its "texture coordinates" within that image so that we can render the
/// right slice.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCell {
    /// Texture coordinate for the top left of this cell.
    /// (0,0) is the top left of the ImageData. (1, 1) is
    /// the bottom right.
    top_left: TextureCoordinate,
    /// Texture coordinates for the bottom right of this cell.
    bottom_right: TextureCoordinate,
    /// References the underlying image data
    data: Arc<ImageData>,
    z_index: i32,
    /// When rendering in the cell, use this offset from the top left
    /// of the cell
    padding_left: u16,
    padding_top: u16,
    padding_right: u16,
    padding_bottom: u16,

    image_id: Option<u32>,
    placement_id: Option<u32>,
}

impl ImageCell {
    pub fn new(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
    ) -> Self {
        Self::with_z_index(top_left, bottom_right, data, 0, 0, 0, 0, 0, None, None)
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.top_left.hash(hasher);
        self.bottom_right.hash(hasher);
        self.data.hash.hash(hasher);
        self.z_index.hash(hasher);
        self.padding_left.hash(hasher);
        self.padding_top.hash(hasher);
        self.padding_right.hash(hasher);
        self.padding_bottom.hash(hasher);
        self.image_id.hash(hasher);
        self.placement_id.hash(hasher);
    }

    pub fn with_z_index(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
        z_index: i32,
        padding_left: u16,
        padding_top: u16,
        padding_right: u16,
        padding_bottom: u16,
        image_id: Option<u32>,
        placement_id: Option<u32>,
    ) -> Self {
        Self {
            top_left,
            bottom_right,
            data,
            z_index,
            padding_left,
            padding_top,
            padding_right,
            padding_bottom,
            image_id,
            placement_id,
        }
    }

    pub fn matches_placement(&self, image_id: u32, placement_id: Option<u32>) -> bool {
        self.image_id == Some(image_id) && self.placement_id == placement_id
    }

    pub fn has_placement_id(&self) -> bool {
        self.placement_id.is_some()
    }

    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    pub fn placement_id(&self) -> Option<u32> {
        self.placement_id
    }

    pub fn top_left(&self) -> TextureCoordinate {
        self.top_left
    }

    pub fn bottom_right(&self) -> TextureCoordinate {
        self.bottom_right
    }

    pub fn image_data(&self) -> &Arc<ImageData> {
        &self.data
    }

    /// negative z_index is rendered beneath the text layer.
    /// >= 0 is rendered above the text.
    /// negative z_index < INT32_MIN/2 will be drawn under cells
    /// with non-default background colors
    pub fn z_index(&self) -> i32 {
        self.z_index
    }

    /// Returns padding (left, top, right, bottom)
    pub fn padding(&self) -> (u16, u16, u16, u16) {
        (
            self.padding_left,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
        )
    }
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, PartialEq, Eq)]
pub enum ImageDataType {
    /// Data is in the native image file format
    /// (best for file formats that have animated content)
    EncodedFile(Vec<u8>),
    /// Data is in the native image file format,
    /// (best for file formats that have animated content)
    /// and is stored as a blob via the blob manager.
    #[cfg(feature = "std")]
    EncodedLease(
        #[cfg_attr(
            feature = "use_serde",
            serde(with = "wezterm_blob_leases::lease_bytes")
        )]
        BlobLease,
    ),
    /// Data is RGBA u8 data
    Rgba8 {
        data: Vec<u8>,
        width: u32,
        height: u32,
        hash: [u8; 32],
    },
    /// Data is an animated sequence
    AnimRgba8 {
        width: u32,
        height: u32,
        durations: Vec<Duration>,
        frames: Vec<Vec<u8>>,
        hashes: Vec<[u8; 32]>,
    },
}

impl std::fmt::Debug for ImageDataType {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::EncodedFile(data) => fmt
                .debug_struct("EncodedFile")
                .field("data_of_len", &data.len())
                .finish(),
            Self::EncodedLease(lease) => lease.fmt(fmt),
            Self::Rgba8 {
                data,
                width,
                height,
                hash,
            } => fmt
                .debug_struct("Rgba8")
                .field("data_of_len", &data.len())
                .field("width", &width)
                .field("height", &height)
                .field("hash", &hash)
                .finish(),
            Self::AnimRgba8 {
                frames,
                width,
                height,
                durations,
                hashes,
            } => fmt
                .debug_struct("AnimRgba8")
                .field("frames_of_len", &frames.len())
                .field("width", &width)
                .field("height", &height)
                .field("durations", durations)
                .field("hashes", hashes)
                .finish(),
        }
    }
}

/// Decoded pixel payloads larger than this skip content hashing: their
/// identity key is a process-unique nonce instead of a SHA-256. Content
/// dedup never hits for video-like kitty streams (every frame differs),
/// while hashing tens of MB per frame dominated the pty read thread; small
/// images keep real content hashes so repeated logos still dedup for free.
pub const CONTENT_HASH_MAX: usize = 1024 * 1024;

/// First 8 bytes of every nonce key, so debug checks and the dedup cache
/// can tell identity-only keys apart from content hashes. A real SHA-256
/// starting with these exact bytes is a ~2^-64 coincidence.
const NONCE_KEY_MARKER: [u8; 8] = [0x54, 0x54, 0x4e, 0x43, 0x9d, 0x1b, 0x7a, 0xe4];

impl ImageDataType {
    pub fn new_single_frame(width: u32, height: u32, data: Vec<u8>) -> Self {
        let hash = Self::content_key(&data);
        Self::new_single_frame_with_hash(width, height, data, hash)
    }

    /// Like `new_single_frame` but always content-hashed, for callers that
    /// rely on "equal pixels => equal key" across reconstructions (e.g. the
    /// background layer reload reusing textures by hash) and only construct
    /// occasionally.
    pub fn new_single_frame_content_hashed(width: u32, height: u32, data: Vec<u8>) -> Self {
        let hash = Self::hash_bytes(&data);
        Self::new_single_frame_with_hash(width, height, data, hash)
    }

    fn new_single_frame_with_hash(width: u32, height: u32, data: Vec<u8>, hash: [u8; 32]) -> Self {
        assert_eq!(
            width * height * 4,
            data.len() as u32,
            "invalid dimensions {}x{} for pixel data of length {}",
            width,
            height,
            data.len()
        );
        Self::Rgba8 {
            width,
            height,
            data,
            hash,
        }
    }

    /// Identity key for an image payload: a real content hash for small
    /// payloads, a process-unique nonce for large ones. The key is opaque
    /// everywhere it travels — mux peers, caches and the GPU only store and
    /// compare it — so the two kinds coexist freely.
    pub fn content_key(bytes: &[u8]) -> [u8; 32] {
        if bytes.len() > CONTENT_HASH_MAX {
            Self::nonce_key()
        } else {
            Self::hash_bytes(bytes)
        }
    }

    /// A key that is unique rather than content-derived:
    /// marker || per-process random salt || counter. The salt keeps keys
    /// minted by different processes (this GUI, every mux server) from
    /// colliding in the client- and GPU-side caches that mix all sources.
    fn nonce_key() -> [u8; 32] {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::OnceLock;
        static SALT: OnceLock<[u8; 16]> = OnceLock::new();
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let salt = SALT.get_or_init(|| {
            // wasm32-unknown-unknown has no process id and SystemTime::now()
            // traps there. A wasm client is single-instance, so it needs no
            // cross-process entropy of its own, and colliding with a peer's
            // randomly-salted nonces would require matching all 16 salt
            // bytes AND the counter.
            #[cfg(target_arch = "wasm32")]
            {
                [
                    0x9c, 0x3f, 0x51, 0xe8, 0x27, 0xb4, 0x6a, 0xd1, 0x08, 0xf5, 0x72, 0xc9, 0x3e,
                    0x84, 0x1b, 0x67,
                ]
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let mut seed = Vec::new();
                seed.extend_from_slice(&std::process::id().to_ne_bytes());
                if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    seed.extend_from_slice(&t.as_nanos().to_ne_bytes());
                }
                Self::hash_bytes(&seed)[..16]
                    .try_into()
                    .expect("16-byte slice")
            }
        });
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&NONCE_KEY_MARKER);
        key[8..24].copy_from_slice(salt);
        key[24..].copy_from_slice(&n.to_be_bytes());
        key
    }

    pub fn is_nonce_key(key: &[u8; 32]) -> bool {
        key[..8] == NONCE_KEY_MARKER
    }

    /// Black pixels
    pub fn placeholder() -> Self {
        let mut data = vec![];
        let size = 8;
        for _ in 0..size * size {
            data.extend_from_slice(&[0, 0, 0, 0xff]);
        }
        ImageDataType::new_single_frame(size, size, data)
    }

    pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    pub fn compute_hash(&self) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        match self {
            ImageDataType::EncodedFile(data) => hasher.update(data),
            ImageDataType::EncodedLease(lease) => return lease.content_id().as_hash_bytes(),
            ImageDataType::Rgba8 { data, hash, .. } => {
                // The stored key is authoritative: the constructors compute
                // it and every in-place pixel edit refreshes it (see
                // terminalstate/kitty.rs), so full-frame payloads are not
                // re-hashed here — at video-like kitty frame rates that
                // second SHA-256 dominated the pty read thread. For
                // content-hashed (small) payloads the debug assert turns any
                // future forgotten refresh into a loud test failure instead
                // of a stale-image glitch; nonce keys are identity-only and
                // have nothing to re-derive.
                if !Self::is_nonce_key(hash) {
                    debug_assert_eq!(
                        *hash,
                        Self::hash_bytes(data),
                        "Rgba8 hash field is stale; a pixel mutation forgot to refresh it"
                    );
                }
                return *hash;
            }
            ImageDataType::AnimRgba8 {
                hashes, durations, ..
            } => {
                // Fold the per-frame keys instead of re-hashing every
                // frame's pixels: the keys already identify the frame
                // content (or are unique nonces), and hashing 32 bytes per
                // frame is microseconds where the old full re-hash was the
                // remaining full-payload SHA-256 on the pty thread.
                for hash in hashes {
                    hasher.update(hash);
                }
                for d in durations {
                    let d = d.as_secs_f32();
                    let b = d.to_ne_bytes();
                    hasher.update(b);
                }
            }
        };
        hasher.finalize().into()
    }

    /// Divides the animation frame durations by the provided
    /// speed_factor, so a factor of 2 will halve the duration.
    /// # Panics
    /// if the speed_factor is negative, non-finite or the result
    /// overflows the allow Duration range.
    pub fn adjust_speed(&mut self, speed_factor: f32) {
        match self {
            Self::AnimRgba8 { durations, .. } => {
                for d in durations {
                    *d = d.mul_f32(1. / speed_factor);
                }
            }
            _ => {}
        }
    }

    #[cfg(feature = "use_image")]
    pub fn dimensions(&self) -> Result<(u32, u32), ImageCellError> {
        fn dimensions_for_data(data: &[u8]) -> image::ImageResult<(u32, u32)> {
            let reader =
                image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
            let (width, height) = reader.into_dimensions()?;

            Ok((width, height))
        }

        match self {
            ImageDataType::EncodedFile(data) => Ok(dimensions_for_data(data)?),
            ImageDataType::EncodedLease(lease) => Ok(dimensions_for_data(&lease.get_data()?)?),
            ImageDataType::AnimRgba8 { width, height, .. }
            | ImageDataType::Rgba8 { width, height, .. } => Ok((*width, *height)),
        }
    }

    /// Migrate an in-memory encoded image blob to on-disk to reduce
    /// the memory footprint
    pub fn swap_out(self) -> Result<Self, ImageCellError> {
        match self {
            Self::EncodedFile(data) => match BlobManager::store(&data) {
                Ok(lease) => Ok(Self::EncodedLease(lease)),
                Err(wezterm_blob_leases::Error::StorageNotInit) => Ok(Self::EncodedFile(data)),
                Err(err) => Err(err.into()),
            },
            other => Ok(other),
        }
    }

    /// Decode an encoded file into either an Rgba8 or AnimRgba8 variant
    /// if we recognize the file format, otherwise the EncodedFile data
    /// is preserved as is.
    #[cfg(feature = "use_image")]
    pub fn decode(self) -> Self {
        use image::{AnimationDecoder, ImageFormat};

        match self {
            Self::EncodedFile(data) => {
                let format = match image::guess_format(&data) {
                    Ok(format) => format,
                    Err(err) => {
                        log::warn!("Unable to decode raw image data: {:#}", err);
                        return Self::EncodedFile(data);
                    }
                };
                let cursor = std::io::Cursor::new(&*data);
                match format {
                    ImageFormat::Gif => image::codecs::gif::GifDecoder::new(cursor)
                        .and_then(|decoder| decoder.into_frames().collect_frames())
                        .and_then(|frames| {
                            if frames.is_empty() {
                                log::error!("decoded image has 0 frames, using placeholder");
                                Ok(Self::placeholder())
                            } else {
                                Ok(Self::decode_frames(frames))
                            }
                        })
                        .unwrap_or_else(|err| {
                            log::error!(
                                "Unable to parse animated gif: {:#}, trying as single frame",
                                err
                            );
                            Self::decode_single(data)
                        }),
                    ImageFormat::Png => {
                        let decoder = match image::codecs::png::PngDecoder::new(cursor) {
                            Ok(d) => d,
                            _ => return Self::EncodedFile(data),
                        };
                        if decoder.is_apng().unwrap_or(false) {
                            match decoder
                                .apng()
                                .and_then(|d| d.into_frames().collect_frames())
                            {
                                Ok(frames) if frames.is_empty() => {
                                    log::error!("decoded image has 0 frames, using placeholder");
                                    Self::placeholder()
                                }
                                Ok(frames) => Self::decode_frames(frames),
                                _ => Self::EncodedFile(data),
                            }
                        } else {
                            Self::decode_single(data)
                        }
                    }
                    ImageFormat::WebP => {
                        let decoder = match image::codecs::webp::WebPDecoder::new(cursor) {
                            Ok(d) => d,
                            _ => return Self::EncodedFile(data),
                        };
                        match decoder.into_frames().collect_frames() {
                            Ok(frames) if frames.is_empty() => {
                                log::error!("decoded image has 0 frames, using placeholder");
                                Self::placeholder()
                            }
                            Ok(frames) => Self::decode_frames(frames),
                            _ => Self::EncodedFile(data),
                        }
                    }
                    _ => Self::decode_single(data),
                }
            }
            data => data,
        }
    }

    #[cfg(not(feature = "use_image"))]
    pub fn decode(self) -> Self {
        self
    }

    #[cfg(feature = "use_image")]
    fn decode_frames(img_frames: Vec<image::Frame>) -> Self {
        let mut width = 0;
        let mut height = 0;
        let mut frames = vec![];
        let mut durations = vec![];
        let mut hashes = vec![];
        for frame in img_frames.into_iter() {
            let duration: Duration = frame.delay().into();
            durations.push(duration);
            let image = image::DynamicImage::ImageRgba8(frame.into_buffer()).to_rgba8();
            let (w, h) = image.dimensions();
            width = w;
            height = h;
            let data = image.into_vec();
            hashes.push(Self::content_key(&data));
            frames.push(data);
        }
        Self::AnimRgba8 {
            width,
            height,
            frames,
            durations,
            hashes,
        }
    }

    #[cfg(feature = "use_image")]
    fn decode_single(data: Vec<u8>) -> Self {
        match image::load_from_memory(&data) {
            Ok(image) => {
                let image = image.to_rgba8();
                let (width, height) = image.dimensions();
                let data = image.into_vec();
                let hash = Self::content_key(&data);
                Self::Rgba8 {
                    width,
                    height,
                    data,
                    hash,
                }
            }
            _ => Self::EncodedFile(data),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ImageCellError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    BlobLease(#[from] wezterm_blob_leases::Error),

    #[error(transparent)]
    ImageError(#[from] image::ImageError),
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct ImageData {
    data: Mutex<ImageDataType>,
    hash: [u8; 32],
}

struct HexSlice<'a>(&'a [u8]);
impl<'a> std::fmt::Display for HexSlice<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        for byte in self.0 {
            write!(fmt, "{byte:x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("ImageData")
            .field("data", &self.data)
            .field("hash", &format_args!("{}", HexSlice(&self.hash)))
            .finish()
    }
}

impl Eq for ImageData {}
impl PartialEq for ImageData {
    fn eq(&self, rhs: &Self) -> bool {
        self.hash == rhs.hash
    }
}

impl ImageData {
    /// Create a new ImageData struct with the provided raw data.
    pub fn with_raw_data(data: Vec<u8>) -> Self {
        let hash = ImageDataType::hash_bytes(&data);
        Self::with_data_and_hash(ImageDataType::EncodedFile(data).decode(), hash)
    }

    /// `hash` must equal `data.compute_hash()`; the caller supplies it to
    /// avoid re-hashing image payloads that were already hashed for cache
    /// lookup (a full-frame SHA-256 per kitty transmission otherwise runs
    /// twice on the pty read thread while it holds the terminal lock).
    pub fn with_data_and_hash(data: ImageDataType, hash: [u8; 32]) -> Self {
        Self {
            data: Mutex::new(data),
            hash,
        }
    }

    pub fn with_data(data: ImageDataType) -> Self {
        let hash = data.compute_hash();
        Self {
            data: Mutex::new(data),
            hash,
        }
    }

    /// Returns the in-memory footprint
    pub fn len(&self) -> usize {
        match &*self.data() {
            ImageDataType::EncodedFile(d) => d.len(),
            ImageDataType::EncodedLease(_) => 0,
            ImageDataType::Rgba8 { data, .. } => data.len(),
            ImageDataType::AnimRgba8 { frames, .. } => frames.len() * frames[0].len(),
        }
    }

    pub fn data(&self) -> MutexGuard<'_, ImageDataType> {
        self.data.lock().unwrap()
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payloads_get_stable_content_hashes() {
        let data = vec![7u8; 64];
        let a = ImageDataType::content_key(&data);
        let b = ImageDataType::content_key(&data);
        assert_eq!(a, b);
        assert!(!ImageDataType::is_nonce_key(&a));
        assert_eq!(a, ImageDataType::hash_bytes(&data));
    }

    #[test]
    fn large_payloads_get_unique_nonce_keys() {
        let data = vec![7u8; CONTENT_HASH_MAX + 1];
        let a = ImageDataType::content_key(&data);
        let b = ImageDataType::content_key(&data);
        assert_ne!(a, b, "identical large payloads must get distinct keys");
        assert!(ImageDataType::is_nonce_key(&a));
        assert!(ImageDataType::is_nonce_key(&b));
    }

    #[test]
    fn anim_identity_folds_frame_keys_without_reading_pixels() {
        let mk = |frames: Vec<Vec<u8>>, hashes: Vec<[u8; 32]>| ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10); frames.len()],
            frames,
            hashes,
        };
        let hashes = vec![[1u8; 32], [2u8; 32]];
        let a = mk(vec![vec![0, 0, 0, 0xff]; 2], hashes.clone());
        // Different pixels but identical per-frame keys: the identity must
        // not change, which proves frames are no longer re-hashed.
        let b = mk(vec![vec![0xff, 0, 0, 0xff]; 2], hashes.clone());
        assert_eq!(a.compute_hash(), b.compute_hash());
        // Different per-frame keys must change the identity.
        let c = mk(
            vec![vec![0, 0, 0, 0xff]; 2],
            vec![[1u8; 32], [3u8; 32]],
        );
        assert_ne!(a.compute_hash(), c.compute_hash());
    }
}
