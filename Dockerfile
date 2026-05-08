# Two-stage Docker build for the control plane.
# Stage 1 compiles better-sqlite3 against Node 22; stage 2 ships only runtime.

FROM node:22-bookworm-slim AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY apps/control-plane/dist/package.json ./
RUN npm install --omit=dev --no-audit --no-fund

FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/node_modules ./node_modules
COPY apps/control-plane/dist/control-plane.mjs ./
# Static web dashboard (built via `pnpm --filter @sprite-machine/mobile export:web`)
COPY apps/mobile/dist-web /app/web
ENV NODE_ENV=production \
    HOST=0.0.0.0 \
    PORT=8787 \
    CONTROL_PLANE_DB_PATH=/data/control-plane.sqlite \
    STATIC_WEB_DIR=/app/web
EXPOSE 8787
CMD ["node", "control-plane.mjs"]
