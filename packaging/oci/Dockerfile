# The Zygo container image.
#
# Alpine, because the binary is already a static musl one (requirement N6) and
# the only reason for a userland at all is the four programs `network =
# "egress"` needs: `pasta` to move packets, `nft` for the allowlist, `tc` for
# the bandwidth limit, and `newuidmap` for a non-root uid range.
#
# The binary comes from the **build context** rather than from a builder stage
# here. The release workflow has already built, size-checked and run it for
# each architecture (`poc/check_dist.sh`, requirement N6), and compiling it a
# second time inside Docker would produce a *different* binary from the one
# that was checked — the one thing a release image must not do. `make
# oci-image` does the same locally.
#
#     docker build -f packaging/oci/Dockerfile -t zygo packaging/oci
#
# What this image needs to run is not `--privileged`; it is three things, and
# `zygo doctor` names each one when it is missing. See `docs/guide.md`
# ("Inside a container") and `examples/kubernetes/`.

FROM alpine:3.21

# `pasta`, `nft`, `tc`: `network = "egress"` is the only feature with
#   userland dependencies, and a host without them refuses egress with a
#   reason rather than falling back to an open network.
# `newuidmap`/`newgidmap`: how a non-root uid writes a range into a user
#   namespace's id map. Without them a non-root Zygo can map exactly one uid,
#   and every sandbox runs as the same one.
# `ca-certificates`: pulling images from a registry over TLS.
RUN apk add --no-cache \
        passt \
        nftables \
        iproute2 \
        shadow-uidmap \
        ca-certificates \
    && rm -rf /var/cache/apk/*

# A uid of its own, and a subordinate range for the sandboxes it builds.
#
# 65532 is the number `distroless` uses for `nonroot`; keeping it means an
# image built on top of this one can share a `runAsUser` with the rest of a
# cluster's manifests. The range is 65536 wide because that is what a sandbox
# maps, and `/etc/subuid` is where `newuidmap` looks.
RUN addgroup -g 65532 -S zygo \
    && adduser -u 65532 -S -G zygo -h /var/lib/zygo zygo \
    && echo "zygo:100000:65536" > /etc/subuid \
    && echo "zygo:100000:65536" > /etc/subgid \
    && mkdir -p /var/lib/zygo /run/zygo \
    && chown -R 65532:65532 /var/lib/zygo /run/zygo

COPY --chmod=0755 zygo /usr/local/bin/zygo

# Images, scripts, blobs, tenants, tokens and secrets. A volume because none
# of it is disposable: losing it loses an embedder's customers, not a cache.
ENV ZYGO_DATA_HOME=/var/lib/zygo
VOLUME ["/var/lib/zygo"]

USER 65532:65532
EXPOSE 7700

# `/healthz` is the route that answers without a token and says whether this
# host should be sent work — which is exactly what a health check is asking.
# `--spider` rather than a body: the status is the answer.
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD wget -q --spider http://127.0.0.1:7700/healthz || exit 1

# The API, on every interface, because a container's loopback is its own.
# **Call-only** by default: a token reaches the functions somebody declared
# and nothing else. `--allow-deploy` is a deliberate argument, not a default,
# because deploy rights over HTTP are a shell.
ENTRYPOINT ["/usr/local/bin/zygo"]
CMD ["api", "--listen", "0.0.0.0:7700"]
