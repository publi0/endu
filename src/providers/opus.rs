//! Ogg Opus uploads for batch providers. The system encoder compresses a
//! finished 16 kHz clip to about a tenth of its WAV size; at 24 kbit/s a
//! Portuguese FLEURS comparison with Whisper (not the providers) showed no
//! measurable accuracy change.

use std::borrow::Cow;

use crate::openrouter::transcribe::SAMPLE_RATE;

const BITRATE: u32 = 24_000;
/// 20 ms packets at 16 kHz, as the native encoder is configured.
const FRAMES_PER_PACKET: u64 = 320;
/// Ogg granule positions always count 48 kHz samples for Opus.
const GRANULE_PER_SAMPLE: u64 = 48_000 / SAMPLE_RATE as u64;
/// About one second of audio per page keeps pages small.
const PACKETS_PER_PAGE: usize = 50;
/// A page holds at most 255 lacing values.
const SEGMENTS_PER_PAGE: usize = 255;
const SERIAL: u32 = u32::from_be_bytes(*b"endu");

const BEGINNING_OF_STREAM: u8 = 0x02;
const END_OF_STREAM: u8 = 0x04;

/// Encodes a finished clip, or `None` when the platform encoder is unavailable
/// or fails; callers then upload the WAV unchanged.
pub(crate) fn encode(samples: &[f32]) -> Option<Vec<u8>> {
    if samples.is_empty() {
        return None;
    }
    let samples = sanitize(samples);
    let (packets, priming) = native::encode(&samples, BITRATE)?;
    mux(&packets, priming, samples.len() as u64)
}

/// The WAV path's rule: non-finite samples become silence and the rest are
/// clamped to full scale, so one bad sample cannot spoil the encoder's input.
fn sanitize(samples: &[f32]) -> Cow<'_, [f32]> {
    if samples
        .iter()
        .all(|sample| sample.is_finite() && sample.abs() <= 1.0)
    {
        return Cow::Borrowed(samples);
    }
    Cow::Owned(
        samples
            .iter()
            .map(|sample| {
                if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect(),
    )
}

/// Segments one packet occupies in the lacing table.
fn segments(packet: &[u8]) -> usize {
    packet.len() / 255 + 1
}

/// Wraps raw Opus packets in an Ogg stream (RFC 7845). `priming` is the
/// encoder delay in 16 kHz frames; the final granule trims the padding.
/// Returns `None` for input no valid Opus encoder produces (no packets, a
/// packet too large for one page, or a delay beyond the 16-bit pre-skip), so
/// the caller sends the WAV rather than a stream that decodes out of step.
fn mux(packets: &[Vec<u8>], priming: u32, sample_count: u64) -> Option<Vec<u8>> {
    let pre_skip = u16::try_from(u64::from(priming) * GRANULE_PER_SAMPLE).ok()?;
    if packets.is_empty()
        || packets
            .iter()
            .any(|packet| segments(packet) > SEGMENTS_PER_PAGE)
    {
        return None;
    }
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(1);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    let vendor = b"endu";
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());

    let mut out = Vec::with_capacity(packets.iter().map(Vec::len).sum::<usize>() + 1024);
    page(&mut out, BEGINNING_OF_STREAM, 0, 0, &[&head]);
    page(&mut out, 0, 0, 1, &[&tags]);
    // Decoded samples include the pre-skip, so only the final page adds it;
    // earlier pages count whole decoded packets.
    let last_granule = u64::from(pre_skip) + sample_count * GRANULE_PER_SAMPLE;
    let mut sequence = 2;
    let mut written = 0usize;
    while written < packets.len() {
        let mut end = written;
        let mut used = 0;
        while end < packets.len()
            && end - written < PACKETS_PER_PAGE
            && used + segments(&packets[end]) <= SEGMENTS_PER_PAGE
        {
            used += segments(&packets[end]);
            end += 1;
        }
        let last = end == packets.len();
        let granule = if last {
            last_granule
        } else {
            (end as u64 * FRAMES_PER_PACKET * GRANULE_PER_SAMPLE).min(last_granule)
        };
        let group: Vec<&[u8]> = packets[written..end].iter().map(Vec::as_slice).collect();
        page(
            &mut out,
            if last { END_OF_STREAM } else { 0 },
            granule,
            sequence,
            &group,
        );
        sequence += 1;
        written = end;
    }
    Some(out)
}

