// Copyright (c) 2026 Conch contributors. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// See license/LICENSE in the repository for license details.

//! Conch v2 cumulative RAM indexes and private file-source bindings.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Number, Value};

pub const INDEX_PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryIndex {
    pub version: u32,
    pub page_size_bytes: u64,
    pub memory_size_bytes: u64,
    pub backends: Vec<MemoryBackendIndex>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryBackendIndex {
    pub id: String,
    pub ram_offset: u64,
    pub size_bytes: u64,
    pub extents: Vec<MemoryExtent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryExtent {
    pub start: u64,
    pub end: u64,
    pub source_layer: String,
    pub source_file: String,
    pub file_offset: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceBindings {
    pub version: u32,
    pub sources: HashMap<String, PathBuf>,
}

#[derive(Debug)]
pub struct IndexedMemoryInput {
    pub index: MemoryIndex,
    pub sources: SourceBindings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRamLayout {
    pub id: String,
    pub ram_offset: u64,
    pub size_bytes: u64,
}

// Deserialize every object, including extension objects, before serde can
// silently discard duplicate keys. Derived structs then enforce integer types,
// exact field names, and presence/non-nullness of required fields.
struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("unique-key JSON")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Number::from_f64(v)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(v.into())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(v)) = seq.next_element()? {
                    values.push(v);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((k, UniqueValue(v))) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(k.clone(), v).is_some() {
                        return Err(de::Error::custom(format!("duplicate JSON key {k}")));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

fn decode<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T> {
    let value: UniqueValue = serde_json::from_slice(data)?;
    Ok(serde_json::from_value(value.0)?)
}

pub fn valid_backend_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id != "index.json"
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn valid_source(source: &str) -> bool {
    source == "self"
        || source.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

fn decimal(value: &str) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("non-canonical RAM file offset {value:?}");
    }
    Ok(value.parse()?)
}

pub fn extent_file_range(
    backend: &MemoryBackendIndex,
    extent: &MemoryExtent,
) -> Result<(u64, u64)> {
    let prefix = format!("memory/{}/", backend.id);
    let name = extent
        .source_file
        .strip_prefix(&prefix)
        .context("RAM source belongs to a different backend")?;
    let (a, b) = name
        .split_once('_')
        .context("RAM range filename lacks separator")?;
    let a = decimal(a)?;
    let b = decimal(b)?;
    if a >= b
        || b > backend.size_bytes
        || a % INDEX_PAGE_SIZE != 0
        || b % INDEX_PAGE_SIZE != 0
        || a > extent.start
        || b < extent.end
        || extent.file_offset != extent.start - a
    {
        bail!("invalid RAM file range or offset: {}", extent.source_file);
    }
    Ok((a, b))
}

/// Open beneath a binding using directory file descriptors. No path component
/// can follow a symlink, and a rename cannot redirect a previously opened parent.
pub fn open_source_file(sources: &SourceBindings, source: &str, relative: &str) -> Result<File> {
    if !valid_source(source) {
        bail!("invalid RAM source identity {source}");
    }
    let root = sources
        .sources
        .get(source)
        .with_context(|| format!("unbound RAM source {source}"))?;
    if !root.is_absolute() {
        bail!("RAM source root is not absolute");
    }
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let parts: Vec<_> = relative.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || *part == "." || *part == ".." {
            bail!("unsafe RAM source path {relative}");
        }
        let name = CString::new(*part)?;
        let directory = i + 1 < parts.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: current owns the directory fd and name is NUL-terminated.
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open RAM source {source}:{relative}"));
        }
        // SAFETY: openat returned a new owned descriptor.
        current = unsafe { File::from_raw_fd(fd) };
    }
    if !current.metadata()?.is_file() {
        bail!("RAM source is not a regular file: {relative}");
    }
    Ok(current)
}

pub fn load_indexed_memory(index_path: &Path, sources_path: &Path) -> Result<IndexedMemoryInput> {
    let sources: SourceBindings = decode(&fs::read(sources_path)?)?;
    if sources.version != 1 || !sources.sources.contains_key("self") {
        bail!("invalid indexed RAM source bindings version or self");
    }
    for (key, path) in &sources.sources {
        if !valid_source(key) || !path.is_absolute() || !fs::symlink_metadata(path)?.is_dir() {
            bail!("invalid RAM binding {key}");
        }
    }
    let expected = sources.sources["self"].join("memory/index.json");
    if index_path != expected {
        bail!("RAM index path must be self/memory/index.json");
    }
    let mut index_file = open_source_file(&sources, "self", "memory/index.json")?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut index_file, &mut data)?;
    let index = decode(&data)?;
    Ok(IndexedMemoryInput { index, sources })
}

pub fn validate_backend(backend: &MemoryBackendIndex) -> Result<()> {
    if !valid_backend_id(&backend.id)
        || backend.size_bytes == 0
        || backend.size_bytes % INDEX_PAGE_SIZE != 0
        || backend.size_bytes > isize::MAX as u64
        || backend.ram_offset % INDEX_PAGE_SIZE != 0
    {
        bail!("invalid indexed RAM backend {}", backend.id);
    }
    let mut end = 0;
    for extent in &backend.extents {
        if extent.start != end
            || extent.end <= extent.start
            || extent.end > backend.size_bytes
            || extent.start % INDEX_PAGE_SIZE != 0
            || extent.end % INDEX_PAGE_SIZE != 0
            || !valid_source(&extent.source_layer)
        {
            bail!(
                "incomplete or invalid RAM extent {}:[{}, {})",
                backend.id,
                extent.start,
                extent.end
            );
        }
        extent_file_range(backend, extent)?;
        end = extent.end;
    }
    if end != backend.size_bytes {
        bail!("incomplete RAM backend {}", backend.id);
    }
    Ok(())
}

pub fn check_vma_budget(required: usize, current: usize, maximum: usize) -> Result<()> {
    // Each fixed replacement can temporarily split the reservation; retain room
    // for thread stacks, KVM, and device mappings created after RAM setup.
    let planned = required
        .checked_mul(2)
        .and_then(|n| n.checked_add(current))
        .and_then(|n| n.checked_add(128))
        .context("VMA count overflow")?;
    if planned > maximum {
        bail!("indexed RAM requires VMA budget {planned}, vm.max_map_count is {maximum}");
    }
    Ok(())
}

pub fn validate_indexed_memory(
    input: &IndexedMemoryInput,
    expected: &[IndexedRamLayout],
    expected_size: u64,
) -> Result<()> {
    let index = &input.index;
    if util::unix::host_page_size() != INDEX_PAGE_SIZE
        || index.version != 1
        || index.page_size_bytes != INDEX_PAGE_SIZE
        || expected_size == 0
        || expected_size > isize::MAX as u64
        || index.memory_size_bytes != expected_size
        || index.backends.is_empty()
        || index.backends.len() != expected.len()
    {
        bail!("unsupported indexed RAM header or native layout");
    }
    if input.sources.version != 1 || !input.sources.sources.contains_key("self") {
        bail!("invalid RAM source bindings");
    }
    let mut offset = 0;
    let mut ids = HashSet::new();
    let mut count = 0usize;
    for (backend, native) in index.backends.iter().zip(expected) {
        validate_backend(backend)?;
        if !ids.insert(&backend.id)
            || backend.id != native.id
            || backend.ram_offset != offset
            || backend.ram_offset != native.ram_offset
            || backend.size_bytes != native.size_bytes
        {
            bail!("RAM backend {} differs from native layout", backend.id);
        }
        offset = offset
            .checked_add(backend.size_bytes)
            .context("RAM size overflow")?;
        count = count
            .checked_add(backend.extents.len())
            .context("RAM extent count overflow")?;
        for extent in &backend.extents {
            let (a, b) = extent_file_range(backend, extent)?;
            let file = open_source_file(&input.sources, &extent.source_layer, &extent.source_file)?;
            if file.metadata()?.len() != b - a {
                bail!("RAM source size mismatch {}", extent.source_file);
            }
        }
    }
    if offset != expected_size {
        bail!("RAM backends do not cover total memory");
    }
    let maximum: usize = fs::read_to_string("/proc/sys/vm/max_map_count")?
        .trim()
        .parse()?;
    let current = fs::read_to_string("/proc/self/maps")?.lines().count();
    check_vma_budget(
        count
            .checked_add(index.backends.len())
            .context("VMA count overflow")?,
        current,
        maximum,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostMemMapping;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "conch-indexed-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn input(&self) -> IndexedMemoryInput {
            let ancestor = self.0.join("ancestor");
            let current = self.0.join("current");
            fs::create_dir_all(ancestor.join("memory/mem0")).unwrap();
            fs::create_dir_all(current.join("memory/mem0")).unwrap();
            let mut bytes = vec![1; 4096];
            bytes.extend(vec![2; 4096]);
            bytes.extend(vec![3; 4096]);
            fs::write(ancestor.join("memory/mem0/0_12288"), bytes).unwrap();
            fs::write(current.join("memory/mem0/4096_8192"), vec![0; 4096]).unwrap();
            let digest = format!("sha256:{}", "a".repeat(64));
            let index = serde_json::json!({"version":1,"pageSizeBytes":4096,"memorySizeBytes":12288,"backends":[{"id":"mem0","ramOffset":0,"sizeBytes":12288,"extents":[
                {"start":0,"end":4096,"sourceLayer":digest,"sourceFile":"memory/mem0/0_12288","fileOffset":0},
                {"start":4096,"end":8192,"sourceLayer":"self","sourceFile":"memory/mem0/4096_8192","fileOffset":0},
                {"start":8192,"end":12288,"sourceLayer":digest,"sourceFile":"memory/mem0/0_12288","fileOffset":8192}
            ]}]});
            fs::write(
                current.join("memory/index.json"),
                serde_json::to_vec(&index).unwrap(),
            )
            .unwrap();
            let bindings =
                serde_json::json!({"version":1,"sources":{"self":current,digest:ancestor}});
            let bindings_path = self.0.join("sources.json");
            fs::write(&bindings_path, serde_json::to_vec(&bindings).unwrap()).unwrap();
            load_indexed_memory(&current.join("memory/index.json"), &bindings_path).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn layout() -> Vec<IndexedRamLayout> {
        vec![IndexedRamLayout {
            id: "mem0".into(),
            ram_offset: 0,
            size_bytes: 12288,
        }]
    }

    #[test]
    fn indexed_mapping_keeps_partial_offsets_zero_overwrite_and_private_cow() {
        let fixture = Fixture::new();
        let input = fixture.input();
        validate_indexed_memory(&input, &layout(), 12288).unwrap();
        let first =
            HostMemMapping::from_indexed_backend(&input.index.backends[0], &input.sources).unwrap();
        let second =
            HostMemMapping::from_indexed_backend(&input.index.backends[0], &input.sources).unwrap();
        assert!(first.is_indexed());
        assert!(first.file_backend().is_none());
        assert!(!first.mem_shared());
        // SAFETY: both owners retain the complete, validated 12288-byte mapping.
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
        assert_eq!(
            fs::read(fixture.0.join("ancestor/memory/mem0/0_12288")).unwrap()[0],
            1
        );
        assert!(fs::read(fixture.0.join("current/memory/mem0/4096_8192"))
            .unwrap()
            .iter()
            .all(|&v| v == 0));
    }

    #[test]
    fn indexed_mapping_failure_unmaps_completed_fragments() {
        let fixture = Fixture::new();
        let input = fixture.input();
        fs::remove_file(fixture.0.join("current/memory/mem0/4096_8192")).unwrap();
        assert!(
            HostMemMapping::from_indexed_backend(&input.index.backends[0], &input.sources).is_err()
        );
        let maps = fs::read_to_string("/proc/self/maps").unwrap();
        assert!(
            !maps.contains(fixture.0.to_str().unwrap()),
            "partial mmap survived failed construction"
        );
    }

    #[test]
    fn indexed_validation_rejects_bad_layout_and_file_sources() {
        let fixture = Fixture::new();
        let mut input = fixture.input();
        input.index.backends[0].extents[2].file_offset = 0;
        assert!(validate_indexed_memory(&input, &layout(), 12288).is_err());
        input.index.backends[0].extents[2].file_offset = 8192;
        input.index.backends[0].id = "index.json".into();
        assert!(validate_indexed_memory(&input, &layout(), 12288).is_err());
        input.index.backends[0].id = "mem0".into();
        let path = fixture.0.join("current/memory/mem0/4096_8192");
        fs::remove_file(&path).unwrap();
        symlink(fixture.0.join("ancestor/memory/mem0/0_12288"), &path).unwrap();
        assert!(validate_indexed_memory(&input, &layout(), 12288).is_err());
        assert!(
            open_source_file(&input.sources, "self", "../ancestor/memory/mem0/0_12288").is_err()
        );
    }

    #[test]
    fn indexed_source_rejects_fifo_without_waiting_for_a_writer() {
        let fixture = Fixture::new();
        let input = fixture.input();
        let path = fixture.0.join("current/memory/mem0/4096_8192");
        fs::remove_file(&path).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path within the private fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open_source_file(&input.sources, "self", "memory/mem0/4096_8192").is_err());
    }

    #[test]
    fn indexed_json_rejects_duplicates_missing_fields_and_float_offsets() {
        for data in [
            r#"{"version":1,"version":1,"sources":{}}"#,
            r#"{"version":1,"sources":{"self":"/a","self":"/b"}}"#,
        ] {
            assert!(decode::<SourceBindings>(data.as_bytes()).is_err());
        }
        assert!(
            decode::<MemoryBackendIndex>(br#"{"id":"mem0","sizeBytes":4096,"extents":[]}"#)
                .is_err()
        );
        assert!(decode::<MemoryBackendIndex>(
            br#"{"id":"mem0","ramOffset":0.0,"sizeBytes":4096,"extents":[]}"#
        )
        .is_err());
        assert!(decode::<MemoryBackendIndex>(
            br#"{"id":"mem0","ramOffset":null,"sizeBytes":4096,"extents":[]}"#
        )
        .is_err());
        assert!(check_vma_budget(10, 100, 256).is_ok());
        assert!(check_vma_budget(10, 100, 200).is_err());
        assert!(check_vma_budget(usize::MAX, 1, usize::MAX).is_err());
    }
    #[test]
    fn indexed_two_backends_keep_independent_offset_spaces() {
        let fixture = Fixture::new();
        let root = &fixture.0;
        let mut backends = Vec::new();
        let mut expected = Vec::new();
        for (i, id) in ["mem0", "mem1"].iter().enumerate() {
            fs::create_dir_all(root.join(format!("memory/{id}"))).unwrap();
            let source = format!("memory/{id}/0_4096");
            fs::write(root.join(&source), vec![i as u8 + 1; 4096]).unwrap();
            backends.push(MemoryBackendIndex {
                id: (*id).into(),
                ram_offset: i as u64 * 4096,
                size_bytes: 4096,
                extents: vec![MemoryExtent {
                    start: 0,
                    end: 4096,
                    source_layer: "self".into(),
                    source_file: source,
                    file_offset: 0,
                }],
            });
            expected.push(IndexedRamLayout {
                id: (*id).into(),
                ram_offset: i as u64 * 4096,
                size_bytes: 4096,
            });
        }
        let mut sources = HashMap::new();
        sources.insert("self".into(), root.clone());
        let input = IndexedMemoryInput {
            index: MemoryIndex {
                version: 1,
                page_size_bytes: 4096,
                memory_size_bytes: 8192,
                backends,
            },
            sources: SourceBindings {
                version: 1,
                sources,
            },
        };
        validate_indexed_memory(&input, &expected, 8192).unwrap();
        for (i, backend) in input.index.backends.iter().enumerate() {
            let mapping = HostMemMapping::from_indexed_backend(backend, &input.sources).unwrap();
            // SAFETY: a live owner retains the mapped backend page.
            assert_eq!(
                unsafe { *(mapping.host_address() as *const u8) },
                i as u8 + 1
            );
        }
        expected[1].ram_offset = 0;
        assert!(validate_indexed_memory(&input, &expected, 8192).is_err());
    }
}
