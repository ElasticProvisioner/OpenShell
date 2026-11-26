// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;

/// Largest driver table a driver reads.
pub const MAX_TABLE_BYTES: u64 = 1024 * 1024;

/// Read the driver table from `reader`. An empty table means the operator
/// wrote none, and the driver uses its defaults.
pub fn read_table(reader: impl Read) -> std::io::Result<String> {
    let mut table = String::new();
    let read = reader
        .take(MAX_TABLE_BYTES + 1)
        .read_to_string(&mut table)?;
    if read as u64 > MAX_TABLE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "driver table is too large",
        ));
    }
    Ok(table)
}

/// Read the driver table from stdin.
pub fn read_stdin_table() -> std::io::Result<String> {
    read_table(std::io::stdin().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_an_empty_table() {
        assert_eq!(read_table(&b""[..]).unwrap(), "");
    }

    #[test]
    fn the_table_is_read_unparsed() {
        let text = "key = \"value\"\n# a comment\n";
        assert_eq!(read_table(text.as_bytes()).unwrap(), text);
    }

    #[test]
    fn an_oversized_table_is_rejected() {
        let big = vec![b'#'; usize::try_from(MAX_TABLE_BYTES).unwrap() + 1];
        assert!(read_table(&big[..]).is_err());
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        assert!(read_table(&[0xff, 0xfe][..]).is_err());
    }
}
