use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const WASM_MAGIC: &[u8; 4] = b"\0asm";

fn read_uleb(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| "truncated Wasm LEB128 value".to_string())?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or_else(|| "Wasm LEB128 value is too large".to_string())?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err("Wasm LEB128 value is too large".into());
        }
    }
}

fn custom_section_name(body: &[u8]) -> Result<&str, String> {
    let mut cursor = 0;
    let length = usize::try_from(read_uleb(body, &mut cursor)?)
        .map_err(|_| "Wasm custom-section name is too large")?;
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| "Wasm custom-section name is too large".to_string())?;
    let name = body
        .get(cursor..end)
        .ok_or_else(|| "truncated Wasm custom-section name".to_string())?;
    std::str::from_utf8(name).map_err(|_| "Wasm custom-section name is not UTF-8".into())
}

fn strip_wasm(bytes: &[u8]) -> Result<(Vec<u8>, usize), String> {
    if bytes.len() < 8 || &bytes[..4] != WASM_MAGIC {
        return Err("not a Wasm binary".into());
    }
    let mut output = bytes[..8].to_vec();
    let mut cursor = 8;
    let mut removed = 0;
    while cursor < bytes.len() {
        let section_start = cursor;
        let section_type = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated Wasm section".to_string())?;
        cursor += 1;
        let section_length = usize::try_from(read_uleb(bytes, &mut cursor)?)
            .map_err(|_| "Wasm section is too large")?;
        let section_end = cursor
            .checked_add(section_length)
            .ok_or_else(|| "Wasm section is too large".to_string())?;
        let body = bytes
            .get(cursor..section_end)
            .ok_or_else(|| "truncated Wasm section".to_string())?;
        let remove = if section_type == 0 {
            match custom_section_name(body) {
                Ok(name) => {
                    name == "name" || name == "sourceMappingURL" || name.starts_with(".debug_")
                }
                Err(_) => false,
            }
        } else {
            false
        };
        if remove {
            removed += 1;
        } else {
            output.extend_from_slice(&bytes[section_start..section_end]);
        }
        cursor = section_end;
    }
    Ok((output, removed))
}

fn walk(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn strip_distribution(directory: &Path) -> Result<(), String> {
    if !directory.is_dir() {
        return Err(format!(
            "UI distribution directory does not exist: {}",
            directory.display()
        ));
    }
    let mut files = Vec::new();
    walk(directory, &mut files)?;
    for path in files {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("map") => {
                fs::remove_file(&path).map_err(|error| error.to_string())?;
                continue;
            }
            Some("wasm") => {}
            _ => continue,
        }
        let original = fs::read(&path).map_err(|error| error.to_string())?;
        let (stripped, removed) = strip_wasm(&original)?;
        if removed > 0 {
            fs::write(&path, stripped).map_err(|error| error.to_string())?;
            println!(
                "Stripped {removed} Wasm metadata section(s) from {}",
                path.strip_prefix(directory)
                    .unwrap_or(&path)
                    .display()
            );
        }
    }
    Ok(())
}

fn main() {
    let directory = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("Usage: strip-wasm-debug <dist-directory>");
            std::process::exit(2);
        });
    if let Err(error) = strip_distribution(&directory) {
        eprintln!("Wasm release stripping failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(section_type: u8, body: &[u8]) -> Vec<u8> {
        let length = u8::try_from(body.len()).expect("test body fits one-byte LEB128");
        let mut bytes = vec![section_type, length];
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn removes_name_section_and_keeps_runtime_sections() {
        let mut wasm = WASM_MAGIC.to_vec();
        wasm.extend_from_slice(&[1, 0, 0, 0]);
        wasm.extend_from_slice(&section(1, &[0]));
        wasm.extend_from_slice(&section(0, &[4, b'n', b'a', b'm', b'e', 1, 0]));

        let (stripped, removed) = strip_wasm(&wasm).expect("valid test Wasm");

        assert_eq!(removed, 1);
        assert_eq!(&stripped, &wasm[..11]);
    }

    #[test]
    fn rejects_truncated_sections() {
        let wasm = [WASM_MAGIC.as_slice(), &[1, 0, 0, 0], &[1, 5, 0]].concat();
        assert!(strip_wasm(&wasm).is_err());
    }
}
