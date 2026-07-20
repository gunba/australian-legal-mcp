# syntax=docker/dockerfile:1.7

FROM docker.io/library/rust@sha256:4c2fd73ef19c5ef9d54bee03b06b2839a392604fbfcd578ed948b71b37c1d7fb AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/legal-model/Cargo.toml crates/legal-model/Cargo.toml
COPY crates/legal-model/src crates/legal-model/src
COPY crates/legal-source-sdk/Cargo.toml crates/legal-source-sdk/Cargo.toml
COPY crates/legal-source-sdk/src crates/legal-source-sdk/src
COPY src src
RUN cargo build --release --locked --features vendored-openssl \
 && strip target/release/legal-mcp

FROM docker.io/library/debian@sha256:63a496b5d3b99214b39f5ed70eb71a61e590a77979c79cbee4faf991f8c0783e AS onnxruntime
ARG ONNXRUNTIME_VERSION=1.25.0
ARG ONNXRUNTIME_SHA256=e0a8998e70416801f9a634a8ea1d369a255ff109741469f9d99cf369a46a1492
RUN apt-get update \
 && apt-get install --yes --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && archive="onnxruntime-linux-x64-${ONNXRUNTIME_VERSION}.tgz" \
 && curl --fail --location --proto '=https' --tlsv1.2 --retry 5 \
      --output "/tmp/${archive}" \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/${archive}" \
 && echo "${ONNXRUNTIME_SHA256}  /tmp/${archive}" | sha256sum --check - \
 && mkdir /opt/onnxruntime \
 && tar --extract --gzip --file "/tmp/${archive}" --directory /opt/onnxruntime \
      --strip-components=1 --no-same-owner --no-same-permissions \
 && test -f "/opt/onnxruntime/lib/libonnxruntime.so.${ONNXRUNTIME_VERSION}" \
 && test -f /opt/onnxruntime/LICENSE \
 && test -f /opt/onnxruntime/ThirdPartyNotices.txt \
 && rm "/tmp/${archive}"

FROM docker.io/library/debian@sha256:63a496b5d3b99214b39f5ed70eb71a61e590a77979c79cbee4faf991f8c0783e
ARG VERSION=0.19.9
ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="Australian Legal MCP" \
      org.opencontainers.image.description="Source-grounded Australian legal MCP server" \
      org.opencontainers.image.source="https://github.com/gunba/australian-legal-mcp" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="MIT" \
      io.australian-legal-mcp.ann-format="flat-int8-v1"
RUN apt-get update \
 && apt-get install --yes --no-install-recommends ca-certificates libgomp1 libstdc++6 \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 971 legal-mcp \
 && useradd --uid 971 --gid 971 --home-dir /nonexistent --no-create-home \
      --shell /usr/sbin/nologin legal-mcp \
 && install -d -o root -g legal-mcp -m 0550 \
      /opt/legal-mcp /opt/legal-mcp/lib /var/lib/legal-mcp \
      /var/lib/legal-mcp/generations /var/lib/legal-mcp/lifecycle \
 && install -d -o legal-mcp -g legal-mcp -m 0700 /var/lib/legal-mcp/state \
 && install -d -o root -g root -m 0755 /run/secrets
COPY --from=builder --chmod=0555 /src/target/release/legal-mcp /usr/local/bin/legal-mcp
COPY --from=onnxruntime --chmod=0444 /opt/onnxruntime/lib/libonnxruntime.so.1.25.0 /opt/legal-mcp/lib/libonnxruntime.so.1.25.0
COPY --from=onnxruntime --chmod=0444 /opt/onnxruntime/LICENSE /opt/legal-mcp/ONNXRUNTIME-LICENSE
COPY --from=onnxruntime --chmod=0444 /opt/onnxruntime/ThirdPartyNotices.txt /opt/legal-mcp/ONNXRUNTIME-THIRD-PARTY-NOTICES
ENV LEGAL_MCP_DATA_DIR=/var/lib/legal-mcp \
    ORT_DYLIB_PATH=/opt/legal-mcp/lib/libonnxruntime.so.1.25.0 \
    MALLOC_ARENA_MAX=4
RUN legal-mcp verify-runtime | grep -F '"onnx_runtime_ready":true'
USER 971:971
WORKDIR /var/lib/legal-mcp
EXPOSE 51235
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/legal-mcp", "healthcheck", "--port", "51235"]
ENTRYPOINT ["/usr/local/bin/legal-mcp"]
CMD ["serve", "--port", "51235", "--network-scope", "container", "--require-http-auth", "--require-ready-corpus"]
