#!/bin/bash
set -x
set -e

TARGET_DIR=${1:-target}

TAG_NAME=${TAG_NAME:-$(git -c "core.abbrev=8" show -s "--format=%cd-%h" "--date=format:%Y%m%d-%H%M%S")}

HERE=$(pwd)

if test -z "${SUDO+x}" && hash sudo 2>/dev/null; then
  SUDO="sudo"
fi

if test -e /etc/os-release; then
  . /etc/os-release
fi


case $OSTYPE in
  darwin*)
    # Each CI job builds a single architecture, so the arch has to be part of
    # the name or the arm64 and x86_64 runs would overwrite each other's zip.
    # Unset means "whatever this machine produced", which is what a plain local
    # `cargo build --release` wants.
    zipdir="ThinkTerm-macos${MACOS_ARCH:+-$MACOS_ARCH}-$TAG_NAME"
    if [[ "$BUILD_REASON" == "Schedule" ]] ; then
      zipname="ThinkTerm-macos${MACOS_ARCH:+-$MACOS_ARCH}-nightly.zip"
    else
      zipname="$zipdir.zip"
    fi
    # Quoted because $TAG_NAME reaches here from an interactive prompt in
    # ci/macos-package.sh.  Unquoted, a tag of "v1 assets" would expand this
    # rm into two paths and delete assets/ from the repository root.
    rm -rf "$zipdir" "$zipname"
    mkdir "$zipdir"
    cp -r assets/macos/ThinkTerm.app "$zipdir/"
    # Omit MetalANGLE for now; it's a bit laggy compared to CGL,
    # and on M1/Big Sur, CGL is implemented in terms of Metal anyway
    rm $zipdir/ThinkTerm.app/*.dylib
    mkdir -p $zipdir/ThinkTerm.app/Contents/MacOS
    mkdir -p $zipdir/ThinkTerm.app/Contents/Resources
    cp assets/icon/ThinkTerm_simple.icns $zipdir/ThinkTerm.app/Contents/Resources/ThinkTerm_simple.icns
    cp -r assets/shell-integration/* $zipdir/ThinkTerm.app/Contents/Resources
    cp -r assets/shell-completion $zipdir/ThinkTerm.app/Contents/Resources
    # The license texts (GPL-3 for this project, MIT for the upstream WezTerm
    # code) and the third-party attributions; all of them have to ship with the
    # binaries that embed the material they cover.
    cp LICENSE.md LICENSE-MIT NOTICE $zipdir/ThinkTerm.app/Contents/Resources/
    tic -xe wezterm -o $zipdir/ThinkTerm.app/Contents/Resources/terminfo termwiz/data/wezterm.terminfo

    # Naming an architecture names its target directory too. Without that the
    # lipo below folds together every $TARGET_DIR/*/release it can see, which
    # on a machine that has also cross-built for Linux means handing lipo an
    # ELF binary -- and on one that has run a plain `cargo build --release`
    # means silently packaging the host's architecture whatever was asked for.
    # ci/macos-package.sh resolves this itself, because it also has to look in
    # the same place to decide whether a build is needed at all.
    macos_bin_dir=${MACOS_BIN_DIR:-}
    if [[ -z "$macos_bin_dir" ]] ; then
      case "${MACOS_ARCH:-}" in
        arm64) macos_bin_dir=$TARGET_DIR/aarch64-apple-darwin/release ;;
        x86_64) macos_bin_dir=$TARGET_DIR/x86_64-apple-darwin/release ;;
        *) macos_bin_dir= ;;
      esac
    fi

    for bin in wezterm thinkterm thinkterm-mux-server thinkterm-gui strip-ansi-escapes ; do
      if [[ -n "$macos_bin_dir" ]] ; then
        cp $macos_bin_dir/$bin $zipdir/ThinkTerm.app/Contents/MacOS/$bin
      # If the user ran a simple `cargo build --release`, then we want to allow
      # a single-arch package to be built
      elif [[ -f $TARGET_DIR/release/$bin ]] ; then
        cp $TARGET_DIR/release/$bin $zipdir/ThinkTerm.app/Contents/MacOS/$bin
      else
        # The CI runs `cargo build --target XXX --release` which means that
        # the binaries will be deployed in `$TARGET_DIR/XXX/release` instead of
        # the plain path above.
        # In that situation, we have two architectures to assemble into a
        # Universal ("fat") binary, so we use the `lipo` tool for that.
        lipo $TARGET_DIR/*/release/$bin -output $zipdir/ThinkTerm.app/Contents/MacOS/$bin -create
      fi
    done

    set +x
    # Only a Developer ID signature is eligible for notarization; the notary
    # service rejects adhoc and Apple Development identities outright.
    notarize=no
    if [[ -n "${MACOS_SIGNING_MODE:-}" ]] ; then
      bash ci/macos-sign-local.sh "$zipdir/ThinkTerm.app" "$MACOS_SIGNING_MODE"
      if [[ "$MACOS_SIGNING_MODE" == developerid ]] ; then
        notarize=yes
      fi
    elif [ -n "$MACOS_TEAM_ID" ] ; then
      MACOS_PW=$(echo $MACOS_CERT_PW | base64 --decode)
      echo "pw sha"
      echo $MACOS_PW | shasum

      # Remove pesky additional quotes from default-keychain output
      def_keychain=$(eval echo $(security default-keychain -d user))
      echo "Default keychain is $def_keychain"
      echo "Speculative delete of build.keychain"
      security delete-keychain build.keychain || true
      echo "Create build.keychain"
      security create-keychain -p "$MACOS_PW" build.keychain
      echo "Make build.keychain the default"
      security default-keychain -d user -s build.keychain
      echo "Unlock build.keychain"
      security unlock-keychain -p "$MACOS_PW" build.keychain
      echo "Import .p12 data"
      echo $MACOS_CERT | base64 --decode > /tmp/certificate.p12
      echo "decoded sha"
      shasum /tmp/certificate.p12
      security import /tmp/certificate.p12 -k build.keychain -P "$MACOS_PW" -T /usr/bin/codesign
      rm /tmp/certificate.p12
      echo "Grant apple tools access to build.keychain"
      security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$MACOS_PW" build.keychain
      echo "Codesign"
      /usr/bin/codesign --keychain build.keychain --force --options runtime \
        --entitlements ci/macos-entitlement.plist --deep --sign "$MACOS_TEAM_ID" $zipdir/ThinkTerm.app/
      echo "Restore default keychain"
      security default-keychain -d user -s $def_keychain
      echo "Remove build.keychain"
      security delete-keychain build.keychain || true
      notarize=yes
    else
      # A normal local package should still be a correctly sealed app bundle.
      # Development/Developer ID identities remain opt-in, but ad-hoc signing
      # is a safer and less surprising default than producing an invalid app.
      bash ci/macos-sign-local.sh "$zipdir/ThinkTerm.app" adhoc
    fi

    # Notarize and staple before packing, not after.  Stapling rewrites the
    # bundle, so an archive built first would ship without the ticket and make
    # every first launch wait on a round trip to Apple's servers -- or fail
    # outright behind a firewall that blocks them.
    if [[ "$notarize" == yes ]] ; then
      bash ci/macos-notarize-local.sh "$zipdir/ThinkTerm.app"
    fi

    set -x
    # --sequesterRsrc is what makes this safe to hand to plain `unzip`.  macOS
    # stamps com.apple.provenance on every file, so ditto emits an AppleDouble
    # ._ sidecar for each one; without sequestering they land inside the bundle
    # on extraction, and codesign then reports "a sealed resource is missing or
    # invalid" -- Finder calls that "damaged".  Sequestering routes them to a
    # sibling __MACOSX/ that extractors ignore.
    /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$zipdir" "$zipname"

    # The cask covers both Macs, but one run of this script builds one archive.
    # Read each hash off the archive itself, so packaging the second
    # architecture -- in either order -- ends with a cask that names both, and
    # a hash can never describe anything other than a file that is really here.
    macos_sha() {
      local zip="ThinkTerm-macos-$1-$TAG_NAME.zip"
      if [[ -f "$zip" ]] ; then
        shasum -a 256 "$zip" | cut -d' ' -f1
      else
        echo "@SHA256_$2@"
      fi
    }
    sed -e "s/@TAG@/$TAG_NAME/g" \
      -e "s/@SHA256_ARM64@/$(macos_sha arm64 ARM64)/g" \
      -e "s/@SHA256_X86_64@/$(macos_sha x86_64 X86_64)/g" \
      < ci/wezterm-homebrew-macos.rb.template > wezterm.rb

    ;;
  # Every Windows bash reports something different here -- Git Bash and MSYS2
  # say msys, Cygwin says cygwin, older mingw builds say win32 or mingw -- and
  # a bare `msys)` silently fell through to the catch-all on GitHub's runner,
  # producing no package at all while still exiting 0.
  msys* | cygwin* | mingw* | win32*)
    zipdir=ThinkTerm-windows-$TAG_NAME
    if [[ "$BUILD_REASON" == "Schedule" ]] ; then
      zipname=ThinkTerm-windows-nightly.zip
      instname=ThinkTerm-nightly-setup
    else
      zipname=$zipdir.zip
      instname=ThinkTerm-${TAG_NAME}-setup
    fi
    rm -rf $zipdir $zipname
    mkdir $zipdir
    cp $TARGET_DIR/release/thinkterm.exe \
      $TARGET_DIR/release/wezterm.exe \
      $TARGET_DIR/release/thinkterm-mux-server.exe \
      $TARGET_DIR/release/thinkterm-gui.exe \
      $TARGET_DIR/release/strip-ansi-escapes.exe \
      assets/windows/conhost/conpty.dll \
      assets/windows/conhost/OpenConsole.exe \
      assets/windows/angle/libEGL.dll \
      assets/windows/angle/libGLESv2.dll \
      LICENSE.md \
      LICENSE-MIT \
      NOTICE \
      $zipdir

    # `[profile.release]` leaves `debug` off, so no PDBs exist today and
    # copying them unconditionally would abort the whole step under `set -e`.
    # Glob rather than name them: turning debuginfo on should ship symbols for
    # every binary, not just the two that happened to be listed here.
    cp $TARGET_DIR/release/*.pdb $zipdir 2>/dev/null || true

    # Same source as the four DLLs above -- wezterm-gui's build script only
    # stages a copy of it next to the exe, so take it from assets directly.
    mkdir $zipdir/mesa
    cp assets/windows/mesa/opengl32.dll $zipdir/mesa
    7z a -tzip $zipname $zipdir
    iscc.exe -DMyAppVersion=${TAG_NAME#nightly} -F${instname} ci/windows-installer.iss
    ;;
  linux-gnu|linux)
    distro=$(lsb_release -is 2>/dev/null || sh -c "source /etc/os-release && echo \$NAME")
    distver=$(lsb_release -rs 2>/dev/null || sh -c "source /etc/os-release && echo \$VERSION_ID")
    case "$distro" in
      *Fedora*|*CentOS*|*SUSE*)
        THINKTERM_RPM_VERSION=$(echo ${TAG_NAME#nightly-} | tr - _)
        distroid=$(sh -c "source /etc/os-release && echo \$ID" | tr - _)
        distver=$(sh -c "source /etc/os-release && echo \$VERSION_ID" | tr - _)

        SPEC_RELEASE="1.${distroid}${distver}"
        if test -n "${COPR_SRPM}" ; then
          SPEC_RELEASE=0
        fi

        # Set up variables for spec generation
        if test -n "${COPR_SRPM}" ; then
          TAR_NAME=$(git -c "core.abbrev=8" show -s "--format=%cd_%h" "--date=format:%Y%m%d_%H%M%S")
          HERE="."
          BUILD_SECTION=$(cat <<'BUILDEOFEOF'
%prep
%autosetup
%build

echo Here I am

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

cargo build --release \
      -p wezterm-gui -p wezterm -p wezterm-mux-server \
      -p strip-ansi-escapes
BUILDEOFEOF
)
          BUILD_REQUIRES=$(cat <<BREQEOF
BuildRequires: gcc, gcc-c++, make, curl, fontconfig-devel, openssl-devel, libxcb-devel, libxkbcommon-devel, libxkbcommon-x11-devel, wayland-devel, xcb-util-devel, xcb-util-keysyms-devel, xcb-util-image-devel, xcb-util-wm-devel, git
%if 0%{?suse_version}
BuildRequires: Mesa-libEGL-devel
%else
BuildRequires: mesa-libEGL-devel
%endif
%if 0%{?fedora} >= 41
BuildRequires: openssl-devel-engine
%endif
Source0: wezterm-${TAR_NAME}.tar.gz
BREQEOF
)
        else
          HERE="${HERE}"
          BUILD_SECTION=$(cat <<'BUILDEOFEOF'
%build
echo build
BUILDEOFEOF
)
          BUILD_REQUIRES=""
        fi

        # Generate single spec with subpackages
        cat > thinkterm.spec <<EOF
Name: thinkterm
Version: ${THINKTERM_RPM_VERSION}
Release: ${SPEC_RELEASE}
Packager: RoversX
License: GPL-3.0-only
URL: https://github.com/RoversX/thinkterm
Summary: ThinkTerm workspace-first terminal emulator.
${BUILD_REQUIRES}
Requires: thinkterm-common, thinkterm-gui, thinkterm-mux-server

%global debug_package %{nil}

%description
ThinkTerm is a terminal emulator with support for modern features
such as fonts with ligatures, hyperlinks, tabs and multiple
windows.

# Subpackage: thinkterm-common
%package -n thinkterm-common
Summary: ThinkTerm - Common CLI components
Requires: openssl
%description -n thinkterm-common
thinkterm-common provides the base CLI launcher and utilities shared by
all ThinkTerm components.

# Subpackage: thinkterm-gui
%package -n thinkterm-gui
Summary: ThinkTerm - GUI and multiplexer
Requires: thinkterm-common
%if 0%{?suse_version}
Requires: dbus-1, fontconfig, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1, libwayland-cursor0, Mesa-libEGL1, libxcb-keysyms1, libxcb-ewmh2, libxcb-icccm4
%else
Requires: dbus, fontconfig, libxcb, libxkbcommon, libxkbcommon-x11, libwayland-client, libwayland-egl, libwayland-cursor, mesa-libEGL, xcb-util-keysyms, xcb-util-wm
%endif
%description -n thinkterm-gui
thinkterm-gui is a GPU-accelerated cross-platform terminal emulator with
support for modern features such as fonts with ligatures, hyperlinks,
tabs and multiple windows.

# Subpackage: thinkterm-mux-server
%package -n thinkterm-mux-server
Summary: ThinkTerm - Multiplexer server (headless)
Requires: openssl
%description -n thinkterm-mux-server
thinkterm-mux-server is a headless terminal multiplexer that can be used
as a session manager for terminal sessions, without requiring X11,
Wayland, or other GUI libraries.

${BUILD_SECTION}

%install
set -x
cd ${HERE}
mkdir -p %{buildroot}/usr/bin %{buildroot}/etc/profile.d %{buildroot}/usr/share/icons/hicolor/128x128/apps %{buildroot}/usr/share/applications %{buildroot}/usr/share/metainfo %{buildroot}/usr/share/nautilus-python/extensions
install -Dm755 assets/open-thinkterm-here assets/open-wezterm-here -t %{buildroot}/usr/bin
install -Dsm755 $TARGET_DIR/release/thinkterm -t %{buildroot}/usr/bin
install -Dsm755 $TARGET_DIR/release/wezterm -t %{buildroot}/usr/bin
install -Dsm755 $TARGET_DIR/release/thinkterm-gui -t %{buildroot}/usr/bin
install -Dsm755 $TARGET_DIR/release/thinkterm-mux-server -t %{buildroot}/usr/bin
install -Dsm755 $TARGET_DIR/release/strip-ansi-escapes -t %{buildroot}/usr/bin
install -Dm644 assets/shell-integration/* -t %{buildroot}/etc/profile.d
install -Dm644 assets/shell-completion/zsh %{buildroot}/usr/share/zsh/site-functions/_thinkterm
install -Dm644 assets/shell-completion/bash %{buildroot}/etc/bash_completion.d/thinkterm
install -Dm644 assets/shell-completion/fish %{buildroot}/usr/share/fish/vendor_completions.d/thinkterm.fish
ln -s thinkterm %{buildroot}/etc/bash_completion.d/wezterm
ln -s thinkterm.fish %{buildroot}/usr/share/fish/vendor_completions.d/wezterm.fish
install -Dm644 assets/icon/terminal.png %{buildroot}/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
install -Dm644 assets/wezterm.desktop %{buildroot}/usr/share/applications/com.roversx.thinkterm.desktop
install -Dm644 assets/wezterm.appdata.xml %{buildroot}/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
install -Dm644 assets/wezterm-nautilus.py %{buildroot}/usr/share/nautilus-python/extensions/wezterm-nautilus.py
install -Dm644 NOTICE %{buildroot}/usr/share/licenses/thinkterm/NOTICE
install -Dm644 LICENSE.md %{buildroot}/usr/share/licenses/thinkterm/LICENSE.md
install -Dm644 LICENSE-MIT %{buildroot}/usr/share/licenses/thinkterm/LICENSE-MIT
# A second copy owned by the standalone mux-server package: it links the
# same third-party material and installs without thinkterm-common, and
# one file owned by two packages would conflict on co-install.
install -Dm644 NOTICE %{buildroot}/usr/share/licenses/thinkterm-mux-server/NOTICE
install -Dm644 LICENSE.md %{buildroot}/usr/share/licenses/thinkterm-mux-server/LICENSE.md
install -Dm644 LICENSE-MIT %{buildroot}/usr/share/licenses/thinkterm-mux-server/LICENSE-MIT

%files
# Main package (metapackage) has no files

%files -n thinkterm-common
/usr/bin/thinkterm
/usr/bin/wezterm
/usr/bin/strip-ansi-escapes
/usr/share/licenses/thinkterm/*
/usr/share/zsh/site-functions/_thinkterm
/usr/share/fish/vendor_completions.d/thinkterm.fish
/usr/share/fish/vendor_completions.d/wezterm.fish
/etc/bash_completion.d/thinkterm
/etc/bash_completion.d/wezterm
/etc/profile.d/*

%files -n thinkterm-gui
/usr/bin/open-thinkterm-here
/usr/bin/open-wezterm-here
/usr/bin/thinkterm-gui
/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
/usr/share/applications/com.roversx.thinkterm.desktop
/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
/usr/share/nautilus-python/extensions/wezterm-nautilus.py*

%files -n thinkterm-mux-server
/usr/bin/thinkterm-mux-server
/usr/share/licenses/thinkterm-mux-server/*

%changelog
* Fri Sep 4 2026 RoversX
- See git for full changelog
EOF

        if test -n "${COPR_SRPM}" ; then
          /usr/bin/rpmbuild -bs --rmspec thinkterm.spec --verbose
          mv $(rpm --eval '%{_srcrpmdir}')/thinkterm-${TAR_NAME}*.src.rpm "${COPR_SRPM}"/
        else
          /usr/bin/rpmbuild -bb --rmspec thinkterm.spec --verbose
        fi

        ;;
      Ubuntu*|Debian*|Pop)
        rm -rf pkg
        mkdir -p pkg/debian/usr/bin pkg/debian/DEBIAN pkg/debian/usr/share/{applications,wezterm}

        if [[ "$BUILD_REASON" == "Schedule" ]] ; then
          pkgname=thinkterm-nightly
          conflicts=thinkterm
        else
          pkgname=thinkterm
          conflicts=thinkterm-nightly
        fi

        cat > pkg/debian/control <<EOF
Package: $pkgname
Version: ${TAG_NAME#nightly-}
Conflicts: $conflicts
Architecture: $(dpkg-architecture -q DEB_BUILD_ARCH_CPU)
Maintainer: RoversX
Section: utils
Priority: optional
Homepage: https://github.com/RoversX/thinkterm
Description: ThinkTerm workspace-first terminal emulator.
 ThinkTerm is a terminal emulator with support for modern features
 such as fonts with ligatures, hyperlinks, tabs and multiple
 windows.
Provides: x-terminal-emulator
Source: https://github.com/RoversX/thinkterm
EOF

        cat > pkg/debian/postinst <<EOF
#!/bin/sh
set -e
if [ "\$1" = "configure" ] ; then
        update-alternatives --remove x-terminal-emulator /usr/bin/open-wezterm-here
        update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator /usr/bin/open-thinkterm-here 20
fi
EOF

        cat > pkg/debian/prerm <<EOF
#!/bin/sh
set -e
if [ "\$1" = "remove" ]; then
	update-alternatives --remove x-terminal-emulator /usr/bin/open-thinkterm-here
	update-alternatives --remove x-terminal-emulator /usr/bin/open-wezterm-here
fi
EOF

        install -Dsm755 -t pkg/debian/usr/bin $TARGET_DIR/release/thinkterm-mux-server
        install -Dsm755 -t pkg/debian/usr/bin $TARGET_DIR/release/thinkterm-gui
        install -Dsm755 -t pkg/debian/usr/bin $TARGET_DIR/release/thinkterm
        install -Dsm755 -t pkg/debian/usr/bin $TARGET_DIR/release/wezterm
        install -Dm755 -t pkg/debian/usr/bin assets/open-thinkterm-here assets/open-wezterm-here
        install -Dsm755 -t pkg/debian/usr/bin $TARGET_DIR/release/strip-ansi-escapes

        deps=$(cd pkg && dpkg-shlibdeps -O -e debian/usr/bin/*)
        mv pkg/debian/postinst pkg/debian/DEBIAN/postinst
        chmod 0755 pkg/debian/DEBIAN/postinst
        mv pkg/debian/prerm pkg/debian/DEBIAN/prerm
        chmod 0755 pkg/debian/DEBIAN/prerm
        mv pkg/debian/control pkg/debian/DEBIAN/control
        sed -i '/^Source:/d' pkg/debian/DEBIAN/control  # The `Source:` field needs to be valid in a binary package
        echo $deps | sed -e 's/shlibs:Depends=/Depends: /' >> pkg/debian/DEBIAN/control
        cat pkg/debian/DEBIAN/control

        install -Dm644 assets/icon/terminal.png pkg/debian/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
        install -Dm644 assets/wezterm.desktop pkg/debian/usr/share/applications/com.roversx.thinkterm.desktop
        install -Dm644 assets/wezterm.appdata.xml pkg/debian/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
        install -Dm644 assets/wezterm-nautilus.py pkg/debian/usr/share/nautilus-python/extensions/wezterm-nautilus.py
        install -Dm644 assets/shell-completion/bash pkg/debian/usr/share/bash-completion/completions/thinkterm
        install -Dm644 assets/shell-completion/zsh pkg/debian/usr/share/zsh/functions/Completion/Unix/_thinkterm
        install -Dm644 assets/shell-completion/fish pkg/debian/usr/share/fish/vendor_completions.d/thinkterm.fish
        ln -s thinkterm pkg/debian/usr/share/bash-completion/completions/wezterm
        ln -s thinkterm.fish pkg/debian/usr/share/fish/vendor_completions.d/wezterm.fish
        install -Dm644 assets/shell-integration/* -t pkg/debian/etc/profile.d
        install -Dm644 NOTICE pkg/debian/usr/share/doc/$pkgname/NOTICE
        install -Dm644 LICENSE.md pkg/debian/usr/share/doc/$pkgname/LICENSE.md
        install -Dm644 LICENSE-MIT pkg/debian/usr/share/doc/$pkgname/LICENSE-MIT

        if [[ "$BUILD_REASON" == "Schedule" ]] ; then
          debname=thinkterm-nightly.$distro$distver
        else
          debname=thinkterm-$TAG_NAME.$distro$distver
        fi
        arch=$(dpkg-architecture -q DEB_BUILD_ARCH_CPU)
        case $arch in
          amd64)
            ;;
          *)
            debname="${debname}.${arch}"
            ;;
        esac

        fakeroot dpkg-deb --build pkg/debian $debname.deb

        if [[ "$BUILD_REASON" != '' ]] ; then
          $SUDO apt-get install ./$debname.deb
        fi

        mv pkg/debian pkg/thinkterm
        tar cJf $debname.tar.xz -C pkg thinkterm
        rm -rf pkg
      ;;
    esac
    ;;
  linux-musl)
    case $ID in
      alpine)
        export SUDO=''
        abuild-keygen -a -n -b 8192
        pkgver="${TAG_NAME#nightly-}"
        cat > APKBUILD <<EOF
# Maintainer: RoversX
pkgname=thinkterm
pkgver=$(echo "$pkgver" | cut -d'-' -f1-2 | tr - .)
_pkgver=$pkgver
pkgrel=0
pkgdesc="A workspace-first terminal emulator and multiplexer written in Rust"
license="GPL-3.0-only"
arch="all"
options="!check"
url="https://github.com/RoversX/thinkterm"
makedepends="cmd:tic"
source="
  $TARGET_DIR/release/thinkterm
  $TARGET_DIR/release/wezterm
  $TARGET_DIR/release/thinkterm-gui
  $TARGET_DIR/release/thinkterm-mux-server
  assets/open-thinkterm-here
  assets/open-wezterm-here
  assets/wezterm.desktop
  assets/wezterm.appdata.xml
  assets/icon/terminal.png
  assets/icon/wezterm-icon.svg
  termwiz/data/wezterm.terminfo
  NOTICE
  LICENSE.md
  LICENSE-MIT
"
builddir="\$srcdir"

build() {
  tic -x -o "\$builddir"/wezterm.terminfo "\$srcdir"/wezterm.terminfo
}

package() {
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/open-thinkterm-here
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/open-wezterm-here
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/thinkterm
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/wezterm
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/thinkterm-gui
  install -Dm755 -t "\$pkgdir"/usr/bin "\$srcdir"/thinkterm-mux-server

  install -Dm644 "\$srcdir"/wezterm.desktop "\$pkgdir"/usr/share/applications/com.roversx.thinkterm.desktop
  install -Dm644 "\$srcdir"/wezterm.appdata.xml "\$pkgdir"/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
  install -Dm644 "\$srcdir"/terminal.png "\$pkgdir"/usr/share/pixmaps/com.roversx.thinkterm.png
  install -Dm644 "\$srcdir"/wezterm-icon.svg "\$pkgdir"/usr/share/pixmaps/com.roversx.thinkterm.svg
  install -Dm644 "\$srcdir"/terminal.png "\$pkgdir"/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
  install -Dm644 "\$srcdir"/wezterm-icon.svg "\$pkgdir"/usr/share/icons/hicolor/scalable/apps/com.roversx.thinkterm.svg
  install -Dm644 "\$builddir"/wezterm.terminfo "\$pkgdir"/usr/share/terminfo/w/wezterm
  install -Dm644 "\$srcdir"/NOTICE "\$pkgdir"/usr/share/licenses/thinkterm/NOTICE
  install -Dm644 "\$srcdir"/LICENSE.md "\$pkgdir"/usr/share/licenses/thinkterm/LICENSE.md
  install -Dm644 "\$srcdir"/LICENSE-MIT "\$pkgdir"/usr/share/licenses/thinkterm/LICENSE-MIT
}
EOF
        abuild -F checksum
        abuild -Fr
      ;;
    esac
    ;;
  *)
    # Refusing here is the point: an unrecognised platform used to package
    # nothing and report success, so the failure only surfaced later as an
    # empty artifact upload -- or not at all, had the upload been lenient.
    echo "ci/deploy.sh: don't know how to package for OSTYPE '$OSTYPE'" >&2
    exit 1
    ;;
esac
