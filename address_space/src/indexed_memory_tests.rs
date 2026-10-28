// Copyright (c) 2026 Conch contributors. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// See license/LICENSE in the repository for license details.

use super::*;
use crate::HostMemMapping;
use std::process::Command;
use vmm_sys_util::tempfile::TempFile;

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn index_value() -> Value {
    serde_json::json!({
        "version": 3, "pageSizeBytes": 4096, "memorySizeBytes": 12288,
        "backends": [{"id": "mem0", "ramOffset": 0, "sizeBytes": 12288, "extents": [
            {"start": 0, "end": 4096, "sourceLayer": digest('a'), "blobOffset": 4096},
            {"start": 4096, "end": 8192, "sourceLayer": "self", "blobOffset": 4096},
            {"start": 8192, "end": 12288, "sourceLayer": digest('a'), "blobOffset": 12288}
        ]}]
    })
}

fn bindings_value() -> Value {
    serde_json::json!({
        "version": 3, "indexLayer": digest('b'), "sources": {
            digest('a'): {"path": "/dev/ancestor", "sizeBytes": 20480, "payloadBytes": 12288},
            digest('b'): {"path": "/dev/current", "sizeBytes": 12288, "payloadBytes": 4096}
        }
    })
}

fn header(payload: u64, index_bytes: u64) -> [u8; 4096] {
    let index_offset = 4096 + payload;
    let blob_bytes = (index_offset + index_bytes + 4095) & !4095;
    let mut bytes = [0; 4096];
    bytes[..8].copy_from_slice(b"CNCHRAM3");
    for (offset, value) in [(8, 3u32), (12, 4096), (16, 4096)] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (24, payload),
        (32, index_offset),
        (40, index_bytes),
        (48, blob_bytes),
    ] {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[test]
fn v3_header_checks_all_fixed_fields_lengths_and_overflow() {
    let binding = LayerDeviceBinding {
        path: "/dev/test".into(),
        size_bytes: 16384,
        payload_bytes: 8192,
    };
    let valid = header(8192, 100);
    assert_eq!(validate_blob_header(&valid, &binding).unwrap(), 12288);
    for offset in [0, 7, 8, 12, 16, 20, 24, 32, 48, 56, 4095] {
        let mut invalid = valid;
        invalid[offset] ^= 1;
        assert!(
            validate_blob_header(&invalid, &binding).is_err(),
            "offset {offset}"
        );
    }
    for (offset, value) in [
        (24, u64::MAX),
        (32, u64::MAX),
        (40, u64::MAX),
        (40, 0),
        (40, 4097),
        (48, 12288),
        (48, 20480),
    ] {
        let mut invalid = valid;
        invalid[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        assert!(validate_blob_header(&invalid, &binding).is_err());
    }
    let wrong_payload = LayerDeviceBinding {
        payload_bytes: 4096,
        ..binding
    };
    assert!(validate_blob_header(&valid, &wrong_payload).is_err());
}

#[test]
fn v3_payload_ranges_reject_metadata_padding_alignment_and_overflow() {
    assert!(validate_payload_range(4096, 8192, 12288).is_ok());
    assert!(validate_payload_range(8192, 4096, 12288).is_ok());
    for (offset, length) in [
        (0, 4096),
        (12288, 4096),
        (8192, 8192),
        (4097, 4096),
        (4096, 4095),
        (4096, 0),
        (u64::MAX - 4095, 8192),
    ] {
        assert!(validate_payload_range(offset, length, 12288).is_err());
    }
}

#[test]
fn v3_decoding_requires_exact_versioned_fields_and_integer_offsets() {
    let valid = index_value();
    let index = decode_index(&serde_json::to_vec(&valid).unwrap()).unwrap();
    assert!(matches!(
        index.backends[0].extents[0].source,
        ExtentSource::Blob { blob_offset: 4096 }
    ));
    for version in [
        serde_json::json!(1),
        serde_json::json!(2),
        serde_json::json!(3.0),
        Value::Null,
    ] {
        let mut value = valid.clone();
        value["version"] = version;
        assert!(decode_index(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for offset in [
        Value::Null,
        serde_json::json!(4096.0),
        serde_json::json!(-1),
        serde_json::json!("4096"),
    ] {
        let mut value = valid.clone();
        value["backends"][0]["extents"][0]["blobOffset"] = offset;
        assert!(decode_index(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut missing = valid.clone();
    missing["backends"][0]["extents"][0]
        .as_object_mut()
        .unwrap()
        .remove("blobOffset");
    assert!(decode_index(&serde_json::to_vec(&missing).unwrap()).is_err());
    let duplicate = serde_json::to_string(&valid).unwrap().replace(
        "\"blobOffset\":4096",
        "\"blobOffset\":4096,\"blobOffset\":8192",
    );
    assert!(decode_index(duplicate.as_bytes()).is_err());
    for field in ["version", "indexLayer", "sources"] {
        let mut bindings = bindings_value();
        bindings.as_object_mut().unwrap().remove(field);
        assert!(decode::<SourceBindings>(&serde_json::to_vec(&bindings).unwrap()).is_err());
    }
    for field in ["path", "sizeBytes", "payloadBytes"] {
        let mut bindings = bindings_value();
        bindings["sources"][digest('a')][field] = Value::Null;
        assert!(decode::<SourceBindings>(&serde_json::to_vec(&bindings).unwrap()).is_err());
    }
}

#[test]
fn v3_bindings_resolve_self_require_exact_sources_and_allow_metadata_only_latest() {
    let mut index = decode_index(&serde_json::to_vec(&index_value()).unwrap()).unwrap();
    let mut bindings = bindings_value();
    let check = |index: &MemoryIndex, value: &Value| {
        validate_source_bindings(index, &decode(&serde_json::to_vec(value).unwrap()).unwrap())
    };
    assert!(check(&index, &bindings).is_ok());
    bindings["indexLayer"] = serde_json::json!("self");
    assert!(check(&index, &bindings).is_err());
    bindings = bindings_value();
    bindings["sources"]
        .as_object_mut()
        .unwrap()
        .remove(&digest('b'));
    assert!(check(&index, &bindings).is_err());
    // A new index with no self references needs no device for its own layer.
    index.backends[0].extents[1].source_layer = digest('a');
    assert!(check(&index, &bindings).is_ok());
    assert!(check(&index, &bindings_value()).is_err());
    bindings["sources"]["self"] = bindings["sources"][digest('a')].clone();
    assert!(check(&index, &bindings).is_err());
    let v2 = SourceBindings::V2(V2DirectoryBindings {
        version: 1,
        sources: HashMap::new(),
    });
    assert!(validate_source_bindings(&index, &v2).is_err());
    index.version = 1;
    let v3: SourceBindings = decode(&serde_json::to_vec(&bindings_value()).unwrap()).unwrap();
    assert!(validate_source_bindings(&index, &v3).is_err());
}

#[test]
fn v3_backend_rejects_gaps_overlaps_unaligned_and_overflowing_extents() {
    let index = decode_index(&serde_json::to_vec(&index_value()).unwrap()).unwrap();
    let backend = &index.backends[0];
    validate_backend(backend).unwrap();
    for start in [0, 8192, 4097] {
        let mut invalid = backend.clone();
        invalid.extents[1].start = start;
        assert!(validate_backend(&invalid).is_err());
    }
    for blob_offset in [0, 4097, 1u64 << 63, u64::MAX - 4095] {
        let mut invalid = backend.clone();
        invalid.extents[0].source = ExtentSource::Blob { blob_offset };
        assert!(validate_backend(&invalid).is_err());
    }
    let mut invalid = backend.clone();
    invalid.extents.pop();
    assert!(validate_backend(&invalid).is_err());
}

#[test]
fn v3_open_rejects_regular_files_instead_of_falling_back_to_v2() {
    let file = TempFile::new().unwrap();
    let mut bindings = bindings_value();
    for digest in [digest('a'), digest('b')] {
        bindings["sources"][digest]["path"] = serde_json::json!(file.as_path());
    }
    let SourceBindings::V3(mut bindings) = decode(&serde_json::to_vec(&bindings).unwrap()).unwrap()
    else {
        unreachable!()
    };
    let error = bindings.open_devices().unwrap_err().to_string();
    assert!(error.contains("not a block device"), "{error}");
}

struct LoopDevice {
    path: PathBuf,
    _backing: TempFile,
}

impl LoopDevice {
    fn new(payload: &[u8], index: &[u8]) -> Self {
        let file = TempFile::new().unwrap();
        let header = header(payload.len() as u64, index.len() as u64);
        let size = u64::from_le_bytes(header[48..56].try_into().unwrap()) as usize;
        let mut bytes = Vec::with_capacity(size);
        bytes.extend(header);
        bytes.extend(payload);
        bytes.extend(index);
        bytes.resize(size, 0);
        fs::write(file.as_path(), bytes).unwrap();
        let output = Command::new("losetup")
            .args(["--find", "--show", "--read-only"])
            .arg(file.as_path())
            .output()
            .expect("requires losetup and CAP_SYS_ADMIN");
        assert!(
            output.status.success(),
            "losetup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self {
            path: PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()),
            _backing: file,
        }
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let status = Command::new("losetup")
            .arg("--detach")
            .arg(&self.path)
            .status();
        assert!(
            status.is_ok_and(|s| s.success()),
            "could not detach owned loop device"
        );
    }
}

pub(crate) struct BlockFixture {
    pub input: IndexedMemoryInput,
    _ancestor: LoopDevice,
    _latest: LoopDevice,
}

impl BlockFixture {
    pub fn new() -> Self {
        let mut payload = vec![1; 4096];
        payload.extend(vec![2; 4096]);
        payload.extend(vec![3; 4096]);
        // The consumer must not reread or interpret an ancestor index.
        let ancestor = LoopDevice::new(&payload, b"ancestor index is not read");
        let index_bytes = serde_json::to_vec(&index_value()).unwrap();
        let latest = LoopDevice::new(&vec![0; 4096], &index_bytes);
        let index_file = TempFile::new().unwrap();
        fs::write(index_file.as_path(), &index_bytes).unwrap();
        let mut bindings = bindings_value();
        bindings["sources"][digest('a')]["path"] = serde_json::json!(ancestor.path);
        bindings["sources"][digest('b')]["path"] = serde_json::json!(latest.path);
        let source_file = TempFile::new().unwrap();
        fs::write(
            source_file.as_path(),
            serde_json::to_vec(&bindings).unwrap(),
        )
        .unwrap();
        let input = load_indexed_memory(index_file.as_path(), source_file.as_path()).unwrap();
        Self {
            input,
            _ancestor: ancestor,
            _latest: latest,
        }
    }

    pub fn mapping(&self) -> HostMemMapping {
        HostMemMapping::from_indexed_backend(&self.input.index.backends[0], &self.input.sources)
            .unwrap()
    }
}

#[test]
#[ignore = "requires losetup and CAP_SYS_ADMIN for owned read-only block devices"]
fn v3_block_mapping_preserves_offsets_cow_fd_identity_and_rolls_back_failure() {
    let mut fixture = BlockFixture::new();
    let input = &mut fixture.input;
    let layout = [IndexedRamLayout {
        id: "mem0".into(),
        ram_offset: 0,
        size_bytes: 12288,
    }];
    validate_indexed_memory(input, &layout, 12288).unwrap();
    // Changing the path after opening cannot redirect a validated source fd.
    let SourceBindings::V3(bindings) = &mut input.sources else {
        unreachable!()
    };
    bindings.sources.get_mut(&digest('a')).unwrap().path = "/dev/not-the-source".into();
    let first = fixture.mapping();
    let second = fixture.mapping();
    assert!(first.is_indexed());
    assert!(first.file_backend().is_none());
    assert!(!first.mem_shared());
    // SAFETY: both mappings retain all three pages for the duration of these accesses.
    unsafe {
        let a = std::slice::from_raw_parts_mut(first.host_address() as *mut u8, 12288);
        let b = std::slice::from_raw_parts(second.host_address() as *const u8, 12288);
        assert!(a[..4096].iter().all(|&v| v == 1));
        assert!(a[4096..8192].iter().all(|&v| v == 0));
        assert!(a[8192..].iter().all(|&v| v == 3));
        a[0] = 99;
        a[4096] = 88;
        assert_eq!(b[0], 1);
        assert_eq!(b[4096], 0);
    }
    drop(first);
    drop(second);
    // A metadata-only latest layer is represented by its raw external index;
    // loading must require only the ancestor device referenced by that index.
    let mut inherited = index_value();
    inherited["backends"][0]["extents"][1]["sourceLayer"] = serde_json::json!(digest('a'));
    inherited["backends"][0]["extents"][1]["blobOffset"] = serde_json::json!(8192);
    let mut inherited_bindings = bindings_value();
    inherited_bindings["sources"]
        .as_object_mut()
        .unwrap()
        .remove(&digest('b'));
    inherited_bindings["sources"][digest('a')]["path"] = serde_json::json!(fixture._ancestor.path);
    let raw_index = TempFile::new().unwrap();
    let sources = TempFile::new().unwrap();
    fs::write(raw_index.as_path(), serde_json::to_vec(&inherited).unwrap()).unwrap();
    fs::write(
        sources.as_path(),
        serde_json::to_vec(&inherited_bindings).unwrap(),
    )
    .unwrap();
    let inherited = load_indexed_memory(raw_index.as_path(), sources.as_path()).unwrap();
    validate_indexed_memory(&inherited, &layout, 12288).unwrap();
    let inherited_mapping =
        HostMemMapping::from_indexed_backend(&inherited.index.backends[0], &inherited.sources)
            .unwrap();
    // SAFETY: the mapping owns this byte of its second page.
    assert_eq!(
        unsafe { *((inherited_mapping.host_address() + 4096) as *const u8) },
        2
    );
    drop(inherited_mapping);

    // Mapping the first fragment succeeds; the second references metadata.
    fixture.input.index.backends[0].extents[1].source = ExtentSource::Blob { blob_offset: 8192 };
    assert!(HostMemMapping::from_indexed_backend(
        &fixture.input.index.backends[0],
        &fixture.input.sources
    )
    .is_err());
    let maps = fs::read_to_string("/proc/self/maps").unwrap();
    for device in [&fixture._ancestor, &fixture._latest] {
        assert!(
            !maps.contains(device.path.to_str().unwrap()),
            "partial block mapping survived rollback"
        );
    }
    let SourceBindings::V3(bindings) = &mut fixture.input.sources else {
        unreachable!()
    };
    bindings.sources.get_mut(&digest('a')).unwrap().path = fixture._ancestor.path.clone();
    bindings.sources.get_mut(&digest('a')).unwrap().size_bytes += 4096;
    assert!(bindings
        .open_devices()
        .unwrap_err()
        .to_string()
        .contains("capacity differs"));
    bindings.sources.get_mut(&digest('a')).unwrap().size_bytes -= 4096;
    bindings.sources.get_mut(&digest('b')).unwrap().path = fixture._ancestor.path.clone();
    bindings.sources.get_mut(&digest('b')).unwrap().size_bytes = 20480;
    bindings
        .sources
        .get_mut(&digest('b'))
        .unwrap()
        .payload_bytes = 12288;
    assert!(bindings
        .open_devices()
        .unwrap_err()
        .to_string()
        .contains("alias the same block"));
}
