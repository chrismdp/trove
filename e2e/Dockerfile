# Runtime image for the trove e2e. The build context is staged by
# `scripts/e2e.sh` (and the e2e workflow) with the release binary + libjfs +
# entrypoint already in it, so the image build is fast and reuses exactly the
# artefacts the release pipeline produces — no Rust/Go toolchain in the image.
# Base must ship glibc >= the build host's: trove is built on Ubuntu (locally
# and on `ubuntu-latest` runners), so it needs glibc 2.39. ubuntu:24.04 matches;
# a Debian-stable base (bookworm = 2.36) fails with `GLIBC_2.39 not found`.
# (libjfs is cgo/glibc, so a musl-static binary isn't an option.)
FROM ubuntu:24.04

RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# trove and libjfs sit side by side: build.rs sets an $ORIGIN rpath so the
# binary finds the shared library next to itself.
COPY trove /usr/local/bin/trove
COPY libjfs-amd64.so /usr/local/bin/libjfs-amd64.so
COPY docker-entrypoint.sh /usr/local/bin/trove-mount
RUN chmod +x /usr/local/bin/trove /usr/local/bin/trove-mount

ENV VAULT_DIR=/vault/notes
ENTRYPOINT ["trove-mount"]
