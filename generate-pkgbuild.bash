#!/usr/bin/env bash
# Generate the AUR PKGBUILD for a source build of hegn.
#
# Usage: generate-pkgbuild.bash <tag>
# The generated PKGBUILD is written to ./PKGBUILD.
set -euo pipefail

tag="${1:-}"
if [[ -z "$tag" ]]; then
    echo "usage: $0 <tag>" >&2
    exit 1
fi
# The tag is embedded in the generated PKGBUILD, which downstream users build.
# Restrict it to a version-like token so quotes, '$' or backticks cannot break
# out of the PKGBUILD strings.
if [[ ! "$tag" =~ ^v?[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
    echo "refusing to generate a PKGBUILD for unexpected tag '$tag'" >&2
    exit 1
fi

version="${tag#v}"
# pkgver must not contain a hyphen (it separates pkgver from pkgrel).
pkgver="${version//-/_}"
url="https://github.com/hegn-tui/hegn"

cat >PKGBUILD <<EOF
# Maintainer: hegn maintainers
pkgname=hegn
pkgver=${pkgver}
pkgrel=1
pkgdesc='Firecracker MicroVM sandbox manager and TUI for coding agents'
arch=('x86_64' 'aarch64')
url='${url}'
license=('MIT' 'Apache-2.0')
depends=('gcc-libs')
makedepends=('git' 'rust')
source=("\$pkgname::git+${url}.git#tag=${tag}")
sha256sums=('SKIP')

build() {
    cd "\$srcdir/\$pkgname"
    export CARGO_TARGET_DIR="\$srcdir/target"
    cargo build --release --locked
}

package() {
    cd "\$srcdir/\$pkgname"
    install -Dm755 "\$srcdir/target/release/hegn" "\$pkgdir/usr/bin/hegn"
}
EOF

echo "wrote PKGBUILD for hegn ${pkgver} (tag ${tag})"
