// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use machine_manager::config::SnapshotMemoryMode;
use serde::{Deserialize, Serialize};

use crate::uffd::{store_uffd_backend, UffdMemoryBackend};
use crate::{AddressAttr, AddressSpace, FileBackend, GuestAddress, HostMemMapping, Region};
use migration::{
    DeviceStateDesc, MemBlock, MigrationError, MigrationHook, RestoreMode, StateTransfer,
};
use migration_derive::DescSerde;
use util::aio::ALIGNMENT_SIZE;
use util::num_ops::round_up;
use util::unix::host_page_size;

// -------------------------------------------
// |    MIGRATION HEADER (4096 align)        |
// -------------------------------------------
// |    Address Space Header (4096 align)    |
// -------------------------------------------
// |    Address Space Region Data            |
// -------------------------------------------
// |    RamList Region Header                |
// -------------------------------------------
// |    RamList Region Data                  |
// -------------------------------------------

#[derive(Clone, DescSerde, Serialize, Deserialize)]
#[desc_version(current_version = "0.1.0")]
pub struct RamRegionState {
    // Region name.
    pub name: String,
    // Offset has different meanings in different region.
    // 1) Address space RAM region: offset in the serialized RAM payload (FULL)
    //    or in machine RAM / the flat external export (EXTERNAL).
    // 2) Ramlist RAM region: representing the offset from the first ramlist ram region.
    pub offset: u64,
    // Region size.
    pub size: u64,
}

#[derive(Clone, DescSerde, Serialize, Deserialize)]
#[desc_version(current_version = "0.1.0")]
pub struct AliasRegionState {
    // Region name.
    pub name: String,
    // Alias offset.
    pub alias_offset: u64,
    // Region offset.
    pub offset: u64,
    // Region size.
    pub size: u64,
}

#[derive(Clone, Default, DescSerde, Serialize, Deserialize)]
#[desc_version(current_version = "0.1.0")]
pub struct AddressSpaceState {
    // Total size of address space memory region.
    total_region_size: u64,
    ram_region_state: Vec<RamRegionState>,
    alias_region_state: Vec<AliasRegionState>,
}

fn build_mapped_ram_regions(
    machine_ram: &Region,
    ram_states: &[RamRegionState],
    memfile_arc: &Arc<File>,
    first_region_offset: u64,
) -> Result<()> {
    for ram_state in ram_states.iter() {
        let offset = ram_state
            .offset
            .checked_add(first_region_offset)
            .with_context(|| {
                format!(
                    "Mapped ram offset overflow: {} + {}",
                    ram_state.offset, first_region_offset
                )
            })?;
        let file_backend = FileBackend {
            file: memfile_arc.clone(),
            offset,
            page_size: host_page_size(),
        };
        let host_mmap = Arc::new(
            HostMemMapping::new(
                GuestAddress(0),
                None,
                ram_state.size,
                Some(file_backend),
                false,
                false,
                false,
            )
            .map_err(|e| MigrationError::RestoreVmMemoryErr(e.to_string()))?,
        );

        machine_ram
            .add_subregion_not_update(
                Region::init_ram_region(host_mmap, &ram_state.name),
                ram_state.offset,
            )
            .map_err(|e| MigrationError::RestoreVmMemoryErr(e.to_string()))?;
    }

    Ok(())
}

fn build_alias_regions(
    root: &Region,
    machine_ram: &Arc<Region>,
    alias_states: &[AliasRegionState],
) -> Result<()> {
    for alias_state in alias_states.iter() {
        let ram = Region::init_alias_region(
            machine_ram.clone(),
            alias_state.alias_offset,
            alias_state.size,
            &alias_state.name,
        );
        root.add_subregion(ram, alias_state.offset)?;
    }

    Ok(())
}

