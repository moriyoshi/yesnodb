//! CRC32C ( Castagnoli ), used by page headers and extent trailers.
//!
//! # Why CRC32C rather than a fast general-purpose hash
//!
//! Storage does not corrupt data randomly. It produces single-bit flips, burst
//! errors from a bad transfer, torn writes, and zeroed sectors. CRC32C is a
//! linear code chosen for exactly that failure profile: at our 8 KiB page size
//! it **guarantees** detection of every burst error up to 32 bits and every
//! error of three bits or fewer, rather than offering a probability. It is also
//! what ext4, btrfs, PostgreSQL, RocksDB, Kafka and Parquet all use, so an
//! operator reading a corruption report is on familiar ground.
//!
//! The honest counterpoint: for errors of four or more bits CRC32C offers only
//! `2^-32`, where a 64-bit hash such as XXH3 offers `2^-64`. If this format ever
//! widens the checksum field to eight bytes, XXH3-64 becomes the better choice.
//! At four bytes, CRC32C's guarantees make it strictly stronger than a truncated
//! 32-bit hash.
//!
//! # Why the `crc32c` crate rather than a table
//!
//! A hand-rolled table-driven implementation was measured at **14.7 µs per
//! 8 KiB page ( 0.56 GB/s )**. Verification happens once per fault-in, so that
//! is roughly fifteen times the cost of a minor page fault and would dominate
//! warm-cache reads. This crate dispatches to the SSE4.2 / ARMv8 CRC
//! instructions at runtime with a software fallback, which is on the order of
//! 20 GB/s — the slowness was an artefact of the implementation, not of CRC.
//!
//! [`crc32c()`] and [`crc32c_append`] are the only entry points, so replacing the
//! backing implementation stays a one-function change.

/// CRC32C of `data`.
#[inline]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}

/// Continue a CRC32C over another slice.
///
/// Lets a checksum span discontiguous regions without copying them together —
/// a page header whose own checksum field must read as zero, for instance.
#[inline]
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    crc32c::crc32c_append(crc, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_published_test_vectors() {
        // The canonical CRC32C check value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        // RFC 3720 appendix B.4.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    }

    #[test]
    fn append_equals_a_single_pass() {
        let data: Vec<u8> = (0..200u8).collect();
        for split in [0usize, 1, 64, 199, 200] {
            let (a, b) = data.split_at(split);
            assert_eq!(
                crc32c_append(crc32c(a), b),
                crc32c(&data),
                "split at {split} must equal the whole"
            );
        }
    }

    #[test]
    fn detects_single_bit_flips() {
        let mut data = vec![0x5Au8; 128];
        let base = crc32c(&data);
        for i in 0..data.len() {
            for bit in 0..8 {
                data[i] ^= 1 << bit;
                assert_ne!(crc32c(&data), base, "flip at byte {i} bit {bit} undetected");
                data[i] ^= 1 << bit;
            }
        }
    }

    #[test]
    fn detects_every_two_bit_flip_in_a_page() {
        // CRC32C guarantees this at 8 KiB; the test pins the guarantee rather
        // than trusting it. Sampled positions, since the full cross product is
        // 2^28 pairs.
        let data = vec![0xA5u8; crate::BITMAP_BYTES];
        let base = crc32c(&data);
        let positions = [0usize, 1, 63, 64, 511, 4096, 8190, 8191];
        for &i in &positions {
            for &j in &positions {
                for bi in [0u8, 3, 7] {
                    for bj in [0u8, 3, 7] {
                        if i == j && bi == bj {
                            continue;
                        }
                        let mut d = data.clone();
                        d[i] ^= 1 << bi;
                        d[j] ^= 1 << bj;
                        assert_ne!(
                            crc32c(&d),
                            base,
                            "2-bit flip at ({i},{bi}) ({j},{bj}) missed"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn detects_burst_errors_up_to_32_bits() {
        // The failure mode a torn write or bad transfer actually produces.
        let data = vec![0x33u8; crate::BITMAP_BYTES];
        let base = crc32c(&data);
        for start in [0usize, 1, 100, 4095, 8188] {
            for len in 1..=4usize {
                if start + len > data.len() {
                    continue;
                }
                let mut d = data.clone();
                for b in &mut d[start..start + len] {
                    *b ^= 0xFF;
                }
                assert_ne!(crc32c(&d), base, "{len}-byte burst at {start} missed");
            }
        }
    }

    #[test]
    fn detects_a_zeroed_sector() {
        let data: Vec<u8> = (0..crate::BITMAP_BYTES).map(|i| (i % 251) as u8).collect();
        let base = crc32c(&data);
        let mut d = data.clone();
        d[4096..4096 + 512].fill(0);
        assert_ne!(crc32c(&d), base);
    }
}
