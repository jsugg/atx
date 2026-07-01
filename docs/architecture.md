# Architecture

ATX uses three process roles:

- the CLI validates a request, writes it, and wakes the scheduler;
- one per-user supervisor owns waiting deadlines;
- one short-lived run monitor starts and waits for each active command.

Ten thousand waiting jobs still use one supervisor. There is no sleeping
process per job.

The dependency direction is:

```text
CLI / supervisor / monitor -> application -> domain
                                      |
                                      +-> infrastructure ports
```

The domain does not know about SQLite, files, signals, sockets, or a platform
clock. Adapters implement those edges.

More detail, including startup transactions and recovery, will be added as
those pieces land.
