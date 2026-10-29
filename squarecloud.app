DISPLAY_NAME=WolOS CDN
SUBDOMAIN=wolos
DESCRIPTION=WolOS package and release bucket: serves pacman repos and ISOs, caches Arch and CachyOS mirrors within its disk budget.
RUNTIME=rust
START=cargo build --release --locked && DATA_DIR=data ORIGIN=https://github.com/wonderlandos/wolos-packages/releases/download MIRRORS=archive=https://archive.archlinux.org,arch=https://geo.mirror.pkgbuild.com,cachy=https://mirror.cachyos.org/repo ./target/release/wolos-cdn
MEMORY=512
VERSION=recommended
AUTORESTART=true
