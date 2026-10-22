# Conch indexed RAM snapshots

This extension restores Conch v2 RAM from a cumulative index of immutable local
files. Each logical RAM backend has one contiguous host virtual address range;
its file extents use private writable mappings. Guest writes create private COW
pages and never modify the source files. File extents do not create additional
guest RAM backends, NUMA nodes, or KVM memory slots.

The source baseline is `ee6b2753d994e04e4f7042e1707a1212e751ef75`. Snapshot
compatibility applies to the selected build and its machine configuration, not
to arbitrary StratoVirt versions.

## Build and inputs

Build the required device features:

```sh
cargo build --locked --release -p stratovirt --features virtio_pmem,vhost_vsock
```

Append these arguments to the same CPU, RAM, device and platform configuration
used to create the snapshot:

```text
-mem-map /run/conch/ram/current/memory/index.json
-mem-map-sources /run/conch/memory-sources.json
-incoming file:/run/conch/native,memory=external,mapped=true
-S
```

Both index arguments are required together and must be absolute paths. They are
only valid for mapped file restore with `memory=external`. They cannot be
combined with `uffd_sock`, a separate RAM `mem-path`, shared RAM, HugeTLB, memory
preallocation, or a memfd RAM backend. The host page size must be 4096 bytes.

The local source binding file has this form:

```json
{
  "version": 1,
  "sources": {
    "self": "/run/conch/ram/current",
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": "/run/conch/ram/base"
  }
}
```

The index uses `version`, `pageSizeBytes`, `memorySizeBytes` and `backends`.
Each backend contains `id`, `ramOffset`, `sizeBytes` and `extents`; each extent
contains `start`, `end`, `sourceLayer`, `sourceFile` and `fileOffset`. A
`sourceFile` names `memory/<backend-id>/<start>_<end>` beneath its bound source
directory. Ranges use canonical decimal byte offsets and 4096-byte alignment.
Files must be regular files of the declared range length. Symlinks, path
traversal, duplicate JSON keys, missing sources, range overflow, gaps, overlap
and invalid backend identities are rejected. `index.json` is a reserved backend
name. The validator also checks current mappings, required extents and a reserve
against `vm.max_map_count` before creating the mappings.

The default backend is `DefaultRam`. Explicit backends retain their object IDs,
sizes, NUMA bindings, host-node choices and memory policy. An indexed
`memory-backend-file` RAM object does not need its own `mem-path`: its sources
come from the index. Objects referenced only by virtio-pmem remain independent
file backends and require `share=off`. The memory-backend-file parser does not
accept a `readonly=on` argument; virtio-pmem selects its read-only open from
`share=off`.

Only the validated virtio-blk, virtio-pmem and MMIO kernel vhost-vsock device
paths are accepted for indexed restore. Network backends, vhost-user, balloon,
VFIO and other devices are rejected. PCI vhost-vsock with direct IRQFD delivery
is also rejected because its interrupt completion barrier is not implemented.
The Conch adapter uses microvm with `vhost-vsock-device`.

## Restore and dirty tracking

Restore compares the native RAM/backend/alias layout against both the index and
the requested machine configuration. Native pmem GPA and size, and writable
block capacity, must match. The Conch host adapter additionally validates its
native-package metadata for ordered lower digests and stable guest device
identity before launching the process.

Indexed RAM enables independent userfaultfd write protection before device
initialization and native state restoration. It requires `WP_ASYNC` and
`WP_UNPOPULATED`; registration is WP-only and never discards existing COW
contents. An unsupported kernel, denied userfaultfd access, or failed file
mapping registration aborts startup. There is no resident-page fallback for
indexed dirty tracking.

With `-S`, successful incoming restore remains paused. The host waits for
`query-migrate` to report `completed`, verifies `query-status` is `paused`,
sends one `cont`, and verifies `running`. A startup pause does not itself
establish a runtime capture barrier.

## Capture sequence and QMP

Use the existing QMP commands; there is no `quiesce` command. QMP IDs in this
implementation are strings.

1. Send `stop` and wait for success. The selected devices stop submissions,
   settle RAM-writing completion callbacks, drain block I/O and flush block
   backing. Kernel vsock queues remain stopped and pending userspace interrupt
   notifications are settled before success.
2. Query `query-mem-mappings`. For an incremental capture, query
   `query-mem-dirty-bitmap` once, retaining the returned bitmap.
