
FROM docker.io/library/postgres:19beta4-trixie

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential \
      clang \
      llvm \
      git \
      make \
      postgresql-server-dev-19 \
      postgresql-contrib-19 \
      gdb \
      pkg-config \
      curl \
      ca-certificates \
      build-essential \
      pkg-config \
      libssl-dev \
      npm \
    && rm -rf /var/lib/apt/lists/*

    # Rust installieren
    ENV RUSTUP_HOME=/opt/rustup
    ENV CARGO_HOME=/opt/cargo
    ENV PATH="/opt/cargo/bin:/root/.opencode/bin:${PATH}"
    RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal 

    # OpenCode 2 installieren
    RUN curl -fsSL https://opencode.ai/v2/install | bash

    # MCP-Adapter installieren und bauen
    RUN git clone \
        https://github.com/Traves-Theberge/opencode-mcp.git \
        /opt/opencode-mcp \
    && cd /opt/opencode-mcp \
    && sed -i \
        "/httpPort: process.env.MCP_HTTP_PORT ? parseInt(process.env.MCP_HTTP_PORT, 10) : undefined,/a\\        httpHostname: process.env.MCP_HTTP_HOST ?? '127.0.0.1'," \
        src/server/mcp.ts \
    && grep -q "httpHostname: process.env.MCP_HTTP_HOST" src/server/mcp.ts \
    && npm install \
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
