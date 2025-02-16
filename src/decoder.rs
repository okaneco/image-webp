use byteorder::{LittleEndian, ReadBytesExt};
use std::collections::HashMap;

use std::io::{self, BufReader, Cursor, Read, Seek};

use std::ops::Range;
use thiserror::Error;

use crate::extended::{self, get_alpha_predictor, read_alpha_chunk, WebPExtendedInfo};

use super::lossless::LosslessDecoder;
use super::vp8::Vp8Decoder;

/// Errors that can occur when attempting to decode a WebP image
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum DecodingError {
    /// An IO error occurred while reading the file
    #[error("IO Error: {0}")]
    IoError(#[from] io::Error),

    /// RIFF's "RIFF" signature not found or invalid
    #[error("Invalid RIFF signature: {0:x?}")]
    RiffSignatureInvalid([u8; 4]),

    /// WebP's "WEBP" signature not found or invalid
    #[error("Invalid WebP signature: {0:x?}")]
    WebpSignatureInvalid([u8; 4]),

    /// An expected chunk was missing
    #[error("An expected chunk was missing")]
    ChunkMissing,

    /// Chunk Header was incorrect or invalid in its usage
    #[error("Invalid Chunk header: {0:?}")]
    ChunkHeaderInvalid([u8; 4]),

    /// Some bits were invalid
    #[error("Invalid info bits: {name} {value}")]
    InfoBitsInvalid { name: &'static str, value: u32 },

    /// Alpha chunk doesn't match the frame's size
    #[error("Alpha chunk size mismatch")]
    AlphaChunkSizeMismatch,

    /// Image is too large, either for the platform's pointer size or generally
    #[error("Image too large")]
    ImageTooLarge,

    /// Frame would go out of the canvas
    #[error("Frame outside image")]
    FrameOutsideImage,

    /// Signature of 0x2f not found
    #[error("Invalid lossless signature: {0:x?}")]
    LosslessSignatureInvalid(u8),

    /// Version Number was not zero
    #[error("Invalid lossless version number: {0}")]
    VersionNumberInvalid(u8),

    #[error("Invalid color cache bits: {0}")]
    InvalidColorCacheBits(u8),

    /// An invalid Huffman code was encountered
    #[error("Invalid Huffman code")]
    HuffmanError,

    /// The bitstream was somehow corrupt
    #[error("Corrupt bitstream")]
    BitStreamError,

    /// The transforms specified were invalid
    #[error("Invalid transform")]
    TransformError,

    /// VP8's `[0x9D, 0x01, 0x2A]` magic not found or invalid
    #[error("Invalid VP8 magic: {0:x?}")]
    Vp8MagicInvalid([u8; 3]),

    /// VP8 Decoder initialisation wasn't provided with enough data
    #[error("Not enough VP8 init data")]
    NotEnoughInitData,

    /// At time of writing, only the YUV colour-space encoded as `0` is specified
    #[error("Invalid VP8 color space: {0}")]
    ColorSpaceInvalid(u8),

    /// LUMA prediction mode was not recognised
    #[error("Invalid VP8 luma prediction mode: {0}")]
    LumaPredictionModeInvalid(i8),

    /// Intra-prediction mode was not recognised
    #[error("Invalid VP8 intra prediction mode: {0}")]
    IntraPredictionModeInvalid(i8),

    /// Chroma prediction mode was not recognised
    #[error("Invalid VP8 chroma prediction mode: {0}")]
    ChromaPredictionModeInvalid(i8),

    /// Inconsistent image sizes
    #[error("Inconsistent image sizes")]
    InconsistentImageSizes,

    /// The file may be valid, but this crate doesn't support decoding it.
    #[error("Unsupported feature: {0}")]
    UnsupportedFeature(String),

    /// Invalid function call or parameter
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    /// Memory limit exceeded
    #[error("Memory limit exceeded")]
    MemoryLimitExceeded,

    /// Invalid chunk size
    #[error("Invalid chunk size")]
    InvalidChunkSize,
}

/// All possible RIFF chunks in a WebP image file
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
pub(crate) enum WebPRiffChunk {
    RIFF,
    WEBP,
    VP8,
    VP8L,
    VP8X,
    ANIM,
    ANMF,
    ALPH,
    ICCP,
    EXIF,
    XMP,
    Unknown([u8; 4]),
}

impl WebPRiffChunk {
    pub(crate) fn from_fourcc(chunk_fourcc: [u8; 4]) -> Self {
        match &chunk_fourcc {
            b"RIFF" => Self::RIFF,
            b"WEBP" => Self::WEBP,
            b"VP8 " => Self::VP8,
            b"VP8L" => Self::VP8L,
            b"VP8X" => Self::VP8X,
            b"ANIM" => Self::ANIM,
            b"ANMF" => Self::ANMF,
            b"ALPH" => Self::ALPH,
            b"ICCP" => Self::ICCP,
            b"EXIF" => Self::EXIF,
            b"XMP " => Self::XMP,
            _ => Self::Unknown(chunk_fourcc),
        }
    }

    pub(crate) fn to_fourcc(self) -> [u8; 4] {
        match self {
            Self::RIFF => *b"RIFF",
            Self::WEBP => *b"WEBP",
            Self::VP8 => *b"VP8 ",
            Self::VP8L => *b"VP8L",
            Self::VP8X => *b"VP8X",
            Self::ANIM => *b"ANIM",
            Self::ANMF => *b"ANMF",
            Self::ALPH => *b"ALPH",
            Self::ICCP => *b"ICCP",
            Self::EXIF => *b"EXIF",
            Self::XMP => *b"XMP ",
            Self::Unknown(fourcc) => fourcc,
        }
    }

    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

// enum WebPImage {
//     Lossy(VP8Frame),
//     Lossless(LosslessFrame),
//     Extended(ExtendedImage),
// }

enum ImageKind {
    Lossy,
    Lossless,
    Extended(WebPExtendedInfo),
}

struct AnimationState {
    next_frame: usize,
    loops_before_done: Option<u16>,
    next_frame_start: u64,
    dispose_next_frame: bool,
    canvas: Option<Vec<u8>>,
}
impl Default for AnimationState {
    fn default() -> Self {
        Self {
            next_frame: 0,
            loops_before_done: None,
            next_frame_start: 0,
            dispose_next_frame: true,
            canvas: None,
        }
    }
}

/// WebP image format decoder.
pub struct WebPDecoder<R> {
    r: R,
    memory_limit: usize,

    width: u32,
    height: u32,

    num_frames: usize,
    animation: AnimationState,

    kind: ImageKind,
    is_lossy: bool,

    chunks: HashMap<WebPRiffChunk, Range<u64>>,
}

impl<R: Read + Seek> WebPDecoder<R> {
    /// Create a new WebPDecoder from the reader `r`. The decoder performs many small reads, so the
    /// reader should be buffered.
    pub fn new(r: R) -> Result<WebPDecoder<R>, DecodingError> {
        let mut decoder = WebPDecoder {
            r,
            width: 0,
            height: 0,
            num_frames: 0,
            kind: ImageKind::Lossy,
            chunks: HashMap::new(),
            animation: Default::default(),
            memory_limit: usize::MAX,
            is_lossy: false,
        };
        decoder.read_data()?;
        Ok(decoder)
    }

    fn read_data(&mut self) -> Result<(), DecodingError> {
        let (WebPRiffChunk::RIFF, riff_size, _) = read_chunk_header(&mut self.r)? else {
            return Err(DecodingError::ChunkHeaderInvalid(*b"RIFF"));
        };

        match &read_fourcc(&mut self.r)? {
            WebPRiffChunk::WEBP => {}
            fourcc => return Err(DecodingError::WebpSignatureInvalid(fourcc.to_fourcc())),
        }

        let (chunk, chunk_size, chunk_size_rounded) = read_chunk_header(&mut self.r)?;
        let start = self.r.stream_position()?;

        match chunk {
            WebPRiffChunk::VP8 => {
                let tag = self.r.read_u24::<LittleEndian>()?;

                let keyframe = tag & 1 == 0;
                if !keyframe {
                    return Err(DecodingError::UnsupportedFeature(
                        "Non-keyframe frames".to_owned(),
                    ));
                }

                let mut tag = [0u8; 3];
                self.r.read_exact(&mut tag)?;
                if tag != [0x9d, 0x01, 0x2a] {
                    return Err(DecodingError::Vp8MagicInvalid(tag));
                }

                let w = self.r.read_u16::<LittleEndian>()?;
                let h = self.r.read_u16::<LittleEndian>()?;

                self.width = (w & 0x3FFF) as u32;
                self.height = (h & 0x3FFF) as u32;
                self.chunks
                    .insert(WebPRiffChunk::VP8, start..start + chunk_size as u64);
                self.kind = ImageKind::Lossy;
                self.is_lossy = true;
            }
            WebPRiffChunk::VP8L => {
                let signature = self.r.read_u8()?;
                if signature != 0x2f {
                    return Err(DecodingError::LosslessSignatureInvalid(signature));
                }

                let header = self.r.read_u32::<LittleEndian>()?;
                let version = header >> 29;
                if version != 0 {
                    return Err(DecodingError::VersionNumberInvalid(version as u8));
                }

                self.width = (1 + header) & 0x3FFF;
                self.height = (1 + (header >> 14)) & 0x3FFF;
                self.chunks
                    .insert(WebPRiffChunk::VP8L, start..start + chunk_size as u64);
                self.kind = ImageKind::Lossless;
            }
            WebPRiffChunk::VP8X => {
                let mut info = extended::read_extended_header(&mut self.r)?;
                self.width = info.canvas_width;
                self.height = info.canvas_height;

                let mut position = start + u64::from(chunk_size_rounded);
                let max_position = position + u64::from(riff_size.saturating_sub(12));
                self.r.seek(io::SeekFrom::Start(position))?;

                // Resist denial of service attacks by using a BufReader. In most images there
                // should be a very small number of chunks. However, nothing prevents a malicious
                // image from having an extremely large number of "unknown" chunks. Issuing
                // millions of reads and seeks against the underlying reader might be very
                // expensive.
                let mut reader = BufReader::with_capacity(64 << 10, &mut self.r);

                while position < max_position {
                    match read_chunk_header(&mut reader) {
                        Ok((chunk, chunk_size, chunk_size_rounded)) => {
                            if chunk.is_unknown() {
                                break;
                            }

                            let range = position + 8..position + 8 + u64::from(chunk_size);
                            position += 8 + u64::from(chunk_size_rounded);
                            self.chunks.entry(chunk).or_insert(range);

                            if let WebPRiffChunk::ANMF = chunk {
                                self.num_frames += 1;

                                // If the image is animated, the image data chunk will be inside the
                                // ANMF chunks, so we must inspect them to determine whether the
                                // image contains any lossy image data. VP8 chunks store lossy data
                                // and the spec says that lossless images SHOULD NOT contain ALPH
                                // chunks, so we treat both as indicators of lossy images.
                                if !self.is_lossy {
                                    let (subchunk, ..) = read_chunk_header(&mut reader)?;
                                    if let WebPRiffChunk::VP8 | WebPRiffChunk::ALPH = subchunk {
                                        self.is_lossy = true;
                                    }
                                    reader.seek_relative(i64::from(chunk_size_rounded) - 8)?;
                                    continue;
                                }
                            }

                            reader.seek_relative(i64::from(chunk_size_rounded))?;
                        }
                        Err(DecodingError::IoError(e))
                            if e.kind() == io::ErrorKind::UnexpectedEof =>
                        {
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
                self.is_lossy = self.is_lossy || self.chunks.contains_key(&WebPRiffChunk::VP8);

                if info.animation
                    && (!self.chunks.contains_key(&WebPRiffChunk::ANIM)
                        || !self.chunks.contains_key(&WebPRiffChunk::ANMF))
                    || info.icc_profile && !self.chunks.contains_key(&WebPRiffChunk::ICCP)
                    || info.exif_metadata && !self.chunks.contains_key(&WebPRiffChunk::EXIF)
                    || info.xmp_metadata && !self.chunks.contains_key(&WebPRiffChunk::XMP)
                    || !info.animation
                        && self.chunks.contains_key(&WebPRiffChunk::VP8)
                            == self.chunks.contains_key(&WebPRiffChunk::VP8L)
                {
                    return Err(DecodingError::ChunkMissing);
                }

                // Decode ANIM chunk.
                if info.animation {
                    match self.read_chunk(WebPRiffChunk::ANIM, 6) {
                        Ok(Some(chunk)) => {
                            let mut cursor = Cursor::new(chunk);
                            cursor.read_exact(&mut info.background_color)?;
                            match cursor.read_u16::<LittleEndian>()? {
                                0 => self.animation.loops_before_done = None,
                                n => self.animation.loops_before_done = Some(n),
                            }
                            self.animation.next_frame_start =
                                self.chunks.get(&WebPRiffChunk::ANMF).unwrap().start;
                        }
                        Ok(None) => return Err(DecodingError::ChunkMissing),
                        Err(DecodingError::MemoryLimitExceeded) => {
                            return Err(DecodingError::InvalidChunkSize)
                        }
                        Err(e) => return Err(e),
                    }
                }

                // If the image is animated, the image data chunk will be inside the ANMF chunks. We
                // store the ALPH, VP8, and VP8L chunks (as applicable) of the first frame in the
                // hashmap so that we can read them later.
                if let Some(range) = self.chunks.get(&WebPRiffChunk::ANMF).cloned() {
                    self.r.seek(io::SeekFrom::Start(range.start))?;
                    let mut position = range.start;

                    for _ in 0..2 {
                        let (subchunk, subchunk_size, subchunk_size_rounded) =
                            read_chunk_header(&mut self.r)?;
                        let subrange = position + 8..position + 8 + u64::from(subchunk_size);
                        self.chunks.entry(subchunk).or_insert(subrange.clone());

                        position += 8 + u64::from(subchunk_size_rounded);
                        if position + 8 > range.end {
                            break;
                        }
                    }
                }

                self.kind = ImageKind::Extended(info);
            }
            _ => return Err(DecodingError::ChunkHeaderInvalid(chunk.to_fourcc())),
        };

        Ok(())
    }

    /// Sets the maximum amount of memory that the decoder is allowed to allocate at once.
    ///
    /// TODO: Some allocations currently ignore this limit.
    pub fn set_memory_limit(&mut self, limit: usize) {
        self.memory_limit = limit;
    }

    /// Returns true if the image is animated.
    pub fn has_animation(&self) -> bool {
        match &self.kind {
            ImageKind::Lossy | ImageKind::Lossless => false,
            ImageKind::Extended(extended) => extended.animation,
        }
    }

    /// Returns whether the image has an alpha channel. If so, the pixel format is Rgba8 and
    /// otherwise Rgb8.
    pub fn has_alpha(&self) -> bool {
        match &self.kind {
            ImageKind::Lossy => false,
            ImageKind::Lossless => true,
            ImageKind::Extended(extended) => extended.alpha,
        }
    }

    /// Returns whether the image is lossy. For animated images, this is true if any frame is lossy.
    pub fn is_lossy(&mut self) -> bool {
        self.is_lossy
    }

    /// Sets the background color if the image is an extended and animated webp.
    pub fn set_background_color(&mut self, color: [u8; 4]) -> Result<(), DecodingError> {
        if let ImageKind::Extended(info) = &mut self.kind {
            info.background_color = color;
            Ok(())
        } else {
            Err(DecodingError::InvalidParameter(
                "Background color can only be set on animated webp".to_owned(),
            ))
        }
    }

    /// Returns the (width, height) of the image in pixels.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn read_chunk(
        &mut self,
        chunk: WebPRiffChunk,
        max_size: usize,
    ) -> Result<Option<Vec<u8>>, DecodingError> {
        match self.chunks.get(&chunk) {
            Some(range) => {
                if range.end - range.start > max_size as u64 {
                    return Err(DecodingError::MemoryLimitExceeded);
                }

                self.r.seek(io::SeekFrom::Start(range.start))?;
                let mut data = vec![0; (range.end - range.start) as usize];
                self.r.read_exact(&mut data)?;
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }

    /// Returns the raw bytes of the ICC profile, or None if there is no ICC profile.
    pub fn icc_profile(&mut self) -> Result<Option<Vec<u8>>, DecodingError> {
        self.read_chunk(WebPRiffChunk::ICCP, self.memory_limit)
    }

    /// Returns the raw bytes of the EXIF metadata, or None if there is no EXIF metadata.
    pub fn exif_metadata(&mut self) -> Result<Option<Vec<u8>>, DecodingError> {
        self.read_chunk(WebPRiffChunk::EXIF, self.memory_limit)
    }

    // Returns the raw bytes of the XMP metadata, or None if there is no XMP metadata.
    pub fn xmp_metadata(&mut self) -> Result<Option<Vec<u8>>, DecodingError> {
        self.read_chunk(WebPRiffChunk::XMP, self.memory_limit)
    }

    /// Returns the number of bytes required to store the image or a single frame.
    pub fn output_buffer_size(&self) -> usize {
        let bytes_per_pixel = if self.has_alpha() { 4 } else { 3 };
        self.width as usize * self.height as usize * bytes_per_pixel
    }

    /// Returns the raw bytes of the image. For animated images, this is the first frame.
    pub fn read_image(&mut self, buf: &mut [u8]) -> Result<(), DecodingError> {
        assert_eq!(buf.len(), self.output_buffer_size());

        if let Some(range) = self.chunks.get(&WebPRiffChunk::VP8L) {
            let mut frame = LosslessDecoder::new(range_reader(&mut self.r, range.clone())?);
            let frame = frame.decode_frame()?;
            if u32::from(frame.width) != self.width || u32::from(frame.height) != self.height {
                return Err(DecodingError::InconsistentImageSizes);
            }

            frame.fill_rgba(buf);
        } else {
            let range = self
                .chunks
                .get(&WebPRiffChunk::VP8)
                .ok_or(DecodingError::ChunkMissing)?;
            // TODO: avoid cloning frame
            let frame = Vp8Decoder::new(range_reader(&mut self.r, range.start..range.end)?)
                .decode_frame()?
                .clone();
            if u32::from(frame.width) != self.width || u32::from(frame.height) != self.height {
                return Err(DecodingError::InconsistentImageSizes);
            }

            if self.has_alpha() {
                frame.fill_rgba(buf);

                let range = self
                    .chunks
                    .get(&WebPRiffChunk::ALPH)
                    .ok_or(DecodingError::ChunkMissing)?
                    .clone();
                let alpha_chunk = read_alpha_chunk(
                    &mut range_reader(&mut self.r, range.start..range.end)?,
                    self.width,
                    self.height,
                )?;

                for y in 0..frame.height {
                    for x in 0..frame.width {
                        let predictor: u8 = get_alpha_predictor(
                            x.into(),
                            y.into(),
                            frame.width.into(),
                            alpha_chunk.filtering_method,
                            buf,
                        );

                        let alpha_index =
                            usize::from(y) * usize::from(frame.width) + usize::from(x);
                        let buffer_index = alpha_index * 4 + 3;

                        buf[buffer_index] = predictor.wrapping_add(alpha_chunk.data[alpha_index]);
                    }
                }
            } else {
                frame.fill_rgb(buf);
            }
        }

        Ok(())
    }

    /// Reads the next frame of the animation.
    ///
    /// The frame contents are written into `buf` and the method returns the delay of the frame in
    /// milliseconds. If there are no more frames, the method returns `None` and `buf` is left
    /// unchanged.
    ///
    /// Panics if the image is not animated.
    pub fn read_frame(&mut self, buf: &mut [u8]) -> Result<Option<u32>, DecodingError> {
        assert!(self.has_animation());

        if self.animation.loops_before_done == Some(0) {
            return Ok(None);
        }

        let ImageKind::Extended(info) = &self.kind else {
            unreachable!()
        };

        self.r
            .seek(io::SeekFrom::Start(self.animation.next_frame_start))?;

        let anmf_size = match read_chunk_header(&mut self.r)? {
            (WebPRiffChunk::ANMF, size, _) if size >= 32 => size,
            _ => return Err(DecodingError::ChunkHeaderInvalid(*b"ANMF")),
        };

        // Read ANMF chunk
        let frame_x = extended::read_3_bytes(&mut self.r)? * 2;
        let frame_y = extended::read_3_bytes(&mut self.r)? * 2;
        let frame_width = extended::read_3_bytes(&mut self.r)? + 1;
        let frame_height = extended::read_3_bytes(&mut self.r)? + 1;
        if frame_x + frame_width > self.width || frame_y + frame_height > self.height {
            return Err(DecodingError::FrameOutsideImage);
        }
        let duration = extended::read_3_bytes(&mut self.r)?;
        let frame_info = self.r.read_u8()?;
        let reserved = frame_info & 0b11111100;
        if reserved != 0 {
            return Err(DecodingError::InfoBitsInvalid {
                name: "reserved",
                value: reserved.into(),
            });
        }
        let use_alpha_blending = frame_info & 0b00000010 == 0;
        let dispose = frame_info & 0b00000001 != 0;

        let clear_color = if self.animation.dispose_next_frame {
            Some(info.background_color)
        } else {
            None
        };

        //read normal bitstream now
        let (chunk, chunk_size, chunk_size_rounded) = read_chunk_header(&mut self.r)?;
        if chunk_size_rounded + 32 < anmf_size {
            return Err(DecodingError::ChunkHeaderInvalid(chunk.to_fourcc()));
        }

        let (frame, frame_has_alpha): (Vec<u8>, bool) = match chunk {
            WebPRiffChunk::VP8 => {
                let reader = (&mut self.r).take(chunk_size as u64);
                let mut vp8_decoder = Vp8Decoder::new(reader);
                let raw_frame = vp8_decoder.decode_frame()?;
                if raw_frame.width as u32 != frame_width || raw_frame.height as u32 != frame_height
                {
                    return Err(DecodingError::InconsistentImageSizes);
                }
                let mut rgb_frame = vec![0; frame_width as usize * frame_height as usize * 3];
                raw_frame.fill_rgb(&mut rgb_frame);
                (rgb_frame, false)
            }
            WebPRiffChunk::VP8L => {
                let reader = (&mut self.r).take(chunk_size as u64);
                let mut lossless_decoder = LosslessDecoder::new(reader);
                let frame = lossless_decoder.decode_frame()?;
                if frame.width as u32 != frame_width || frame.height as u32 != frame_height {
                    return Err(DecodingError::InconsistentImageSizes);
                }
                let mut rgba_frame = vec![0; frame_width as usize * frame_height as usize * 4];
                frame.fill_rgba(&mut rgba_frame);
                (rgba_frame, true)
            }
            WebPRiffChunk::ALPH => {
                if chunk_size_rounded + 40 < anmf_size {
                    return Err(DecodingError::ChunkHeaderInvalid(chunk.to_fourcc()));
                }

                // read alpha
                let next_chunk_start = self.r.stream_position()? + chunk_size_rounded as u64;
                let mut reader = (&mut self.r).take(chunk_size as u64);
                let alpha_chunk = read_alpha_chunk(&mut reader, frame_width, frame_height)?;

                // read opaque
                self.r.seek(io::SeekFrom::Start(next_chunk_start))?;
                let (next_chunk, next_chunk_size, _) = read_chunk_header(&mut self.r)?;
                if chunk_size + next_chunk_size + 40 > anmf_size {
                    return Err(DecodingError::ChunkHeaderInvalid(next_chunk.to_fourcc()));
                }

                let mut vp8_decoder = Vp8Decoder::new((&mut self.r).take(chunk_size as u64));
                let frame = vp8_decoder.decode_frame()?;

                let mut rgba_frame = vec![0; frame_width as usize * frame_height as usize * 4];
                frame.fill_rgba(&mut rgba_frame);

                for y in 0..frame.height {
                    for x in 0..frame.width {
                        let predictor: u8 = get_alpha_predictor(
                            x.into(),
                            y.into(),
                            frame.width.into(),
                            alpha_chunk.filtering_method,
                            &rgba_frame,
                        );

                        let alpha_index =
                            usize::from(y) * usize::from(frame.width) + usize::from(x);
                        let buffer_index = alpha_index * 4 + 3;

                        rgba_frame[buffer_index] =
                            predictor.wrapping_add(alpha_chunk.data[alpha_index]);
                    }
                }

                (rgba_frame, true)
            }
            _ => return Err(DecodingError::ChunkHeaderInvalid(chunk.to_fourcc())),
        };

        if self.animation.canvas.is_none() {
            self.animation.canvas = Some(vec![0; (self.width * self.height * 4) as usize]);
        }
        extended::composite_frame(
            self.animation.canvas.as_mut().unwrap(),
            self.width,
            self.height,
            clear_color,
            &frame,
            frame_x,
            frame_y,
            frame_width,
            frame_height,
            frame_has_alpha,
            use_alpha_blending,
        );

        self.animation.dispose_next_frame = dispose;
        self.animation.next_frame_start += anmf_size as u64 + 8;
        self.animation.next_frame += 1;

        if self.animation.next_frame >= self.num_frames {
            self.animation.next_frame = 0;
            if self.animation.loops_before_done.is_some() {
                *self.animation.loops_before_done.as_mut().unwrap() -= 1;
            }
            self.animation.next_frame_start = self.chunks.get(&WebPRiffChunk::ANMF).unwrap().start;
            self.animation.dispose_next_frame = true;
        }

        buf.copy_from_slice(self.animation.canvas.as_ref().unwrap());

        Ok(Some(duration))
    }
}

pub(crate) fn range_reader<R: Read + Seek>(
    mut r: R,
    range: Range<u64>,
) -> Result<impl Read, DecodingError> {
    r.seek(io::SeekFrom::Start(range.start))?;
    Ok(r.take(range.end - range.start))
}

pub(crate) fn read_fourcc<R: Read>(mut r: R) -> Result<WebPRiffChunk, DecodingError> {
    let mut chunk_fourcc = [0; 4];
    r.read_exact(&mut chunk_fourcc)?;
    Ok(WebPRiffChunk::from_fourcc(chunk_fourcc))
}

pub(crate) fn read_chunk_header<R: Read>(
    mut r: R,
) -> Result<(WebPRiffChunk, u32, u32), DecodingError> {
    let chunk = read_fourcc(&mut r)?;
    let chunk_size = r.read_u32::<LittleEndian>()?;
    let chunk_size_rounded = chunk_size.saturating_add(chunk_size & 1);
    Ok((chunk, chunk_size, chunk_size_rounded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_with_overflow_size() {
        let bytes = vec![
            0x52, 0x49, 0x46, 0x46, 0xaf, 0x37, 0x80, 0x47, 0x57, 0x45, 0x42, 0x50, 0x6c, 0x64,
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xfb, 0x7e, 0x73, 0x00, 0x06, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
            0x40, 0xfb, 0xff, 0xff, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65, 0x65,
            0x00, 0x00, 0x00, 0x00, 0x62, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49,
            0x49, 0x54, 0x55, 0x50, 0x4c, 0x54, 0x59, 0x50, 0x45, 0x33, 0x37, 0x44, 0x4d, 0x46,
        ];

        let data = std::io::Cursor::new(bytes);

        let _ = WebPDecoder::new(data);
    }
}