3. Save native state using `migrate` with
   `{"uri":"file:/run/conch/capture/native,memory=external"}`. Export RAM from the
   actual VMM process addresses and seal the writable disk while the same
   pause remains in effect.
4. Wait for the native result and all host-side RAM/disk work. Publish or retain
   the fixed outputs according to the host lifecycle. Send `cont` only after
   all capture readers and writers have finished.

`query-mem-mappings` returns one entry per logical RAM backend:

```json
{
  "mappings": [
    {
      "id": "DefaultRam",
      "base-host-virt-addr": 139637976727552,
      "size": 134217728,
      "offset": 0,
      "page-size": 4096
    }
  ]
}
```

The address above is illustrative and is not a reusable process address.
Entries follow native logical RAM order; `offset` is the contiguous flat RAM
offset. Pmem and GPA holes are not guest RAM and do not appear in this list.

The dirty result has `bitmap` (an array of unsigned 64-bit words) and
`page-size`. In indexed mode the page size is 4096 and bits follow the same flat
RAM order. Zero overwrites set bits too; unused tail bits are zero. Querying
reprotects the existing mappings for the next capture. The host must not retry
an ambiguous dirty query because the earlier request may already have reset
tracking. Unchanged blocks require no RAM reads, and changed zero blocks may be
written as sparse holes while their bitmap bits remain set. A baseline capture
defines every logical RAM byte; residency alone cannot classify file-backed
RAM as zero.

RAM queries require an established device-quiescence barrier as well as paused
vCPUs. Repeated `stop` retains that interval. Native snapshot completion or
failure does not reopen device queues; `cont` reopens them. Vsock transport
reset is deferred until execution resumes, so native save does not modify the
RAM still being captured. Resume is rejected while native writers remain active.

## Native format and lifetime

The native `state` file contains device/vCPU state. The `memory` file retains
its EXTERNAL header, RAM/backend/alias metadata and RamList contents, but has no
full guest RAM payload. Indexed restore validates the RAM metadata and leaves
the file cursor immediately after it so RamList is read from the correct
position. It does not seek across a nonexistent guest RAM payload.

Native metadata and mapping queries use the same logical RAM order. Existing
FULL and external-handler UFFD decoding paths remain distinct from indexed
restore. Snapshot save/restore joins auxiliary work even when the main writer
or reader fails; a terminal result does not leave an auxiliary worker using the
output directory. Lost transport responses still require host-side process and
writer-lifetime handling.

The host must keep every source file, lower device, writable device and native
input available until the VMM and all auxiliary tasks have stopped. Publishing
a new image does not replace the backing of an already running VM.

## Guest platform and qualification

The kernel/initrd must provide virtio-mmio, virtio-pmem, DAX, EROFS, virtio-blk,
ext4, OverlayFS and virtio-vsock. Conch's guest PID 1 mounts the EROFS lowers and
ext4 upper/work, switches to the merged root, reaps children and listens on
vsock port 4065. The host uses a fresh CID and connection after each restore.
The readiness exchange is external to QMP: a four-byte big-endian length and
1–16384 bytes of JSON, version 2, with matching `requestId`. It is a platform
readiness query, not a guest command execution API.

The implementation has unit coverage for index validation, private mappings,
native metadata cursor handling, dirty epochs, pause retention, drain failures
and native writer completion. Privileged host checks have exercised WP_ASYNC
with private EROFS mappings.

Conch's explicitly gated `tests/system/engine_linux_test.go` has also completed
real KVM acceptance on x86_64 Linux 7.2.11 with its modular conch-init initrd,
the pinned AgentENV daemon and this VMM build. The direct binaries ran with
seccomp enabled. The test covers two EROFS lowers, two concurrent instances,
two checkpoints, clones, writable snapshot removal/relinking, Pause/Resume
with a new process/CID, parent exit while a child remains running, and complete
device/View/lease cleanup.

The optional memory probe reserves two guest pages with the kernel command
line. Controlled host writes to those excluded pages verify private COW,
nonzero-to-zero incremental replacement and unchanged source files. Present,
nonzero pagemap PFNs demonstrate both same-generation and cross-generation
sharing of unchanged source inode/offset pairs. Separate unmounted raw ublk
devices verify active-upper write isolation and layered zero replacement.
These checks do not exercise arbitrary guest application workloads or qualify
other host kernels, architectures, machine layouts or PCI IRQFD vsock. Run the
system test against the actual deployment profile when changing those inputs.
