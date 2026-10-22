# Immagine minimale (binario statico musl) del server DecoRemote.
# Build dalla radice del repository:  docker build -f deploy/Dockerfile -t decoremote-server .
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p decoremote-server

FROM scratch
COPY --from=build /src/target/release/decoremote-server /decoremote-server
USER 65534:65534
EXPOSE 21116/tcp 21116/udp 21117/udp
ENTRYPOINT ["/decoremote-server"]
CMD ["0.0.0.0:21116"]
