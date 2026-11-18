# Source build, AppDir assembly, and packaging all happen at
# `podman run` time via container-build.sh (bind-mounted from the repo at
# /src/dev/linux/appimage/container-build.sh), with host bind mounts for
# incremental state. See container-build.sh and dev/linux/appimage/appimage.just.
FROM fedora:44
# RPM Fusion provides the full FFmpeg with the complete decoder set, so mpv
# can decode the media and the device profile advertises it to the server.
# ffmpeg-devel below pulls it in; --allowerasing handles the swap for the
# libplacebo/libass deps.
RUN dnf -y upgrade && \
    dnf -y install \
        "https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm" \
        "https://mirrors.rpmfusion.org/nonfree/fedora/rpmfusion-nonfree-release-$(rpm -E %fedora).noarch.rpm" && \
    dnf -y install --allowerasing \
        gcc gcc-c++ make cmake git ninja-build meson python3 pkgconf-pkg-config \
        cargo rust \
        clang-devel \
        glibc-devel \
        alsa-lib-devel at-spi2-core-devel at-spi2-atk-devel atk-devel cairo-devel cups-devel \
        libXcomposite-devel libXdamage-devel libxkbcommon-devel \
        libxkbcommon-x11-devel libXrandr-devel nss-devel nspr-devel pango-devel \
        ffmpeg-devel libass-devel wayland-devel \
        glslang-devel libshaderc-devel spirv-tools-devel lcms2-devel \
        libdovi-devel libunwind-devel xxhash-devel \
        python3-jinja2 python3-mako python3-markupsafe \
        libXpresent-devel libXScrnSaver-devel \
        vulkan-headers vulkan-loader-devel wayland-protocols-devel python3-docutils \
        libglvnd-devel mesa-libGL-devel mesa-libEGL-devel mesa-libgbm-devel \
        systemd-devel libdrm-devel libX11-devel xcb-util-cursor-devel \
        pipewire-devel luajit-devel rubberband-devel uchardet-devel \
        xcb-util-devel \
        plasma-wayland-protocols-devel \
        wget file patchelf desktop-file-utils && \
    dnf clean all

ARG LIBPLACEBO_VERSION=v7.387.0
ARG MPV_VERSION=v0.42.0

# libplacebo for mpv; same tag as the Flatpak manifest and
# build-macos-legacy.yml. Installed to /usr/local so it coexists with
# Fedora's libplacebo that ffmpeg links. PKG_CONFIG_PATH makes mpv's meson
# pick it; container-build.sh bundles the resulting .so into the AppDir.
RUN git clone --depth 1 --recursive --branch "${LIBPLACEBO_VERSION}" \
        https://github.com/haasn/libplacebo.git /tmp/libplacebo && \
    meson setup /tmp/libplacebo/build /tmp/libplacebo \
        --prefix=/usr/local --libdir=lib64 --buildtype=release \
        -Ddemos=false -Dtests=false -Dd3d11=disabled && \
    meson install -C /tmp/libplacebo/build --strip && \
    rm -rf /tmp/libplacebo
ENV PKG_CONFIG_PATH=/usr/local/lib64/pkgconfig

# `cargo xtask build` finds this libmpv through pkg-config; ldconfig lets the
# linker resolve its libplacebo dependency. container-build.sh bundles the .so.
RUN git clone --depth 1 --branch "${MPV_VERSION}" \
        https://github.com/mpv-player/mpv.git /tmp/mpv && \
    meson setup /tmp/mpv/build /tmp/mpv \
        --prefix=/usr/local --libdir=lib64 --default-library=shared \
        -Dlibmpv=true -Dcplayer=false -Dlua=luajit && \
    meson install -C /tmp/mpv/build --strip && \
    rm -rf /tmp/mpv && \
    echo /usr/local/lib64 > /etc/ld.so.conf.d/usr-local-lib64.conf && \
    ldconfig

WORKDIR /opt/tools
RUN ARCH="$(uname -m)" && \
    wget -q "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage" && \
    chmod +x "appimagetool-${ARCH}.AppImage" && \
    "./appimagetool-${ARCH}.AppImage" --appimage-extract && \
    mv squashfs-root appimagetool && rm "appimagetool-${ARCH}.AppImage" && \
    wget -q -O "/opt/tools/runtime-${ARCH}" \
    "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-${ARCH}"

