# Reference capacity benchmark

Every release is checked against the reference capacity in the
`control-plane-operations` spec. On two CPUs, while four deployments of a
100-node, 1,000-edge graph run on a driver that takes time, and status requests
arrive at 10 a second, the 95th percentile of at least 100 samples must stay:

- below 2 seconds for planning;
- below 500 ms for the status API.

The benchmark also reports peak memory and how much the data directory grew.

Run it with:

```sh
just bench
```

It builds the server's tests in release mode and runs the ignored
`reference_capacity_benchmark` test in `crates/server/src/tests/benchmark.rs`,
pinned to two CPUs with `taskset`. It prints a report, with the configuration
it ran with, and writes it to `target/benchmark.json`. The test fails if either p95 is over its limit or the
graph is smaller than promised.

## What it measures

- **The graph.** It has 100 nodes and 1,013 edges, counted from
  `GET .../resources/graph`:
  - 97 `config` resources, each referencing up to 11 of the ones before it;
  - one workload resource that references the last 11;
  - the workload;
  - the environment.

  The chain makes the graph 98 layers deep.
- **Driver load.** Four applications deploy the graph again and again, which
  matches the default driver concurrency of 4. Each resource's driver is
  `openporch/echo`, made to wait 20 ms before every provisioning. The report
  shows the load was real:
  - how many provisionings the driver started while previews ran (the test
    fails below 100);
  - the mean number of deployments running at each status tick.
- **Planning.** 100 previews (`POST .../plans`) of the graph in another
  application, one after another. A preview resolves the deployment set,
  builds and checks the graph, classifies every resource, and stores the
  preview, the same work a deployment's acceptance does.
- **Status.** `GET` of the environments being deployed, in turn, sent every
  100 ms without waiting for the previous answer. They run for as long as
  previews run and at least 100 times, and the driver load lasts until both
  are done.
- **Peak memory.** The peak resident memory of the process, which holds the
  server and the test's client. The fake driver starts no subprocesses, so
  subprocess memory is zero. OpenTofu's own memory per run depends on its
  providers and is not part of the reference.
- **Disk growth.** How much the data directory grew while the load ran,
  mostly deployment records, logs and stored previews.

Requests go through the server's router in the same process, so the numbers
leave out the network. The spec excludes external latency.

## Results

Release mode, on 2 of 4 vCPUs of an Intel Xeon at 2.10 GHz, with Rust 1.97.0:

| Measure | Result | Limit |
| --- | --- | --- |
| Graph | 100 nodes, 1,013 edges | 100 nodes, 1,000 edges |
| Planning p95 (100 samples) | 82 ms (max 254 ms) | 2 s |
| Status p95 (101 samples) | 40 ms (max 133 ms) | 500 ms |
| Deployments running at each status tick | 4.0 on average | |
| Provisionings while previews ran | 387 | |
| Deployments finished during the run | 13 | |
| Peak memory | 63 MiB | 2 GiB host |
| Subprocess peak memory | 0 (in-process fake driver) | |
| Data directory growth | 13.2 MiB | |

The test does not enforce the 2 GiB of the reference host. Peak memory stays
far below it, and `systemd-run --scope -p MemoryMax=2G just bench` checks it
on a systemd host.
