//! `.torrent` metainfo parser.

use std::borrow::Cow;
use std::collections::BTreeMap;

use magpie_bt_bencode::{DecodeError, Value, decode, dict_value_span};

use crate::error::{ParseError, ParseErrorKind};
use crate::info_hash::{InfoHash, sha1, sha256};
use crate::types::{FileListV1, FileTreeNode, FileV1, Info, InfoV1, InfoV2, MetaInfo};

/// Parses a single `.torrent` byte slice.
///
/// # Errors
/// Returns [`ParseError`] when the input is not a well-formed BEP 3/52
/// metainfo document. Structural bencode errors are wrapped as
/// [`ParseErrorKind::Bencode`].
pub fn parse(input: &[u8]) -> Result<MetaInfo<'_>, ParseError> {
    let root = decode(input)?;
    let root_dict = root.as_dict().ok_or_else(|| {
        ParseError::new(ParseErrorKind::WrongType {
            field: "<root>",
            expected: "dict",
        })
    })?;

    let info_bytes = {
        let info_span = dict_value_span(input, b"info")?
            .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info")))?;
        &input[info_span]
    };

    let info_value = root_dict
        .get(&b"info"[..])
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info")))?;
    let info_dict = info_value.as_dict().ok_or_else(|| {
        ParseError::new(ParseErrorKind::WrongType {
            field: "info",
            expected: "dict",
        })
    })?;

    let info = parse_info(info_dict)?;
    let info_hash = compute_info_hash(&info, info_bytes);

    Ok(MetaInfo {
        announce: get_bytes(root_dict, "announce"),
        announce_list: parse_announce_list(root_dict)?,
        comment: get_bytes(root_dict, "comment"),
        created_by: get_bytes(root_dict, "created by"),
        creation_date: get_int(root_dict, "creation date"),
        encoding: get_bytes(root_dict, "encoding"),
        info,
        info_bytes,
        info_hash,
    })
}

fn compute_info_hash(info: &Info<'_>, info_bytes: &[u8]) -> InfoHash {
    match (info.v1.is_some(), info.v2.is_some()) {
        (true, false) => InfoHash::V1(sha1(info_bytes)),
        (false, true) => InfoHash::V2(sha256(info_bytes)),
        (true, true) => InfoHash::Hybrid {
            v1: sha1(info_bytes),
            v2: sha256(info_bytes),
        },
        (false, false) => unreachable!("parse_info rejects info dicts that are neither v1 nor v2"),
    }
}

type Dict<'a> = BTreeMap<Cow<'a, [u8]>, Value<'a>>;

fn parse_info<'a>(info: &Dict<'a>) -> Result<Info<'a>, ParseError> {
    let name = get_bytes(info, "name")
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.name")))?;
    let piece_length = get_int(info, "piece length")
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.piece length")))?;
    let piece_length = u64_positive_pow2(piece_length, "info.piece length")?;
    let private = get_int(info, "private").is_some_and(|v| v != 0);

    let has_pieces = info.contains_key(&b"pieces"[..]);
    let has_file_tree = info.contains_key(&b"file tree"[..]);
    let meta_version = get_int(info, "meta version");

    let v1 = if has_pieces {
        Some(parse_info_v1(info)?)
    } else {
        None
    };

    let v2 = match (has_file_tree, meta_version) {
        (true, Some(mv)) => {
            let mv = u64::try_from(mv)
                .map_err(|_| ParseError::new(ParseErrorKind::UnsupportedMetaVersion(u64::MAX)))?;
            if mv != 2 {
                return Err(ParseError::new(ParseErrorKind::UnsupportedMetaVersion(mv)));
            }
            Some(parse_info_v2(info, mv)?)
        }
        (true, None) => {
            return Err(ParseError::new(ParseErrorKind::MissingField(
                "info.meta version",
            )));
        }
        (false, Some(2)) => {
            return Err(ParseError::new(ParseErrorKind::MissingField(
                "info.file tree",
            )));
        }
        _ => None,
    };

    if v1.is_none() && v2.is_none() {
        return Err(ParseError::new(ParseErrorKind::UnrecognisedInfo));
    }

    Ok(Info {
        name,
        piece_length,
        private,
        v1,
        v2,
    })
}

