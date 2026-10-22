#!/usr/bin/env bash
# Generate the AUR PKGBUILD for the pre-built hegn release binary.
#
# Usage: generate-pkgbuild-bin.bash <tag>
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
if [[ ! "$tag" =~ ^v?[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.]+)?$ ]]; then
    echo "refusing to generate a PKGBUILD for unexpected tag '$tag'" >&2
    exit 1
fi

version="${tag#v}"
# pkgver must not contain a hyphen (it separates pkgver from pkgrel).
pkgver="${version//-/_}"
url="https://github.com/hegn-tui/hegn"
# This is the asset name produced by .github/workflows/rust.yml.
asset="hegn-${tag}-x86_64-linux-gnu"

cat >PKGBUILD <<EOF
# Maintainer: hegn maintainers
pkgname=hegn-bin
pkgver=${pkgver}
pkgrel=1
pkgdesc='Firecracker MicroVM sandbox manager and TUI for coding agents (pre-built binary)'
arch=('x86_64')
url='${url}'
license=('MIT' 'Apache-2.0')
depends=('gcc-libs')
provides=('hegn')
conflicts=('hegn')
source=("\$pkgname-\$pkgver::${url}/releases/download/${tag}/${asset}")
sha256sums=('SKIP')

package() {
    install -Dm755 "\$srcdir/\$pkgname-\$pkgver" "\$pkgdir/usr/bin/hegn"
}
EOF

echo "wrote PKGBUILD for hegn-bin ${pkgver} (asset ${asset})"
