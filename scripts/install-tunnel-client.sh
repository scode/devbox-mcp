#!/bin/sh
# Installs the pinned tunnel-client release used by the tunnel tests (tests/tunnel.rs) and docs/testing.md, and prints
# the installed binary's path.
#
# The binary goes to a versioned directory under target/tools/, and target/tools/bin/tunnel-client is pointed at it.
# That stable path is where the tests look by default, so they need not know the pinned version. Running this again is
# cheap: an existing install is reused. Each platform's archive is pinned by its SHA-256 from the release's SHA256SUMS.txt, so a
# re-published or tampered release fails the check instead of being installed.
#
# This is a plain POSIX shell script, not Deno like other scripts would be, because CI runs it as a bare step before
# any other tooling is installed. To move to a newer release, update `version` and all four hashes together.
set -eu

version=v0.0.16

case "$(uname -s)-$(uname -m)" in
Linux-x86_64)
    platform=linux-amd64
    sha256=d60cdba019bce451bcc3a15478cc5b9cb11270b049f5b56ea39a80b517f8b117
    ;;
Linux-aarch64)
    platform=linux-arm64
    sha256=963d0384aaa7c798778479c45673f9051a4039dd891aef8f8978d2a1a628b74f
    ;;
Darwin-x86_64)
    platform=darwin-amd64
    sha256=57b3dd73f2d042c7aeb5664681f7538078359f55538b9e048640f1df71a0c83f
    ;;
Darwin-arm64)
    platform=darwin-arm64
    sha256=a160820d45089b5253d8d671fe2a689bde0f73d78c135d1497a7b33f28a3ac62
    ;;
*)
    echo "install-tunnel-client: unsupported platform $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
# The platform is part of the path so a checkout or cache shared between architectures never reuses the wrong binary.
dest="$root/target/tools/tunnel-client-$version-$platform"
link_dir="$root/target/tools/bin"

link_and_report() {
    mkdir -p "$link_dir"
    ln -sfn "$dest/tunnel-client" "$link_dir/tunnel-client"
    echo "$link_dir/tunnel-client"
}

if [ -x "$dest/tunnel-client" ]; then
    link_and_report
    exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
archive="tunnel-client-$version-$platform.zip"
curl -fsSL -o "$tmp/$archive" "https://github.com/openai/tunnel-client/releases/download/$version/$archive"

# macOS ships shasum but not sha256sum.
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
else
    actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
fi
if [ "$actual" != "$sha256" ]; then
    echo "install-tunnel-client: checksum mismatch for $archive: expected $sha256, got $actual" >&2
    exit 1
fi

unzip -q -j "$tmp/$archive" tunnel-client -d "$tmp/extracted"
mkdir -p "$dest"
# Move into place last, so an interrupted install never leaves a half-written binary that the reuse check above trusts.
mv "$tmp/extracted/tunnel-client" "$dest/tunnel-client.partial"
chmod 755 "$dest/tunnel-client.partial"
mv "$dest/tunnel-client.partial" "$dest/tunnel-client"
link_and_report
