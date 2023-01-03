
# alloc-track

This project allows per-thread and per-backtrace realtime memory profiling.

## Use Cases

* Diagnosing memory fragmentation (in the form of volatile allocations)
* Diagnosing memory leaks
* Profiling memory consumption of individual components

## Usage

1. Add the following dependency to your project:
```alloc-track = "0.2.3"```

2. Set a global allocator wrapped by `alloc_track::AllocTrack`

    Default rust allocator:
    ```

    use alloc_track::{AllocTrack, BacktraceMode};
    use std::alloc::System;

    #[global_allocator]
    static GLOBAL_ALLOC: AllocTrack<System> = AllocTrack::new(System, BacktraceMode::Short);
    ```

    Jemallocator allocator:
    ```

    use alloc_track::{AllocTrack, BacktraceMode};
    use jemallocator::Jemalloc;

    #[global_allocator]
    static GLOBAL_ALLOC: AllocTrack<Jemalloc> = AllocTrack::new(Jemalloc, BacktraceMode::Short);
    ```

3. Call `alloc_track::thread_report()` or `alloc_track::backtrace_report()` to generate a report. Note that `backtrace_report` requires the `backtrace` feature and the `BacktraceMode::Short` or `BacktraceMode::Full` flag to be passed to `AllocTrack::new`.

## Performance

In `BacktraceMode::None` or without the `backtrace` feature enabled, the thread memory profiling is reasonably performant. It is not something you would want to run in a production environment though, so feature-gating is a good idea.

When backtrace logging is enabled, the performance will degrade substantially depending on the number of allocations and stack depth. Symbol resolution is delaying, but a lot of allocations means a lot of backtraces. `backtrace_report` takes a single argument, which is a filter for individual backtrace records. Filtering out uninteresting backtraces is both easier to read, and substantially faster to generate a report as symbol resolution can be skipped. See `examples/example.rs` for an example.

## Real World Example

At LeakSignal, we had extreme memory segmentation in a high-bandwidth/high-concurrency gRPC service. We suspected a known hyper issue with high concurrency, but needed to confirm the cause and fix the issue ASAP. Existing tooling (bpftrace, valgrind) wasn't able to give us a concrete cause. I had created a prototype of this project back in 2019 or so, and it's time had come to shine. In a staging environment, I added an HTTP endpoint to generate a thread and backtrace report. I was able to identify a location where a large multi-allocation object was being cloned and dropped very often. A quick fix there solved our memory segmentation issue.
