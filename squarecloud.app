DISPLAY_NAME=WolOS CDN
DESCRIPTION=WolOS package and release bucket: serves pacman repos and ISOs, pulls missing files from GitHub releases.
RUNTIME=rust
START=cargo build --release --locked && DATA_DIR=data ORIGIN=https://github.com/wonderlandos/wolos/releases/download ./target/release/wolos-cdn
MEMORY=512
VERSION=recommended
AUTORESTART=true
