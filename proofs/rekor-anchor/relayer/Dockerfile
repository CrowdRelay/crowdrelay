# Pinned by digest: `22-alpine` is a rolling tag.
FROM node:26-alpine@sha256:dbaa92e5758cbbcf85d65d5403fdb530fe3442cbe8c6dbfb7ef23365450d5070
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