fn parse_info_v1<'a>(info: &Dict<'a>) -> Result<InfoV1<'a>, ParseError> {
    let pieces = get_bytes(info, "pieces")
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.pieces")))?;
    if !pieces.len().is_multiple_of(20) {
        return Err(ParseError::new(ParseErrorKind::InvalidPiecesBlob(
            pieces.len(),
        )));
    }

    let has_length = info.contains_key(&b"length"[..]);
    let has_files = info.contains_key(&b"files"[..]);
    let files = match (has_length, has_files) {
        (true, true) => return Err(ParseError::new(ParseErrorKind::ConflictingV1Layout)),
        (true, false) => {
            let length = get_int(info, "length")
                .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.length")))?;
            FileListV1::Single {
                length: u64_non_negative(length, "info.length")?,
            }
        }
        (false, true) => {
            let files_list = info
                .get(&b"files"[..])
                .and_then(Value::as_list)
                .ok_or_else(|| {
                    ParseError::new(ParseErrorKind::WrongType {
                        field: "info.files",
                        expected: "list",
                    })
                })?;
            let mut files = Vec::with_capacity(files_list.len());
            for entry in files_list {
                files.push(parse_file_v1(entry)?);
            }
            FileListV1::Multi { files }
        }
        (false, false) => {
            // A pure v2 torrent may omit both; but we are in the v1 branch
            // only because `pieces` is present, so missing both here is an
            // error.
            return Err(ParseError::new(ParseErrorKind::MissingField(
                "info.length or info.files",
            )));
        }
    };

    Ok(InfoV1 { pieces, files })
}

fn parse_file_v1<'a>(entry: &Value<'a>) -> Result<FileV1<'a>, ParseError> {
    let dict = entry.as_dict().ok_or_else(|| {
        ParseError::new(ParseErrorKind::WrongType {
            field: "info.files[*]",
            expected: "dict",
        })
    })?;
    let length = get_int(dict, "length")
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.files[*].length")))?;
    let length = u64_non_negative(length, "info.files[*].length")?;
    let path_list = dict
        .get(&b"path"[..])
        .and_then(Value::as_list)
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.files[*].path")))?;
    let mut path = Vec::with_capacity(path_list.len());
    for component in path_list {
        let bytes = component.as_borrowed_bytes().ok_or_else(|| {
            ParseError::new(ParseErrorKind::WrongType {
                field: "info.files[*].path[*]",
                expected: "byte string",
            })
        })?;
        validate_path_component(bytes)?;
        path.push(bytes);
    }
    Ok(FileV1 { length, path })
}

fn parse_info_v2<'a>(info: &Dict<'a>, meta_version: u64) -> Result<InfoV2<'a>, ParseError> {
    let tree_value = info
        .get(&b"file tree"[..])
        .ok_or_else(|| ParseError::new(ParseErrorKind::MissingField("info.file tree")))?;
    let tree_dict = tree_value.as_dict().ok_or_else(|| {
        ParseError::new(ParseErrorKind::WrongType {
            field: "info.file tree",
            expected: "dict",
        })
    })?;
    let file_tree = parse_file_tree_dir(tree_dict)?;
    Ok(InfoV2 {
        meta_version,
        file_tree,
    })
}

