
# Getestet wird gegen die neueste PostgreSQL-Version
FROM docker.io/library/postgres:19beta4-trixie

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential \
      clang \
      llvm \
      git \
      make \
      postgresql-server-dev-19 \
      libclang-dev \
      gdb \
      pkg-config \
      curl \
      ca-certificates \
      libssl-dev \
      sudo \
      npm \
    && rm -rf /var/lib/apt/lists/*

    # Rust installieren
    ENV RUSTUP_HOME=/opt/rustup
    ENV CARGO_HOME=/opt/cargo
    ENV PATH="/opt/cargo/bin:/root/.opencode/bin:${PATH}"
    RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal 
    RUN rustup component add clippy

    # pgrx für die PostgreSQL-Extension; nach /usr/local, weil /opt/cargo ein Volume ist
    ARG PGRX_VERSION=0.19.3
    RUN cargo install --locked --root /usr/local cargo-pgrx --version "$PGRX_VERSION" \
    && cargo pgrx init --pg19 /usr/lib/postgresql/19/bin/pg_config

    # OpenCode 2 installieren
    RUN curl -fsSL https://opencode.ai/v2/install | bash

    # MCP-Adapter aus dem Fork (HTTP-Fix + OpenCode-2-Unterstützung, PRs an upstream offen)
    ARG OPENCODE_MCP_REPO=https://github.com/Beerlesklopfer/opencode-mcp.git
    ARG OPENCODE_MCP_REF=6a1645a63a73b44fb2ddb2d22143af157141ab4b
    RUN git clone "$OPENCODE_MCP_REPO" /opt/opencode-mcp \
    && cd /opt/opencode-mcp \
    && git checkout "$OPENCODE_MCP_REF" \
    && npm ci \
    && npm run build \
    && test -f dist/index.js

    # OpenCode im PATH verfügbar machen
    ENV PATH="/root/.opencode/bin:${PATH}"
    # OpenCode-Konfiguration
    ENV OPENCODE_SERVER_URL=http://127.0.0.1:4096
    ENV OPENCODE_DEFAULT_PROJECT=/workspace

    # MCP HTTP-Konfiguration
    ENV MCP_TRANSPORT=http
    ENV MCP_HTTP_HOST=0.0.0.0
    ENV MCP_HTTP_PORT=14096

    # Installation prüfen
    RUN opencode --version

    WORKDIR /workspace

    # Installation prüfen
    RUN rustc --version && cargo --version

    # Startskript
    COPY scripts/start.sh /usr/local/bin/start.sh
    RUN chmod +x /usr/local/bin/start.sh

    WORKDIR /workspace

# Persistente Verzeichnisse
VOLUME ["/var/lib/postgresql", \
        "/workspace", \
        "/opt/cargo", \
        "/opt/rustup"]
EXPOSE 5432 4096
ENTRYPOINT ["/bin/bash", "/usr/local/bin/start.sh"]
