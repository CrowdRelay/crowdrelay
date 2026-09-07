# Pinned by digest: `22-alpine` is a rolling tag.
FROM node:26-alpine@sha256:2d984a15c9b54fd0aeb608b8e0d0d83529eb34d2966db27a1fb4f1edc3d298a3
WORKDIR /app
ENV NODE_ENV=production
COPY package.json ./
COPY index.mjs proof-utils.mjs batch-runner.mjs ./
RUN mkdir -p /data \
    && chown -R node:node /app /data \
    && chmod 700 /data
USER node
EXPOSE 8081
CMD ["node", "index.mjs"]
