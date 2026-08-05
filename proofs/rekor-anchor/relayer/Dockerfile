FROM node:22-alpine
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
