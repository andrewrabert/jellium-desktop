#!/bin/sh
# Run inside the appimage build container (jellium-desktop-appimage:base).
# Bind mounts (set up by `just appimage build`):
#   /src           rw  -- repo root (CEF + submodules must be populated by host)
#   /build         rw  -- cargo + meson incremental state, persists on host
#   /host-output   rw  -- .AppImage output dir
# Env:
#   VERSION        -- version string for the output filename
set -eu

: "${VERSION:?VERSION env must be set}"

ARCH="$(uname -m)"

cd /src

cargo xtask build --out /build
strip /build/*.so /build/jellium-desktop

BUILD=/build

# -- AppDir assembly (ephemeral) --
APPDIR=/tmp/AppDir
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share"

# Binary + CEF resources (CEF finds resources relative to /proc/self/exe)
cp "$BUILD"/jellium-desktop "$APPDIR/usr/bin/"
cp "$BUILD"/*.pak "$APPDIR/usr/bin/"
cp "$BUILD"/icudtl.dat "$APPDIR/usr/bin/"
cp "$BUILD"/v8_context_snapshot.bin "$APPDIR/usr/bin/"
cp -r "$BUILD"/locales "$APPDIR/usr/bin/"
cp "$BUILD"/vk_swiftshader_icd.json "$APPDIR/usr/bin/" 2>/dev/null || true

# CEF's own libs (SwiftShader) live in usr/bin/, separated from system
# GPU libs in usr/lib/ which get removed below.
cp "$BUILD"/libcef.so "$APPDIR/usr/bin/"
cp "$BUILD"/libvk_swiftshader.so "$APPDIR/usr/bin/"

# Ship the system library stack minus glibc and the GPU/driver libs (both
# removed below). The host provides glibc, so the host glibc must be at least
# the build image's (Fedora 44).
# Fedora x86_64: shared libs live in /usr/lib64; /usr/lib holds noarch only.
cp -a /usr/lib64 "$APPDIR/usr/lib"

# mpv lib (xtask build copies it next to jellium-desktop)
cp "$BUILD"/libmpv.so.2 "$APPDIR/usr/lib/"

# Fedora's ffmpeg links GnuTLS; GnuTLS needs a system priority file from
# crypto-policies. Bundle the DEFAULT one; AppRun points GNUTLS_SYSTEM_PRIORITY_FILE at it.
mkdir -p "$APPDIR/usr/share/crypto-policies/DEFAULT"
cp /usr/share/crypto-policies/DEFAULT/gnutls.txt \
   "$APPDIR/usr/share/crypto-policies/DEFAULT/gnutls.txt"

# Desktop integration
mkdir -p "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
         "$APPDIR/usr/share/metainfo"
cp /src/resources/linux/net.nullsum.JelliumDesktop.desktop \
   "$APPDIR/usr/share/applications/"
cp /src/resources/linux/net.nullsum.JelliumDesktop.svg \
   "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
cp /src/resources/linux/net.nullsum.JelliumDesktop.metainfo.xml \
   "$APPDIR/usr/share/metainfo/"

# Strip non-runtime cruft to reduce size
find "$APPDIR/usr/lib" -name '*.a' -delete
rm -rf "$APPDIR/usr/lib/pkgconfig" \
       "$APPDIR/usr/lib/cmake" \
       "$APPDIR/usr/lib/python"* \
       "$APPDIR/usr/lib/perl"* \
       "$APPDIR/usr/lib/ruby"* \
       "$APPDIR/usr/lib/node_modules" \
       "$APPDIR/usr/lib/gcc" \
       "$APPDIR/usr/lib/bfd-plugins" \
       "$APPDIR/usr/lib/ldscripts" \
       "$APPDIR/usr/lib/systemd" \
       "$APPDIR"/usr/lib/libLLVM* \
       "$APPDIR"/usr/lib/llvm* \
       "$APPDIR"/usr/lib/clang* \
       "$APPDIR"/usr/lib/LLVMgold* \
       "$APPDIR"/usr/lib/libLTO* \
       "$APPDIR"/usr/lib/libgallium* \
       "$APPDIR"/usr/lib/dri_gbm.so* \
       "$APPDIR"/usr/lib/gbm/dri_gbm.so* \
       "$APPDIR"/usr/lib/udev* \
       "$APPDIR"/usr/lib/guile* \
       "$APPDIR"/usr/lib/git* \
       "$APPDIR/usr/share/doc" \
       "$APPDIR/usr/share/man" \
       "$APPDIR/usr/share/info" \
       "$APPDIR/usr/share/gtk-doc" \
       "$APPDIR/usr/share/locale" \
       "$APPDIR/usr/share/i18n" \
       "$APPDIR/usr/share/help" \
       "$APPDIR/usr/share/bash-completion" \
       "$APPDIR/usr/share/zsh" \
       "$APPDIR/usr/share/fish" \
       "$APPDIR/usr/share/vala"

# glibc comes from the host (AppImage excludelist); host GPU drivers link
# against the host glibc.
rm -f "$APPDIR"/usr/lib/ld-linux*.so* \
      "$APPDIR"/usr/lib/libanl.so* \
      "$APPDIR"/usr/lib/libBrokenLocale.so* \
      "$APPDIR"/usr/lib/libc.so* \
      "$APPDIR"/usr/lib/libc_malloc_debug.so* \
      "$APPDIR"/usr/lib/libdl.so* \
      "$APPDIR"/usr/lib/libm.so* \
      "$APPDIR"/usr/lib/libmvec.so* \
      "$APPDIR"/usr/lib/libnss_compat.so* \
      "$APPDIR"/usr/lib/libnss_dns.so* \
      "$APPDIR"/usr/lib/libnss_files.so* \
      "$APPDIR"/usr/lib/libnss_hesiod.so* \
      "$APPDIR"/usr/lib/libpthread.so* \
      "$APPDIR"/usr/lib/libresolv.so* \
      "$APPDIR"/usr/lib/librt.so* \
      "$APPDIR"/usr/lib/libthread_db.so* \
      "$APPDIR"/usr/lib/libutil.so*
rm -rf "$APPDIR/usr/lib/gconv" "$APPDIR/usr/lib/audit"

# Remove system GPU libs — must come from host (kernel driver match required).
# CEF's own libs in usr/bin/ are not affected. The libxcb/libX11/libwayland
# families are excluded whole: the AppImage excludelist ties them to Mesa,
# and splitting a family between host and bundle mismatches its symbols.
rm -rf "$APPDIR/usr/lib/dri" "$APPDIR/usr/lib/vdpau" "$APPDIR/usr/lib/gbm"
rm -f "$APPDIR"/usr/lib/libEGL.so* \
      "$APPDIR"/usr/lib/libEGL_mesa.so* \
      "$APPDIR"/usr/lib/libGL.so* \
      "$APPDIR"/usr/lib/libGLX*.so* \
      "$APPDIR"/usr/lib/libGLESv1_CM.so* \
      "$APPDIR"/usr/lib/libGLESv2.so* \
      "$APPDIR"/usr/lib/libGLdispatch.so* \
      "$APPDIR"/usr/lib/libOpenGL.so* \
      "$APPDIR"/usr/lib/libgbm.so* \
      "$APPDIR"/usr/lib/libvulkan.so* \
      "$APPDIR"/usr/lib/libdrm.so* \
      "$APPDIR"/usr/lib/libdrm_*.so* \
      "$APPDIR"/usr/lib/libxshmfence.so* \
      "$APPDIR"/usr/lib/libglapi.so* \
      "$APPDIR"/usr/lib/libvulkan_*.so* \
      "$APPDIR"/usr/lib/libxcb*.so* \
      "$APPDIR"/usr/lib/libX11.so* \
      "$APPDIR"/usr/lib/libX11-xcb.so* \
      "$APPDIR"/usr/lib/libwayland-*.so*

# Flatten libs from subdirs (e.g. pulseaudio/, pipewire-*/) into usr/lib so
# AppRun's single --library-path resolves them all. Hard copies, not symlinks.
find "$APPDIR/usr/lib" -mindepth 2 \( -name '*.so' -o -name '*.so.*' \) | while read lib; do
    base="$(basename "$lib")"
    [ ! -e "$APPDIR/usr/lib/$base" ] && cp -L "$lib" "$APPDIR/usr/lib/$base"
