# Pinned by digest: `22-alpine` is a rolling tag.
FROM node:26-alpine@sha256:ef24c5053d50fdc3e4e56eb4e7ddb7861874ab0fdc797046ba897581deb8e868
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
