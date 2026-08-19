use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Rotate a key left by `n` bytes. The Syncleo framing derives both the AES key
/// and the IV this way, using the two nibbles of the frame sequence number.
pub fn rotl(key: &[u8; 16], n: u8) -> [u8; 16] {
    let n = (n as usize) % 16;
    core::array::from_fn(|i| key[(i + n) % 16])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub inkey: [u8; 16],
    pub outkey: [u8; 16],
}

fn reversed(bytes: [u8; 32]) -> [u8; 32] {
    let mut out = bytes;
    out.reverse();
    out
}

/// Our X25519 public key in the byte order the firmware expects: reversed.
pub fn public_wire(our_private: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*our_private);
    reversed(PublicKey::from(&secret).to_bytes())
}

/// Derive the two AES-128 half-keys from the X25519 shared secret.
///
/// The firmware reverses byte order in three places: our public key on the wire,
/// the device public key before import, and the shared secret before hashing.
pub fn derive(our_private: &[u8; 32], device_public_wire: &[u8; 32]) -> SessionKeys {
    let secret = StaticSecret::from(*our_private);
    let device = PublicKey::from(reversed(*device_public_wire));
    let shared = reversed(secret.diffie_hellman(&device).to_bytes());

    let digest = Sha256::digest(shared);
    SessionKeys {
        inkey: digest[..16].try_into().expect("sha256 is 32 bytes"),
        outkey: digest[16..].try_into().expect("sha256 is 32 bytes"),
    }
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

    // Golden vector produced by the Python reference implementation
    // (gch1p/polaris_pwk_1725cgld) with a fixed private key.
    const OUR_PRIVATE: [u8; 32] = [7; 32];
    const DEVICE_PUBLIC_WIRE: &str =
        "21d4043d930c3d75140c158c3406257204670512254e6e145eae239f354bdb57";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn our_public_key_goes_on_the_wire_reversed() {
        assert_eq!(
            public_wire(&OUR_PRIVATE).to_vec(),
            unhex("6d7be97f7ff374c67e2228812774d1811872009cfc5833fdc704f2eaea4fbe13")
        );
    }

    #[test]
    fn derives_the_reference_session_keys() {
        let device: [u8; 32] = unhex(DEVICE_PUBLIC_WIRE).try_into().unwrap();
        let keys = derive(&OUR_PRIVATE, &device);

        assert_eq!(
            keys.inkey.to_vec(),
            unhex("3ec02e08f3f3bea4dacc8179f46f493d")
        );
        assert_eq!(
            keys.outkey.to_vec(),
            unhex("8c6715bf7555d1e7e0032db0a7d3da76")
        );
    }
}
