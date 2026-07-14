pub fn encode_varint(value: u64, out: &mut Vec<u8>) {
    match value {
        0..=0xfc => out.push(value as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

pub fn parse_u32_be_hex(value: &str) -> Option<u32> {
    u32::from_str_radix(value.trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_varint_uses_bitcoin_compact_size_boundaries() {
        let mut out = Vec::new();
        encode_varint(0xfc, &mut out);
        assert_eq!(out, vec![0xfc]);

        out.clear();
        encode_varint(0xfd, &mut out);
        assert_eq!(out, vec![0xfd, 0xfd, 0x00]);

        out.clear();
        encode_varint(0x1_0000, &mut out);
        assert_eq!(out, vec![0xfe, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn parse_u32_be_hex_accepts_optional_prefix() {
        assert_eq!(parse_u32_be_hex("20000000"), Some(0x2000_0000));
        assert_eq!(parse_u32_be_hex("0x20000000"), Some(0x2000_0000));
        assert_eq!(parse_u32_be_hex("invalid"), None);
    }
}
