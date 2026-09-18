FROM rust:bookworm AS build

ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  pkg-config \
  libssl-dev \
  libclang-dev \
  wget \
  tar \
  && rm -rf /var/lib/apt/lists/*

COPY . /src/surfpool

WORKDIR /src/surfpool/

RUN mkdir /out

RUN cargo build --release --bin surfpool --locked

RUN cp /src/surfpool/target/release/surfpool /out

FROM debian:bookworm-slim

# Bind on all container interfaces, but advertise localhost by default so
# local Docker users get client-friendly URLs without extra configuration.
ENV SURFPOOL_NETWORK_HOST=0.0.0.0
ENV SURFPOOL_PUBLIC_HOST=127.0.0.1

RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates \
  curl \
  libssl-dev \
  && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/ /bin/

WORKDIR /workspace

EXPOSE 8899 8900 18488

HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD curl --fail --silent --output /dev/null \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
    http://127.0.0.1:8899

# Create a shell script that provides default behavior
RUN echo '#!/bin/bash\n\
# Default behavior for surfpool\n\
# Can be overridden by passing arguments to docker run\n\
\n\
if [ $# -eq 0 ]; then\n\
    # Default behavior - start surfnet with default configuration\n\
    echo "Starting surfpool with default configuration..."\n\
    exec surfpool start --no-tui\n\
else\n\
    # Use provided arguments\n\
    # Note: make sure the cli argument "--no-tui" is being provided.\n\
    echo "Starting surfpool with custom arguments: $@"\n\
    exec surfpool "$@"\n\
fi' > /usr/local/bin/entrypoint.sh && chmod +x /usr/local/bin/entrypoint.sh

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
