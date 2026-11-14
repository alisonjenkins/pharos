//! Tiny audio files made in memory for tests: never a real song.

/// A mono 16-bit sine as `(samples, sample_rate)`.
pub fn sine(hz: f32, rate: u32, seconds: f32, amp: f32) -> Vec<i16> {
    let n = (rate as f32 * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / rate as f32;
            (amp * (std::f32::consts::TAU * hz * t).sin() * f32::from(i16::MAX)) as i16
        })
        .collect()
}

/// A RIFF/WAVE file of 16-bit PCM, `channels` interleaved.
pub fn wav(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * u32::from(channels) * 2).to_le_bytes());
    out.extend((channels * 2).to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for s in samples {
        out.extend(s.to_le_bytes());
    }
    out
}

fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
        crc
    })
}

fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |mut crc, &b| {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// A FLAC file of mono 16-bit audio with VERBATIM subframes: the codec's
/// framing and checksums, without compression.
pub fn flac(samples: &[i16], rate: u32) -> Vec<u8> {
    const BLOCK: usize = 1152;
    let mut out = Vec::new();
    out.extend(b"fLaC");
    // STREAMINFO, the last metadata block.
    out.push(0x80);
    out.extend([0, 0, 34]);
    out.extend((BLOCK as u16).to_be_bytes());
    out.extend((BLOCK as u16).to_be_bytes());
    out.extend([0, 0, 0, 0, 0, 0]);
    let total = samples.len() as u64;
    // 20 bits rate, 3 bits channels-1, 5 bits bps-1, 36 bits total samples.
    let packed: u64 = (u64::from(rate) << 44) | (15u64 << 36) | total;
    out.extend(packed.to_be_bytes());
    out.extend([0u8; 16]);
    for (n, block) in samples.chunks(BLOCK).enumerate() {
        let mut f = vec![0xFF, 0xF8];
        // Block size is the 16-bit value after the number; rate and bps are
        // in STREAMINFO; one channel.
        f.push(0x70);
        f.push(0x00);
        let mut number = [0u8; 4];
        if let Some(c) = char::from_u32(n as u32) {
            f.extend(c.encode_utf8(&mut number).as_bytes());
        }
        f.extend(((block.len() - 1) as u16).to_be_bytes());
        f.push(crc8(&f));
        f.push(0x02);
        for s in block {
            f.extend(s.to_be_bytes());
        }
        let crc = crc16(&f);
        f.extend(crc.to_be_bytes());
        out.extend(f);
    }
    out
}
