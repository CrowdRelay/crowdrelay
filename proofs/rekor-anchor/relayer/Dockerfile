# Pinned by digest: `22-alpine` is a rolling tag.
FROM node:26-alpine@sha256:aadf416b2cdce311a8811ba3f0608a61b77dbf997500e2eafe781b51f6a0b019
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
