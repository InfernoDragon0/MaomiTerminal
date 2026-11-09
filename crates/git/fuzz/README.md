# maomi-git fuzz targets

cargo-fuzz targets for the git output parsers (`status`, `log` incl. chunked `LogParser::push`,
`diff`, `refs` incl. stashes / remotes / ahead-behind). Needs nightly and `cargo install cargo-fuzz`
(libFuzzer does not support the MSVC toolchain well; use Linux/WSL or macOS).

```sh
cd crates/git
cargo +nightly fuzz run status
cargo +nightly fuzz run log -- -max_total_time=300
```

Seed a corpus from the real-git fixtures: `cp tests/fixtures/status-*.bin fuzz/corpus/status/`.
