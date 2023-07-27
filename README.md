# `cargo-heaptrack`

A `cargo` subcommand to profile executables with
[`heaptrack`](https://github.com/KDE/heaptrack).

The code is 99% copied from
[`cargo-flamegraph`](https://github.com/flamegraph-rs/flamegraph) since all the
logic for command line parsing and finding targets is the same. The only
difference is that instead of running `perf`, this subcommands calls
`heaptrack`. The original copyright notices have been preserved.