/// Writes one page; `packets` must fit in [`SEGMENTS_PER_PAGE`] lacing values.
fn page(out: &mut Vec<u8>, flags: u8, granule: u64, sequence: u32, packets: &[&[u8]]) {
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&SERIAL.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    let lacing: Vec<u8> = packets
        .iter()
        .flat_map(|packet| {
            std::iter::repeat_n(255, packet.len() / 255)
                .chain(std::iter::once((packet.len() % 255) as u8))
        })
        .collect();
    assert!(
        lacing.len() <= SEGMENTS_PER_PAGE,
        "Ogg page lacing overflow"
    );
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    for packet in packets {
        out.extend_from_slice(packet);
    }
    let checksum = crc(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&checksum.to_le_bytes());
}

/// The Ogg CRC-32: polynomial 0x04C11DB7, unreflected, zero initial value.
fn crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04C1_1DB7
            };
        }
    }
    crc
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn endu_opus_encode(
            samples: *const f32,
            count: usize,
            bitrate: u32,
            data: *mut *mut u8,
            data_len: *mut usize,
            sizes: *mut *mut u32,
            packet_count: *mut usize,
            priming: *mut u32,
        ) -> i32;
        fn endu_opus_free(pointer: *mut c_void);
    }

    pub(super) fn encode(samples: &[f32], bitrate: u32) -> Option<(Vec<Vec<u8>>, u32)> {
        let mut data = std::ptr::null_mut();
        let mut data_len = 0;
        let mut sizes = std::ptr::null_mut();
        let mut packet_count = 0;
        let mut priming = 0;
        // SAFETY: the encoder reads `samples` only during the call and returns
        // malloc'd buffers of the reported lengths, released below.
        let status = unsafe {
            endu_opus_encode(
                samples.as_ptr(),
                samples.len(),
                bitrate,
                &mut data,
                &mut data_len,
                &mut sizes,
                &mut packet_count,
                &mut priming,
            )
        };
        if status != 0 || data.is_null() || sizes.is_null() {
            return None;
        }
        // SAFETY: on success both buffers are valid for the reported lengths
        // until they are freed below, after the last use of these slices.
        let (bytes, lengths) = unsafe {
            (
                std::slice::from_raw_parts(data, data_len),
                std::slice::from_raw_parts(sizes, packet_count),
            )
        };
        let packets = split(bytes, lengths);
        // SAFETY: both pointers came from the encoder and are freed once.
        unsafe {
            endu_opus_free(data.cast());
            endu_opus_free(sizes.cast());
        }
        packets.map(|packets| (packets, priming))
    }

    /// Splits concatenated packets by their lengths, which must cover `bytes`.
    fn split(bytes: &[u8], lengths: &[u32]) -> Option<Vec<Vec<u8>>> {
        let mut packets = Vec::with_capacity(lengths.len());
        let mut offset = 0usize;
        for &length in lengths {
            let end = offset.checked_add(length as usize)?;
            packets.push(bytes.get(offset..end)?.to_vec());
            offset = end;
        }
        (offset == bytes.len()).then_some(packets)
    }

    #[cfg(test)]
    unsafe extern "C" {
        fn endu_opus_decode_for_tests(
            data: *const u8,
            sizes: *const u32,
            count: usize,
            out: *mut f32,
            capacity: usize,
            written: *mut usize,
        ) -> i32;
    }

    /// Decodes packets with the system Opus decoder, without trimming.
    #[cfg(test)]
    pub(super) fn decode(packets: &[&[u8]]) -> Vec<f32> {
        let data: Vec<u8> = packets.concat();
        let sizes: Vec<u32> = packets.iter().map(|packet| packet.len() as u32).collect();
        let mut out = vec![0.0f32; (packets.len() + 4) * super::FRAMES_PER_PACKET as usize];
        let mut written = 0;
        // SAFETY: every buffer is valid for the lengths passed.
        let status = unsafe {
            endu_opus_decode_for_tests(
                data.as_ptr(),
                sizes.as_ptr(),
                sizes.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        assert_eq!(status, 0, "the system Opus decoder must accept the packets");
        out.truncate(written);
        out
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub(super) fn encode(_samples: &[f32], _bitrate: u32) -> Option<(Vec<Vec<u8>>, u32)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Page<'a> {
        flags: u8,
        granule: u64,
        sequence: u32,
        packets: Vec<&'a [u8]>,
    }

    fn pages(mut bytes: &[u8]) -> Vec<Page<'_>> {
        let mut pages = Vec::new();
        while !bytes.is_empty() {
            assert_eq!(&bytes[..4], b"OggS");
            let segments = bytes[26] as usize;
            let lacing = &bytes[27..27 + segments];
            let length = 27 + segments + lacing.iter().map(|&value| value as usize).sum::<usize>();
            let mut copy = bytes[..length].to_vec();
            copy[22..26].fill(0);
            assert_eq!(
                crc(&copy).to_le_bytes(),
                bytes[22..26],
                "page checksum must match"
            );
            let mut packets = Vec::new();
            let mut offset = 27 + segments;
            let mut size = 0;
            for &value in lacing {
                size += value as usize;
                if value < 255 {
                    packets.push(&bytes[offset..offset + size]);
                    offset += size;
                    size = 0;
                }
            }
            pages.push(Page {
                flags: bytes[5],
                granule: u64::from_le_bytes(bytes[6..14].try_into().unwrap()),
                sequence: u32::from_le_bytes(bytes[18..22].try_into().unwrap()),
                packets,
            });
            bytes = &bytes[length..];
        }
        pages
    }

    #[test]
    fn crc_matches_the_ogg_reference_polynomial() {
        assert_eq!(crc(b""), 0);
        assert_eq!(crc(b"123456789"), 0x89A1_897F);
        // An OpusHead page written by libogg (through ffmpeg).
        let mut page = *b"OggS\0\x02\0\0\0\0\0\0\0\0\x7a\xd8\xa0\x1c\0\0\0\0\xd7\xcf\x73\x79\x01\x13OpusHead\x01\x01\x38\x01\x80\x3e\0\0\0\0\0";
        let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].fill(0);
        assert_eq!(crc(&page), stored);
    }

    #[test]
    fn mux_writes_headers_granules_and_end_of_stream() {
        let packets: Vec<Vec<u8>> = (0..120u8).map(|index| vec![index; 60]).collect();
        let samples = 119 * FRAMES_PER_PACKET + 100;
        let ogg = mux(&packets, 104, samples).unwrap();
        let pages = pages(&ogg);

        assert_eq!(pages.len(), 5);
        assert_eq!(pages[0].flags, BEGINNING_OF_STREAM);
        assert_eq!(&pages[0].packets[0][..8], b"OpusHead");
        assert_eq!(pages[0].packets[0][9], 1, "mono");
        assert_eq!(
            u16::from_le_bytes([pages[0].packets[0][10], pages[0].packets[0][11]]),
            312,
            "pre-skip counts 48 kHz samples"
        );
        assert_eq!(&pages[1].packets[0][..8], b"OpusTags");
        assert_eq!(
            pages.iter().map(|page| page.sequence).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            pages[2].granule,
            50 * 320 * 3,
            "decoded samples already include pre-skip"
        );
        assert_eq!(pages[3].granule, 100 * 320 * 3);
        assert_eq!(pages[4].flags, END_OF_STREAM);
        assert_eq!(pages[4].granule, 312 + samples * 3, "end trims padding");
        let audio: Vec<&[u8]> = pages[2..]
            .iter()
            .flat_map(|page| page.packets.clone())
            .collect();
        assert_eq!(audio.len(), 120);
        assert!(
            audio
                .iter()
                .zip(&packets)
                .all(|(left, right)| *left == right)
        );
    }

    #[test]
    fn lacing_keeps_packets_that_fill_whole_segments_intact() {
        let packets = vec![vec![1; 255], vec![2; 510], vec![3; 3]];
        let ogg = mux(&packets, 0, 3 * FRAMES_PER_PACKET).unwrap();
        let pages = pages(&ogg);
        assert_eq!(pages[2].packets.len(), 3);
        assert_eq!(pages[2].packets[0].len(), 255);
        assert_eq!(pages[2].packets[1].len(), 510);
        assert_eq!(pages[2].packets[2], [3, 3, 3]);
    }

    #[test]
    fn sanitize_matches_the_wav_rule() {
        let clean = [0.0, -1.0, 1.0, 0.25];
        assert!(matches!(sanitize(&clean), Cow::Borrowed(_)));
        let dirty = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.5, -2.0, 0.25];
        assert_eq!(*sanitize(&dirty), [0.0, 0.0, 0.0, 1.0, -1.0, 0.25]);
    }

    #[test]
    fn empty_clip_is_not_encoded() {
        assert_eq!(encode(&[]), None);
    }

    #[test]
    fn large_packets_split_pages_by_lacing_values() {
        // 2000-byte packets need 8 lacing values each: 31 fit in a page.
        let packets: Vec<Vec<u8>> = (0..50u8).map(|index| vec![index; 2000]).collect();
        let samples = 50 * FRAMES_PER_PACKET - 7;
        let ogg = mux(&packets, 104, samples).unwrap();
        let pages = pages(&ogg);

        assert_eq!(pages.len(), 4);
        assert_eq!(pages[2].packets.len(), 31);
        assert_eq!(pages[2].flags, 0);
        assert_eq!(pages[2].granule, 31 * 320 * 3);
        assert_eq!(pages[3].packets.len(), 19);
        assert_eq!(pages[3].flags, END_OF_STREAM);
        assert_eq!(pages[3].granule, 312 + samples * 3);
        let audio: Vec<&[u8]> = pages[2..]
            .iter()
            .flat_map(|page| page.packets.clone())
            .collect();
        assert!(
            audio
                .iter()
                .zip(&packets)
                .all(|(left, right)| *left == right)
        );
    }

    #[test]
    fn mux_rejects_streams_it_cannot_describe() {
        assert_eq!(mux(&[], 104, 320), None, "no packets");
        assert_eq!(
            mux(&[vec![0; 255 * 255]], 0, 320),
            None,
            "packet over a page"
        );
        assert!(mux(&[vec![0; 255 * 255 - 1]], 0, 320).is_some());
        assert_eq!(
            mux(&[vec![0; 3]], 65_535 / 3 + 1, 320),
            None,
            "pre-skip beyond 16 bits would desync the final granule"
        );
        let ogg = mux(&[vec![0; 3]], 65_535 / 3, 320).unwrap();
        assert_eq!(pages(&ogg)[2].granule, 65_535 / 3 * 3 + 320 * 3);
    }

    #[cfg(target_os = "macos")]
    fn tone(count: usize) -> Vec<f32> {
        (0..count)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                0.3 * (2.0 * std::f32::consts::PI * 220.0 * time).sin()
                    * (2.0 * std::f32::consts::PI * 3.0 * time).sin().abs()
            })
            .collect()
    }

    /// Checks the RFC 7845 page invariants and returns pre-skip and packets.
    #[cfg(target_os = "macos")]
    fn audio_packets(ogg: &[u8], sample_count: usize) -> (u64, Vec<&[u8]>) {
        let pages = pages(ogg);
        assert_eq!(pages[0].flags, BEGINNING_OF_STREAM);
        assert_eq!(&pages[1].packets[0][..8], b"OpusTags");
        let pre_skip = u64::from(u16::from_le_bytes([
            pages[0].packets[0][10],
            pages[0].packets[0][11],
        ]));
        let mut packets = Vec::new();
        let mut previous = 0;
        for (index, page) in pages.iter().enumerate().skip(2) {
            packets.extend(page.packets.iter().copied());
            let last = index == pages.len() - 1;
            assert_eq!(page.flags, if last { END_OF_STREAM } else { 0 });
            assert!(page.granule >= previous, "granules never decrease");
            if last {
                assert_eq!(page.granule, pre_skip + sample_count as u64 * 3);
            } else {
                assert_eq!(page.granule, packets.len() as u64 * 320 * 3);
            }
            previous = page.granule;
        }
        assert!(
            packets.len() as u64 * 320 * 3 >= pre_skip + sample_count as u64 * 3,
            "the packets cover pre-skip and every input sample"
        );
        (pre_skip, packets)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn system_encoder_produces_a_compact_ogg_opus_stream() {
        let samples = tone(SAMPLE_RATE as usize * 3);
        let ogg = encode(&samples).expect("AudioToolbox provides an Opus encoder");
        let (pre_skip, packets) = audio_packets(&ogg, samples.len());
        assert!(
            packets.len() >= 150,
            "three seconds need at least 150 packets"
        );
        assert!(pre_skip > 0, "the encoder reports its delay");
        let wav_bytes = samples.len() * 2;
        assert!(
            ogg.len() < wav_bytes / 6,
            "24 kbit/s Opus must be far smaller than PCM16 ({} vs {wav_bytes})",
            ogg.len()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn short_and_silent_clips_still_form_valid_streams() {
        for samples in [
            vec![0.2],
            tone(SAMPLE_RATE as usize * 3 / 10),
            vec![0.0; SAMPLE_RATE as usize],
        ] {
            let ogg = encode(&samples).expect("AudioToolbox provides an Opus encoder");
            let (_, packets) = audio_packets(&ogg, samples.len());
            assert!(!packets.is_empty());
            assert!(!native::decode(&packets).is_empty());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn non_finite_samples_do_not_silence_the_clip() {
        let mut samples = tone(SAMPLE_RATE as usize * 2);
        for (index, sample) in samples.iter_mut().enumerate().step_by(997) {
            *sample = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY][index % 3];
        }
        let ogg = encode(&samples).unwrap();
        let silence = encode(&vec![0.0; samples.len()]).unwrap();
        assert!(
            ogg.len() > silence.len() * 3,
            "the tone survives ({} vs silence {})",
            ogg.len(),
            silence.len()
        );
        let (pre_skip, packets) = audio_packets(&ogg, samples.len());
        let decoded = native::decode(&packets);
        let skip = (pre_skip / 3) as usize;
        let energy: f32 = decoded[skip..skip + samples.len()]
            .iter()
            .map(|sample| sample * sample)
            .sum();
        assert!(
            energy / samples.len() as f32 > 0.005,
            "decoded audio is not silent"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn system_decoder_round_trips_the_stream_after_pre_skip() {
        // A chirp, not a tone: a periodic signal correlates almost as well one
        // period off, so it cannot pin the alignment to a single lag.
        let count = SAMPLE_RATE as usize * 2 + 123;
        let duration = count as f64 / f64::from(SAMPLE_RATE);
        let (start, end) = (200.0, 4_000.0);
        let samples: Vec<f32> = (0..count)
            .map(|index| {
                let time = index as f64 / f64::from(SAMPLE_RATE);
                let phase = start * time + (end - start) * time * time / (2.0 * duration);
                (0.5 * (std::f64::consts::TAU * phase).sin()) as f32
            })
            .collect();
        let ogg = encode(&samples).unwrap();
        let (pre_skip, packets) = audio_packets(&ogg, samples.len());
        let decoded = native::decode(&packets);
        assert!(decoded.len() >= packets.len() * 320 - 320);
        // Trimmed exactly as the granules say: drop pre-skip, keep the input length.
        let skip = (pre_skip / 3) as usize;
        assert!(
            decoded.len() >= skip + samples.len(),
            "nothing is lost at the end"
        );
        let correlation = |lag: usize| {
            let (mut both, mut left, mut right) = (0.0f64, 0.0f64, 0.0f64);
            for (a, b) in samples.iter().zip(&decoded[lag..]) {
                both += f64::from(*a) * f64::from(*b);
                left += f64::from(*a) * f64::from(*a);
                right += f64::from(*b) * f64::from(*b);
            }
            both / (left * right).sqrt()
        };
        // Dropping exactly pre-skip (at 16 kHz) aligns the decoded audio with
        // the input, as ffmpeg's 48 kHz decode does.
        let best = (0..=2 * skip)
            .max_by(|left, right| correlation(*left).total_cmp(&correlation(*right)))
            .unwrap();
        assert_eq!(
            best, skip,
            "pre-skip aligns the decoded audio with the input"
        );
        assert!(correlation(best) > 0.95, "{:.3}", correlation(best));
    }
}