fn validate_ram_regions(
    snapshot_machine_ram: &Region,
    expected_ram_states: &[RamRegionState],
) -> Result<()> {
    let snapshot_regions = snapshot_machine_ram.subregions();
    if snapshot_regions.len() != expected_ram_states.len() {
        bail!(
            "Mapped ram region count mismatch: snapshot {}, expected {}",
            snapshot_regions.len(),
            expected_ram_states.len()
        );
    }

    for expected_ram_state in expected_ram_states.iter() {
        let snapshot_region = snapshot_regions
            .iter()
            .find(|region| region.name == expected_ram_state.name)
            .ok_or_else(|| {
                anyhow!(
                    "Expected ram region {} is missing from snapshot layout",
                    expected_ram_state.name
                )
            })?;

        if snapshot_region.size() != expected_ram_state.size {
            bail!(
                "Mapped ram region {} size mismatch: snapshot {}, expected {}",
                expected_ram_state.name,
                snapshot_region.size(),
                expected_ram_state.size
            );
        }
        if snapshot_region.offset().raw_value() != expected_ram_state.offset {
            bail!(
                "Mapped ram region {} offset mismatch: snapshot {}, expected {}",
                expected_ram_state.name,
                snapshot_region.offset().raw_value(),
                expected_ram_state.offset
            );
        }
    }

    Ok(())
}

fn validate_alias_regions(
    snapshot_root: &Region,
    expected_alias_states: &[AliasRegionState],
) -> Result<()> {
    let snapshot_root_subregions = snapshot_root.subregions();
    let snapshot_alias_count = snapshot_root_subregions
        .iter()
        .filter(|region| region.alias_name().is_some())
        .count();
    if snapshot_alias_count != expected_alias_states.len() {
        bail!(
            "Mapped alias region count mismatch: snapshot {}, expected {}",
            snapshot_alias_count,
            expected_alias_states.len()
        );
    }

    for expected_alias_state in expected_alias_states.iter() {
        let snapshot_region = snapshot_root_subregions
            .iter()
            .find(|region| region.name == expected_alias_state.name)
            .ok_or_else(|| {
                anyhow!(
                    "Expected alias region {} is missing from snapshot layout",
                    expected_alias_state.name
                )
            })?;
        if snapshot_region.alias_name().is_none() {
            bail!(
                "Snapshot region {} is no longer an alias region",
                expected_alias_state.name
            );
        }
        if snapshot_region.alias_offset() != expected_alias_state.alias_offset {
            bail!(
                "Mapped alias region {} alias offset mismatch: snapshot {}, expected {}",
                expected_alias_state.name,
                snapshot_region.alias_offset(),
                expected_alias_state.alias_offset
            );
        }
        if snapshot_region.offset().raw_value() != expected_alias_state.offset {
            bail!(
                "Mapped alias region {} offset mismatch: snapshot {}, expected {}",
                expected_alias_state.name,
                snapshot_region.offset().raw_value(),
                expected_alias_state.offset
            );
        }
        if snapshot_region.size() != expected_alias_state.size {
            bail!(
                "Mapped alias region {} size mismatch: snapshot {}, expected {}",
                expected_alias_state.name,
                snapshot_region.size(),
                expected_alias_state.size
            );
        }
    }

    Ok(())
}

fn read_address_space_state(memory: &mut File) -> Result<AddressSpaceState> {
    let data_slice = read_state_slice(memory)
        .with_context(|| "Failed to read state slice while restoring state")?;
    serde_json::from_slice(&data_slice).with_context(|| MigrationError::FromBytesError("MEMORY"))
}

impl AddressSpace {
    /// Build RAM mappings from the payload in a full native snapshot.
    pub fn build_mapped_ram_from_snapshot(&self, memory: &mut File) -> Result<()> {
        let address_space_state = read_address_space_state(memory)?;
        let first_region_offset = memory.stream_position()?;
        let memfile_arc = Arc::new(memory.try_clone().context("Failed to clone memory file")?);

        let machine_ram = self
            .get_machine_ram()
            .with_context(|| "This address space does not support migration.")?;
        if !machine_ram.subregions().is_empty() {
            bail!("Mapped ram layout has already been initialized");
        }

        build_mapped_ram_regions(
            machine_ram,
            &address_space_state.ram_region_state,
            &memfile_arc,
            first_region_offset,
        )?;
        build_alias_regions(
            self.root(),
            machine_ram,
            &address_space_state.alias_region_state,
        )?;

        Ok(())
    }

