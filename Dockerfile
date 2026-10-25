# syntax=docker/dockerfile:1

FROM rust:1.82-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY protocol ./protocol
COPY client-utils ./client-utils
COPY applications/server-click ./applications/server-click
RUN (cargo update -p time@0.3.21 --precise 0.3.36 || true) \
 && cargo build --release -p server-click \
 && cp target/release/server-click /server-click

FROM node:20-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV NODE_ENV=production \
    PORT=3005 \
    DATA_DIR=/data \
    SERVER_CLICK_BIN=/app/bin/server-click
WORKDIR /app
COPY package*.json ./
RUN npm install --omit=dev
COPY server.js ./
COPY public ./public
COPY --from=rust-build /server-click ./bin/server-click
RUN mkdir -p /data
VOLUME ["/data"]
EXPOSE 3005
HEALTHCHECK --interval=30s --timeout=10s --retries=3 CMD node -e "fetch('http://127.0.0.1:3005/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["node", "server.js"]
