// SPDX-License-Identifier: MPL-2.0 OR LGPL-2.1-or-later
//
// glycin-dng: a glycin loader for Adobe DNG files.
//
// DNG files carry one or more camera-rendered JPEG previews next to the raw
// mosaic data, and iPhone ProRAW files embed one at full resolution. This
// loader serves the largest embedded preview instead of developing the raw
// data. The result matches what the camera shows, loads fast, and does not
// depend on raw-decoder support for the newest DNG variants (ProRAW Max
// compresses its raw data with JPEG XL, but the preview is still plain JPEG).

use std::collections::HashSet;
use std::io::{Cursor, Read};

use glycin_utils::*;

init_main_loader!(ImgDecoder);

pub struct ImgDecoder {
    rgb: image::RgbImage,
}

#[derive(Debug)]
struct Candidate {
    offset: usize,
    len: usize,
    /// NewSubfileType == 1 marks the main reduced-resolution/preview image.
    primary: bool,
    /// PhotometricInterpretation, if present (1 = grayscale, e.g. masks).
    photometric: Option<u64>,
}

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

impl Tiff<'_> {
    fn u16(&self, o: usize) -> Option<u16> {
        let b = self.d.get(o..o + 2)?;
        Some(if self.le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn u32(&self, o: usize) -> Option<u32> {
        let b = self.d.get(o..o + 4)?;
        Some(if self.le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn type_size(typ: u16) -> Option<usize> {
        match typ {
            1 | 2 | 6 | 7 => Some(1),
            3 | 8 => Some(2),
            4 | 9 | 11 | 13 => Some(4),
            5 | 10 | 12 => Some(8),
            _ => None,
        }
    }

    /// Where the entry's data lives: inline in the value field if it fits
    /// into 4 bytes, otherwise at the offset stored there.
    fn data_pos(&self, entry: usize, typ: u16, cnt: u32) -> Option<usize> {
        let size = Self::type_size(typ)?.checked_mul(cnt as usize)?;
        if size <= 4 {
            Some(entry + 8)
        } else {
            Some(self.u32(entry + 8)? as usize)
        }
    }

    /// First element of a SHORT/LONG/IFD entry as u64.
    fn scalar(&self, entry: usize, typ: u16, cnt: u32) -> Option<u64> {
        if cnt == 0 {
            return None;
        }
        let pos = self.data_pos(entry, typ, cnt)?;
        match typ {
            3 => self.u16(pos).map(u64::from),
            4 | 13 => self.u32(pos).map(u64::from),
            _ => None,
        }
    }

    /// All elements of a SHORT/LONG/IFD entry as u64s.
    fn array(&self, entry: usize, typ: u16, cnt: u32) -> Option<Vec<u64>> {
        let size = Self::type_size(typ)?;
        if !matches!(typ, 3 | 4 | 13) {
            return None;
        }
        let base = self.data_pos(entry, typ, cnt)?;
        let mut out = Vec::with_capacity(cnt as usize);
        for i in 0..cnt as usize {
            let pos = base.checked_add(i * size)?;
            out.push(match typ {
                3 => u64::from(self.u16(pos)?),
                _ => u64::from(self.u32(pos)?),
            });
        }
        Some(out)
    }
}

/// Walk all IFDs (the IFD0 chain plus SubIFDs) and collect every embedded
/// JPEG blob, plus the Orientation tag of the first IFD that carries one.
fn scan_dng(d: &[u8]) -> Result<(Vec<Candidate>, Option<u16>), String> {
    if d.len() < 8 {
        return Err("file too small for a TIFF header".into());
    }
    let le = match &d[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err("not a TIFF/DNG file".into()),
    };
    let t = Tiff { d, le };
    if t.u16(2) != Some(42) {
        return Err("bad TIFF magic".into());
    }

    let mut queue = vec![t.u32(4).ok_or("truncated header")? as usize];
    let mut seen: HashSet<usize> = HashSet::new();
    let mut candidates = Vec::new();
    let mut orientation: Option<u16> = None;
    let mut hops = 0;

    while let Some(ifd) = queue.pop() {
        hops += 1;
        if hops > 64 || ifd == 0 || !seen.insert(ifd) {
            continue;
        }
        let Some(n) = t.u16(ifd) else { continue };
        let n = n as usize;

        let mut nst = None;
        let mut photometric = None;
        let mut comp = None;
        let mut strip_offs: Option<Vec<u64>> = None;
        let mut strip_cnts: Option<Vec<u64>> = None;
        let mut jif = None;
        let mut jif_len = None;

        for i in 0..n {
            let e = ifd + 2 + i * 12;
            let (Some(tag), Some(typ), Some(cnt)) = (t.u16(e), t.u16(e + 2), t.u32(e + 4)) else {
                continue;
            };
            match tag {
                0x00FE => nst = t.scalar(e, typ, cnt),
                0x0106 => photometric = t.scalar(e, typ, cnt),
                0x0103 => comp = t.scalar(e, typ, cnt),
                0x0111 => strip_offs = t.array(e, typ, cnt),
                0x0117 => strip_cnts = t.array(e, typ, cnt),
                0x0201 => jif = t.scalar(e, typ, cnt),
                0x0202 => jif_len = t.scalar(e, typ, cnt),
                0x0112 => {
                    if orientation.is_none() {
                        orientation = t.scalar(e, typ, cnt).and_then(|v| u16::try_from(v).ok());
                    }
                }
                0x014A => {
                    if let Some(subs) = t.array(e, typ, cnt) {
                        queue.extend(subs.into_iter().map(|o| o as usize));
                    }
                }
                _ => {}
            }
        }

        if let Some(next) = t.u32(ifd + 2 + n * 12) {
            queue.push(next as usize);
        }

        // A JPEG blob is either an old-style JPEGInterchangeFormat pair or a
        // single JPEG-compressed strip covering the whole image.
        let blob = match (jif, jif_len) {
            (Some(o), Some(l)) => Some((o as usize, l as usize)),
            _ => match (comp, &strip_offs, &strip_cnts) {
                (Some(6 | 7), Some(offs), Some(cnts)) if offs.len() == 1 && cnts.len() == 1 => {
                    Some((offs[0] as usize, cnts[0] as usize))
                }
                _ => None,
            },
        };

        if let Some((offset, len)) = blob {
            let valid = len >= 4
                && offset.checked_add(len).is_some_and(|end| end <= d.len())
                && d[offset] == 0xFF
                && d[offset + 1] == 0xD8;
            if valid {
                candidates.push(Candidate {
                    offset,
                    len,
                    primary: nst == Some(1),
                    photometric,
                });
            }
        }
    }

    Ok((candidates, orientation))
}

/// Pick the preview to serve: the primary preview IFD first, then color over
/// grayscale (DNG semantic masks are grayscale), then the biggest blob.
fn best_jpeg(d: &[u8], mut cands: Vec<Candidate>) -> Option<&[u8]> {
    cands.sort_by_key(|c| {
        (
            std::cmp::Reverse(c.primary as u8),
            std::cmp::Reverse((c.photometric != Some(1)) as u8),
            std::cmp::Reverse(c.len),
        )
    });
    let c = cands.first()?;
    d.get(c.offset..c.offset + c.len)
}

impl LoaderImplementation for ImgDecoder {
    fn init(
        mut stream: UnixStream,
        _mime_type: String,
        _details: InitializationDetails,
    ) -> Result<(ImgDecoder, ImageDetails), ProcessError> {
        let mut buf = vec![];
        stream.read_to_end(&mut buf).internal_error()?;

        let (cands, orientation) = scan_dng(&buf)
            .map_err(std::io::Error::other)
            .expected_error()?;
        let jpeg = best_jpeg(&buf, cands)
            .ok_or_else(|| std::io::Error::other("no embedded JPEG preview found in DNG"))
            .expected_error()?;

        let rgb = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
            .map_err(|e| std::io::Error::other(format!("embedded preview decode failed: {e}")))
            .expected_error()?
            .into_rgb8();

        let mut image_info = ImageDetails::new(rgb.width(), rgb.height());
        image_info.info_format_name = Some(String::from("DNG"));
        image_info.transformation_orientation =
            orientation.and_then(|x| gufo_common::orientation::Orientation::try_from(x).ok());
        image_info.transformation_ignore_exif = false;

        Ok((ImgDecoder { rgb }, image_info))
    }

    fn frame(&mut self, _frame_request: FrameRequest) -> Result<Frame, ProcessError> {
        let data = self.rgb.as_raw();
        let mut memory = SharedMemory::new(data.len() as u64).expected_error()?;
        Cursor::new(data).read_exact(&mut memory).internal_error()?;
        let texture = memory.into_binary_data();

        Frame::new(
            self.rgb.width(),
            self.rgb.height(),
            MemoryFormat::R8g8b8,
            texture,
        )
        .internal_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];
    const JPEG_BIG: &[u8] = &[0xFF, 0xD8, 0, 0, 0, 0, 0xFF, 0xD9];

    /// Little-endian TIFF writer for tests. Entry values are passed as the
    /// raw 4-byte inline field, which covers every tag the scanner reads.
    struct Builder {
        d: Vec<u8>,
    }

    impl Builder {
        fn new() -> Self {
            let mut d = Vec::new();
            d.extend(b"II");
            d.extend(42u16.to_le_bytes());
            d.extend(0u32.to_le_bytes());
            Self { d }
        }

        fn set_ifd0(&mut self, off: u32) {
            self.d[4..8].copy_from_slice(&off.to_le_bytes());
        }

        fn blob(&mut self, bytes: &[u8]) -> u32 {
            let p = self.d.len() as u32;
            self.d.extend(bytes);
            p
        }

        fn ifd(&mut self, entries: &[(u16, u16, u32, u32)], next: u32) -> u32 {
            let p = self.d.len() as u32;
            self.d.extend((entries.len() as u16).to_le_bytes());
            for &(tag, typ, cnt, val) in entries {
                self.d.extend(tag.to_le_bytes());
                self.d.extend(typ.to_le_bytes());
                self.d.extend(cnt.to_le_bytes());
                self.d.extend(val.to_le_bytes());
            }
            self.d.extend(next.to_le_bytes());
            p
        }
    }

    #[test]
    fn finds_jif_preview_and_orientation() {
        let mut b = Builder::new();
        let blob = b.blob(JPEG);
        let ifd = b.ifd(
            &[
                (0x00FE, 4, 1, 1),
                (0x0112, 3, 1, 6),
                (0x0201, 4, 1, blob),
                (0x0202, 4, 1, JPEG.len() as u32),
            ],
            0,
        );
        b.set_ifd0(ifd);

        let (cands, orientation) = scan_dng(&b.d).unwrap();
        assert_eq!(orientation, Some(6));
        assert_eq!(cands.len(), 1);
        assert!(cands[0].primary);
        assert_eq!(best_jpeg(&b.d, cands).unwrap(), JPEG);
    }

    #[test]
    fn finds_single_strip_preview() {
        let mut b = Builder::new();
        let blob = b.blob(JPEG);
        let ifd = b.ifd(
            &[
                (0x0103, 3, 1, 7),
                (0x0111, 4, 1, blob),
                (0x0117, 4, 1, JPEG.len() as u32),
            ],
            0,
        );
        b.set_ifd0(ifd);

        let (cands, _) = scan_dng(&b.d).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(best_jpeg(&b.d, cands).unwrap(), JPEG);
    }

    #[test]
    fn prefers_primary_preview_over_bigger_grayscale_mask() {
        let mut b = Builder::new();
        let small = b.blob(JPEG);
        let big = b.blob(JPEG_BIG);
        // A DNG semantic mask: bigger, grayscale, NewSubfileType 0x10004.
        let sub = b.ifd(
            &[
                (0x00FE, 4, 1, 0x10004),
                (0x0106, 3, 1, 1),
                (0x0201, 4, 1, big),
                (0x0202, 4, 1, JPEG_BIG.len() as u32),
            ],
            0,
        );
        let ifd0 = b.ifd(
            &[
                (0x00FE, 4, 1, 1),
                (0x0201, 4, 1, small),
                (0x0202, 4, 1, JPEG.len() as u32),
                (0x014A, 4, 1, sub),
            ],
            0,
        );
        b.set_ifd0(ifd0);

        let (cands, _) = scan_dng(&b.d).unwrap();
        assert_eq!(cands.len(), 2);
        assert_eq!(best_jpeg(&b.d, cands).unwrap(), JPEG);
    }

    #[test]
    fn rejects_non_tiff_and_truncated_input() {
        assert!(scan_dng(b"not a tiff").is_err());
        assert!(scan_dng(b"II").is_err());

        // Valid header, but no preview anywhere.
        let mut b = Builder::new();
        let ifd = b.ifd(&[(0x0100, 4, 1, 640)], 0);
        b.set_ifd0(ifd);
        let (cands, _) = scan_dng(&b.d).unwrap();
        assert!(best_jpeg(&b.d, cands).is_none());
    }

    #[test]
    fn survives_ifd_offset_cycles() {
        let mut b = Builder::new();
        // IFD pointing at itself through the next-IFD field.
        let ifd = b.ifd(&[(0x0100, 4, 1, 640)], 8);
        b.set_ifd0(ifd);
        assert!(scan_dng(&b.d).is_ok());
    }
}
