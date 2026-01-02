/// Fast usize parser (decimal).
#[inline(always)]
pub(crate) fn parse_usize(buf: &[u8]) -> Option<usize> {
    let mut res: usize = 0;
    let mut found = false;

    for &b in buf {
        if b.is_ascii_digit() {
            // Check for overflow could be added here if needed,
            // but wrapping is standard for "fast" parsing logic.
            res = res.wrapping_mul(10).wrapping_add((b - b'0') as usize);
            found = true;
        } else if found {
            // We were parsing numbers, now we hit a non-digit: stop.
            break;
        } else if b == b' ' || b == b'\t' {
            // Skip leading whitespace
            continue;
        } else {
            // Found a non-digit before any digit (e.g., letters)
            return None;
        }
    }

    if found { Some(res) } else { None }
}

/// Returns the total length of the chunked body (including the final 0\r\n\r\n).
#[inline]
pub(crate) fn parse_chunked_body(buf: &[u8]) -> Option<usize> {
    let mut pos = 0;
    let len = buf.len();

    loop {
        if pos >= len {
            return None;
        }

        // Find CRLF fast.
        // We only scan a limited window because chunk sizes are usually small strings.
        let mut i = pos;
        let mut found_crlf = false;

        // Scan for LF. The max chunk size line usually isn't massive.
        while i < len {
            if buf[i] == b'\n' {
                found_crlf = true;
                break;
            }
            i += 1;
        }

        if !found_crlf {
            return None;
        } // Incomplete chunk size line

        // Parse hex from pos to i-1 (ignoring \r)
        // i points to \n, i-1 should be \r.
        if i == 0 || buf[i - 1] != b'\r' {
            return None;
        } // Invalid format

        let hex_end = i - 1;
        // Parse hex digits until semicolon (extension) or end of line
        let mut chunk_size = 0usize;
        for &b in &buf[pos..hex_end] {
            if b == b';' {
                break;
            } // Ignore chunk extensions

            let val = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => continue, // Skip whitespace or invalid chars silently for speed
            };

            // Check overflow if necessary, but for HTTP chunks usize is usually enough
            chunk_size = (chunk_size << 4) | (val as usize);
        }

        // Move pos after the \n
        pos = i + 1;

        if chunk_size == 0 {
            // Last chunk. Need to skip trailers and find final empty line (\r\n).
            // Current pos is after "0\r\n".
            // The simplest end is immediately "\r\n".
            if pos + 2 <= len && &buf[pos..pos + 2] == b"\r\n" {
                return Some(pos + 2);
            }
            // If there are trailers, we need to scan for \r\n\r\n
            // Scanning for double CRLF
            let mut k = pos;
            while k + 3 < len {
                if buf[k] == b'\r'
                    && buf[k + 1] == b'\n'
                    && buf[k + 2] == b'\r'
                    && buf[k + 3] == b'\n'
                {
                    return Some(k + 4);
                }
                k += 1;
            }
            return None; // Incomplete trailers
        }

        // Check if full chunk is available: data (chunk_size) + CRLF (2)
        let next_start = pos + chunk_size + 2;
        if next_start > len {
            return None; // Incomplete chunk data
        }
        pos = next_start;
    }
}

/// Check for "chunked" case-insensitive.
#[inline(always)]
pub(crate) fn is_chunked_slice(buf: &[u8]) -> bool {
    let mut start = 0;
    while start < buf.len() && matches!(buf[start], b' ' | b'\t') {
        start += 1;
    }

    let mut end = buf.len();
    while end > start && matches!(buf[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
        end -= 1;
    }

    let sliced = &buf[start..end];
    if sliced.len() != 7 {
        return false;
    }

    (sliced[0] | 0x20) == b'c'
        && (sliced[1] | 0x20) == b'h'
        && (sliced[2] | 0x20) == b'u'
        && (sliced[3] | 0x20) == b'n'
        && (sliced[4] | 0x20) == b'k'
        && (sliced[5] | 0x20) == b'e'
        && (sliced[6] | 0x20) == b'd'
}
