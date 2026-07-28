//! Node-compatible host capabilities for BamTS.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bamts_runtime::Host;

/// Concrete Node-compatible capability state.
///
/// Environment and arguments are explicit rather than inherited from the
/// embedding process, keeping executions independent of the invoking machine.
pub struct NodeHost {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    started: Instant,
    random_state: u64,
}

impl Default for NodeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeHost {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            argv: Vec::new(),
            env: BTreeMap::new(),
            started: Instant::now(),
            random_state: 0x6a09_e667_f3bc_c909,
        }
    }

    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn set_argv(&mut self, argv: impl IntoIterator<Item = String>) {
        self.argv = argv.into_iter().collect();
    }

    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    #[must_use]
    pub fn env(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    pub fn set_env(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.env.insert(name.into(), value.into());
    }

    pub fn delete_env(&mut self, name: &str) -> bool {
        self.env.remove(name).is_some()
    }
}

impl Host for NodeHost {
    fn write_stdout(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
    }

    fn exit_code(&self) -> i32 {
        self.exit_code
    }

    fn set_exit_code(&mut self, exit_code: i32) {
        self.exit_code = exit_code;
    }

    fn argv(&self) -> &[String] {
        &self.argv
    }

    fn env(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    fn set_env(&mut self, name: &str, value: &str) {
        self.env.insert(name.to_owned(), value.to_owned());
    }

    fn delete_env(&mut self, name: &str) -> bool {
        self.env.remove(name).is_some()
    }

    fn now_ms(&mut self) -> u64 {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    fn monotonic_ns(&mut self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn random(&mut self) -> f64 {
        // xorshift64*: deterministic, non-cryptographic entropy for Math.random.
        let mut state = self.random_state;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.random_state = state;
        let bits = state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11;
        (bits as f64) * (1.0 / ((1_u64 << 53) as f64))
    }

    fn hash(&mut self, algorithm: &str, data: &[u8]) -> Option<Vec<u8>> {
        match algorithm.to_ascii_lowercase().replace('-', "").as_str() {
            "sha256" => Some(sha256(data).to_vec()),
            "sha512" => Some(sha512(data).to_vec()),
            _ => None,
        }
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (word, bytes) in words[..16].iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().expect("four bytes"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0_u8; 32];
    for (chunk, value) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

fn sha512(data: &[u8]) -> [u8; 64] {
    const INITIAL: [u64; 8] = [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];
    const K: [u64; 80] = [
        0x428a2f98d728ae22,
        0x7137449123ef65cd,
        0xb5c0fbcfec4d3b2f,
        0xe9b5dba58189dbbc,
        0x3956c25bf348b538,
        0x59f111f1b605d019,
        0x923f82a4af194f9b,
        0xab1c5ed5da6d8118,
        0xd807aa98a3030242,
        0x12835b0145706fbe,
        0x243185be4ee4b28c,
        0x550c7dc3d5ffb4e2,
        0x72be5d74f27b896f,
        0x80deb1fe3b1696b1,
        0x9bdc06a725c71235,
        0xc19bf174cf692694,
        0xe49b69c19ef14ad2,
        0xefbe4786384f25e3,
        0x0fc19dc68b8cd5b5,
        0x240ca1cc77ac9c65,
        0x2de92c6f592b0275,
        0x4a7484aa6ea6e483,
        0x5cb0a9dcbd41fbd4,
        0x76f988da831153b5,
        0x983e5152ee66dfab,
        0xa831c66d2db43210,
        0xb00327c898fb213f,
        0xbf597fc7beef0ee4,
        0xc6e00bf33da88fc2,
        0xd5a79147930aa725,
        0x06ca6351e003826f,
        0x142929670a0e6e70,
        0x27b70a8546d22ffc,
        0x2e1b21385c26c926,
        0x4d2c6dfc5ac42aed,
        0x53380d139d95b3df,
        0x650a73548baf63de,
        0x766a0abb3c77b2a8,
        0x81c2c92e47edaee6,
        0x92722c851482353b,
        0xa2bfe8a14cf10364,
        0xa81a664bbc423001,
        0xc24b8b70d0f89791,
        0xc76c51a30654be30,
        0xd192e819d6ef5218,
        0xd69906245565a910,
        0xf40e35855771202a,
        0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8,
        0x1e376c085141ab53,
        0x2748774cdf8eeb99,
        0x34b0bcb5e19b48a8,
        0x391c0cb3c5c95a63,
        0x4ed8aa4ae3418acb,
        0x5b9cca4f7763e373,
        0x682e6ff3d6b2b8a3,
        0x748f82ee5defb2fc,
        0x78a5636f43172f60,
        0x84c87814a1f0ab72,
        0x8cc702081a6439ec,
        0x90befffa23631e28,
        0xa4506cebde82bde9,
        0xbef9a3f7b2c67915,
        0xc67178f2e372532b,
        0xca273eceea26619c,
        0xd186b8c721c0c207,
        0xeada7dd6cde0eb1e,
        0xf57d4f7fee6ed178,
        0x06f067aa72176fba,
        0x0a637dc5a2c898a6,
        0x113f9804bef90dae,
        0x1b710b35131c471b,
        0x28db77f523047d84,
        0x32caab7b40c72493,
        0x3c9ebe0a15c9bebc,
        0x431d67c49c100d4c,
        0x4cc5d4becb3e42b6,
        0x597f299cfc657e2a,
        0x5fcb6fab3ad6faec,
        0x6c44198c4a475817,
    ];
    let bit_len = (data.len() as u128).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 128 != 112 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for block in padded.chunks_exact(128) {
        let mut words = [0_u64; 80];
        for (word, bytes) in words[..16].iter_mut().zip(block.chunks_exact(8)) {
            *word = u64::from_be_bytes(bytes.try_into().expect("eight bytes"));
        }
        for index in 16..80 {
            let s0 = words[index - 15].rotate_right(1)
                ^ words[index - 15].rotate_right(8)
                ^ (words[index - 15] >> 7);
            let s1 = words[index - 2].rotate_right(19)
                ^ words[index - 2].rotate_right(61)
                ^ (words[index - 2] >> 6);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let choice = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0_u8; 64];
    for (chunk, value) in digest.chunks_exact_mut(8).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(feature = "aot-main")]
fn run_aot_main() -> i32 {
    use std::io::Write;

    use bamts_bytecode::{DecodeLimits, decode_verified};
    use bamts_native::linked_program;
    use bamts_runtime::{Limits, run_linked_program};

    let linked = match linked_program() {
        Ok(linked) => linked,
        Err(_) => return 1,
    };
    let module = match decode_verified(linked.bytecode(), &DecodeLimits::default()) {
        Ok(module) => module,
        Err(_) => return 1,
    };
    let mut host = NodeHost::new();
    let outcome = match run_linked_program(&module, &linked, &mut host, &Limits::default()) {
        Ok(outcome) => outcome,
        Err(_) => return 1,
    };
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(host.stdout()).is_err() || stdout.write_all(&outcome.stdout).is_err() {
        return 1;
    }
    if host.exit_code() == 0 {
        outcome.exit_code
    } else {
        host.exit_code()
    }
}

/// C process entry for a linked BamTS AOT image.
#[cfg(feature = "aot-main")]
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    run_aot_main()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        text
    }

    #[test]
    fn capabilities_capture_bytes_and_mutate_process_state() {
        let mut host = NodeHost::new();
        Host::write_stdout(&mut host, b"out");
        Host::write_stderr(&mut host, b"err");
        Host::set_exit_code(&mut host, 23);
        host.set_argv(["bamts".to_owned(), "file.ts".to_owned()]);
        Host::set_env(&mut host, "NODE_ENV", "test");
        assert_eq!(host.stdout(), b"out");
        assert_eq!(host.stderr(), b"err");
        assert_eq!(host.exit_code(), 23);
        assert_eq!(host.argv(), ["bamts", "file.ts"]);
        assert_eq!(host.env("NODE_ENV"), Some("test"));
        assert!(Host::delete_env(&mut host, "NODE_ENV"));
        assert_eq!(host.env("NODE_ENV"), None);
    }

    #[test]
    fn sha2_matches_standard_vectors() {
        let mut host = NodeHost::new();
        assert_eq!(
            hex(&Host::hash(&mut host, "sha-256", b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Host::hash(&mut host, "SHA512", b"abc").unwrap()),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(Host::hash(&mut host, "md5", b"abc"), None);
    }

    #[test]
    fn clocks_and_random_are_capabilities() {
        let mut host = NodeHost::new();
        assert!(Host::now_ms(&mut host) > 0);
        let first = Host::monotonic_ns(&mut host);
        let second = Host::monotonic_ns(&mut host);
        assert!(second >= first);
        let random = Host::random(&mut host);
        assert!((0.0..1.0).contains(&random));
    }
}
