# k8squad-test: nfs-csi default StorageClass — RESOLVED (ISI-6335)

Status: **RESOLVED 2026-10-20.** The default StorageClass `nfs-csi` now provisions
PVCs end-to-end. Kept here as the resolution record.

## Original symptom

Every PVC on the default SC failed to provision:

```
mount failed: exit status 32
mount.nfs: rpc.statd is not running but is required for remote locking.
mount.nfs: mounting 10.0.0.178:/mnt/truenas/pvc failed, reason given by
           server: No such file or directory
```

## Actual root cause: wrong NFS server IP (not a missing export, not statd)

The SC pointed at `server: 10.0.0.178`. That host ("zetlab") only exports
`/nfs/backup`, so `/mnt/truenas/pvc` returned `No such file or directory`. The
**real TrueNAS is 10.0.0.145**, which already exports `/mnt/truenas/pvc` (verified
via `showmount -e 10.0.0.145`). Fixed under **ISI-6336**: the SC now has
`server: 10.0.0.145, share: /mnt/truenas/pvc`.

The `rpc.statd` line was a **red herring**: it only appeared because the broken
mount against .178 was negotiating NFSv3. The correct server (10.0.0.145)
negotiates **NFSv4.2**, where locking is carried in-protocol and `rpc.statd`/NLM is
never invoked — so no node-side `nfs-common`/`rpcbind`/statd change is needed. (This
matters because the nodes are CAPI-managed VMs where manual package installs would
not survive a rollout anyway.)

## Verification (2026-10-20, ISI-6335)

```
kubectl apply -f <pvc on nfs-csi> + <pod writing to it>
# PVC -> Bound in seconds
# pod wrote+read /data/probe OK
# mount: 10.0.0.145:/mnt/truenas/pvc/pvc-... on /data type nfs4
#        (rw,...,vers=4.2,...)   <-- NFSv4.2, no statd
```

## Known follow-up (NOT this issue): ISI-6343 drift-guard

Fixing the SC via `kubectl apply` made it lose its `csi-driver-nfs` Helm ownership
and flipped `reclaimPolicy` to `Retain`. A future `helm upgrade` of the chart could
revert `server` back to the broken `.178`. Tracked under **ISI-6343**: re-adopt the
SC into the chart's `storageClass` values pinned to `server: 10.0.0.145`. Optional
hardening to pin there: `mountOptions: [nfsvers=4.1]` (belt-and-suspenders against a
future client defaulting to v3 and reintroducing the statd requirement).