done

# Some Arch packages embed absolute paths in DT_NEEDED (e.g. /usr/lib/libmujs.so).
# The dynamic linker follows literal paths even with --library-path, so rewrite
# them to bare sonames.
find "$APPDIR/usr/lib" "$APPDIR/usr/bin" -type f \( -name '*.so*' -o -executable \) 2>/dev/null | while read f; do
    patchelf --print-needed "$f" 2>/dev/null | grep '^/' | while read lib; do
        base="$(basename "$lib")"
        patchelf --replace-needed "$lib" "$base" "$f" 2>/dev/null || true
    done
done

# AppDir root files (per AppImage spec)
cp "$APPDIR/usr/share/applications/net.nullsum.JelliumDesktop.desktop" "$APPDIR/"
cp "$APPDIR/usr/share/icons/hicolor/scalable/apps/net.nullsum.JelliumDesktop.svg" "$APPDIR/"
ln -sf net.nullsum.JelliumDesktop.svg "$APPDIR/.DirIcon"
cp /src/dev/linux/appimage/AppRun "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"

# Package
ARCH="$ARCH" /opt/tools/appimagetool/AppRun --no-appstream \
    --runtime-file "/opt/tools/runtime-${ARCH}" \
    "$APPDIR" "/host-output/JelliumDesktop-${VERSION}-${ARCH}.AppImage"
