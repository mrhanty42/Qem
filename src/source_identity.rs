use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const FNV64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV64_PRIME: u64 = 0x100000001b3;
const FULL_FINGERPRINT_MAX_BYTES: usize = 32 * 1024;
const FINGERPRINT_WINDOW_BYTES: usize = 8 * 1024;

pub(crate) fn sampled_content_fingerprint_budget(len: usize) -> usize {
    if len <= FULL_FINGERPRINT_MAX_BYTES {
        return len;
    }

    sample_starts(len)
        .into_iter()
        .map(|start| FINGERPRINT_WINDOW_BYTES.min(len.saturating_sub(start)))
        .sum()
}

pub(crate) fn sampled_content_fingerprint(bytes: &[u8]) -> u64 {
    let mut state = FNV64_OFFSET_BASIS;
    hash_u64(&mut state, bytes.len() as u64);

    if bytes.len() <= FULL_FINGERPRINT_MAX_BYTES {
        hash_sample(&mut state, 0, bytes);
        return state;
    }

    for start in sample_starts(bytes.len()) {
        let end = start
            .saturating_add(FINGERPRINT_WINDOW_BYTES)
            .min(bytes.len());
        hash_sample(&mut state, start, &bytes[start..end]);
    }

    state
}

pub(crate) fn sampled_file_fingerprint(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len() as usize;

    let mut state = FNV64_OFFSET_BASIS;
    hash_u64(&mut state, len as u64);

    if len <= FULL_FINGERPRINT_MAX_BYTES {
        let mut buf = Vec::with_capacity(len);
        file.read_to_end(&mut buf)?;
        hash_sample(&mut state, 0, &buf);
        return Ok(state);
    }

    let mut buf = vec![0u8; FINGERPRINT_WINDOW_BYTES];
    for start in sample_starts(len) {
        let sample_len = FINGERPRINT_WINDOW_BYTES.min(len.saturating_sub(start));
        file.seek(SeekFrom::Start(start as u64))?;
        file.read_exact(&mut buf[..sample_len])?;
        hash_sample(&mut state, start, &buf[..sample_len]);
    }

    Ok(state)
}

fn sample_starts(len: usize) -> Vec<usize> {
    debug_assert!(len > FULL_FINGERPRINT_MAX_BYTES);

    let window = FINGERPRINT_WINDOW_BYTES.min(len);
    let last_start = len.saturating_sub(window);
    let anchors = [
        0usize,
        len / 4,
        len / 2,
        (len / 4).saturating_mul(3),
        last_start,
    ];

    let mut starts = Vec::with_capacity(anchors.len());
    for (index, anchor) in anchors.into_iter().enumerate() {
        let start = if index == 0 || index + 1 == anchors.len() {
            anchor.min(last_start)
        } else {
            anchor.saturating_sub(window / 2).min(last_start)
        };
        if starts.last().copied() != Some(start) {
            starts.push(start);
        }
    }
    starts
}

fn hash_sample(state: &mut u64, start: usize, bytes: &[u8]) {
    hash_u64(state, start as u64);
    hash_u64(state, bytes.len() as u64);
    hash_bytes(state, bytes);
}

fn hash_u64(state: &mut u64, value: u64) {
    hash_bytes(state, &value.to_le_bytes());
}

fn hash_bytes(state: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *state ^= u64::from(byte);
        *state = state.wrapping_mul(FNV64_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        sampled_content_fingerprint, sampled_content_fingerprint_budget, sampled_file_fingerprint,
        FINGERPRINT_WINDOW_BYTES, FULL_FINGERPRINT_MAX_BYTES,
    };
    use std::fs;

    #[test]
    fn sampled_content_fingerprint_changes_with_small_edits() {
        let left = b"alpha\nbeta\ngamma\n".repeat(4096);
        let mut right = left.clone();
        let middle = right.len() / 2;
        right[middle] ^= 1;

        assert_ne!(
            sampled_content_fingerprint(&left),
            sampled_content_fingerprint(&right)
        );
    }

    #[test]
    fn sampled_file_fingerprint_matches_in_memory_version() {
        let dir =
            std::env::temp_dir().join(format!("qem-source-fingerprint-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sample.txt");
        let bytes = b"abcd\n".repeat(10_000);
        fs::write(&path, &bytes).unwrap();

        let file_fingerprint = sampled_file_fingerprint(&path).unwrap();
        let memory_fingerprint = sampled_content_fingerprint(&bytes);
        assert_eq!(file_fingerprint, memory_fingerprint);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sampled_content_fingerprint_budget_matches_sampled_windows() {
        assert_eq!(sampled_content_fingerprint_budget(0), 0);
        assert_eq!(sampled_content_fingerprint_budget(17), 17);
        assert_eq!(
            sampled_content_fingerprint_budget(FULL_FINGERPRINT_MAX_BYTES),
            FULL_FINGERPRINT_MAX_BYTES
        );
        assert_eq!(
            sampled_content_fingerprint_budget(FULL_FINGERPRINT_MAX_BYTES + 1),
            5 * FINGERPRINT_WINDOW_BYTES
        );
    }
}
