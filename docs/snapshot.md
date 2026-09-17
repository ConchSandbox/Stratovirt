# Snapshot and Restore

StratoVirt supports to take a snapshot of a paused VM as VM template. This template can be used to warm start a new VM. Warm start skips the kernel boot stage and userspace initialization stage to boot VM in a very short time.

## Create VM template

First, we create a StratoVirt VM:
```shell
$ ./stratovirt \
    -machine microvm \
    -kernel path/to/vmlinux.bin \
    -append "console=ttyS0 pci=off reboot=k quiet panic=1 root=/dev/vda" \
    -drive file=path/to/rootfs,id=rootfs,readonly=off,direct=off \
    -device virtio-blk-device,drive=rootfs,id=rootfs \
    -qmp unix:path/to/socket,server,nowait \
    -serial stdio
```

After the VM boot up, pause the VM with QMP:
```shell
$ ncat -U path/to/socket
{"QMP":{"version":{"StratoVirt":{"micro":1,"minor":0,"major":0},"package":""},"capabilities":[]}}
{"execute":"stop"}
{"event":"STOP","data":{},"timestamp":{"seconds":1583908726,"microseconds":162739}}
{"return":{}}
```

When VM is in paused state, it's safe to take a snapshot of the VM into the specified directory with QMP.
```shell
$ ncat -U path/to/socket
{"QMP":{"version":{"StratoVirt":{"micro":1,"minor":0,"major":0},"package":""},"capabilities":[]}}
{"execute":"migrate", "arguments":{"uri":"file:path/to/template"}}
{"return":{}}
```

Two files will be created in given directory on the system.
```shell
$ ls path/to/template
memory  state
```
File `state` contains the device state data of VM devices. File `memory` contains guest memory data of VM memory. The file size is explained by the size of VM guest memory.

## Restore from VM template

Restore from VM template with below command:
```shell
$ ./stratovirt \
    -machine microvm \
    -kernel path/to/vmlinux.bin \
    -append "console=ttyS0 pci=off reboot=k quiet panic=1 root=/dev/vda" \
    -drive file=path/to/rootfs,id=rootfs,readonly=off,direct=off \
    -device virtio-blk-device,drive=rootfs,id=rootfs \
    -qmp unix:path/to/socket,server,nowait \
    -serial stdio \
    -incoming file:path/to/template
```

The device configuration must be the same with template VM. Its cpu number, guest memory size, device number and type can be changed. For drive file, only support previous file or its backups. After that, the VM is created from template successfully.

## Restore with file memory backends

Restore a `memory=external` snapshot using the existing memory backend options.
For a VM without NUMA, provide its complete raw Guest RAM image with `-mem-path`:

```text
-mem-path /mnt/erofs/memory/ram.img
-incoming file:/path/to/snapshot,memory=external,uffd-track-dirty=true
```

Keep `mem-share=off` (the default). For a NUMA VM, configure a separate file for
each RAM backend and explicitly set `share=off`; file objects default to sharing:

```text
-m 2G -smp 2
-object memory-backend-file,id=mem0,size=1G,mem-path=/mnt/erofs/memory/node0.img,share=off
-object memory-backend-file,id=mem1,size=1G,mem-path=/mnt/erofs/memory/node1.img,share=off
-numa node,nodeid=0,cpus=0,memdev=mem0
-numa node,nodeid=1,cpus=1,memdev=mem1
-incoming file:/path/to/snapshot,memory=external,uffd-track-dirty=true
```

The NUMA topology, backend IDs, sizes, and RAM layout must match the source VM.
A memory object supplies Guest RAM only when bound through NUMA `memdev`.
Each file begins at offset zero and must be a regular file whose logical size
exactly matches its backend. To use a flat multi-region RAM export, split it into
backend files according to the offsets and sizes from `query-mem-mappings`.

External mapped restore opens existing files read-only and maps them privately
with copy-on-write. Guest RAM remains writable; Guest writes do not change the
files. Missing, empty, incorrectly sized, or shared backends are rejected.
`mem-prealloc` and host NUMA policies retain their normal meaning; preallocation
touches all pages and therefore forfeits lazy loading and initial COW sharing.

The snapshot and RAM files must represent the same VM state and remain immutable
during use. A file backing a private mapping does not automatically contain the
VM's later writes; obtain a consistent RAM export before restoring it.

This mode requires `memory=external` and `mapped=true` (the default), and cannot
be combined with `uffd_sock`. Dirty tracking is optional. The native snapshot
still supplies the RAM layout, RamList memory, and device state. Full snapshots
continue to supply their embedded RAM; external UFFD restore continues to use
`mapped=false,uffd_sock=...`.

## Incremental memory tracking

Enable the existing UFFD WP_ASYNC dirty tracker without an external page-fault
handler by adding `uffd-track-dirty=true` to snapshot restore:

```text
-incoming file:/path/to/snapshot,uffd-track-dirty=true
-incoming file:/path/to/snapshot,mapped=false,uffd-track-dirty=true
```

The option defaults to false and cannot be combined with `uffd_sock`. Requested
tracking requires `UFFD_FEATURE_WP_ASYNC`; initialization fails if unavailable.
Mapped RAM is protected before device initialization; copy mode is protected after
RAM loading and before device restore. No `-S` or manual `cont` is needed.

At each checkpoint, pause the VM and quiesce all RAM writers, then use the existing
query interface:

```json
{"execute":"query-mem-dirty-bitmap"}
```

The result contains `bitmap` (64-bit words) and `page-size` (bytes), with pages
numbered in `query-mem-mappings` order. Each query resets tracking for the next
round. Keep RAM writers paused until the selected pages and VM state are saved.
During tracking, keep mappings stable and avoid page discard or writes through
other aliases or previously pinned DMA buffers.

## Snapshot state check

Use QMP command `query-migrate` to check snapshot state:
```shell
$ ncat -U path/to/socket
{"QMP":{"version":{"StratoVirt":{"micro":1,"minor":0,"major":0},"package":""},"capabilities":[]}}
{"execute":"query-migrate"}
{"return":{"status":"completed"}}
```

Now there are 5 states during snapshot:
- `None`: Resource is not prepared all.
- `Setup`: Resource is setup, ready to do snapshot.
- `Active`: In snapshot.
- `Completed`: Snapshot succeed.
- `Failed`: Snapshot failed.

## Limitations

Snapshot-restore support machine type:
- `microvm`
- `q35` (on x86_64 platform)
- `virt` (on aarch64 platform)

Some devices and feature don't support to be snapshot yet:
- `vhost-net`
- `vfio` devices
- `balloon`
- `hugepage`, `mem-shared` (private regular-file backends are supported for
  external mapped restore as described above)
- `pmu`
- `sve`
- `gic-version=2`

Some device attributes can't be changed:
- `virtio-net`: mac
- `virtio-blk`: file(only ordinary file or copy file), serial_num
- `device`: bus, addr
- `smp`
- `m`

For machine type `microvm`, if use `hot-replace` before snapshot, add newly replaced device to restore command.
