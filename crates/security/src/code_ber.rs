//! Normalize CMS's constructed BER lengths before passing it to the DER parser.
//! Primitive payloads (including signatures) are never decoded or rewritten.
const INVALID: i32 = -67045;

fn encode(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else {
        let bytes = body.len().to_be_bytes();
        let start = bytes.iter().position(|b| *b != 0).unwrap();
        out.push(0x80 | (bytes.len() - start) as u8);
        out.extend_from_slice(&bytes[start..]);
    }
    out.extend_from_slice(body);
    out
}

fn element(
    input: &[u8],
    cursor: &mut usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<Vec<u8>, i32> {
    *remaining = remaining.checked_sub(1).ok_or(INVALID)?;
    if depth > 32 {
        return Err(INVALID);
    }
    let tag = *input.get(*cursor).ok_or(INVALID)?;
    *cursor += 1;
    if tag == 0 || tag & 31 == 31 {
        return Err(INVALID);
    }
    let size = *input.get(*cursor).ok_or(INVALID)?;
    *cursor += 1;
    let length = if size == 0x80 {
        None
    } else if size < 0x80 {
        Some(size as usize)
    } else {
        let count = (size & 0x7f) as usize;
        if count > size_of::<usize>() {
            return Err(INVALID);
        }
        let mut length = 0usize;
        for _ in 0..count {
            length = length
                .checked_mul(256)
                .and_then(|n| input.get(*cursor).and_then(|b| n.checked_add(*b as usize)))
                .ok_or(INVALID)?;
            *cursor += 1;
        }
        Some(length)
    };
    if tag & 0x20 == 0 {
        let end = cursor.checked_add(length.ok_or(INVALID)?).ok_or(INVALID)?;
        let bytes = input.get(*cursor..end).ok_or(INVALID)?;
        *cursor = end;
        return Ok(encode(tag, bytes));
    }
    let end = length
        .map(|n| cursor.checked_add(n).ok_or(INVALID))
        .transpose()?;
    if end.is_some_and(|end| end > input.len()) {
        return Err(INVALID);
    }
    let mut children = Vec::new();
    loop {
        if let Some(end) = end {
            if *cursor == end {
                break;
            }
            if *cursor > end {
                return Err(INVALID);
            }
        } else if input.get(*cursor..cursor.checked_add(2).ok_or(INVALID)?) == Some(&[0, 0]) {
            *cursor += 2;
            break;
        }
        children.push(element(
            if let Some(end) = end {
                &input[..end]
            } else {
                input
            },
            cursor,
            depth + 1,
            remaining,
        )?);
    }
    if tag == 0x31 {
        children.sort();
    }
    Ok(encode(tag, &children.concat()))
}

pub(super) fn normalize(input: &[u8]) -> Result<Vec<u8>, i32> {
    if input.len() > 16 * 1024 * 1024 {
        return Err(INVALID);
    }
    let mut cursor = 0;
    let output = element(input, &mut cursor, 0, &mut 65536)?;
    if cursor != input.len() {
        return Err(INVALID);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_indefinite_sequences_and_rejects_invalid_lengths() {
        assert_eq!(
            normalize(&[0x30, 0x80, 4, 1, 42, 0, 0]).unwrap(),
            [0x30, 3, 4, 1, 42]
        );
        for bad in [
            &[0x30, 0x80, 4, 1, 42][..],
            &[4, 0x80, 0, 0],
            &[0x30, 1, 4],
            &[0x30, 0, 0],
        ] {
            assert!(normalize(bad).is_err());
        }
    }
}
