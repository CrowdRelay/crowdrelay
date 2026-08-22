# Running the production host on 1 GiB

CrowdRelay's own processes are not what fills a 1 GiB box. The API and worker
are Rust binaries with `TOKIO_WORKER_THREADS` pinned to 2 and 1, a `min_connections(1)`
pool that drops back to a single connection after five idle minutes, and a
256 MiB Compose ceiling that is a limit rather than a reservation. Measure before
changing them:

```bash
./crowdrelayctl resources
```

That reads the host over `CROWDRELAY_SSH_TARGET` and reports memory and swap,
the processes and containers actually holding it, what PostgreSQL reserved, and
whether anything from a retired Node/n8n install is still running or still on
disk. It mutates nothing.

## What normally holds the memory

**PostgreSQL, by reservation rather than by use.** The upstream image defaults
are sized for a much larger machine: `shared_buffers` 128 MiB is locked shared
memory, and `effective_cache_size` 4 GiB tells the planner there is four times
more page cache than the host physically has, which biases it toward plans that
then miss. `max_connections` 100 sizes the lock and proc arrays for a hundred
backends that will never exist — the API and worker pools are five each and idle
down to one. PostgreSQL 18's `io_workers` adds processes on top.

A profile that fits a 1 GiB host, applied as `-c` flags on the postgres service:

```
-c shared_buffers=96MB
-c effective_cache_size=256MB
-c max_connections=20
-c work_mem=2MB
-c maintenance_work_mem=32MB
-c autovacuum_work_mem=32MB
-c io_workers=1
-c max_worker_processes=4
```

`max_connections=20` covers the API pool (5), the worker pool (5), the one-shot
setup container and headroom for an operator `psql`. Raise it only alongside
`CROWDRELAY_DATABASE_MAX_CONNECTIONS`; the two must stay in step.

**Node, if a retired n8n is still installed.** n8n idles in the hundreds of
megabytes, which on this host is the difference between comfortable and
swapping. The `resources` report lists any container, image, volume, unit,
process or on-disk install left behind. Volumes survive `docker compose down`
and are invisible in `docker ps`, so check that section specifically.

**Docker itself.** `dockerd` plus `containerd` plus one shim per container is a
fixed six-figure-KiB cost that no tuning removes. `docker system df` in the
report shows what is reclaimable; build cache in particular grows without bound
on a host that builds anything locally.

## Swap

Disk swap on this class of instance is slow enough that the box feels stalled
rather than slow. Compressed RAM swap trades a little CPU for a large latency
win and is the right default here:

```bash
sudo apt install systemd-zram-generator
printf '[zram0]\nzram-size = 512\ncompression-algorithm = zstd\n' |
  sudo tee /etc/systemd/zram-generator.conf
printf 'vm.swappiness = 180\nvm.page-cluster = 0\n' |
  sudo tee /etc/sysctl.d/99-zram.conf
```

The high swappiness is deliberate and specific to zram: pages move to compressed
RAM, not to disk, so the kernel should prefer it. Keep any disk swap file as a
lower-priority backstop rather than removing it.

## Moving off the 1 GiB shape

Oracle's always-free allocation includes Ampere A1. The ceiling was halved in
2026: a pure Always Free account now gets 2 OCPU and 12 GiB, where it used to
get 4 and 24. An account upgraded to Pay As You Go still reaches 4 OCPU and
24 GiB inside the same free monthly pool — 4 OCPU is 2,920 OCPU-hours against a
3,000-hour allowance and 24 GiB is 17,520 GB-hours against 18,000, so it runs
continuously without leaving the pool. PAYG means a payment method on file and
anything past the pool bills, so it is a deliberate trade rather than a free
upgrade. Source:
<https://linuxiac.com/oracle-quietly-cuts-free-tier-ampere-a1-resources-in-half/>

Either shape is a large step up from the current VM.Standard.E2.1.Micro: even
the reduced always-free A1 is twelve times the memory and twice the cores. The
tuning above is what makes 1 GiB survivable; it is not what makes 1 GiB the
right size.

The blocker is architecture, not budget. `publish-images.yml` builds
`linux/amd64` only, deliberately, so the production daemon never pulls a
manifest it cannot run. `make build-arm64` proves the build works, so the change
is to publish a two-platform manifest — which needs a native arm64 runner rather
than QEMU, or Rust release builds under emulation become the slowest step in the
pipeline.

Decide that before moving. Everything above is worth doing either way.
