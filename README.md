# Cargo static registry RFC proof of concept

Testing whether it's feasible to serve crates-io registry over HTTP as static files.

Currently it only discovers which registry files needed to be fetched and fetches them. There's no caching and no integration with Cargo.

Requires `reqwest` with HTTP/2 support.

## Usage

```sh
cargo run --release -- rand@0.7 serde@1
```