    /// Match RAM created from the configured backends against external metadata.
    pub fn validate_external_ram_from_snapshot(&self, memory: &mut File) -> Result<()> {
        self.validate_external_ram_layout(&read_address_space_state(memory)?)
    }

    fn validate_external_ram_layout(&self, state: &AddressSpaceState) -> Result<()> {
        let mut names = HashSet::new();
        let mut total_size = 0_u64;
        for region in &state.ram_region_state {
            if !names.insert(&region.name) {
                bail!("Duplicate RAM region {} in external snapshot", region.name);
            }
            total_size = total_size
                .checked_add(region.size)
                .context("External snapshot RAM size overflow")?;
        }
        if total_size == 0 || total_size != state.total_region_size {
            bail!("External snapshot total RAM size does not match its regions");
        }
        names.clear();
        for alias in &state.alias_region_state {
            if !names.insert(&alias.name) {
                bail!("Duplicate RAM alias {} in external snapshot", alias.name);
            }
        }
        self.validate_mapped_ram_from_snapshot(&state.ram_region_state, &state.alias_region_state)
    }

    pub fn validate_mapped_ram_from_snapshot(
        &self,
        expected_ram_states: &[RamRegionState],
        expected_alias_states: &[AliasRegionState],
    ) -> Result<()> {
        let machine_ram = self
            .get_machine_ram()
            .with_context(|| "This address space does not support migration.")?;
        if machine_ram.subregions().is_empty() {
            bail!("Mapped ram layout has not been initialized");
        }

        validate_ram_regions(machine_ram, expected_ram_states)?;
        validate_alias_regions(self.root(), expected_alias_states)?;

        Ok(())
    }

    pub fn skip_mapped_ram_from_snapshot(
        &self,
        memory: &mut File,
        mode: SnapshotMemoryMode,
    ) -> Result<()> {
        let address_space_state = read_address_space_state(memory)?;
        let first_region_offset = memory.stream_position()?;
        let machine_ram = self
            .get_machine_ram()
            .with_context(|| "This address space does not support migration.")?;
        if machine_ram.subregions().is_empty() {
            bail!("Mapped ram layout has not been initialized");
        }

        if mode == SnapshotMemoryMode::External {
            // There is no RAM payload in this file; the RamList header follows.
            return self.validate_external_ram_layout(&address_space_state);
        }
        let region_data_end = first_region_offset
            .checked_add(address_space_state.total_region_size)
            .with_context(|| {
                format!(
                    "Restore memory add overflow: {} + {}",
                    first_region_offset, address_space_state.total_region_size
                )
            })?;
        memory.seek(SeekFrom::Start(region_data_end))?;

        Ok(())
    }

