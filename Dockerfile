FROM rust:bullseye AS build

ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

RUN apt update && apt install -y \
  ca-certificates \
  pkg-config \
  libssl-dev \
  libclang-11-dev \
  wget \
  tar

COPY . /src/surfpool

WORKDIR /src/surfpool/

RUN mkdir /out

RUN cargo build --release --bin surfpool --locked

RUN cp /src/surfpool/target/release/surfpool /out

FROM debian:bullseye-slim

# Set default network host
ENV SURFPOOL_NETWORK_HOST=127.0.0.1

RUN apt update && apt install -y ca-certificates libssl-dev

COPY --from=build /out/ /bin/

WORKDIR /workspace

EXPOSE 8899 8900 18488

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
