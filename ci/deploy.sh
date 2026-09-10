#!/bin/bash
set -x

# Whether the browser client was built (ci/build-web.sh). A package without
# it has a server that accepts browser connections and serves no page. The
# release workflow always builds it, so there its absence is a broken run
# and this fails; on a laptop it is only a warning, since a local package
# may deliberately leave it out.
require_web_bundle() {
  if [[ -f thinkterm-web/www/pkg/thinkterm_web.js ]] ; then
    return 0
  fi
  if [[ -n "${CI:-}" ]] ; then
    echo "error: no browser bundle in thinkterm-web/www; ci/build-web.sh did not run before deploy.sh" >&2
    exit 1
  fi
  echo "warning: no browser bundle in thinkterm-web/www (run ci/build-web.sh); packaging without the web client" >&2
  return 1
}
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
    # The browser client, when ci/build-web.sh ran before this: the server
    # inside the bundle finds it at <exe>/../Resources/web.
    if require_web_bundle ; then
      mkdir -p $zipdir/ThinkTerm.app/Contents/Resources/web
      # -p throughout: the server decides whether a .gz still stands for
      # its file by comparing their modification times, and a copy that
      # rewrites them can make a stale sibling look current.
      cp -p thinkterm-web/www/index.html thinkterm-web/www/schemes.json $zipdir/ThinkTerm.app/Contents/Resources/web/
      # The precompressed siblings, when ci/build-web.sh made them.
      for gz in thinkterm-web/www/*.gz ; do
        [[ -f "$gz" ]] && cp -p "$gz" $zipdir/ThinkTerm.app/Contents/Resources/web/
      done
      cp -Rp thinkterm-web/www/assets thinkterm-web/www/pkg thinkterm-web/www/fonts $zipdir/ThinkTerm.app/Contents/Resources/web/
    fi
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
    # The browser client, beside the executables: the server looks for
    # <exe>/web on a flat layout like this one. -p keeps the modification
    # times the .gz freshness check reads.
    if require_web_bundle ; then
      mkdir -p $zipdir/web
      cp -p thinkterm-web/www/index.html thinkterm-web/www/schemes.json $zipdir/web/
      for gz in thinkterm-web/www/*.gz ; do
        [[ -f "$gz" ]] && cp -p "$gz" $zipdir/web/
      done
      cp -Rp thinkterm-web/www/assets thinkterm-web/www/pkg thinkterm-web/www/fonts $zipdir/web/
    fi
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

        # One spec, two packages, each complete on its own and refusing to
        # co-install with the other. Distribution is a file from the releases
        # page, where every dependency is another download the user has to
        # find; a metapackage over subpackages produced exactly that failure.
        #
        #   thinkterm         GUI + CLI (with the TUI linked in) + mux server
        #   thinkterm-server  the same without the GUI, so no graphics library
        #
        # Files the two share are listed under both: rpm allows it within a
        # spec, and the Conflicts guarantee they are never installed together.
        cat > thinkterm.spec <<EOF
Name: thinkterm
Version: ${THINKTERM_RPM_VERSION}
Release: ${SPEC_RELEASE}
Packager: RoversX
License: GPL-3.0-only
URL: https://github.com/RoversX/thinkterm
Summary: ThinkTerm workspace-first terminal emulator.
${BUILD_REQUIRES}
Conflicts: thinkterm-server, wezterm
Requires: openssl
%if 0%{?suse_version}
Requires: dbus-1, fontconfig, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1, libwayland-cursor0, Mesa-libEGL1, libxcb-keysyms1, libxcb-ewmh2, libxcb-icccm4
%else
Requires: dbus, fontconfig, libxcb, libxkbcommon, libxkbcommon-x11, libwayland-client, libwayland-egl, libwayland-cursor, mesa-libEGL, xcb-util-keysyms, xcb-util-wm
%endif

%global debug_package %{nil}

%description
ThinkTerm is a terminal emulator with support for modern features
such as fonts with ligatures, hyperlinks, tabs and multiple
windows. This package carries the GUI, the command line (including
the TUI) and the multiplexer server.

%package -n thinkterm-server
Summary: ThinkTerm - command line, TUI and multiplexer server, no GUI
Conflicts: thinkterm, wezterm
Requires: openssl
%description -n thinkterm-server
thinkterm-server is ThinkTerm without the GUI: the thinkterm command
line (with the TUI), the wezterm compatibility shim and the headless
multiplexer server, for hosts that are reached over SSH. It needs no
X11, Wayland or other graphics library.

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
# The server package owns its own copy under its own name, so either
# package can be installed on its own with the licenses it needs.
install -Dm644 NOTICE %{buildroot}/usr/share/licenses/thinkterm-server/NOTICE
install -Dm644 LICENSE.md %{buildroot}/usr/share/licenses/thinkterm-server/LICENSE.md
install -Dm644 LICENSE-MIT %{buildroot}/usr/share/licenses/thinkterm-server/LICENSE-MIT

%files
/usr/bin/thinkterm
/usr/bin/wezterm
/usr/bin/thinkterm-gui
/usr/bin/thinkterm-mux-server
/usr/bin/strip-ansi-escapes
/usr/bin/open-thinkterm-here
/usr/bin/open-wezterm-here
/usr/share/licenses/thinkterm/*
/usr/share/zsh/site-functions/_thinkterm
/usr/share/fish/vendor_completions.d/thinkterm.fish
/usr/share/fish/vendor_completions.d/wezterm.fish
/etc/bash_completion.d/thinkterm
/etc/bash_completion.d/wezterm
/etc/profile.d/*
/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
/usr/share/applications/com.roversx.thinkterm.desktop
/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
/usr/share/nautilus-python/extensions/wezterm-nautilus.py*

%files -n thinkterm-server
/usr/bin/thinkterm
/usr/bin/wezterm
/usr/bin/thinkterm-mux-server
/usr/bin/strip-ansi-escapes
/usr/share/licenses/thinkterm-server/*
/usr/share/zsh/site-functions/_thinkterm
/usr/share/fish/vendor_completions.d/thinkterm.fish
/usr/share/fish/vendor_completions.d/wezterm.fish
/etc/bash_completion.d/thinkterm
/etc/bash_completion.d/wezterm
/etc/profile.d/*

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
        # Two debs, each complete on its own and conflicting with the other:
        #
        #   thinkterm         GUI + CLI (with the TUI linked in) + mux server
        #   thinkterm-server  the same without the GUI
        #
        # Each is assembled in its own tree so that dpkg-shlibdeps computes
        # its Depends from its own binaries: a shared tree would hand the
        # server package the GUI's X11/Wayland/EGL dependencies, which is the
        # whole thing the split exists to avoid.
        arch=$(dpkg-architecture -q DEB_BUILD_ARCH_CPU)

        # build_deb <variant>: variant is "desktop" or "server".
        build_deb() {
          local variant=$1 pkgname conflicts debname root other
          # Four package names exist: the two variants, each with a nightly
          # twin. Whichever one this is, it conflicts with the other three.
          pkgname=thinkterm
          [[ "$variant" != server ]] || pkgname=thinkterm-server
          debname=$pkgname-$TAG_NAME
          if [[ "$BUILD_REASON" == "Schedule" ]] ; then
            pkgname=$pkgname-nightly
            debname=$pkgname
          fi
          # ... and with a real wezterm package, which owns /usr/bin/wezterm too.
          conflicts=wezterm
          for other in thinkterm thinkterm-nightly thinkterm-server thinkterm-server-nightly ; do
            [[ "$other" == "$pkgname" ]] && continue
            conflicts="$conflicts, $other"
          done
          debname=$debname.$distro$distver
          case $arch in
            amd64) ;;
            *) debname="${debname}.${arch}" ;;
          esac

          root=pkg/$variant/debian
          rm -rf "pkg/$variant"
          mkdir -p $root/usr/bin $root/DEBIAN

          if [[ "$variant" == desktop ]] ; then
            cat > $root/DEBIAN/control <<EOF
Package: $pkgname
Version: ${TAG_NAME#nightly-}
Conflicts: $conflicts
Architecture: $arch
Maintainer: RoversX
Section: utils
Priority: optional
Homepage: https://github.com/RoversX/thinkterm
Description: ThinkTerm workspace-first terminal emulator.
 ThinkTerm is a terminal emulator with support for modern features
 such as fonts with ligatures, hyperlinks, tabs and multiple
 windows. This package carries the GUI, the command line (including
 the TUI) and the multiplexer server.
Provides: x-terminal-emulator
EOF
            cat > $root/DEBIAN/postinst <<EOF
#!/bin/sh
set -e
if [ "\$1" = "configure" ] ; then
        update-alternatives --remove x-terminal-emulator /usr/bin/open-wezterm-here
        update-alternatives --install /usr/bin/x-terminal-emulator x-terminal-emulator /usr/bin/open-thinkterm-here 20
fi
EOF
            cat > $root/DEBIAN/prerm <<EOF
#!/bin/sh
set -e
if [ "\$1" = "remove" ]; then
	update-alternatives --remove x-terminal-emulator /usr/bin/open-thinkterm-here
	update-alternatives --remove x-terminal-emulator /usr/bin/open-wezterm-here
fi
EOF
            chmod 0755 $root/DEBIAN/postinst $root/DEBIAN/prerm
          else
            cat > $root/DEBIAN/control <<EOF
Package: $pkgname
Version: ${TAG_NAME#nightly-}
Conflicts: $conflicts
Architecture: $arch
Maintainer: RoversX
Section: utils
Priority: optional
Homepage: https://github.com/RoversX/thinkterm
Description: ThinkTerm command line, TUI and multiplexer server, no GUI.
 ThinkTerm without the GUI: the thinkterm command line (with the TUI),
 the wezterm compatibility shim and the headless multiplexer server,
 for hosts that are reached over SSH. It needs no X11, Wayland or
 other graphics library.
EOF
          fi

          install -Dsm755 -t $root/usr/bin $TARGET_DIR/release/thinkterm-mux-server
          install -Dsm755 -t $root/usr/bin $TARGET_DIR/release/thinkterm
          install -Dsm755 -t $root/usr/bin $TARGET_DIR/release/wezterm
          install -Dsm755 -t $root/usr/bin $TARGET_DIR/release/strip-ansi-escapes
          if [[ "$variant" == desktop ]] ; then
            install -Dsm755 -t $root/usr/bin $TARGET_DIR/release/thinkterm-gui
            install -Dm755 -t $root/usr/bin assets/open-thinkterm-here assets/open-wezterm-here
            install -Dm644 assets/icon/terminal.png $root/usr/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png
            install -Dm644 assets/wezterm.desktop $root/usr/share/applications/com.roversx.thinkterm.desktop
            install -Dm644 assets/wezterm.appdata.xml $root/usr/share/metainfo/com.roversx.thinkterm.appdata.xml
            install -Dm644 assets/wezterm-nautilus.py $root/usr/share/nautilus-python/extensions/wezterm-nautilus.py
          fi

          # dpkg-shlibdeps reads source control information at debian/control,
          # separately from the binary package's DEBIAN/control.
          {
            printf 'Source: thinkterm\nMaintainer: RoversX\n\n'
            cat "$root/DEBIAN/control"
          } > "$root/control"
          local deps
          deps=$(cd "pkg/$variant" && dpkg-shlibdeps -O -e debian/usr/bin/*)
          rm "$root/control"
          echo $deps | sed -e 's/shlibs:Depends=/Depends: /' >> $root/DEBIAN/control
          cat $root/DEBIAN/control

          install -Dm644 assets/shell-completion/bash $root/usr/share/bash-completion/completions/thinkterm
          install -Dm644 assets/shell-completion/zsh $root/usr/share/zsh/functions/Completion/Unix/_thinkterm
          install -Dm644 assets/shell-completion/fish $root/usr/share/fish/vendor_completions.d/thinkterm.fish
          ln -s thinkterm $root/usr/share/bash-completion/completions/wezterm
          ln -s thinkterm.fish $root/usr/share/fish/vendor_completions.d/wezterm.fish
          install -Dm644 assets/shell-integration/* -t $root/etc/profile.d
          install -Dm644 NOTICE $root/usr/share/doc/$pkgname/NOTICE
          install -Dm644 LICENSE.md $root/usr/share/doc/$pkgname/LICENSE.md
          install -Dm644 LICENSE-MIT $root/usr/share/doc/$pkgname/LICENSE-MIT

          fakeroot dpkg-deb --build $root $debname.deb

          if [[ "$variant" == desktop ]] ; then
            # Installing the desktop deb is the smoke test that its Depends
            # resolve on this distro. Only one of the two can be installed
            # at a time, and this is the one with dependencies worth testing.
            if [[ "$BUILD_REASON" != '' ]] ; then
              $SUDO apt-get install ./$debname.deb
            fi
            # The raw tree, for people who want the files without dpkg.
            mv $root pkg/$variant/thinkterm
            tar cJf $debname.tar.xz -C pkg/$variant thinkterm
          fi
          rm -rf "pkg/$variant"
        }
        build_deb desktop
        build_deb server
        rm -rf pkg

        # The tarballs install.sh downloads: one per variant, both built here
        # rather than in the Fedora leg because this is the oldest glibc we
        # build on, and a tarball has no package manager to refuse a host
        # whose libc is too old -- install.sh checks the version instead.
        #
        #   thinkterm-<ver>-linux-<arch>         GUI + CLI (with the TUI linked
        #                                        in) + mux server
        #   thinkterm-server-<ver>-linux-<arch>  the same without the GUI, so
        #                                        it needs no graphics library
        #
        # Both carry the wezterm shim that third-party tools expect on PATH
        # whenever TERM_PROGRAM says WezTerm. The arch is uname -m as-is
        # (x86_64, aarch64): install.sh runs the same command on the target
        # host and wants a name it can build without a mapping table.
        linux_tarball() {
          local variant=$1 tardir
          shift
          if [[ "$BUILD_REASON" == "Schedule" ]] ; then
            tardir=$variant-nightly-linux-$(uname -m)
          else
            tardir=$variant-$TAG_NAME-linux-$(uname -m)
          fi
          rm -rf "$tardir" "$tardir.tar.gz"
          install -Dsm755 -t "$tardir/bin" "$@"
          install -Dm644 -t "$tardir/share/shell-completion" assets/shell-completion/*
          install -Dm644 -t "$tardir/share/shell-integration" assets/shell-integration/*
          install -Dm644 -t "$tardir" NOTICE LICENSE.md LICENSE-MIT
          # The browser client, when ci/build-web.sh ran before this: the
          # server finds it at <exe>/../share/thinkterm/web.
          if require_web_bundle ; then
            # -p keeps the modification times the .gz freshness check reads.
            install -Dpm644 -t "$tardir/share/thinkterm/web" thinkterm-web/www/index.html thinkterm-web/www/schemes.json
            for gz in thinkterm-web/www/*.gz ; do
              [[ -f "$gz" ]] && install -Dpm644 -t "$tardir/share/thinkterm/web" "$gz"
            done
            install -Dpm644 -t "$tardir/share/thinkterm/web/assets" thinkterm-web/www/assets/*
            install -Dpm644 -t "$tardir/share/thinkterm/web/pkg" thinkterm-web/www/pkg/*
            install -Dpm644 -t "$tardir/share/thinkterm/web/fonts" thinkterm-web/www/fonts/*
          fi
          if [[ "$variant" == thinkterm ]] ; then
            install -Dm755 -t "$tardir/bin" assets/open-thinkterm-here assets/open-wezterm-here
            install -Dm644 assets/wezterm.desktop "$tardir/share/applications/com.roversx.thinkterm.desktop"
            install -Dm644 assets/icon/terminal.png "$tardir/share/icons/hicolor/128x128/apps/com.roversx.thinkterm.png"
          fi
          # --owner/--group so the archive does not carry the CI runner's
          # uid, which would surface as a stray numeric owner on extraction
          # as root.
          tar czf "$tardir.tar.gz" --owner=0 --group=0 "$tardir"
          rm -rf "$tardir"
        }
        linux_tarball thinkterm \
          $TARGET_DIR/release/thinkterm \
          $TARGET_DIR/release/wezterm \
          $TARGET_DIR/release/thinkterm-mux-server \
          $TARGET_DIR/release/thinkterm-gui \
          $TARGET_DIR/release/strip-ansi-escapes
        linux_tarball thinkterm-server \
          $TARGET_DIR/release/thinkterm \
          $TARGET_DIR/release/wezterm \
          $TARGET_DIR/release/thinkterm-mux-server \
          $TARGET_DIR/release/strip-ansi-escapes
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
