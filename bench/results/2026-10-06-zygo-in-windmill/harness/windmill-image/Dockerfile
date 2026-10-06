# Windmill's worker image with what a networked Zygo sandbox needs: pasta for its packets,
# nftables for its allowlist. pasta is moved off /usr/bin because Ubuntu hosts load an
# AppArmor profile that attaches to that path, inside containers too.
FROM ghcr.io/windmill-labs/windmill:main
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends passt nftables iproute2 >/dev/null \
    && cp -L /usr/bin/pasta /usr/local/bin/pasta && rm /usr/bin/pasta \
    && rm -rf /var/lib/apt/lists/*