    /// Hand guest RAM over to the external uffd daemon instead of copying it
    /// from the snapshot memory file.
    ///
    /// Registers every machine RAM subregion with a userfaultfd, sends the fd
    /// together with each region's offset within the snapshot memory file to
    /// the daemon. On return the file cursor is positioned past the RAM data,
    /// so the ram list section that follows can be restored from the file as
    /// usual.
    pub fn restore_uffd_ram_from_snapshot(
        &self,
        memory: &mut File,
        socket_path: &str,
        snapshot_memory: SnapshotMemoryMode,
    ) -> Result<()> {
        let address_space_state = read_address_space_state(memory)?;
        let ram_data_file_offset = memory.stream_position()?;

        let machine_ram = self
            .get_machine_ram()
            .with_context(|| "This address space does not support migration.")?;

        let mut uffd = UffdMemoryBackend::new(socket_path)
            .with_context(|| "UFFD restore: failed to create userfaultfd")?;
        let registered_offset_base = match snapshot_memory {
            SnapshotMemoryMode::Full => ram_data_file_offset,
            SnapshotMemoryMode::External => 0,
        };
        let mut registered_offset = registered_offset_base;
        for region in machine_ram.subregions().iter() {
            let state = address_space_state
                .ram_region_state
                .iter()
                .find(|state| state.name == region.name)
                .with_context(|| format!("Can not find ram region {:?}", region.name))?;
            if state.size != region.size() {
                bail!(
                    "Size of ram region {} changed, saved size {}, now {}.",
                    region.name,
                    state.size,
                    region.size()
                );
            }

            let offset = registered_offset_base
                .checked_add(state.offset)
                .with_context(|| {
                    format!(
                        "Restore memory overflow: {} + {}",
                        registered_offset_base, state.offset
                    )
                })?;

            // SAFETY: the region comes from the machine RAM subregion list; its
            // host mapping stays alive for the VM's lifetime.
            if let Some(host_addr) = unsafe { region.get_host_address(AddressAttr::Ram) } {
                uffd.register_region(host_addr, region.size(), offset)
                    .with_context(|| {
                        format!(
                            "UFFD restore: register region '{}' host={:#x} size={:#x}",
                            region.name,
                            host_addr,
                            region.size()
                        )
                    })?;
                registered_offset += region.size();
            }
        }

        // The daemon serves page faults using the file offsets registered
        // above; a size mismatch means the snapshot layout does not match the
        // current RAM layout and pages would be served from wrong offsets.
        let registered_size = registered_offset - registered_offset_base;
        if registered_size != address_space_state.total_region_size {
            bail!(
                "UFFD restore: registered RAM size {:#x} does not match snapshot region size {:#x}",
                registered_size,
                address_space_state.total_region_size
            );
        }

        uffd.send_to_external_uffd_daemon()
            .with_context(|| "UFFD restore: send_to_external_uffd_daemon failed")?;
        store_uffd_backend(uffd);

        if snapshot_memory == SnapshotMemoryMode::Full {
            // Skip the RAM data so the caller can restore the ram list section.
            let region_data_end = ram_data_file_offset
                .checked_add(address_space_state.total_region_size)
                .with_context(|| {
                    format!(
                        "Restore memory add overflow: {} + {}",
                        ram_data_file_offset, address_space_state.total_region_size
                    )
                })?;
            memory.seek(SeekFrom::Start(region_data_end))?;
        }

        Ok(())
    }
}

impl AddressSpace {
    fn snapshot_state_vec(&self, memory: SnapshotMemoryMode) -> Result<Vec<u8>> {
        let mut state = AddressSpaceState::default();
        let mut offset = 0;
        let machine_ram = self
            .get_machine_ram()
            .with_context(|| "This address space does not support migration.")?;
        let mut regions = machine_ram.subregions();
        if memory == SnapshotMemoryMode::External {
            // Container iteration follows region priority, not RAM addresses.
            // External exports use the address order from query-mem-mappings.
            regions.sort_by_key(|region| region.offset().raw_value());
        }
        for region in &regions {
            state.ram_region_state.push(RamRegionState {
                name: region.name.clone(),
                offset: if memory == SnapshotMemoryMode::External {
                    region.offset().raw_value()
                } else {
                    offset
                },
                size: region.size(),
            });
            offset += region.size();
            state.total_region_size += region.size();
        }

        for region in self.root().subregions().iter() {
            if region.alias_name().is_some() {
                state.alias_region_state.push(AliasRegionState {
                    name: region.name.clone(),
                    alias_offset: region.alias_offset(),
                    offset: region.offset().0,
                    size: region.size(),
                });
            }
        }

        Ok(serde_json::to_vec(&state)?)
    }
}

