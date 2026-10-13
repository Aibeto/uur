FROM alpine:edge AS build

RUN apk add --no-cache \
    bash cargo rust gcc musl-dev pipewire-dev wayland-dev pkgconf \
    mingw-w64-gcc mingw-w64-binutils

WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo test --release --locked \
    && cargo build --release --locked \
    && ./hook/build.sh \
    && ./capture/build.sh \
    && ./packaging/stage.sh /out

FROM alpine:edge
RUN apk add --no-cache \
    bash ca-certificates curl glib pipewire-libs wayland-libs-client pulseaudio-utils wine xdg-desktop-portal
COPY --from=build /out/ /
ENTRYPOINT ["/usr/bin/uur"]
