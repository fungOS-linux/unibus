//! Bounded HDHomeRun IPv4 discovery codec; no control/upgrade forwarding.
//! Wire reference: Silicondust libhdhomerun/hdhomerun_pkt.h.

use anyhow::{Context, Result, ensure};

pub const PORT: u16 = 65001;
pub const MAX_PACKET: usize = 1460;

fn decode(bytes: &[u8], expected: u16) -> Result<Vec<(u8, &[u8])>> {
    ensure!(
        (8..=MAX_PACKET).contains(&bytes.len()),
        "invalid packet size"
    );
    ensure!(
        u16::from_be_bytes(bytes[..2].try_into()?) == expected,
        "unsupported packet type"
    );
    let length = usize::from(u16::from_be_bytes(bytes[2..4].try_into()?));
    ensure!(length + 8 == bytes.len(), "invalid payload length");
    ensure!(
        crc32fast::hash(&bytes[..length + 4])
            == u32::from_le_bytes(bytes[length + 4..].try_into()?),
        "invalid CRC"
    );
    let mut payload = &bytes[4..length + 4];
    let mut tags = Vec::new();
    while !payload.is_empty() {
        ensure!(payload.len() >= 2, "truncated TLV");
        let tag = payload[0];
        let mut length = usize::from(payload[1] & 0x7f);
        let header = if payload[1] & 0x80 != 0 {
            length |= usize::from(*payload.get(2).context("truncated TLV length")?) << 7;
            3
        } else {
            2
        };
        ensure!(header + length <= payload.len(), "truncated TLV value");
        tags.push((tag, &payload[header..header + length]));
        payload = &payload[header + length..];
    }
    Ok(tags)
}

fn encode(kind: u16, tags: &[(u8, &[u8])]) -> Result<Vec<u8>> {
    let mut packet = vec![0; 4];
    for (tag, value) in tags {
        ensure!(value.len() <= MAX_PACKET - 12, "TLV too large");
        packet.push(*tag);
        if value.len() < 128 {
            packet.push(value.len() as u8);
        } else {
            packet.push((value.len() as u8 & 0x7f) | 0x80);
            packet.push((value.len() >> 7) as u8);
        }
        packet.extend_from_slice(value);
    }
    ensure!(packet.len() + 4 <= MAX_PACKET, "packet too large");
    let length = (packet.len() - 4) as u16;
    packet[..2].copy_from_slice(&kind.to_be_bytes());
    packet[2..4].copy_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&crc32fast::hash(&packet).to_le_bytes());
    Ok(packet)
}

pub fn response(
    request: &[u8],
    device: u32,
    tuners: u8,
    base_url: &str,
    auth: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let tags = decode(request, 2)?;
    let mut device_type = None;
    let mut device_id = None;
    for (tag, value) in tags {
        match tag {
            1 | 0x2d => {
                ensure!(
                    device_type.is_none()
                        && !value.is_empty()
                        && value.len() % 4 == 0
                        && (tag != 1 || value.len() == 4),
                    "invalid/duplicate device type"
                );
                device_type = Some(
                    value
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|v| matches!(u32::from_be_bytes(*v), 1 | u32::MAX)),
                );
            }
            2 => {
                ensure!(
                    device_id.is_none() && value.len() == 4,
                    "invalid/duplicate device ID"
                );
                device_id = Some(u32::from_be_bytes(value.try_into()?));
            }
            _ => {}
        }
    }
    // Current libhdhomerun omits wildcard DeviceID entirely.
    if device_type != Some(true) || device_id.is_some_and(|id| id != u32::MAX && id != device) {
        return Ok(None);
    }
    let kind = 1u32.to_be_bytes();
    let id = device.to_be_bytes();
    let count = [tuners];
    let lineup = format!("{base_url}/lineup.json");
    let mut tags: Vec<(u8, &[u8])> = vec![
        (1, &kind),
        (2, &id),
        (0x10, &count),
        (0x2a, base_url.as_bytes()),
        (0x27, lineup.as_bytes()),
    ];
    if let Some(auth) = auth {
        tags.push((0x2b, auth.as_bytes()));
    }
    Ok(Some(encode(3, &tags)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modern_wildcard_targeted_and_multitype_discovery() {
        for tags in [
            vec![(1, 1u32.to_be_bytes().to_vec())],
            vec![
                (1, 1u32.to_be_bytes().to_vec()),
                (2, 0x12345678u32.to_be_bytes().to_vec()),
            ],
            vec![(0x2d, [5u32.to_be_bytes(), 1u32.to_be_bytes()].concat())],
        ] {
            let tags: Vec<_> = tags.iter().map(|(t, v)| (*t, v.as_slice())).collect();
            let request = encode(2, &tags).unwrap();
            let reply = response(&request, 0x12345678, 2, "http://192.168.2.74:8088", None)
                .unwrap()
                .unwrap();
            let fields = decode(&reply, 3).unwrap();
            assert!(fields.contains(&(0x10, &[2][..])));
            assert!(
                fields
                    .iter()
                    .any(|(tag, value)| *tag == 0x2a && *value == b"http://192.168.2.74:8088")
            );
        }
    }
    #[test]
    fn corruption_truncation_and_wrong_target_do_not_reply() {
        let request = encode(2, &[(1, &1u32.to_be_bytes()), (2, &42u32.to_be_bytes())]).unwrap();
        assert!(
            response(&request, 43, 2, "http://local", None)
                .unwrap()
                .is_none()
        );
        for len in 0..request.len() {
            assert!(response(&request[..len], 42, 2, "http://local", None).is_err());
        }
        let mut corrupt = request.clone();
        corrupt[7] ^= 1;
        assert!(response(&corrupt, 42, 2, "http://local", None).is_err());
        let malformed = encode(2, &[(1, &[1]), (2, &42u32.to_be_bytes())]).unwrap();
        assert!(response(&malformed, 42, 2, "http://local", None).is_err());
    }
    #[test]
    fn long_tlv_roundtrip_and_crc_known_vector() {
        assert_eq!(crc32fast::hash(b"123456789"), 0xcbf43926);
        let value = vec![7; 200];
        let encoded = encode(2, &[(99, &value)]).unwrap();
        assert_eq!(decode(&encoded, 2).unwrap(), vec![(99, value.as_slice())]);
    }
}
