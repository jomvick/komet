#!/usr/bin/env bash
# RPM packaging: build release binary and produce
#   target/package/komet-<version>-1.<arch>.rpm
#
# Usage: scripts/package-rpm.sh
# Env:   PROFILE=debug for fast unoptimized package; default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)"

OUT_DIR="$ROOT/target/package"
RPM_BUILD_DIR="$OUT_DIR/rpmbuild"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p komet
  BIN="$ROOT/target/release/komet"
else
  cargo build -p komet
  BIN="$ROOT/target/debug/komet"
fi

if ! command -v rpmbuild >/dev/null 2>&1; then
  echo "Error: rpmbuild is required to build RPM packages." >&2
  exit 1
fi

rm -rf "$RPM_BUILD_DIR"
mkdir -p "$RPM_BUILD_DIR"/{BUILD,RPMS,SOURCES,SPECS,SRPMS,tmp,buildroot}

# Prepare SOURCES
cp "$BIN" "$RPM_BUILD_DIR/SOURCES/komet"
# Strip unneeded symbols if not already stripped
strip --strip-unneeded "$RPM_BUILD_DIR/SOURCES/komet" 2>/dev/null || true
cp "$ROOT/dist/komet.desktop" "$RPM_BUILD_DIR/SOURCES/komet.desktop"
cp "$ROOT/dist/komet.png" "$RPM_BUILD_DIR/SOURCES/komet.png"
cp -r "$ROOT/dist/icons/hicolor" "$RPM_BUILD_DIR/SOURCES/hicolor"

# Generate SPEC file
SPEC_FILE="$RPM_BUILD_DIR/SPECS/komet.spec"
cat >"$SPEC_FILE" <<EOF
Name:           komet
Version:        ${VERSION}
Release:        1%{?dist}
Summary:        Multi-device controller for coding agents
License:        Apache-2.0
URL:            https://komet.sh
AutoReqProv:    yes
BuildRoot:      %{_topdir}/buildroot

%description
Komet is a unified coding agent controller supporting autonomous agent loops,
terminal sandboxing, and multi-device sync.

%prep

%build

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}%{_bindir}
mkdir -p %{buildroot}%{_datadir}/applications
mkdir -p %{buildroot}%{_datadir}/icons/hicolor
mkdir -p %{buildroot}%{_datadir}/pixmaps

install -m 755 %{_sourcedir}/komet %{buildroot}%{_bindir}/komet
install -m 644 %{_sourcedir}/komet.desktop %{buildroot}%{_datadir}/applications/komet.desktop
cp -r %{_sourcedir}/hicolor/* %{buildroot}%{_datadir}/icons/hicolor/
install -m 644 %{_sourcedir}/komet.png %{buildroot}%{_datadir}/pixmaps/komet.png

%post
if [ \$1 -eq 1 ]; then
    update-desktop-database &> /dev/null || :
    touch --no-create %{_datadir}/icons/hicolor &>/dev/null || :
    gtk-update-icon-cache %{_datadir}/icons/hicolor &>/dev/null || :
fi

%postun
if [ \$1 -eq 0 ]; then
    update-desktop-database &> /dev/null || :
    touch --no-create %{_datadir}/icons/hicolor &>/dev/null || :
    gtk-update-icon-cache %{_datadir}/icons/hicolor &>/dev/null || :
fi

%files
%{_bindir}/komet
%{_datadir}/applications/komet.desktop
%{_datadir}/icons/hicolor/*/apps/komet.png
%{_datadir}/pixmaps/komet.png

%changelog
* Wed Sep 02 2026 Komet Team <support@komet.sh> - ${VERSION}-1
- Official release ${VERSION}
EOF

rpmbuild \
  --define "_topdir $RPM_BUILD_DIR" \
  --define "_tmppath $RPM_BUILD_DIR/tmp" \
  -bb "$SPEC_FILE"

RPM_FILE="$(find "$RPM_BUILD_DIR/RPMS" -name "*.rpm" | head -n 1)"
DEST_RPM="$OUT_DIR/$(basename "$RPM_FILE")"
mv "$RPM_FILE" "$DEST_RPM"

rm -rf "$RPM_BUILD_DIR"
echo "packaged: $DEST_RPM"
ls -lh "$DEST_RPM"
