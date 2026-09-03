//! Stabiler 64-Bit-Hash (FNV-1a), gemeinsam genutzt von [`crate::template`]
//! (Template-IDs) und [`crate::baseline`] (Unit-Schlüssel).
//!
//! Bewusst keine Nutzung von `std::collections::hash_map::DefaultHasher`:
//! dessen Algorithmus ist laut Std-Doku *nicht* über Rust-Versionen hinweg
//! stabil, hier werden aber über Neustarts/Rust-Updates hinweg
//! reproduzierbare IDs gebraucht.

/// Berechnet einen stabilen 64-Bit-Hash (FNV-1a) über die gegebenen Bytes.
pub fn fnv1a_hash64(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gleiche_eingabe_ergibt_gleichen_hash() {
        assert_eq!(fnv1a_hash64(b"sshd.service"), fnv1a_hash64(b"sshd.service"));
    }

    #[test]
    fn unterschiedliche_eingaben_ergeben_ueblicherweise_unterschiedliche_hashes() {
        assert_ne!(fnv1a_hash64(b"sshd.service"), fnv1a_hash64(b"cron.service"));
    }

    #[test]
    fn leere_eingabe_ist_der_offset_basis_wert() {
        assert_eq!(fnv1a_hash64(b""), 0xcbf2_9ce4_8422_2325);
    }
}
