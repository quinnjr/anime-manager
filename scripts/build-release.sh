#!/usr/bin/env bash
# Build the Linux release bundles into dist/, the same way CI does.
#
#   ./scripts/build-release.sh                 # deb, rpm and AppImage
#   BUNDLES=deb ./scripts/build-release.sh     # just one
#   SKIP_TESTS=1 ./scripts/build-release.sh    # skip the test gate
set -euo pipefail

cd "$(dirname "$0")/.."

BUNDLES="${BUNDLES:-deb,rpm,appimage}"
DIST="${DIST:-dist}"

# linuxdeploy and appimagetool are themselves AppImages. Without a usable FUSE mount they
# cannot self-mount and bundling dies; this makes them unpack to a temp dir instead.
# Harmless where FUSE does work.
export APPIMAGE_EXTRACT_AND_RUN=1

# linuxdeploy bundles an old binutils whose `strip` cannot parse the DT_RELR relocations
# (`.relr.dyn`) that Arch and recent Fedora build their system libraries with. It fails on
# every such library and aborts the bundle. Skipping the strip is the upstream workaround;
# the cost is a larger AppImage. Export NO_STRIP=0 on a distro that does not need it.
export NO_STRIP="${NO_STRIP:-1}"

pnpm install --frozen-lockfile

if [ -z "${SKIP_TESTS:-}" ]; then
  # Single-threaded: the player tests drive a fake mpv through process-global env vars.
  cargo test --manifest-path src-tauri/Cargo.toml -- --test-threads=1
  pnpm test
  pnpm check
fi

pnpm tauri build --bundles "$BUNDLES"

rm -rf "$DIST"
mkdir -p "$DIST"
find src-tauri/target/release/bundle \
  \( -name '*.deb' -o -name '*.rpm' -o -name '*.AppImage' \) \
  -exec cp {} "$DIST"/ \;

# Fail loudly rather than shipping with a format silently missing.
missing=0
IFS=',' read -ra want <<< "$BUNDLES"
for b in "${want[@]}"; do
  case "$b" in
    deb) ext=deb ;;
    rpm) ext=rpm ;;
    appimage) ext=AppImage ;;
    *) continue ;;
  esac
  if ! compgen -G "$DIST/*.$ext" > /dev/null; then
    echo "error: no .$ext was produced" >&2
    missing=1
  fi
done
[ "$missing" -eq 0 ] || exit 1

(cd "$DIST" && sha256sum ./* > SHA256SUMS.txt)
ls -lh "$DIST"