impl StateTransfer for AddressSpace {
    fn get_state_vec(&self) -> Result<Vec<u8>> {
        self.snapshot_state_vec(SnapshotMemoryMode::Full)
    }

    fn get_device_alias(&self) -> u64 {
        self.root().size()
    }
}

impl MigrationHook for AddressSpace {
    fn start_dirty_tracking(&self) -> Result<()> {
        let machine_ram = self
            .get_machine_ram()
            .context("This address space does not support snapshot dirty tracking")?;
        crate::uffd::start_dirty_tracking(machine_ram)
    }

    fn save_memory(&self, fd: &mut File, memory: SnapshotMemoryMode) -> Result<()> {
        // Save address space header.
        let ram_state = self.snapshot_state_vec(memory)?;
        let data_slice = get_state_slice(&ram_state)
            .with_context(|| "Failed to get state slice while saving state")?;
        fd.write_all(&data_slice)?;

        if memory == SnapshotMemoryMode::External {
            return Ok(());
        }

        // Save address space region.
        if let Some(machine_ram) = self.get_machine_ram() {
            for region in machine_ram.subregions().iter() {
                if let Some(base_addr) = region.start_addr() {
                    region
                        .read(fd, base_addr, 0, region.size())
                        .map_err(|e| MigrationError::SaveVmMemoryErr(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    fn restore_memory(&self, memory: &mut File, mode: &RestoreMode) -> Result<()> {
        match mode {
            // The memory file is already mmap'ed as the RAM backend; just move
            // the cursor past the RAM data.
            RestoreMode::Mapped => {
                self.skip_mapped_ram_from_snapshot(memory, SnapshotMemoryMode::Full)?
            }
            RestoreMode::MappedExternal => {
                self.skip_mapped_ram_from_snapshot(memory, SnapshotMemoryMode::External)?
            }
            RestoreMode::Copy => {
                if let Some(machine_ram) = self.get_machine_ram() {
                    let _ = read_address_space_state(memory)?;
                    for region in machine_ram.subregions().iter() {
                        if let Some(base_addr) = region.start_addr() {
                            region
                                .write(memory, base_addr, 0, region.size())
                                .map_err(|e| MigrationError::RestoreVmMemoryErr(e.to_string()))?;
                        }
                    }
                }
            }
            RestoreMode::Uffd {
                socket_path,
                memory: snapshot_memory,
            } => self.restore_uffd_ram_from_snapshot(memory, socket_path, *snapshot_memory)?,
        }

        Ok(())
    }

    fn send_memory(&self, fd: &mut dyn Write, range: MemBlock) -> Result<()> {
        self.read(fd, GuestAddress(range.gpa), range.len, AddressAttr::Ram)
            .map_err(|e| MigrationError::SendVmMemoryErr(e.to_string()))?;

        Ok(())
    }

    fn recv_memory(&self, fd: &mut dyn Read, range: MemBlock) -> Result<()> {
        self.write(fd, GuestAddress(range.gpa), range.len, AddressAttr::Ram)
            .map_err(|e| MigrationError::RecvVmMemoryErr(e.to_string()))?;

        Ok(())
    }
}

pub fn get_state_slice(state: &[u8]) -> Result<Vec<u8>> {
    let state_len = state.len();
    let le_bytes = (state_len as u64).to_le_bytes();
    let le_len = le_bytes.len();
    let total_len = le_len + state_len;

    // Aligned up to ALIGNMENT_SIZE.
    let aligned_len = round_up(total_len as u64, ALIGNMENT_SIZE)
        .with_context(|| format!("Failed to round up, total state length {}", total_len))?;
    let mut data_slice = vec![0u8; aligned_len as usize];
    data_slice[0..le_len].copy_from_slice(&le_bytes);
    data_slice[le_len..total_len].copy_from_slice(state);

    Ok(data_slice)
}

pub fn read_state_slice(file: &mut File) -> Result<Vec<u8>> {
    // Read state length.
    let size = size_of::<u64>();
    let mut le_bytes = vec![0u8; size];
    if let Err(e) = file.read_exact(&mut le_bytes) {
        bail!("Read state length error {:?}", e);
    }
    let state_len = u64::from_le_bytes(le_bytes.try_into().unwrap());
    let total_len = size as u64 + state_len;
    let aligned_len = round_up(total_len, ALIGNMENT_SIZE)
        .with_context(|| format!("Failed to round up, total state len {}", total_len))?;

    // Read state content.
    let mut data_slice = vec![0u8; aligned_len as usize - size];
    if let Err(e) = file.read_exact(&mut data_slice) {
        bail!("Read state content error {:?}", e);
    }

    Ok(data_slice[..state_len as usize].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{create_backend_mem, create_default_mem, MemoryBackendMode};
    use machine_manager::config::{MachineMemConfig, VmConfig};
    use std::os::unix::fs::FileExt;
    use vmm_sys_util::tempfile::TempFile;

    fn space_with_regions(regions: Vec<Region>) -> Arc<AddressSpace> {
        let size = regions.iter().map(Region::size).sum();
        let root = Region::init_container_region(size, "root");
        let machine_ram = Arc::new(Region::init_container_region(size, "machine-ram"));
        let mut offset = 0;
        for region in regions {
            let size = region.size();
            machine_ram
                .add_subregion_not_update(region, offset)
                .unwrap();
            offset += size;
        }
        root.add_subregion(
            Region::init_alias_region(machine_ram.clone(), 0, size, "ram-alias"),
            0,
        )
        .unwrap();
        AddressSpace::new(root, "space", Some(machine_ram)).unwrap()
    }

    #[test]
    fn test_external_backend_restore_preserves_cow_and_metadata_cursor() {
        let mut config = MachineMemConfig {
            mem_size: 0x1000,
            ..Default::default()
        };
        let space = space_with_regions(vec![create_default_mem(
            &config,
            1,
            MemoryBackendMode::Allocate,
        )
        .unwrap()]);
        space
            .write_object(&42_u8, GuestAddress(0), AddressAttr::Ram)
            .unwrap();
        let mut metadata_file = TempFile::new().unwrap().into_file();
        let mut full_file = TempFile::new().unwrap().into_file();
        space
            .save_memory(&mut metadata_file, SnapshotMemoryMode::External)
            .unwrap();
        space
            .save_memory(&mut full_file, SnapshotMemoryMode::Full)
            .unwrap();

        let metadata_len = metadata_file.stream_position().unwrap();
        let full_len = full_file.stream_position().unwrap();
        assert_eq!(full_len - metadata_len, 0x1000);
        // Stand in for the following RamList section to catch an incorrect seek.
        metadata_file.write_all(b"ram-list").unwrap();

        let raw = TempFile::new().unwrap();
        raw.as_file().set_len(0x1000).unwrap();
        raw.as_file().write_all_at(&[42], 0).unwrap();
        config.mem_path = Some(raw.as_path().to_str().unwrap().to_string());
        let restored = space_with_regions(vec![create_default_mem(
            &config,
            1,
            MemoryBackendMode::Snapshot,
        )
        .unwrap()]);
        metadata_file.rewind().unwrap();
        restored
            .validate_external_ram_from_snapshot(&mut metadata_file)
            .unwrap();
        assert_eq!(
            restored
                .read_object::<u8>(GuestAddress(0), AddressAttr::Ram)
                .unwrap(),
            42
        );
        restored
            .write_object(&99_u8, GuestAddress(0), AddressAttr::Ram)
            .unwrap();
        let mut value = [0];
        raw.as_file().read_exact_at(&mut value, 0).unwrap();
        assert_eq!(value, [42]);
        metadata_file.rewind().unwrap();
        restored
            .restore_memory(&mut metadata_file, &RestoreMode::MappedExternal)
            .unwrap();
        assert_eq!(metadata_file.stream_position().unwrap(), metadata_len);
        let mut marker = [0; 8];
        metadata_file.read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"ram-list");

        // A full snapshot still supplies its own RAM for mapped and copy restore.
        let mapped = AddressSpace::new(
            Region::init_container_region(0x1000, "root"),
            "mapped",
            Some(Arc::new(Region::init_container_region(
                0x1000,
                "machine-ram",
            ))),
        )
        .unwrap();
        full_file.rewind().unwrap();
        mapped
            .build_mapped_ram_from_snapshot(&mut full_file)
            .unwrap();
        assert_eq!(
            mapped
                .read_object::<u8>(GuestAddress(0), AddressAttr::Ram)
                .unwrap(),
            42
        );
        full_file.rewind().unwrap();
        mapped
            .restore_memory(&mut full_file, &RestoreMode::Mapped)
            .unwrap();
        assert_eq!(full_file.stream_position().unwrap(), full_len);
        full_file.rewind().unwrap();
        restored
            .restore_memory(&mut full_file, &RestoreMode::Copy)
            .unwrap();
        assert_eq!(
            restored
                .read_object::<u8>(GuestAddress(0), AddressAttr::Ram)
                .unwrap(),
            42
        );
        assert_eq!(full_file.stream_position().unwrap(), full_len);
    }

    #[test]
    fn test_external_backend_restore_checks_each_region_and_alias() {
        let mut vm_config = VmConfig::default();
        let mut files = Vec::new();
        let mut regions = Vec::new();
        for (id, value) in [("ram0", 42), ("ram1", 84)] {
            let raw = TempFile::new().unwrap();
            raw.as_file().set_len(0x1000).unwrap();
            raw.as_file().write_all_at(&[value], 0).unwrap();
            let config = vm_config
                .add_mem_backend(&format!(
                    "memory-backend-file,id={id},size=4K,share=off,mem-path={}",
                    raw.as_path().display()
                ))
                .unwrap();
            regions.push(create_backend_mem(&config, 1, MemoryBackendMode::Snapshot).unwrap());
            files.push(raw);
        }
        let space = space_with_regions(regions);
        assert_eq!(
            space
                .read_object::<u8>(GuestAddress(0), AddressAttr::Ram)
                .unwrap(),
            42
        );
        assert_eq!(
            space
                .read_object::<u8>(GuestAddress(0x1000), AddressAttr::Ram)
                .unwrap(),
            84
        );
        space
            .write_object(&99_u8, GuestAddress(0x1000), AddressAttr::Ram)
            .unwrap();
        let mut value = [0];
        files[1].as_file().read_exact_at(&mut value, 0).unwrap();
        assert_eq!(value, [84]);

        let mut metadata = TempFile::new().unwrap().into_file();
        space
            .save_memory(&mut metadata, SnapshotMemoryMode::External)
            .unwrap();
        metadata.rewind().unwrap();
        let state = read_address_space_state(&mut metadata).unwrap();
        assert_eq!(state.ram_region_state[0].name, "ram0");
        assert_eq!(state.ram_region_state[1].offset, 0x1000);
        space.validate_external_ram_layout(&state).unwrap();
        let mut invalid = state.clone();
        invalid.total_region_size += 0x1000;
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.ram_region_state[0].name = "wrong".to_string();
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.ram_region_state[1] = invalid.ram_region_state[0].clone();
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.ram_region_state[1].offset = 0;
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.ram_region_state[0].size += 0x1000;
        invalid.total_region_size += 0x1000;
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.alias_region_state[0].alias_offset = 0x1000;
        assert!(space.validate_external_ram_layout(&invalid).is_err());
        let mut invalid = state.clone();
        invalid.alias_region_state.clear();
        assert!(space.validate_external_ram_layout(&invalid).is_err());
    }
}
