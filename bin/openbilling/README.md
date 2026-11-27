# openbilling

The openbilling-rs service. A binary only composes the crates (agent guide, rule 4); it uses `anyhow` for its errors.

**Now:** it answers `--help` (`-h`) and `--version` (`-V`) and exits 0. With no argument, or any other, it exits 1
and says why. There is nothing to serve yet.

```sh
cargo run -p openbilling -- --version
```

## Tests

`cargo test -p openbilling`: the argument parser (unit tests) and the built binary run as a process (`tests/cli.rs`).
