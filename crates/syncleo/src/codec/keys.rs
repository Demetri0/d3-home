/// Rotate a key left by `n` bytes. The Syncleo framing derives both the AES key
/// and the IV this way, using the two nibbles of the frame sequence number.
pub fn rotl(key: &[u8; 16], n: u8) -> [u8; 16] {
    let n = (n as usize) % 16;
    core::array::from_fn(|i| key[(i + n) % 16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotl_by_zero_is_identity() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(rotl(&key, 0), key);
    }

    #[test]
    fn rotl_moves_bytes_to_the_front() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let got = rotl(&key, 3);
        assert_eq!(got[0], 3, "byte at n must become the first byte");
        assert_eq!(got[12], 15, "last original byte lands at len-n-1");
        assert_eq!(got[13], 0, "wrapped bytes follow");
        assert_eq!(got[15], 2);
    }

    #[test]
    fn rotl_wraps_past_the_key_length() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(rotl(&key, 19), rotl(&key, 3));
    }
}