fn parse_file_tree_dir<'a>(dict: &Dict<'a>) -> Result<FileTreeNode<'a>, ParseError> {
    // A BEP 52 file-tree node is either:
    //   - a file leaf, encoded as { "": { "length": N, ("pieces root": H)? } }
    //   - a directory, encoded as a dict of named children (same shape)
    if let Some(leaf_value) = dict.get(&b""[..]) {
        // File leaf.
        let leaf_dict = leaf_value
            .as_dict()
            .ok_or_else(|| ParseError::new(ParseErrorKind::MalformedFileTree))?;
        let length = get_int(leaf_dict, "length").ok_or_else(|| {
            ParseError::new(ParseErrorKind::MissingField("file tree leaf.length"))
        })?;
        let length = u64_non_negative(length, "file tree leaf.length")?;
        let pieces_root = match leaf_dict.get(&b"pieces root"[..]) {
            Some(v) => {
                let bytes = v.as_borrowed_bytes().ok_or_else(|| {
                    ParseError::new(ParseErrorKind::WrongType {
                        field: "file tree leaf.pieces root",
                        expected: "byte string",
                    })
                })?;
                if bytes.len() != 32 {
                    return Err(ParseError::new(ParseErrorKind::InvalidPiecesRoot(
                        bytes.len(),
                    )));
                }
                let mut root = [0_u8; 32];
                root.copy_from_slice(bytes);
                Some(root)
            }
            None => None,
        };
        // A leaf dict must contain nothing except the empty key — any other
        // key at the same level signals a malformed tree.
        if dict.len() != 1 {
            return Err(ParseError::new(ParseErrorKind::MalformedFileTree));
        }
        return Ok(FileTreeNode::File {
            length,
            pieces_root,
        });
    }

    // Directory: every entry names a child.
    let mut children = BTreeMap::new();
    for (name, child_value) in dict {
        let Cow::Borrowed(name_slice) = name else {
            return Err(ParseError::new(ParseErrorKind::MalformedFileTree));
        };
        validate_path_component(name_slice)?;
        let child_dict = child_value
            .as_dict()
            .ok_or_else(|| ParseError::new(ParseErrorKind::MalformedFileTree))?;
        let child = parse_file_tree_dir(child_dict)?;
        children.insert(*name_slice, child);
    }
    Ok(FileTreeNode::Dir(children))
}

fn parse_announce_list<'a>(root: &Dict<'a>) -> Result<Option<Vec<Vec<&'a [u8]>>>, ParseError> {
    let Some(value) = root.get(&b"announce-list"[..]) else {
        return Ok(None);
    };
    let outer = value.as_list().ok_or_else(|| {
        ParseError::new(ParseErrorKind::WrongType {
            field: "announce-list",
            expected: "list",
        })
    })?;
    let mut tiers = Vec::with_capacity(outer.len());
    for tier in outer {
        let inner = tier.as_list().ok_or_else(|| {
            ParseError::new(ParseErrorKind::WrongType {
                field: "announce-list[*]",
                expected: "list",
            })
        })?;
        let mut urls = Vec::with_capacity(inner.len());
        for url in inner {
            let bytes = url.as_borrowed_bytes().ok_or_else(|| {
                ParseError::new(ParseErrorKind::WrongType {
                    field: "announce-list[*][*]",
                    expected: "byte string",
                })
            })?;
            urls.push(bytes);
        }
        tiers.push(urls);
    }
    Ok(Some(tiers))
}

fn validate_path_component(c: &[u8]) -> Result<(), ParseError> {
    if c.is_empty() || c.contains(&b'/') || c.contains(&0) {
        return Err(ParseError::new(ParseErrorKind::InvalidPathComponent));
    }
    Ok(())
}

fn u64_non_negative(v: i64, field: &'static str) -> Result<u64, ParseError> {
    u64::try_from(v).map_err(|_| {
        ParseError::new(ParseErrorKind::ValueOutOfRange {
            field,
            value: v.to_string(),
        })
    })
}

fn u64_positive_pow2(v: i64, field: &'static str) -> Result<u64, ParseError> {
    let u = u64_non_negative(v, field)?;
    if u == 0 || !u.is_power_of_two() {
        return Err(ParseError::new(ParseErrorKind::InvalidPieceLength(u)));
    }
    Ok(u)
}

fn get_bytes<'a>(dict: &Dict<'a>, key: &str) -> Option<&'a [u8]> {
    dict.get(key.as_bytes()).and_then(Value::as_borrowed_bytes)
}

fn get_int(dict: &Dict<'_>, key: &str) -> Option<i64> {
    dict.get(key.as_bytes()).and_then(Value::as_int)
}

#[allow(dead_code)]
fn _explicit_err(err: DecodeError) -> ParseError {
    err.into()
}
