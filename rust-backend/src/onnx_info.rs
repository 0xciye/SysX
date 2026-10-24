//! Reads what model import needs from an ONNX file without loading a
//! runtime: the input image size and the class names Ultralytics exports
//! store in the `names` metadata entry.

use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OnnxInfo {
    /// Width and height of the single NCHW input, when both are fixed.
    pub input_size: Option<(u32, u32)>,
    pub class_names: Option<HashMap<u32, String>>,
}

/// One protobuf field: number and either a varint or a length-delimited payload.
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, &'static str> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("truncated ONNX file")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("invalid ONNX varint")
}

fn fields(bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>, &'static str> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let value = match key & 7 {
            0 => Value::Varint(varint(bytes, &mut at)?),
            2 => {
                let length =
                    usize::try_from(varint(bytes, &mut at)?).map_err(|_| "invalid ONNX length")?;
                let end = at.checked_add(length).ok_or("invalid ONNX length")?;
                let payload = bytes.get(at..end).ok_or("truncated ONNX file")?;
                at = end;
                Value::Bytes(payload)
            }
            1 => {
                at += 8;
                Value::Other
            }
            5 => {
                at += 4;
                Value::Other
            }
            _ => return Err("unsupported ONNX field type"),
        };
        out.push((key >> 3, value));
    }
    Ok(out)
}

fn bytes_of<'a, 'f>(
    fields: &'f [(u64, Value<'a>)],
    number: u64,
) -> impl Iterator<Item = &'a [u8]> + 'f {
    fields.iter().filter_map(move |(field, value)| match value {
        Value::Bytes(bytes) if *field == number => Some(*bytes),
        _ => None,
    })
}

/// Fixed dimensions of a ValueInfoProto's tensor shape; `None` for a
/// symbolic dimension.
fn dimensions(value_info: &[u8]) -> Result<Vec<Option<u64>>, &'static str> {
    let info = fields(value_info)?;
    let mut dims = Vec::new();
    for kind in bytes_of(&info, 2) {
        for tensor in bytes_of(&fields(kind)?, 1) {
            for shape in bytes_of(&fields(tensor)?, 2) {
                for dim in bytes_of(&fields(shape)?, 1) {
                    let dim = fields(dim)?;
                    dims.push(dim.iter().find_map(|(field, value)| match value {
                        Value::Varint(size) if *field == 1 => Some(*size),
                        _ => None,
                    }));
                }
            }
        }
    }
    Ok(dims)
}

fn initializer_names(graph: &[(u64, Value<'_>)]) -> Result<Vec<String>, &'static str> {
    let mut names = Vec::new();
    for initializer in bytes_of(graph, 5) {
        for name in bytes_of(&fields(initializer)?, 8) {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    Ok(names)
}

/// Parses `{0: 'Enemy', 1: 'Head'}` as Ultralytics writes it.
fn parse_names(text: &str) -> Option<HashMap<u32, String>> {
    let inner = text.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut names = HashMap::new();
    let mut rest = inner;
    while !rest.trim().is_empty() {
        let (id, after) = rest.split_once(':')?;
        let id = id
            .trim()
            .trim_start_matches(',')
            .trim()
            .parse::<u32>()
            .ok()?;
        let after = after.trim_start();
        let quote = after.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        let body = &after[1..];
        let end = body.find(quote)?;
        names.insert(id, body[..end].to_owned());
        rest = &body[end + 1..];
    }
    (!names.is_empty()).then_some(names)
}

pub fn read(bytes: &[u8]) -> Result<OnnxInfo, &'static str> {
    let model = fields(bytes)?;
    let mut info = OnnxInfo::default();
    for entry in bytes_of(&model, 14) {
        let entry = fields(entry)?;
        let key = bytes_of(&entry, 1).next().unwrap_or_default();
        if key == b"names" {
            let value = bytes_of(&entry, 2).next().unwrap_or_default();
            info.class_names = parse_names(&String::from_utf8_lossy(value));
        }
    }
    let graph = bytes_of(&model, 7).next().ok_or("ONNX file has no graph")?;
    let graph = fields(graph)?;
    // Older exports also list weights as graph inputs; skip those.
    let weights = initializer_names(&graph)?;
    for input in bytes_of(&graph, 11) {
        let name = bytes_of(&fields(input)?, 1)
            .next()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default();
        if weights.contains(&name) {
            continue;
        }
        let dims = dimensions(input)?;
        if let [_, _, Some(height), Some(width)] = dims[..] {
            if let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) {
                info.input_size = Some((width, height));
            }
        }
        break;
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![((field << 3) | 2) as u8, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    fn dim(size: Option<u8>) -> Vec<u8> {
        tag(
            1,
            &match size {
                Some(size) => vec![1 << 3, size],
                None => tag(2, b"batch"),
            },
        )
    }

    fn model(dims: &[Option<u8>], names: &str) -> Vec<u8> {
        let shape = tag(
            2,
            &dims.iter().flat_map(|size| dim(*size)).collect::<Vec<_>>(),
        );
        let tensor = tag(1, &[vec![1 << 3, 1], shape].concat());
        let input = tag(11, &[tag(1, b"images"), tag(2, &tensor)].concat());
        let meta = tag(14, &[tag(1, b"names"), tag(2, names.as_bytes())].concat());
        [tag(7, &input), meta].concat()
    }

    #[test]
    fn reads_the_input_size_and_class_names_of_a_yolo_export() {
        let info = read(&model(
            &[Some(1), Some(3), Some(100), Some(120)],
            "{0: 'Enemy', 1: \"Head\"}",
        ))
        .unwrap();
        assert_eq!(info.input_size, Some((120, 100)));
        let names = info.class_names.unwrap();
        assert_eq!(names[&0], "Enemy");
        assert_eq!(names[&1], "Head");
    }

    #[test]
    fn dynamic_input_sizes_are_left_unknown() {
        let info = read(&model(&[None, Some(3), None, None], "")).unwrap();
        assert_eq!(info.input_size, None);
        assert_eq!(info.class_names, None);
    }

    #[test]
    fn truncated_files_are_rejected() {
        let bytes = model(&[Some(1), Some(3), Some(64), Some(64)], "{0: 'a'}");
        assert!(read(&bytes[..bytes.len() - 3]).is_err());
    }
}
